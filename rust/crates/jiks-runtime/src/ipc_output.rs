//! A bounded last-value mailbox keeps a stalled parent pipe off the coordinator
//! and broker monitor. Only this thread owns stdout; it never owns hardware.
use anyhow::{Result, ensure};
use std::{
    io::Write,
    os::windows::io::AsRawHandle,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
struct Packet {
    bytes: Vec<u8>,
    revision: u64,
    metadata: u64,
}
#[derive(Default)]
struct State {
    pending: Option<Packet>,
    stop: bool,
}
#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    broken: AtomicBool,
    busy_since: AtomicU64,
    revision: AtomicU64,
    metadata: AtomicU64,
}
pub(crate) struct OutputPump {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}
impl OutputPump {
    pub(crate) fn new() -> Result<Self> {
        let shared = Arc::new(Shared::default());
        let work = shared.clone();
        let worker = thread::Builder::new()
            .name("jiks-pipe-output".into())
            .spawn(move || {
                let mut output = std::io::stdout().lock();
                loop {
                    let packet = {
                        let mut state = work.state.lock().unwrap_or_else(|p| p.into_inner());
                        while state.pending.is_none() && !state.stop {
                            state = work.changed.wait(state).unwrap_or_else(|p| p.into_inner());
                        }
                        if state.stop {
                            break;
                        }
                        state.pending.take().expect("packet")
                    };
                    work.busy_since
                        .store(jiks_platform::monotonic_ms().max(1), Ordering::Release);
                    let result = output
                        .write_all(&packet.bytes)
                        .and_then(|()| output.flush());
                    work.busy_since.store(0, Ordering::Release);
                    if result.is_err() {
                        work.broken.store(true, Ordering::Release);
                        break;
                    }
                    work.metadata.store(packet.metadata, Ordering::Release);
                    work.revision.store(packet.revision, Ordering::Release);
                }
            })?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }
    pub(crate) fn send(&self, bytes: Vec<u8>, revision: u64, metadata: u64) {
        let mut state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        state.pending = Some(Packet {
            bytes,
            revision,
            metadata,
        });
        self.shared.changed.notify_one();
    }
    pub(crate) fn written_metadata(&self) -> u64 {
        self.shared.metadata.load(Ordering::Acquire)
    }
    pub(crate) fn broken(&self) -> bool {
        self.shared.broken.load(Ordering::Acquire)
    }
    pub(crate) fn stalled(&self) -> bool {
        let start = self.shared.busy_since.load(Ordering::Acquire);
        start != 0 && jiks_platform::monotonic_ms().saturating_sub(start) >= 5000
    }
    fn cancel(&self) {
        if let Some(worker) = &self.worker {
            unsafe {
                windows_sys::Win32::System::IO::CancelSynchronousIo(worker.as_raw_handle() as _);
            }
        }
    }
    /// Called only after native owner release. Failure concerns pipe delivery;
    /// no hardware thread is killed or joined by this cleanup.
    pub(crate) fn finish(&mut self, revision: u64) -> Result<()> {
        let start = Instant::now();
        while self.shared.revision.load(Ordering::Acquire) < revision
            && !self.broken()
            && start.elapsed() < Duration::from_secs(2)
        {
            thread::sleep(Duration::from_millis(5));
        }
        let delivered = self.shared.revision.load(Ordering::Acquire) >= revision;
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
            state.stop = true;
            state.pending = None;
            self.shared.changed.notify_all();
        }
        self.cancel();
        let until = Instant::now();
        while self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
            && until.elapsed() < Duration::from_secs(1)
        {
            thread::sleep(Duration::from_millis(5));
            self.cancel();
        }
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            let _ = self.worker.take().unwrap().join();
        }
        ensure!(
            delivered,
            "设备清理已完成，但退出确认未能送达界面（父管道阻塞或断开）"
        );
        Ok(())
    }
}
impl Drop for OutputPump {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
            state.stop = true;
            state.pending = None;
            self.shared.changed.notify_all();
        }
        self.cancel();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queued_snapshots_coalesce_to_one_bounded_owned_packet() {
        let shared = Shared::default();
        for revision in 1..1000 {
            shared.state.lock().unwrap().pending = Some(Packet {
                bytes: vec![0; 128],
                revision,
                metadata: 9,
            });
        }
        let state = shared.state.lock().unwrap();
        let packet = state.pending.as_ref().unwrap();
        assert_eq!(packet.revision, 999);
        assert_eq!(packet.bytes.len(), 128);
    }
}
