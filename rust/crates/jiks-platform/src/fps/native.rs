//! Windows SDK ETW control/consumer ABI. Only an explicitly started worker
//! reaches this module. There is no session enumeration or foreign-name stop.
use super::*;
use anyhow::{Context, anyhow, bail};
use std::{
    mem::{size_of, size_of_val, zeroed},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::{
        atomic::{AtomicI64, AtomicU32},
        mpsc::{Receiver, TryRecvError},
    },
    time::Duration,
};
use windows_sys::{
    Win32::{
        Foundation::*,
        System::{
            Diagnostics::Etw::*,
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            Threading::*,
        },
        UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
    },
    core::GUID,
};

const DXG: GUID = GUID::from_u128(0x802ec45a_1e99_4b83_9920_87c98277ba9d);
const PRESENT_ID: u16 = 0xb8;
const UNOPENED: u64 = u64::MAX;
fn tick() -> u64 {
    crate::domain::monotonic_ms()
}
fn qpc() -> Result<i64> {
    let mut value = 0;
    ensure!(
        unsafe { QueryPerformanceCounter(&mut value) } != 0 && value > 0,
        "QueryPerformanceCounter failed"
    );
    Ok(value)
}
fn win_error(stage: &str, code: u32) -> anyhow::Error {
    anyhow!("{stage} failed: Win32 {code} (0x{code:08x})")
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn add_loss(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
        Some(old.saturating_add(1))
    });
}

struct Callback {
    sender: SyncSender<Input>,
    pid: AtomicU32,
    generation: AtomicU64,
    min_qpc: AtomicI64,
    queue_lost: AtomicU64,
    etw_lost: AtomicU64,
    etw_buffers_lost: AtomicU64,
    panicked: AtomicBool,
}
impl Callback {
    fn new(sender: SyncSender<Input>) -> Self {
        Self {
            sender,
            pid: AtomicU32::new(0),
            generation: AtomicU64::new(0),
            min_qpc: AtomicI64::new(i64::MAX),
            queue_lost: AtomicU64::new(0),
            etw_lost: AtomicU64::new(0),
            etw_buffers_lost: AtomicU64::new(0),
            panicked: AtomicBool::new(false),
        }
    }
    fn select(&self, pid: u32, generation: u64, min: i64) {
        self.pid.store(0, Ordering::SeqCst);
        self.generation.store(generation, Ordering::SeqCst);
        self.min_qpc.store(min, Ordering::SeqCst);
        self.pid.store(pid, Ordering::SeqCst);
    }
    fn losses(&self) -> u64 {
        self.queue_lost
            .load(Ordering::Acquire)
            .saturating_add(self.etw_lost.load(Ordering::Acquire))
    }
    fn accept(&self, header: &EVENT_HEADER) {
        if header.ProviderId.data1 != DXG.data1
            || header.ProviderId.data2 != DXG.data2
            || header.ProviderId.data3 != DXG.data3
            || header.ProviderId.data4 != DXG.data4
            || header.EventDescriptor.Id != PRESENT_ID
            || header.TimeStamp <= 0
        {
            return;
        }
        let generation = self.generation.load(Ordering::SeqCst);
        let pid = self.pid.load(Ordering::SeqCst);
        let minimum = self.min_qpc.load(Ordering::SeqCst);
        if pid == 0
            || header.ProcessId != pid
            || header.TimeStamp < minimum
            || generation != self.generation.load(Ordering::SeqCst)
        {
            return;
        }
        if self
            .sender
            .try_send(Input::Present(Present {
                pid,
                generation,
                qpc: header.TimeStamp,
            }))
            .is_err()
        {
            add_loss(&self.queue_lost);
        }
    }
}
unsafe extern "system" fn event_callback(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    unsafe {
        let context = (*record).UserContext as *const Callback;
        if context.is_null() {
            return;
        }
        guard_callback(&*context, || (*context).accept(&(*record).EventHeader));
    }
}
fn guard_callback(context: &Callback, callback: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(callback)).is_err() {
        context.panicked.store(true, Ordering::Release);
        let _ = context.sender.try_send(Input::Wake);
    }
}

struct ProviderFilters {
    pids: [u32; 1],
    event: EVENT_FILTER_EVENT_ID,
}
impl ProviderFilters {
    fn new(pid: u32) -> Self {
        Self {
            pids: [pid],
            event: EVENT_FILTER_EVENT_ID {
                FilterIn: true,
                Reserved: 0,
                Count: 1,
                Events: [PRESENT_ID],
            },
        }
    }
    fn descriptors(&mut self) -> [EVENT_FILTER_DESCRIPTOR; 2] {
        [
            EVENT_FILTER_DESCRIPTOR {
                Ptr: self.pids.as_mut_ptr() as u64,
                Size: size_of_val(&self.pids) as u32,
                Type: EVENT_FILTER_TYPE_PID,
            },
            EVENT_FILTER_DESCRIPTOR {
                Ptr: (&mut self.event as *mut EVENT_FILTER_EVENT_ID) as u64,
                Size: size_of::<EVENT_FILTER_EVENT_ID>() as u32,
                Type: EVENT_FILTER_TYPE_EVENT_ID,
            },
        ]
    }
}

/// Aligned, bounded ETW properties plus a trailing session name. The entire
/// buffer remains owned until ControlTrace confirms this session is gone.
struct Properties {
    words: Vec<u64>,
}
impl Properties {
    fn new(name: &[u16]) -> Self {
        let bytes = size_of::<EVENT_TRACE_PROPERTIES>() + name.len() * 2;
        let mut value = Self {
            words: vec![0; bytes.div_ceil(8)],
        };
        unsafe {
            let p = &mut *value.pointer();
            p.Wnode.BufferSize = (value.words.len() * 8) as u32;
            p.Wnode.ClientContext = 1;
            p.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            p.BufferSize = 16;
            p.MinimumBuffers = 2;
            p.MaximumBuffers = 8;
            p.LogFileMode = EVENT_TRACE_REAL_TIME_MODE | EVENT_TRACE_NO_PER_PROCESSOR_BUFFERING;
            p.FlushTimer = 1;
            p.LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
            ptr::copy_nonoverlapping(
                name.as_ptr(),
                (value.pointer() as *mut u8)
                    .add(size_of::<EVENT_TRACE_PROPERTIES>())
                    .cast::<u16>(),
                name.len(),
            );
        }
        value
    }
    fn pointer(&mut self) -> *mut EVENT_TRACE_PROPERTIES {
        self.words.as_mut_ptr().cast()
    }
}

trait CleanupApi {
    fn stop(&mut self, session: u64) -> Result<()>;
    fn close(&mut self, consumer: u64, joined: bool) -> Result<()>;
}
#[derive(Default)]
struct CleanupRetry {
    failed_at: Option<u64>,
}
impl CleanupRetry {
    fn allowed(&self, epoch: u64) -> bool {
        self.failed_at != Some(epoch)
    }
    fn failed(&mut self, epoch: u64) {
        self.failed_at = Some(epoch);
    }
    fn pending(&mut self) {
        self.failed_at = None;
    }
}
#[derive(Default)]
struct Cleanup {
    session: Option<u64>,
    consumer: Option<u64>,
    consumer_expected: bool,
    consumer_joined: bool,
    close_requested: bool,
}
impl Cleanup {
    fn live(&self) -> bool {
        self.session.is_some()
            || self.consumer.is_some()
            || self.consumer_expected && !self.consumer_joined
    }
    fn advance(&mut self, api: &mut impl CleanupApi) -> Result<bool> {
        if let Some(handle) = self.session {
            api.stop(handle)?;
            self.session = None;
        }
        if let Some(handle) = self.consumer {
            if !self.close_requested {
                api.close(handle, self.consumer_joined)?;
                self.close_requested = true;
            }
            if self.consumer_joined {
                self.consumer = None;
            }
        }
        Ok(!self.live())
    }
}
struct WindowsCleanup<'a> {
    properties: &'a mut Properties,
}
impl CleanupApi for WindowsCleanup<'_> {
    fn stop(&mut self, session: u64) -> Result<()> {
        let code = unsafe {
            ControlTraceW(
                CONTROLTRACE_HANDLE { Value: session },
                ptr::null(),
                self.properties.pointer(),
                EVENT_TRACE_CONTROL_STOP,
            )
        };
        ensure!(
            code == ERROR_SUCCESS || code == ERROR_WMI_INSTANCE_NOT_FOUND,
            "{}",
            win_error("Stop owned FPS ETW session", code)
        );
        Ok(())
    }
    fn close(&mut self, consumer: u64, joined: bool) -> Result<()> {
        let code = unsafe { CloseTrace(PROCESSTRACE_HANDLE { Value: consumer }) };
        ensure!(
            code == ERROR_SUCCESS
                || code == ERROR_CTX_CLOSE_PENDING
                || joined && code == ERROR_INVALID_HANDLE,
            "{}",
            win_error("Close owned FPS ETW consumer", code)
        );
        Ok(())
    }
}
struct Consumer {
    handle: Arc<AtomicU64>,
    join: JoinHandle<u32>,
}
struct Session {
    name: String,
    wide_name: Vec<u16>,
    properties: Properties,
    cleanup: Cleanup,
    consumer: Option<Consumer>,
    callback: Arc<Callback>,
    shared: Arc<Shared>,
    provider_pid: Option<u32>,
}
impl Session {
    fn new(callback: Arc<Callback>, shared: Arc<Shared>) -> Result<Self> {
        let mut creation: FILETIME = unsafe { zeroed() };
        let mut exit = creation;
        let mut kernel = creation;
        let mut user = creation;
        ensure!(
            unsafe {
                GetProcessTimes(
                    GetCurrentProcess(),
                    &mut creation,
                    &mut exit,
                    &mut kernel,
                    &mut user,
                )
            } != 0,
            "Cannot identify this FPS session owner"
        );
        let born = ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
        let name = format!("Jiks-Fps-{}-{born:016x}", unsafe { GetCurrentProcessId() });
        let wide_name = wide(&name);
        let properties = Properties::new(&wide_name);
        Ok(Self {
            name,
            wide_name,
            properties,
            cleanup: Cleanup::default(),
            consumer: None,
            callback,
            shared,
            provider_pid: None,
        })
    }
    fn begin(&mut self) -> Result<()> {
        if self.shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut handle = CONTROLTRACE_HANDLE::default();
        let code = unsafe {
            StartTraceW(
                &mut handle,
                self.wide_name.as_ptr(),
                self.properties.pointer(),
            )
        };
        ensure!(
            code == ERROR_SUCCESS,
            "{}; existing sessions are never taken over",
            win_error("Start owned FPS ETW session", code)
        );
        self.cleanup.session = Some(handle.Value);
        if self.shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let name = self.wide_name.clone();
        let context = self.callback.clone();
        let process_handle = Arc::new(AtomicU64::new(UNOPENED));
        let output_handle = process_handle.clone();
        let (send, receive) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("jiks-fps-etw".into())
            .spawn(move || unsafe {
                // Construct pointer-bearing ABI objects on their owning consumer thread.
                // Both the log and Arc callback context stay alive throughout ProcessTrace.
                let mut log = Box::<EVENT_TRACE_LOGFILEW>::default();
                let mut name = name;
                log.LoggerName = name.as_mut_ptr();
                log.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME
                    | PROCESS_TRACE_MODE_EVENT_RECORD
                    | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
                log.Anonymous2.EventRecordCallback = Some(event_callback);
                log.Context = Arc::as_ptr(&context) as *mut _;
                let trace = OpenTraceW(&mut *log);
                if trace.Value == UNOPENED {
                    let error = GetLastError();
                    let _ = send.send(Err(error));
                    return error;
                }
                output_handle.store(trace.Value, Ordering::Release);
                let _ = send.send(Ok(()));
                ProcessTrace(&trace, 1, ptr::null(), ptr::null())
            })?;
        self.cleanup.consumer_expected = true;
        self.consumer = Some(Consumer {
            handle: process_handle,
            join,
        });
        match receive.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(code)) => Err(win_error("Open owned FPS ETW consumer", code)),
            Err(error) => Err(anyhow!(
                "FPS consumer initialization remains unconfirmed: {error}"
            )),
        }
    }
    fn provider(&mut self, pid: Option<u32>) -> Result<()> {
        if pid.is_some() && self.shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if pid == self.provider_pid {
            return Ok(());
        }
        let Some(session) = self.cleanup.session else {
            bail!("FPS session is no longer owned")
        };
        let code = if let Some(pid) = pid {
            let mut filters = ProviderFilters::new(pid);
            let mut descriptors = filters.descriptors();
            let parameters = ENABLE_TRACE_PARAMETERS {
                Version: ENABLE_TRACE_PARAMETERS_VERSION_2,
                EnableFilterDesc: descriptors.as_mut_ptr(),
                FilterDescCount: 2,
                ..Default::default()
            };
            unsafe {
                EnableTraceEx2(
                    CONTROLTRACE_HANDLE { Value: session },
                    &DXG,
                    EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                    4,
                    1,
                    0,
                    2000,
                    &parameters,
                )
            }
        } else {
            unsafe {
                EnableTraceEx2(
                    CONTROLTRACE_HANDLE { Value: session },
                    &DXG,
                    EVENT_CONTROL_CODE_DISABLE_PROVIDER,
                    0,
                    0,
                    0,
                    2000,
                    ptr::null(),
                )
            }
        };
        ensure!(
            code == ERROR_SUCCESS,
            "{}; no unfiltered fallback is used",
            win_error("Configure scoped FPS Present provider", code)
        );
        self.provider_pid = pid;
        Ok(())
    }
    fn consumer_ended(&self) -> bool {
        self.consumer.as_ref().is_some_and(|c| c.join.is_finished())
    }
    fn query_losses(&mut self) -> Result<()> {
        let handle = self
            .cleanup
            .session
            .context("FPS session is no longer owned")?;
        let code = unsafe {
            ControlTraceW(
                CONTROLTRACE_HANDLE { Value: handle },
                ptr::null(),
                self.properties.pointer(),
                EVENT_TRACE_CONTROL_QUERY,
            )
        };
        ensure!(
            code == ERROR_SUCCESS,
            "{}",
            win_error("Query owned FPS ETW loss statistics", code)
        );
        let properties = unsafe { &*self.properties.pointer() };
        // EVENT_TRACE_LOGFILEW.EventsLost is reserved/not used. Query the actual
        // owned logger's EVENT_TRACE_PROPERTIES counters instead.
        self.callback
            .etw_lost
            .store(properties.EventsLost as u64, Ordering::Release);
        self.callback
            .etw_buffers_lost
            .store(properties.RealTimeBuffersLost as u64, Ordering::Release);
        Ok(())
    }
    fn cleanup_progress(&mut self) -> Result<bool> {
        if let Some(consumer) = &self.consumer {
            let handle = consumer.handle.load(Ordering::Acquire);
            if handle != UNOPENED
                && self.cleanup.consumer.is_none()
                && !self.cleanup.close_requested
            {
                self.cleanup.consumer = Some(handle);
            }
        }
        if self.consumer.as_ref().is_some_and(|c| c.join.is_finished()) {
            let consumer = self.consumer.take().unwrap();
            let _code = consumer.join.join();
            self.cleanup.consumer_joined = true;
        }
        self.cleanup.advance(&mut WindowsCleanup {
            properties: &mut self.properties,
        })
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if !self.cleanup.live() {
            return;
        }
        // An unforeseen unwind must not release callback storage while ProcessTrace
        // can still call it. Retain this entire frame until owned cleanup succeeds.
        self.shared.stop.store(true, Ordering::Release);
        self.callback.select(0, 0, i64::MAX);
        self.shared
            .status(FpsPhase::Stopping, "FPS cleanup retained after unwind");
        self.shared.emit(FpsMessage::Fault {
            message: "FPS cleanup retained after unwind; retry Stop".into(),
            cleanup_pending: true,
        });
        let mut retry = CleanupRetry::default();
        loop {
            let epoch = self.shared.retry.load(Ordering::Acquire);
            if retry.allowed(epoch) {
                match catch_unwind(AssertUnwindSafe(|| self.cleanup_progress())) {
                    Ok(Ok(true)) => break,
                    Ok(Ok(false)) => {
                        retry.pending();
                        self.shared.status(
                            FpsPhase::Stopping,
                            "FPS cleanup waiting for actual consumer join",
                        );
                    }
                    _ => {
                        retry.failed(epoch);
                        self.shared
                            .cleanup_failed("FPS cleanup retained after unwind", epoch);
                    }
                }
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

struct ProcessHandle(HANDLE);
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Target {
    process: FpsProcess,
    handle: ProcessHandle,
}
trait Watched {
    fn alive(&self) -> bool;
}
impl Watched for Target {
    fn alive(&self) -> bool {
        unsafe { WaitForSingleObject(self.handle.0, 0) == WAIT_TIMEOUT }
    }
}
fn invalidate_dead<T: Watched>(
    target: &mut Option<T>,
    stream: &mut Stream,
    callback: &Callback,
    generation: &mut u64,
) -> Result<bool> {
    if target.as_ref().is_some_and(|t| !t.alive()) {
        *generation = generation
            .checked_add(1)
            .context("FPS generation exhausted")?;
        callback.select(0, *generation, i64::MAX);
        stream.select(None, *generation, i64::MAX);
        *target = None;
        return Ok(true);
    }
    Ok(false)
}
fn foreground(options: &FpsOptions) -> Result<Option<Target>> {
    unsafe {
        let window = GetForegroundWindow();
        if window.is_null() {
            return Ok(None);
        }
        let mut pid = 0;
        GetWindowThreadProcessId(window, &mut pid);
        if pid == 0 || pid == 4 || pid == GetCurrentProcessId() {
            return Ok(None);
        }
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        );
        ensure!(
            !handle.is_null(),
            "{}",
            win_error("Read foreground FPS process identity", GetLastError())
        );
        let handle = ProcessHandle(handle);
        let mut creation: FILETIME = zeroed();
        let mut exit = creation;
        let mut kernel = creation;
        let mut user = creation;
        ensure!(
            GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) != 0,
            "Cannot read foreground process creation time"
        );
        let mut path = vec![0u16; 32768];
        let mut size = path.len() as u32;
        ensure!(
            QueryFullProcessImageNameW(handle.0, 0, path.as_mut_ptr(), &mut size) != 0,
            "Cannot read foreground process executable name"
        );
        let path = String::from_utf16_lossy(&path[..size as usize]);
        let name = path.rsplit(['\\', '/']).next().unwrap_or("").to_owned();
        let lower = name.to_lowercase();
        let base = lower.strip_suffix(".exe").unwrap_or(&lower);
        if options
            .blacklist
            .iter()
            .any(|item| item.to_lowercase() == base || item.to_lowercase() == lower)
        {
            return Ok(None);
        }
        let target = Target {
            process: FpsProcess {
                pid,
                creation_filetime: ((creation.dwHighDateTime as u64) << 32)
                    | creation.dwLowDateTime as u64,
                name,
            },
            handle,
        };
        if !target.alive() {
            return Ok(None);
        }
        Ok(Some(target))
    }
}

pub(super) fn run(
    options: FpsOptions,
    shared: Arc<Shared>,
    sender: SyncSender<Input>,
    receive: Receiver<Input>,
) -> Result<FpsStopReport> {
    let callback = Arc::new(Callback::new(sender));
    let mut session = Session::new(callback.clone(), shared.clone())?;
    let mut failure = None;
    let initialized = catch_unwind(AssertUnwindSafe(|| session.begin()));
    match initialized {
        Ok(Ok(())) => {}
        Ok(Err(error)) => failure = Some(format!("{error:#}")),
        Err(_) => failure = Some("FPS initialization panicked".into()),
    }
    if failure.is_none() && !shared.stop.load(Ordering::Acquire) {
        shared.status(FpsPhase::Running, "");
        shared.emit(FpsMessage::Started {
            session_name: session.name.clone(),
        });
        let work = catch_unwind(AssertUnwindSafe(|| {
            run_loop(&options, &shared, &callback, &mut session, &receive)
        }));
        match work {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failure = Some(format!("{error:#}")),
            Err(_) => failure = Some("FPS processing panicked".into()),
        }
    }
    callback.select(0, 0, i64::MAX);
    shared.status(FpsPhase::Stopping, "");
    if let Some(error) = &failure {
        shared.emit(FpsMessage::Fault {
            message: error.clone(),
            cleanup_pending: session.cleanup.live(),
        });
    }
    let mut cleanup_retry = CleanupRetry::default();
    loop {
        let epoch = shared.retry.load(Ordering::Acquire);
        if cleanup_retry.allowed(epoch) {
            match catch_unwind(AssertUnwindSafe(|| session.cleanup_progress())) {
                Ok(Ok(true)) => break,
                Ok(Ok(false)) => {
                    cleanup_retry.pending();
                    shared.status(FpsPhase::Stopping, "");
                }
                result => {
                    cleanup_retry.failed(epoch);
                    let message = match result {
                        Ok(Err(error)) => format!("{error:#}"),
                        _ => "FPS cleanup panicked".into(),
                    };
                    shared.cleanup_failed(message.clone(), epoch);
                    shared.emit(FpsMessage::Fault {
                        message,
                        cleanup_pending: true,
                    });
                }
            }
        }
        let _ = receive.recv_timeout(Duration::from_millis(25));
    }
    let report = FpsStopReport {
        session_name: session.name.clone(),
        session_stopped: session.cleanup.session.is_none(),
        consumer_joined: !session.cleanup.consumer_expected || session.cleanup.consumer_joined,
        events_dropped: callback.losses(),
    };
    shared.status(FpsPhase::Stopped, failure.clone().unwrap_or_default());
    shared.emit(FpsMessage::Stopped(report.clone()));
    if let Some(error) = failure {
        bail!("FPS source failed after verified cleanup: {error}");
    }
    Ok(report)
}
fn run_loop(
    options: &FpsOptions,
    shared: &Shared,
    callback: &Callback,
    session: &mut Session,
    receive: &Receiver<Input>,
) -> Result<()> {
    let mut frequency = 0;
    ensure!(
        unsafe { QueryPerformanceFrequency(&mut frequency) } != 0 && frequency > 0,
        "FPS QPC frequency unavailable"
    );
    let frequency = frequency as u64;
    let mut stream = Stream {
        max_gap_qpc: Some(
            ((frequency as u128 * options.stale_after_ms as u128) / 1000).clamp(1, u64::MAX as u128)
                as u64,
        ),
        ..Default::default()
    };
    let mut target: Option<Target> = None;
    let mut generation = 0u64;
    let mut next_foreground = 0;
    let mut next_report = 0;
    let mut next_loss_query = 0;
    let mut last_buffer_loss = 0;
    let mut unavailable = String::new();
    while !shared.stop.load(Ordering::Acquire) {
        ensure!(
            !callback.panicked.load(Ordering::Acquire),
            "FPS callback panicked"
        );
        ensure!(
            !session.consumer_ended(),
            "FPS ETW consumer ended unexpectedly"
        );
        let now = tick();
        // Keep the selected process HANDLE, and validate it before accepting queued
        // presents and again before publishing. Numeric PID reuse cannot inherit it.
        if invalidate_dead(&mut target, &mut stream, callback, &mut generation)? {
            session.provider(None)?;
            shared.emit(FpsMessage::Unavailable {
                reason: "Foreground process exited; FPS is unknown".into(),
            });
            next_foreground = 0;
        }
        if now >= next_foreground {
            next_foreground = now.saturating_add(options.foreground_poll_ms);
            match foreground(options) {
                Ok(new) => {
                    let changed =
                        target.as_ref().map(|t| &t.process) != new.as_ref().map(|t| &t.process);
                    if changed {
                        generation = generation
                            .checked_add(1)
                            .context("FPS generation exhausted")?;
                        callback.select(0, generation, i64::MAX);
                        session.provider(new.as_ref().map(|t| t.process.pid))?;
                        let minimum = qpc()?;
                        callback.select(
                            new.as_ref().map_or(0, |t| t.process.pid),
                            generation,
                            minimum,
                        );
                        stream.select(new.as_ref().map(|t| t.process.clone()), generation, minimum);
                        target = new;
                        shared.emit(FpsMessage::Unavailable {
                            reason:
                                "Foreground target changed; waiting for fresh Present intervals"
                                    .into(),
                        });
                    }
                }
                Err(error) => {
                    generation = generation
                        .checked_add(1)
                        .context("FPS generation exhausted")?;
                    callback.select(0, generation, i64::MAX);
                    stream.select(None, generation, i64::MAX);
                    target = None;
                    session.provider(None)?;
                    shared.emit(FpsMessage::Unavailable {
                        reason: format!("Foreground identity unavailable: {error:#}"),
                    });
                }
            }
        }
        if now >= next_loss_query {
            session.query_losses()?;
            next_loss_query = now.saturating_add(1000);
        }
        let buffer_loss = callback.etw_buffers_lost.load(Ordering::Acquire);
        let event_loss_changed = stream.lost(callback.losses());
        if event_loss_changed || buffer_loss != last_buffer_loss {
            last_buffer_loss = buffer_loss;
            generation = generation
                .checked_add(1)
                .context("FPS generation exhausted")?;
            let minimum = qpc()?;
            callback.select(
                target.as_ref().map_or(0, |t| t.process.pid),
                generation,
                minimum,
            );
            stream.select(
                target.as_ref().map(|t| t.process.clone()),
                generation,
                minimum,
            );
            shared.emit(FpsMessage::Unavailable {
                reason: "ETW or bounded event queue lost events; old FPS history discarded".into(),
            });
        }
        for _ in 0..256 {
            match receive.try_recv() {
                Ok(Input::Present(event)) => {
                    if invalidate_dead(&mut target, &mut stream, callback, &mut generation)? {
                        session.provider(None)?;
                        next_foreground = 0;
                        shared.emit(FpsMessage::Unavailable {
                            reason: "Selected process identity ended".into(),
                        });
                    }
                    if target.is_some() {
                        stream.present(event);
                    }
                }
                Ok(Input::Wake) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        if now >= next_report {
            let report_tick = tick();
            next_report = report_tick.saturating_add(options.report_interval_ms);
            if invalidate_dead(&mut target, &mut stream, callback, &mut generation)? {
                session.provider(None)?;
                next_foreground = 0;
            }
            let sample = if target.is_some() {
                stream.sample(report_tick, qpc()?, frequency, options.stale_after_ms)
            } else {
                None
            };
            if let Some(mut sample) = sample {
                sample.etw_buffers_lost = buffer_loss;
                unavailable.clear();
                if !shared.stop.load(Ordering::Acquire) {
                    shared.emit(FpsMessage::Sample(sample));
                }
            } else {
                let reason = "No fresh foreground Present cadence available";
                if unavailable != reason {
                    unavailable = reason.into();
                    shared.emit(FpsMessage::Unavailable {
                        reason: unavailable.clone(),
                    });
                }
            }
        }
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        let wait = next_report
            .min(next_foreground)
            .saturating_sub(tick())
            .clamp(1, 25);
        match receive.recv_timeout(Duration::from_millis(wait)) {
            Ok(Input::Present(event)) => {
                if invalidate_dead(&mut target, &mut stream, callback, &mut generation)? {
                    session.provider(None)?;
                    next_foreground = 0;
                    shared.emit(FpsMessage::Unavailable {
                        reason: "Selected process identity ended".into(),
                    });
                }
                if target.is_some() {
                    stream.present(event);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        stop_fails: bool,
        close_fails: bool,
    }
    impl CleanupApi for Fake {
        fn stop(&mut self, h: u64) -> Result<()> {
            self.calls.push(format!("stop:{h}"));
            ensure!(!self.stop_fails, "fixture stop failure");
            Ok(())
        }
        fn close(&mut self, h: u64, _: bool) -> Result<()> {
            self.calls.push(format!("close:{h}"));
            ensure!(!self.close_fails, "fixture close failure");
            Ok(())
        }
    }
    #[test]
    fn failed_stop_retains_owner_and_does_not_close_consumer_until_retry() {
        let mut state = Cleanup {
            session: Some(17),
            consumer: Some(23),
            consumer_expected: true,
            ..Default::default()
        };
        let mut api = Fake {
            stop_fails: true,
            ..Default::default()
        };
        assert!(state.advance(&mut api).is_err());
        assert_eq!(state.session, Some(17));
        assert_eq!(api.calls, ["stop:17"]);
        api.stop_fails = false;
        assert!(!state.advance(&mut api).unwrap());
        assert_eq!(api.calls, ["stop:17", "stop:17", "close:23"]);
        assert!(state.live());
        state.consumer_joined = true;
        assert!(state.advance(&mut api).unwrap());
        assert!(!state.live());
        assert_eq!(api.calls.len(), 3);
    }
    #[test]
    fn pending_open_and_actual_join_gate_exit_even_after_session_stopped() {
        let mut state = Cleanup {
            session: Some(17),
            consumer_expected: true,
            ..Default::default()
        };
        let mut api = Fake::default();
        assert!(!state.advance(&mut api).unwrap());
        state.consumer = Some(23);
        assert!(!state.advance(&mut api).unwrap());
        assert!(state.close_requested);
        assert!(!state.advance(&mut api).unwrap());
        state.consumer_joined = true;
        assert!(state.advance(&mut api).unwrap());
    }
    #[test]
    fn close_failure_retains_handle_and_needs_successful_retry() {
        let mut state = Cleanup {
            session: Some(1),
            consumer: Some(2),
            consumer_expected: true,
            consumer_joined: true,
            ..Default::default()
        };
        let mut api = Fake {
            close_fails: true,
            ..Default::default()
        };
        assert!(state.advance(&mut api).is_err());
        assert_eq!(state.consumer, Some(2));
        assert!(!state.close_requested);
        api.close_fails = false;
        assert!(state.advance(&mut api).unwrap());
        assert_eq!(api.calls, ["stop:1", "close:2", "close:2"]);
    }
    #[test]
    fn successful_retry_keeps_polling_pending_actual_join_without_a_second_retry() {
        let mut retry = CleanupRetry::default();
        assert!(retry.allowed(0));
        retry.failed(0);
        assert!(!retry.allowed(0));
        assert!(retry.allowed(1));
        retry.pending();
        assert!(retry.allowed(1));
        assert!(retry.allowed(1));
    }
    #[test]
    fn failed_creation_never_adopts_or_stops_an_existing_foreign_session() {
        let mut state = Cleanup::default();
        let mut api = Fake::default();
        assert!(state.advance(&mut api).unwrap());
        assert!(api.calls.is_empty());
        assert!(!state.live());
    }
    fn header(pid: u32, time: i64) -> EVENT_HEADER {
        EVENT_HEADER {
            ProviderId: DXG,
            ProcessId: pid,
            TimeStamp: time,
            EventDescriptor: EVENT_DESCRIPTOR {
                Id: PRESENT_ID,
                ..Default::default()
            },
            ..Default::default()
        }
    }
    #[test]
    fn callback_filters_provider_id_pid_generation_boundary_and_bounded_queue() {
        let (send, read) = mpsc::sync_channel(1);
        let callback = Callback::new(send);
        callback.select(10, 7, 100);
        callback.accept(&header(9, 101));
        callback.accept(&header(10, 99));
        let mut wrong = header(10, 101);
        wrong.EventDescriptor.Id = 1;
        callback.accept(&wrong);
        assert!(read.try_recv().is_err());
        callback.accept(&header(10, 100));
        callback.accept(&header(10, 101));
        assert_eq!(callback.losses(), 1);
        let Input::Present(event) = read.try_recv().unwrap() else {
            panic!()
        };
        assert_eq!((event.pid, event.generation, event.qpc), (10, 7, 100));
        callback.select(0, 8, i64::MAX);
        callback.accept(&header(10, 102));
        assert!(read.try_recv().is_err());
    }
    #[test]
    fn event_record_callback_uses_only_owned_context_and_header() {
        let (send, read) = mpsc::sync_channel(1);
        let callback = Callback::new(send);
        callback.select(10, 1, 1);
        let mut record = EVENT_RECORD {
            EventHeader: header(10, 100),
            UserContext: (&callback as *const Callback) as *mut _,
            ..Default::default()
        };
        unsafe {
            event_callback(&mut record);
        }
        assert!(matches!(
            read.try_recv(),
            Ok(Input::Present(Present {
                pid: 10,
                qpc: 100,
                ..
            }))
        ));
    }
    #[test]
    fn properties_are_aligned_bounded_realtime_and_do_not_request_kernel_logger() {
        let name = wide("Jiks-Fps-fixture-owner");
        let mut props = Properties::new(&name);
        assert_eq!((props.pointer() as usize) % 8, 0);
        let p = unsafe { &*props.pointer() };
        assert_eq!(
            p.LogFileMode,
            EVENT_TRACE_REAL_TIME_MODE | EVENT_TRACE_NO_PER_PROCESSOR_BUFFERING
        );
        assert_eq!(p.LogFileNameOffset, 0);
        assert_eq!(p.MaximumBuffers, 8);
        assert_eq!(p.Wnode.ClientContext, 1);
        assert_eq!(
            p.LoggerNameOffset,
            size_of::<EVENT_TRACE_PROPERTIES>() as u32
        );
    }
    #[test]
    fn scope_filter_abi_limits_provider_to_one_pid_and_present_event() {
        let mut filters = ProviderFilters::new(77);
        let descriptors = filters.descriptors();
        assert_eq!(descriptors[0].Type, EVENT_FILTER_TYPE_PID);
        assert_eq!(descriptors[0].Size, 4);
        assert_eq!(unsafe { *(descriptors[0].Ptr as *const u32) }, 77);
        assert_eq!(descriptors[1].Type, EVENT_FILTER_TYPE_EVENT_ID);
        let event = unsafe { &*(descriptors[1].Ptr as *const EVENT_FILTER_EVENT_ID) };
        assert!(event.FilterIn);
        assert_eq!(event.Count, 1);
        assert_eq!(event.Events, [0xb8]);
    }
    #[test]
    fn panic_boundary_converts_callback_panic_to_controlled_fault_signal() {
        let (send, read) = mpsc::sync_channel(1);
        let callback = Callback::new(send);
        guard_callback(&callback, || panic!("pure callback fixture"));
        assert!(callback.panicked.load(Ordering::Acquire));
        assert!(matches!(read.try_recv(), Ok(Input::Wake)));
    }
    #[test]
    fn dead_held_process_invalidates_before_reused_pid_can_publish() {
        struct FakeWatch(bool);
        impl Watched for FakeWatch {
            fn alive(&self) -> bool {
                self.0
            }
        }
        let (send, _read) = mpsc::sync_channel(1);
        let callback = Callback::new(send);
        let mut stream = Stream::default();
        let old = FpsProcess {
            pid: 10,
            creation_filetime: 1,
            name: "old".into(),
        };
        stream.select(Some(old), 7, 1);
        for time in [1, 1001, 2001] {
            stream.present(Present {
                pid: 10,
                generation: 7,
                qpc: time,
            });
        }
        let mut generation = 7;
        let mut target = Some(FakeWatch(false));
        assert!(invalidate_dead(&mut target, &mut stream, &callback, &mut generation).unwrap());
        assert!(target.is_none());
        assert_eq!(generation, 8);
        assert_eq!(callback.pid.load(Ordering::Acquire), 0);
        assert!(stream.sample(1, 2001, 60_000, 2000).is_none());
        stream.select(
            Some(FpsProcess {
                pid: 10,
                creation_filetime: 2,
                name: "new".into(),
            }),
            9,
            3000,
        );
        assert!(!stream.present(Present {
            pid: 10,
            generation: 7,
            qpc: 4000
        }));
        assert!(!stream.present(Present {
            pid: 10,
            generation: 9,
            qpc: 2001
        }));
        assert!(stream.sample(2, 4000, 60_000, 2000).is_none());
    }
}
