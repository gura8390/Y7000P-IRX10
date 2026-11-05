//! Private parent/child pipes. A UI failure does not terminate a physical worker.
use crate::service::{Command, CommandResult, Metric, Snapshot};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Command as ProcessCommand, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender},
    },
    time::Duration,
};

pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub struct LiveSnapshot {
    revision: u64,
    metadata_revision: u64,
    machine: String,
    notice: String,
    backend_ready: bool,
    values: BTreeMap<String, Value>,
    metrics: Vec<Metric>,
    results: VecDeque<CommandResult>,
    shutdown_ready: bool,
}
impl From<&Snapshot> for LiveSnapshot {
    fn from(s: &Snapshot) -> Self {
        Self {
            revision: s.revision,
            metadata_revision: s.metadata_revision,
            machine: s.machine.clone(),
            notice: s.notice.clone(),
            backend_ready: s.backend_ready,
            values: s.values.clone(),
            metrics: s.metrics.clone(),
            results: s.results.clone(),
            shutdown_ready: s.shutdown_ready,
        }
    }
}
impl LiveSnapshot {
    fn apply(self, target: &mut Snapshot) -> Result<()> {
        ensure!(
            self.metadata_revision == target.metadata_revision,
            "后台实时状态缺少对应的元数据快照"
        );
        target.revision = self.revision;
        target.machine = self.machine;
        target.notice = self.notice;
        target.backend_ready = self.backend_ready;
        target.values = self.values;
        target.metrics = self.metrics;
        target.results = self.results;
        target.shutdown_ready = self.shutdown_ready;
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
pub enum Frame {
    Full(Snapshot),
    Live(LiveSnapshot),
    /// Small terminal acknowledgement remains serializable even if a static document exceeded the transport limit.
    Terminal {
        revision: u64,
        shutdown_ready: bool,
        error: Option<String>,
    },
}

fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_FRAME_BYTES, "配置或状态帧超过16MiB限制");
    bytes.push(b'\n');
    Ok(bytes)
}
pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<()> {
    let bytes = encode_frame(value)?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}
pub fn read_frame<T: DeserializeOwned>(reader: &mut impl BufRead) -> Result<Option<T>> {
    // read_until alone can grow without bound when the other endpoint is broken.
    let mut bytes = Vec::new();
    let count = reader
        .take((MAX_FRAME_BYTES + 2) as u64)
        .read_until(b'\n', &mut bytes)?;
    if count == 0 {
        return Ok(None);
    }
    ensure!(bytes.last() == Some(&b'\n'), "IPC帧截断或过长");
    ensure!(bytes.len() - 1 <= MAX_FRAME_BYTES, "IPC帧超过限制");
    Ok(Some(
        serde_json::from_slice(&bytes).context("后台消息格式无效")?,
    ))
}

/// This runs only in the broker child, with stdin/stdout exclusively owned by IPC.
pub fn broker(root: std::path::PathBuf, read_only: bool) -> Result<()> {
    let runtime = crate::service::start(root, read_only)?;
    let input_runtime = runtime.clone();
    std::thread::Builder::new()
        .name("jiks-pipe-input".into())
        .spawn(move || {
            let mut input = BufReader::new(std::io::stdin());
            loop {
                match read_frame::<Command>(&mut input) {
                    Ok(Some(command)) => {
                        // Queue capacity is bounded; backpressure stays in this reader, never a UI callback.
                        let id = command.request_id();
                        if let Err(error) = input_runtime.submit(command) {
                            input_runtime.report(id, false, format!("{error:#}"));
                        }
                    }
                    Ok(None) | Err(_) => {
                        // Parent crash / malformed stream: keep process alive until actual I/O and restoration finish.
                        loop {
                            if input_runtime.snapshot().shutdown_ready
                                || input_runtime
                                    .submit(Command::Shutdown { request_id: 0 })
                                    .is_ok()
                            {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        return;
                    }
                }
            }
        })?;
    let mut output = crate::ipc_output::OutputPump::new()?;
    let mut state = Snapshot::default();
    let mut revision = 0;
    let mut pipe_broken = false;
    let mut disconnected_shutdown_sent = false;
    let mut encoding_error: Option<String> = None;
    loop {
        if let Some(changed) = runtime.snapshot_if_changed(revision) {
            state = changed;
        }
        if output.broken() || output.stalled() {
            pipe_broken = true;
        }
        if !pipe_broken && state.revision != revision {
            let frame = if output.written_metadata() != state.metadata_revision {
                Frame::Full(state.clone())
            } else {
                Frame::Live(LiveSnapshot::from(&state))
            };
            match encode_frame(&frame) {
                Ok(bytes) => {
                    output.send(bytes, state.revision, state.metadata_revision);
                    revision = state.revision;
                }
                Err(error) => {
                    encoding_error = Some(format!("后台状态无法传输：{error:#}"));
                    pipe_broken = true;
                }
            }
        }
        if pipe_broken && !state.shutdown_ready && !disconnected_shutdown_sent {
            disconnected_shutdown_sent =
                runtime.submit(Command::Shutdown { request_id: 0 }).is_ok();
        }
        if state.shutdown_ready {
            if let Some(error) = encoding_error {
                output.send(
                    encode_frame(&Frame::Terminal {
                        revision: state.revision,
                        shutdown_ready: true,
                        error: Some(error),
                    })?,
                    state.revision,
                    state.metadata_revision,
                );
            } else if !output.broken() {
                let frame = Frame::Full(state.clone());
                let bytes = encode_frame(&frame).or_else(|error| {
                    encode_frame(&Frame::Terminal {
                        revision: state.revision,
                        shutdown_ready: true,
                        error: Some(error.to_string()),
                    })
                })?;
                output.send(bytes, state.revision, state.metadata_revision);
            }
            output.finish(state.revision)?;
            if let Some(error) = state
                .values
                .get("backend.terminal_error")
                .and_then(Value::as_str)
            {
                bail!("{error}");
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

struct Outbound {
    bytes: Vec<u8>,
}
#[derive(Default)]
struct Connection {
    closed: AtomicBool,
    close_writer: AtomicBool,
    pending: Mutex<BTreeSet<u64>>,
    overlay: Mutex<ConnectionOverlay>,
}
#[derive(Default)]
struct ConnectionOverlay {
    revision: u64,
    failures: VecDeque<CommandResult>,
    fault: Option<String>,
}
impl Connection {
    fn admit(&self, request_id: u64) -> Result<()> {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        // Serialization can take time after submit's cheap early check. Pair
        // this final check with fail's drain under the same mutex: the request
        // is either rejected here or included in that failure acknowledgement.
        ensure!(!self.closed.load(Ordering::Acquire), "后台连接已关闭");
        pending.insert(request_id);
        Ok(())
    }
    fn fail(&self, message: String) {
        self.closed.store(true, Ordering::Release);
        self.close_writer.store(true, Ordering::Release);
        let ids = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|p| p.into_inner()));
        let mut overlay = self.overlay.lock().unwrap_or_else(|p| p.into_inner());
        overlay.fault = Some(message.clone());
        for request_id in ids {
            overlay.failures.push_back(CommandResult {
                request_id,
                ok: false,
                message: message.clone(),
            });
        }
        while overlay.failures.len() > 128 {
            overlay.failures.pop_front();
        }
        overlay.revision = overlay.revision.wrapping_add(1);
    }
    fn acknowledge(&self, state: &Snapshot) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        for result in &state.results {
            pending.remove(&result.request_id);
        }
    }
}
/// Opaque observation token. A local pipe failure can change what the UI sees
/// without advancing the broker's state revision, so both generations matter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SnapshotCursor {
    state_revision: u64,
    connection_revision: u64,
}
pub struct BrokerClient {
    state: Arc<Mutex<Snapshot>>,
    commands: SyncSender<Outbound>,
    child: Arc<Mutex<Child>>,
    connection: Arc<Connection>,
}
impl BrokerClient {
    pub fn spawn(executable: &Path, root: &Path, read_only: bool) -> Result<Self> {
        use std::os::windows::process::CommandExt;
        let mut process = ProcessCommand::new(executable);
        process.arg("broker").arg("--data-dir").arg(root);
        if read_only {
            process.arg("--read-only");
        }
        process
            .creation_flags(0x0800_0000)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = process.spawn().context("无法启动原生后台进程")?;
        let mut input = child.stdin.take().context("Missing broker input")?;
        let output = child.stdout.take().context("Missing broker output")?;
        let child = Arc::new(Mutex::new(child));
        let state = Arc::new(Mutex::new(Snapshot::default()));
        let connection = Arc::new(Connection::default());
        let (commands, receiver) = mpsc::sync_channel::<Outbound>(16);
        let writer_connection = connection.clone();
        std::thread::Builder::new()
            .name("jiks-pipe-write".into())
            .spawn(move || {
                loop {
                    if writer_connection.close_writer.load(Ordering::Acquire) {
                        break;
                    }
                    match receiver.recv_timeout(Duration::from_millis(250)) {
                        Ok(outbound) => {
                            if let Err(error) = input
                                .write_all(&outbound.bytes)
                                .and_then(|()| input.flush())
                            {
                                writer_connection
                                    .fail(format!("后台连接已断开，恢复状态未确认：{error}"));
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                // Drop stdin even when the reader detected a protocol failure. Child EOF requests verified shutdown.
            })?;
        let reader_state = state.clone();
        let reader_connection = connection.clone();
        std::thread::Builder::new()
            .name("jiks-pipe-read".into())
            .spawn(move || {
                let mut reader = BufReader::new(output);
                let outcome: Result<()> = (|| {
                    while let Some(frame) = read_frame::<Frame>(&mut reader)? {
                        let mut state = reader_state.lock().unwrap_or_else(|p| p.into_inner());
                        match frame {
                            Frame::Full(full) => *state = full,
                            Frame::Live(live) => live.apply(&mut state)?,
                            Frame::Terminal {
                                revision,
                                shutdown_ready,
                                error,
                            } => {
                                state.revision = revision;
                                state.shutdown_ready = shutdown_ready;
                                if let Some(error) = error {
                                    state.values.insert(
                                        "backend.terminal_error".into(),
                                        Value::String(error.clone()),
                                    );
                                    state.notice = error;
                                }
                            }
                        }
                        reader_connection.acknowledge(&state);
                    }
                    Ok(())
                })();
                let state = reader_state.lock().unwrap_or_else(|p| p.into_inner());
                if !state.shutdown_ready {
                    let message = match outcome {
                        Ok(()) => "后台意外断开，设备恢复状态尚未确认".into(),
                        Err(error) => format!("后台连接异常：{error:#}"),
                    };
                    drop(state);
                    reader_connection.fail(message);
                }
            })?;
        Ok(Self {
            state,
            commands,
            child,
            connection,
        })
    }
    pub fn snapshot(&self) -> Snapshot {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let overlay = self
            .connection
            .overlay
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        snapshot_with_overlay(&state, &overlay)
    }
    /// Returns the next observed state only when either the broker or the
    /// connection diagnostics changed. Never starts I/O or discards failures.
    pub fn snapshot_if_changed(&self, cursor: &mut SnapshotCursor) -> Option<Snapshot> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let overlay = self
            .connection
            .overlay
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        snapshot_if_changed(&state, &overlay, cursor)
    }
    pub fn submit(&self, command: Command) -> Result<()> {
        ensure!(
            !self.connection.closed.load(Ordering::Acquire),
            "后台连接已关闭"
        );
        // Reject oversized user input before touching a live pipe or replacing its worker.
        let bytes = encode_frame(&command)?;
        let request_id = command.request_id();
        self.connection.admit(request_id)?;
        if let Err(error) = self.commands.try_send(Outbound { bytes }) {
            self.connection
                .pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&request_id);
            bail!("后台操作队列已满或断开：{error}");
        }
        Ok(())
    }
    pub fn process_id(&self) -> u32 {
        self.child.lock().unwrap_or_else(|p| p.into_inner()).id()
    }
    pub fn has_exited(&self) -> Result<bool> {
        Ok(self
            .child
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .try_wait()?
            .is_some())
    }
    /// Only after shutdown_ready. Never kills a broker while its physical work is pending.
    pub fn wait_verified(&self) -> Result<()> {
        if !self.snapshot().shutdown_ready {
            bail!("后台恢复尚未确认，不能视为已安全结束");
        }
        let status = self
            .child
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .wait()?;
        ensure!(status.success(), "后台退出代码异常：{status}");
        Ok(())
    }
}
fn snapshot_if_changed(
    state: &Snapshot,
    overlay: &ConnectionOverlay,
    cursor: &mut SnapshotCursor,
) -> Option<Snapshot> {
    let current = SnapshotCursor {
        state_revision: state.revision,
        connection_revision: overlay.revision,
    };
    if *cursor == current {
        return None;
    }
    let result = snapshot_with_overlay(state, overlay);
    *cursor = current;
    Some(result)
}
fn snapshot_with_overlay(state: &Snapshot, overlay: &ConnectionOverlay) -> Snapshot {
    let mut state = state.clone();
    for failure in &overlay.failures {
        if !state
            .results
            .iter()
            .any(|r| r.request_id == failure.request_id)
        {
            state.results.push_back(failure.clone());
        }
    }
    while state.results.len() > 128 {
        state.results.pop_front();
    }
    if !state.shutdown_ready {
        if let Some(error) = &overlay.fault {
            state.backend_ready = false;
            state.notice = error.clone();
            state
                .values
                .insert("backend.connection_failed".into(), Value::Bool(true));
        }
    }
    // Advance for every local failure too, including a later terminal
    // broker frame. Native views use this revision to refresh/finish exit.
    state.revision = state.revision.wrapping_add(overlay.revision);
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_roundtrip_keeps_unicode_and_document_numbers() {
        let command = Command::Execute {
            request_id: 42,
            command: "保存".into(),
            payload: serde_json::json!({"number":12345678901234567890u64}),
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &command).unwrap();
        let decoded: Command = read_frame(&mut std::io::Cursor::new(bytes))
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(command).unwrap(),
            serde_json::to_value(decoded).unwrap()
        );
    }
    #[test]
    fn truncated_and_oversized_frames_fail_without_unbounded_reads() {
        assert!(read_frame::<Value>(&mut std::io::Cursor::new(b"{}".as_slice())).is_err());
        let mut bytes = vec![b' '; MAX_FRAME_BYTES + 4];
        bytes.push(b'\n');
        let mut cursor = std::io::Cursor::new(bytes);
        assert!(read_frame::<Value>(&mut cursor).is_err());
        assert_eq!(cursor.position(), (MAX_FRAME_BYTES + 2) as u64);
    }
    #[test]
    fn realtime_frames_require_matching_static_revision() {
        let mut old = Snapshot::default();
        let mut new = old.clone();
        new.metadata_revision += 1;
        assert!(LiveSnapshot::from(&new).apply(&mut old).is_err());
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &Frame::Full(new.clone())).unwrap();
        if let Frame::Full(full) = read_frame(&mut std::io::Cursor::new(bytes))
            .unwrap()
            .unwrap()
        {
            old = full;
        } else {
            panic!()
        }
        new.revision += 1;
        new.notice = "新读数".into();
        LiveSnapshot::from(&new).apply(&mut old).unwrap();
        assert_eq!(old.notice, "新读数");
    }
    #[test]
    fn closing_connection_fails_every_outstanding_request_without_losing_metadata() {
        let c = Connection::default();
        for request_id in [1, 2, 3] {
            c.admit(request_id).unwrap();
        }
        c.fail("pipe broken".into());
        assert!(c.closed.load(Ordering::Acquire));
        assert!(c.pending.lock().unwrap().is_empty());
        assert_eq!(c.overlay.lock().unwrap().failures.len(), 3);
        assert!(c.overlay.lock().unwrap().failures.iter().all(|r| !r.ok));
    }
    #[test]
    fn disconnect_after_preflight_cannot_strand_a_new_request() {
        let c = Connection::default();
        // submit's initial check succeeded, then another thread detected the
        // broken pipe while the command was being encoded.
        assert!(!c.closed.load(Ordering::Acquire));
        c.fail("pipe closed during serialization".into());
        assert!(c.admit(42).is_err());
        assert!(c.pending.lock().unwrap().is_empty());
        assert!(c.overlay.lock().unwrap().failures.is_empty());
    }
    #[test]
    fn incremental_snapshots_surface_local_failures_then_terminal_cleanup() {
        let mut state = Snapshot::default();
        state.backend_ready = true;
        let connection = Connection::default();
        let mut cursor = SnapshotCursor::default();
        let initial =
            snapshot_if_changed(&state, &connection.overlay.lock().unwrap(), &mut cursor).unwrap();
        assert!(
            snapshot_if_changed(&state, &connection.overlay.lock().unwrap(), &mut cursor).is_none()
        );
        connection.pending.lock().unwrap().insert(42);
        connection.fail("first pipe fault".into());
        let failed =
            snapshot_if_changed(&state, &connection.overlay.lock().unwrap(), &mut cursor).unwrap();
        assert_ne!(failed.revision, initial.revision);
        assert!(!failed.backend_ready);
        assert_eq!(failed.results.back().unwrap().request_id, 42);
        assert!(Arc::ptr_eq(&initial.documents, &failed.documents));
        assert!(
            snapshot_if_changed(&state, &connection.overlay.lock().unwrap(), &mut cursor).is_none()
        );
        connection.fail("second pipe fault".into());
        let second =
            snapshot_if_changed(&state, &connection.overlay.lock().unwrap(), &mut cursor).unwrap();
        assert_ne!(second.revision, failed.revision);
        assert_eq!(second.notice, "second pipe fault");
        state.shutdown_ready = true;
        state.revision += 1;
        let terminal =
            snapshot_if_changed(&state, &connection.overlay.lock().unwrap(), &mut cursor).unwrap();
        assert_ne!(terminal.revision, second.revision);
        assert!(terminal.shutdown_ready);
        assert_eq!(terminal.results.len(), 1);
    }
    #[test]
    fn oversized_command_is_rejected_before_a_pipe_is_written() {
        let command = Command::SaveDocument {
            request_id: 1,
            document: "macro".into(),
            value: serde_json::json!({"text":"x".repeat(MAX_FRAME_BYTES)}),
        };
        assert!(encode_frame(&command).is_err());
    }
}
