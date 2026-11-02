//! Explicit asynchronous Run action. Construction and validation are inert.
//! A launch acknowledgement means CreateProcess succeeded, not that the child
//! finished its work. Cancellation stops waiting/reading; it never kills an
//! external program. Native subprocess execution is not used by these tests.
use anyhow::{Context, Result, ensure};
use jiks_core::automation::{AutomationEnvironment, RunOptions};
use std::{
    io::Read,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const OUTPUT_LIMIT: usize = 1024 * 1024;
pub const ERROR_LIMIT: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct ProcessRequest {
    pub options: RunOptions,
    pub working_directory: Option<String>,
    pub environment: AutomationEnvironment,
}
impl ProcessRequest {
    pub fn validate(&self) -> Result<()> {
        for text in self
            .options
            .script_path
            .iter()
            .chain(Some(&self.options.script_arguments))
            .chain(self.working_directory.iter())
        {
            ensure!(!text.contains('\0'), "Process strings cannot contain NUL");
        }
        let command_units = self
            .options
            .script_path
            .as_ref()
            .map_or(0, |s| s.encode_utf16().count())
            .saturating_add(self.options.script_arguments.encode_utf16().count())
            .saturating_add(4);
        ensure!(
            command_units <= 32767,
            "Windows process command line exceeds UTF16 limit"
        );
        ensure!(
            self.environment.variables.len() <= 1024,
            "Process environment variable capacity exceeded"
        );
        let mut bytes = 0usize;
        for (key, value) in &self.environment.variables {
            ensure!(
                !key.is_empty() && !key.contains(['\0', '=']),
                "Invalid process environment variable name"
            );
            if let Some(value) = value {
                ensure!(
                    !value.contains('\0'),
                    "Process environment value contains NUL"
                );
            }
            bytes = bytes
                .saturating_add(key.len())
                .saturating_add(value.as_ref().map_or(0, String::len));
        }
        ensure!(
            bytes <= 1024 * 1024,
            "Process environment capacity exceeded"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    Skipped,
    Spawned,
    Exited,
    Cancelled,
    Failed,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutcome {
    pub status: ProcessStatus,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub failure: Option<String>,
    pub message: String,
}
impl ProcessOutcome {
    fn empty(status: ProcessStatus, message: impl Into<String>) -> Self {
        Self {
            status,
            pid: None,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            failure: None,
            message: message.into(),
        }
    }
    fn failed(error: impl Into<String>) -> Self {
        let error = error.into();
        let mut result = Self::empty(ProcessStatus::Failed, "Process action failed");
        result.failure = Some(error);
        result
    }
    fn cancelled(pid: Option<u32>) -> Self {
        let mut result = Self::empty(
            ProcessStatus::Cancelled,
            "Stopped waiting; external process was not terminated",
        );
        result.pid = pid;
        result.failure = Some("Process action cancelled".into());
        result
    }
    /// Legacy CheckInstance/missing-path no-op leaves the previous RUN_OUTPUT.
    /// A successfully detached launch supplies the legacy empty output.
    pub fn run_output(&self) -> Option<&str> {
        matches!(self.status, ProcessStatus::Spawned | ProcessStatus::Exited)
            .then_some(self.stdout.as_str())
    }
}

pub trait ProcessTransport {
    /// Called only by explicit start after whole-request validation.
    fn start(&mut self, request: ProcessRequest) -> Result<()>;
    /// Return only after this transport's actual launch/wait/cleanup has ended.
    fn poll(&mut self) -> Option<ProcessOutcome>;
    fn cancel(&self);
}

pub struct ProcessRunner<T: ProcessTransport = NativeProcessTransport> {
    transport: T,
    pending: bool,
    ready: Option<ProcessOutcome>,
}
impl<T: ProcessTransport> ProcessRunner<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            pending: false,
            ready: None,
        }
    }
    pub fn start(&mut self, request: ProcessRequest) -> Result<()> {
        ensure!(
            !self.pending && self.ready.is_none(),
            "Previous process action has not completed"
        );
        request.validate()?;
        if request
            .options
            .script_path
            .as_ref()
            .is_none_or(|s| s.trim().is_empty())
        {
            self.ready = Some(ProcessOutcome::empty(
                ProcessStatus::Skipped,
                "No executable configured",
            ));
            return Ok(());
        }
        self.transport.start(request)?;
        self.pending = true;
        Ok(())
    }
    pub fn is_busy(&self) -> bool {
        self.pending
    }
    pub fn poll(&mut self) -> Option<ProcessOutcome> {
        if let Some(ready) = self.ready.take() {
            return Some(ready);
        }
        if !self.pending {
            return None;
        }
        let result = self.transport.poll()?;
        self.pending = false;
        Some(result)
    }
    pub fn cancel(&self) {
        if self.pending {
            self.transport.cancel();
        }
    }
}
impl<T: ProcessTransport> Drop for ProcessRunner<T> {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Default)]
pub struct NativeProcessTransport {
    cancel: Arc<AtomicBool>,
    join: Option<JoinHandle<ProcessOutcome>>,
}
impl NativeProcessTransport {
    pub fn new() -> Self {
        Self::default()
    }
}
impl ProcessTransport for NativeProcessTransport {
    fn start(&mut self, request: ProcessRequest) -> Result<()> {
        ensure!(
            self.join.is_none(),
            "Previous native process worker still owned"
        );
        request.validate()?;
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        self.join = Some(
            thread::Builder::new()
                .name("jiks-explicit-process".into())
                .spawn(move || {
                    native_run(request, &worker_cancel)
                        .unwrap_or_else(|e| ProcessOutcome::failed(format!("{e:#}")))
                })?,
        );
        self.cancel = cancel;
        Ok(())
    }
    fn poll(&mut self) -> Option<ProcessOutcome> {
        if !self.join.as_ref()?.is_finished() {
            return None;
        }
        Some(
            self.join
                .take()
                .expect("finished process worker")
                .join()
                .unwrap_or_else(|_| ProcessOutcome::failed("Native process worker panicked")),
        )
    }
    fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
}
impl Drop for NativeProcessTransport {
    fn drop(&mut self) {
        self.cancel(); /* Worker and drains retain owned handles until completion. No Kill/TerminateProcess. */
    }
}

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
}
impl Capture {
    fn append(&mut self, bytes: &[u8], limit: usize) {
        let take = limit.saturating_sub(self.bytes.len()).min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..take]);
        self.truncated |= take != bytes.len();
    }
}
pub fn decode_output(bytes: &[u8]) -> String {
    if let Some(bytes) = bytes.strip_prefix(&[0xff, 0xfe]) {
        let units: Vec<_> = bytes
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else if let Some(bytes) = bytes.strip_prefix(&[0xfe, 0xff]) {
        let units: Vec<_> = bytes
            .chunks_exact(2)
            .map(|p| u16::from_be_bytes([p[0], p[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
            .into_owned()
    }
}

/// This intentionally preserves old RunAutomationStep's case-sensitive path
/// Contains(ProcessName) CheckInstance rule. It is not executable identity or a
/// security boundary, and never terminates or attaches to a matching process.
pub fn matches_legacy_instance(script_path: &str, process_name: &str) -> bool {
    !process_name.is_empty() && script_path.contains(process_name)
}

#[cfg(windows)]
fn check_instance(script_path: &str) -> Result<bool> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::*,
    };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    ensure!(
        snapshot != INVALID_HANDLE_VALUE,
        "CheckInstance snapshot failed"
    );
    struct Handle(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
    let _snapshot = Handle(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    ensure!(
        unsafe { Process32FirstW(snapshot, &mut entry) } != 0,
        "CheckInstance first process failed"
    );
    let mut count = 0;
    loop {
        count += 1;
        ensure!(count <= 8192, "CheckInstance process capacity exceeded");
        let length = entry
            .szExeFile
            .iter()
            .position(|u| *u == 0)
            .unwrap_or(entry.szExeFile.len());
        let file = String::from_utf16_lossy(&entry.szExeFile[..length]);
        let name = if file.to_ascii_lowercase().ends_with(".exe") {
            &file[..file.len() - 4]
        } else {
            &file
        };
        if matches_legacy_instance(script_path, name) {
            return Ok(true);
        }
        if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
            ensure!(
                unsafe { GetLastError() } == 18,
                "CheckInstance incomplete snapshot"
            );
            break;
        }
    }
    Ok(false)
}

#[cfg(windows)]
fn drain_pipe(
    mut reader: impl Read + std::os::windows::io::AsRawHandle + Send + 'static,
    capture: Arc<Mutex<Capture>>,
    limit: usize,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<Result<()>>> {
    use windows_sys::Win32::{Foundation::GetLastError, System::Pipes::PeekNamedPipe};
    Ok(thread::Builder::new()
        .name("jiks-process-output".into())
        .spawn(move || {
            let mut buffer = [0u8; 8192];
            while !stop.load(Ordering::Acquire) {
                let mut available = 0;
                if unsafe {
                    PeekNamedPipe(
                        reader.as_raw_handle(),
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null_mut(),
                        &mut available,
                        std::ptr::null_mut(),
                    )
                } == 0
                {
                    let error = unsafe { GetLastError() };
                    if matches!(error, 109 | 232 | 233) {
                        return Ok(());
                    }
                    ensure!(false, "Process pipe peek failed: {error}");
                }
                if available == 0 {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let count = (available as usize).min(buffer.len());
                let count = reader
                    .read(&mut buffer[..count])
                    .context("Process output read failed")?;
                if count == 0 {
                    return Ok(());
                }
                capture
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Process capture poisoned"))?
                    .append(&buffer[..count], limit);
            }
            Ok(())
        })?)
}

struct Drains {
    stop: Arc<AtomicBool>,
    joins: Vec<JoinHandle<Result<()>>>,
}
impl Drains {
    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        let mut failures = Vec::new();
        for join in self.joins.drain(..) {
            match join.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => failures.push(format!("{e:#}")),
                Err(_) => failures.push("Output drainer panicked".into()),
            }
        }
        ensure!(failures.is_empty(), "{}", failures.join("; "));
        Ok(())
    }
}
impl Drop for Drains {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[cfg(windows)]
fn native_run(request: ProcessRequest, cancel: &AtomicBool) -> Result<ProcessOutcome> {
    use std::{
        os::windows::process::CommandExt,
        process::{Command, Stdio},
    };
    let script = request
        .options
        .script_path
        .as_deref()
        .context("No executable")?;
    if cancel.load(Ordering::Acquire) {
        return Ok(ProcessOutcome::cancelled(None));
    }
    if request.options.check_instance && check_instance(script)? {
        return Ok(if cancel.load(Ordering::Acquire) {
            ProcessOutcome::cancelled(None)
        } else {
            ProcessOutcome::empty(
                ProcessStatus::Skipped,
                "CheckInstance matched an existing process",
            )
        });
    }
    let mut command = Command::new(script);
    // raw_arg preserves the legacy Windows Arguments string. No cmd.exe,
    // PowerShell or additional quoting/argument splitting is introduced here.
    if !request.options.script_arguments.is_empty() {
        command.raw_arg(&request.options.script_arguments);
    }
    if let Some(directory) = request
        .working_directory
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        command.current_dir(directory);
    }
    for (key, value) in &request.environment.variables {
        if let Some(value) = value {
            command.env(key, value);
        } else {
            command.env_remove(key);
        }
    }
    let capture = request.options.run_silently && request.options.wait_until_finished;
    if request.options.run_silently {
        command.creation_flags(0x08000000).stdin(Stdio::null()); // CREATE_NO_WINDOW
        if capture {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
        } else {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    if cancel.load(Ordering::Acquire) {
        return Ok(ProcessOutcome::cancelled(None));
    }
    let mut child = command.spawn().context("CreateProcess failed")?;
    let pid = Some(child.id());
    let result = (|| -> Result<ProcessOutcome> {
        if !request.options.wait_until_finished {
            let mut result = if cancel.load(Ordering::Acquire) {
                ProcessOutcome::cancelled(pid)
            } else {
                ProcessOutcome::empty(
                    ProcessStatus::Spawned,
                    "Process launched; execution completion was not requested",
                )
            };
            result.pid = pid;
            return Ok(result);
        }
        let stdout = Arc::new(Mutex::new(Capture::default()));
        let stderr = Arc::new(Mutex::new(Capture::default()));
        let mut drains = Drains {
            stop: Arc::new(AtomicBool::new(false)),
            joins: Vec::new(),
        };
        if capture {
            drains.joins.push(drain_pipe(
                child.stdout.take().context("stdout pipe missing")?,
                stdout.clone(),
                OUTPUT_LIMIT,
                drains.stop.clone(),
            )?);
            drains.joins.push(drain_pipe(
                child.stderr.take().context("stderr pipe missing")?,
                stderr.clone(),
                ERROR_LIMIT,
                drains.stop.clone(),
            )?);
        }
        let exit = loop {
            if cancel.load(Ordering::Acquire) {
                drains.finish()?;
                return Ok(ProcessOutcome::cancelled(pid));
            }
            if let Some(exit) = child.try_wait().context("Process wait failed")? {
                break exit;
            }
            thread::sleep(Duration::from_millis(10));
        };
        // A grandchild can inherit these pipes after the requested child exits. Its
        // lifetime is not ours: bound this final drain and close our read ends.
        let deadline = Instant::now() + Duration::from_millis(500);
        while drains.joins.iter().any(|j| !j.is_finished())
            && Instant::now() < deadline
            && !cancel.load(Ordering::Acquire)
        {
            thread::sleep(Duration::from_millis(5));
        }
        let inherited_pipe_timeout = drains.joins.iter().any(|j| !j.is_finished());
        drains.finish()?;
        if cancel.load(Ordering::Acquire) {
            return Ok(ProcessOutcome::cancelled(pid));
        }
        let stdout = stdout
            .lock()
            .map_err(|_| anyhow::anyhow!("stdout capture poisoned"))?;
        let stderr = stderr
            .lock()
            .map_err(|_| anyhow::anyhow!("stderr capture poisoned"))?;
        let mut result =
            ProcessOutcome::empty(ProcessStatus::Exited, "Requested process actually exited");
        result.pid = pid;
        result.exit_code = exit.code();
        result.stdout = decode_output(&stdout.bytes);
        result.stderr = decode_output(&stderr.bytes);
        result.stdout_truncated = stdout.truncated;
        result.stderr_truncated = stderr.truncated;
        if stdout.truncated || stderr.truncated || inherited_pipe_timeout {
            result.failure=Some("Process output was truncated or a descendant kept its pipe open beyond the final drain deadline".into());
        }
        // Legacy RunAutomationStep ignored the numeric exit code, retaining stdout.
        // It is exposed accurately here; a nonzero code is not silently rewritten.
        Ok(result)
    })();
    Ok(result.unwrap_or_else(|error| {
        let mut outcome = ProcessOutcome::failed(format!(
            "Process already launched; waiting/output cleanup failed: {error:#}"
        ));
        outcome.pid = pid;
        outcome
    }))
}

#[cfg(not(windows))]
fn native_run(_: ProcessRequest, _: &AtomicBool) -> Result<ProcessOutcome> {
    anyhow::bail!("Native process transport requires Windows")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};
    #[derive(Default)]
    struct FakeClock(Cell<u64>);
    struct Fake {
        clock: Rc<FakeClock>,
        due: u64,
        started: usize,
        request: Option<ProcessRequest>,
        cancel: Cell<bool>,
        outcome: Option<ProcessOutcome>,
    }
    impl ProcessTransport for Fake {
        fn start(&mut self, request: ProcessRequest) -> Result<()> {
            self.started += 1;
            self.request = Some(request);
            Ok(())
        }
        fn poll(&mut self) -> Option<ProcessOutcome> {
            if self.clock.0.get() < self.due {
                return None;
            }
            if self.cancel.get() {
                Some(ProcessOutcome::cancelled(Some(99)))
            } else {
                self.outcome.take()
            }
        }
        fn cancel(&self) {
            self.cancel.set(true);
        }
    }
    fn request() -> ProcessRequest {
        ProcessRequest {
            options: RunOptions {
                script_path: Some("D:\\synthetic\\fixture.exe".into()),
                script_arguments: "--quoted \"two words\" & literal".into(),
                run_silently: true,
                wait_until_finished: true,
                check_instance: false,
            },
            working_directory: Some("D:\\synthetic".into()),
            environment: AutomationEnvironment::default(),
        }
    }
    fn fake(clock: Rc<FakeClock>, status: ProcessStatus) -> ProcessRunner<Fake> {
        ProcessRunner::new(Fake {
            clock,
            due: 100,
            started: 0,
            request: None,
            cancel: Cell::new(false),
            outcome: Some(ProcessOutcome::empty(status, "fake actual result")),
        })
    }
    #[test]
    fn constructors_validation_and_missing_executable_never_touch_transport() {
        let _native = NativeProcessTransport::new();
        let mut runner = fake(Rc::default(), ProcessStatus::Exited);
        assert_eq!(runner.transport.started, 0);
        let mut invalid = request();
        invalid.options.script_arguments.push('\0');
        assert!(runner.start(invalid).is_err());
        assert_eq!(runner.transport.started, 0);
        let mut empty = request();
        empty.options.script_path = None;
        runner.start(empty).unwrap();
        assert_eq!(runner.poll().unwrap().status, ProcessStatus::Skipped);
        assert_eq!(runner.transport.started, 0);
    }
    #[test]
    fn fake_actual_clock_completion_required_and_original_arguments_environment_are_owned() {
        let clock = Rc::new(FakeClock::default());
        let mut runner = fake(clock.clone(), ProcessStatus::Exited);
        let mut request = request();
        request
            .environment
            .variables
            .insert("LLT_ON_STARTUP".into(), Some("TRUE".into()));
        let expected = request.options.script_arguments.clone();
        runner.start(request).unwrap();
        assert!(runner.is_busy());
        assert!(runner.poll().is_none());
        assert!(runner.start(super::tests::request()).is_err());
        assert_eq!(
            runner
                .transport
                .request
                .as_ref()
                .unwrap()
                .options
                .script_arguments,
            expected
        );
        assert_eq!(
            runner
                .transport
                .request
                .as_ref()
                .unwrap()
                .environment
                .variables["LLT_ON_STARTUP"]
                .as_deref(),
            Some("TRUE")
        );
        clock.0.set(100);
        assert_eq!(runner.poll().unwrap().status, ProcessStatus::Exited);
        assert!(!runner.is_busy());
    }
    #[test]
    fn cancel_does_not_ack_until_transport_actual_cleanup_and_is_never_success() {
        let clock = Rc::new(FakeClock::default());
        let mut runner = fake(clock.clone(), ProcessStatus::Exited);
        runner.start(request()).unwrap();
        runner.cancel();
        assert!(runner.is_busy());
        assert!(runner.poll().is_none());
        clock.0.set(100);
        let result = runner.poll().unwrap();
        assert_eq!(result.status, ProcessStatus::Cancelled);
        assert!(result.failure.is_some());
        assert!(result.run_output().is_none());
    }
    #[test]
    fn bounded_capture_continues_draining_and_exposes_truncation() {
        let mut output = Capture::default();
        output.append(b"abcd", 3);
        output.append(b"more", 3);
        assert_eq!(output.bytes, b"abc");
        assert!(output.truncated);
        assert_eq!(decode_output(&[0xef, 0xbb, 0xbf, b'a']), "a");
        assert_eq!(decode_output(&[0xff, 0xfe, 0x2d, 0x4e]), "中");
        assert_eq!(decode_output(&[0xfe, 0xff, 0x4e, 0x2d]), "中");
    }
    #[test]
    fn legacy_noop_preserves_output_and_spawned_does_not_claim_exit() {
        assert!(
            ProcessOutcome::empty(ProcessStatus::Skipped, "existing")
                .run_output()
                .is_none()
        );
        let spawned = ProcessOutcome::empty(ProcessStatus::Spawned, "actual launch");
        assert_eq!(spawned.run_output(), Some(""));
        assert!(spawned.exit_code.is_none());
        assert!(matches_legacy_instance("D:\\fixture\\game.exe", "game"));
        assert!(!matches_legacy_instance("D:\\fixture\\game.exe", "GAME"));
        assert!(!matches_legacy_instance("D:\\fixture\\game.exe", ""));
    }
    #[test]
    fn all_environment_fields_validate_before_transport_admission() {
        let mut runner = fake(Rc::default(), ProcessStatus::Exited);
        let mut bad = request();
        bad.environment
            .variables
            .insert("valid".into(), Some("ok".into()));
        bad.environment
            .variables
            .insert("bad=name".into(), Some("bad".into()));
        assert!(runner.start(bad).is_err());
        assert_eq!(runner.transport.started, 0);
    }
}
