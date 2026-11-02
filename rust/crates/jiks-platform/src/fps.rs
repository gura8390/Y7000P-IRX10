//! Opt-in native ETW provider for the original LLT FPS capability.
//! This measures foreground-process DxgKrnl Present cadence, aggregated across
//! that process's presentation streams. It is NOT displayed/scanout FPS, and
//! does not identify swapchains, dropped frames or frame generation.
//! `new`, `default`, idle `poll` and disabled options do no native IO.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
};
mod math;
mod native;

pub const SOURCE: &str =
    "foreground-process DxgKrnl kernel Present cadence (process aggregate; not displayed FPS)";
#[derive(Debug, Clone)]
pub struct FpsOptions {
    pub enabled: bool,
    pub report_interval_ms: u64,
    pub stale_after_ms: u64,
    pub foreground_poll_ms: u64,
    pub blacklist: BTreeSet<String>,
}
impl Default for FpsOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            report_interval_ms: 100,
            stale_after_ms: 2000,
            foreground_poll_ms: 1000,
            blacklist: [
                "explorer",
                "taskmgr",
                "ApplicationFrameHost",
                "System",
                "svchost",
                "csrss",
                "wininit",
                "services",
                "lsass",
                "winlogon",
                "smss",
                "spoolsv",
                "SearchIndexer",
                "SearchUI",
                "RuntimeBroker",
                "dwm",
                "ctfmon",
                "audiodg",
                "fontdrvhost",
                "taskhost",
                "conhost",
                "sihost",
                "StartMenuExperienceHost",
                "ShellExperienceHost",
                "Lenovo Legion Toolkit",
                "Jiks",
            ]
            .into_iter()
            .map(|s| s.to_lowercase())
            .collect(),
        }
    }
}
impl FpsOptions {
    fn validate(&self) -> Result<()> {
        ensure!(self.enabled, "FPS collection requires explicit enablement");
        ensure!(
            (50..=2000).contains(&self.report_interval_ms),
            "FPS report interval must be 50..2000 ms"
        );
        ensure!(
            (self.report_interval_ms..=30_000).contains(&self.stale_after_ms),
            "Invalid FPS freshness limit"
        );
        ensure!(
            (100..=2000).contains(&self.foreground_poll_ms),
            "Invalid FPS foreground refresh interval"
        );
        ensure!(
            self.blacklist.len() <= 1024
                && self
                    .blacklist
                    .iter()
                    .all(|s| s.len() <= 260 && !s.contains('\0')),
            "Invalid FPS blacklist"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FpsProcess {
    pub pid: u32,
    pub creation_filetime: u64,
    pub name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FpsSample {
    pub process: FpsProcess,
    pub generation: u64,
    pub fps: Option<f64>,
    pub low_fps: Option<f64>,
    pub frame_time_ms: Option<f64>,
    pub intervals: usize,
    pub window_capacity: usize,
    pub sampled_tick_ms: u64,
    pub last_present_tick_ms: u64,
    pub events_dropped: u64,
    pub etw_buffers_lost: u64,
    pub source: String,
    pub process_aggregate: bool,
    pub displayed_fps_verified: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FpsStopReport {
    pub session_name: String,
    pub session_stopped: bool,
    pub consumer_joined: bool,
    pub events_dropped: u64,
}
#[derive(Debug, Clone)]
pub enum FpsMessage {
    Started {
        session_name: String,
    },
    Sample(FpsSample),
    Unavailable {
        reason: String,
    },
    Fault {
        message: String,
        cleanup_pending: bool,
    },
    Stopped(FpsStopReport),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpsPhase {
    Starting,
    Running,
    Stopping,
    CleanupFailed,
    Stopped,
}
#[derive(Debug, Clone)]
pub struct FpsStatus {
    pub phase: FpsPhase,
    pub reason: String,
    /// Exact retry epoch whose cleanup attempt actually failed. A queued old
    /// failure must not acknowledge a newer explicit cleanup request.
    pub cleanup_attempt: Option<u64>,
}

#[derive(Default)]
struct Mailbox {
    controls: VecDeque<FpsMessage>,
    sample: Option<FpsSample>,
    status: Option<FpsStatus>,
}
struct Shared {
    stop: AtomicBool,
    retry: AtomicU64,
    mailbox: Mutex<Mailbox>,
}
impl Shared {
    fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            retry: AtomicU64::new(0),
            mailbox: Mutex::new(Mailbox::default()),
        }
    }
    fn emit(&self, message: FpsMessage) {
        let mut box_ = self.mailbox.lock().unwrap_or_else(|p| p.into_inner());
        match message {
            FpsMessage::Sample(value) => box_.sample = Some(value),
            other => {
                if matches!(
                    other,
                    FpsMessage::Unavailable { .. }
                        | FpsMessage::Fault { .. }
                        | FpsMessage::Stopped(_)
                ) {
                    box_.sample = None;
                }
                if box_.controls.len() == 16 {
                    box_.controls.pop_front();
                }
                box_.controls.push_back(other);
            }
        }
    }
    fn status(&self, phase: FpsPhase, reason: impl Into<String>) {
        self.mailbox
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .status = Some(FpsStatus {
            phase,
            reason: reason.into(),
            cleanup_attempt: None,
        });
    }
    fn cleanup_failed(&self, reason: impl Into<String>, attempt: u64) {
        self.mailbox
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .status = Some(FpsStatus {
            phase: FpsPhase::CleanupFailed,
            reason: reason.into(),
            cleanup_attempt: Some(attempt),
        });
    }
}
#[derive(Debug, Clone, Copy)]
struct Present {
    pid: u32,
    generation: u64,
    qpc: i64,
}
enum Input {
    Present(Present),
    Wake,
}
struct Running {
    shared: Arc<Shared>,
    sender: SyncSender<Input>,
    join: JoinHandle<Result<FpsStopReport>>,
}
static ACTIVE: AtomicBool = AtomicBool::new(false);
struct Lease;
impl Drop for Lease {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Release);
    }
}
#[derive(Default)]
pub struct FpsSampler {
    running: Option<Running>,
}
impl FpsSampler {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn start(&mut self, options: FpsOptions) -> Result<()> {
        options.validate()?;
        ensure!(
            self.running.is_none(),
            "FPS worker has not finished cleanup"
        );
        ensure!(
            ACTIVE
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "This process already owns an FPS worker"
        );
        let lease = Lease;
        let shared = Arc::new(Shared::new());
        shared.status(FpsPhase::Starting, "");
        let (sender, receive) = mpsc::sync_channel(2048);
        let worker_shared = shared.clone();
        let worker_sender = sender.clone();
        let join = thread::Builder::new()
            .name("jiks-fps-control".into())
            .spawn(move || {
                let _lease = lease;
                let result = native::run(options, worker_shared.clone(), worker_sender, receive);
                if let Err(error) = &result {
                    worker_shared.status(FpsPhase::Stopped, format!("{error:#}"));
                    worker_shared.emit(FpsMessage::Fault {
                        message: format!("{error:#}"),
                        cleanup_pending: false,
                    });
                }
                result
            })?;
        self.running = Some(Running {
            shared,
            sender,
            join,
        });
        Ok(())
    }
    pub fn is_active(&self) -> bool {
        self.running.is_some()
    }
    pub fn status(&self) -> Option<FpsStatus> {
        self.running
            .as_ref()?
            .shared
            .mailbox
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .status
            .clone()
    }
    pub fn poll(&mut self) -> Option<FpsMessage> {
        let running = self.running.as_ref()?;
        let mut box_ = running
            .shared
            .mailbox
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        box_.controls
            .pop_front()
            .or_else(|| box_.sample.take().map(FpsMessage::Sample))
    }
    /// Nonblocking. Repeating Stop is an explicit retry after cleanup failure.
    pub fn stop(&mut self) -> bool {
        self.stop_ticket().is_some()
    }
    /// Admission marker for explicit Stop/retry. Only a matching actual
    /// cleanup_attempt failure or try_finish can complete the caller's request.
    pub fn stop_ticket(&mut self) -> Option<u64> {
        let Some(running) = &self.running else {
            return None;
        };
        running.shared.stop.store(true, Ordering::Release);
        let previous = running
            .shared
            .retry
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                epoch.checked_add(1)
            })
            .ok()?;
        running.shared.emit(FpsMessage::Unavailable {
            reason: "FPS collection is stopping; no active reading".into(),
        });
        let _ = running.sender.try_send(Input::Wake);
        Some(previous + 1)
    }
    /// Some means the control thread actually ended, not merely that Stop was sent.
    /// Cleanup failures with owned ETW resources keep this None until a retry works.
    pub fn try_finish(&mut self) -> Option<Result<FpsStopReport>> {
        if !self.running.as_ref()?.join.is_finished() {
            return None;
        }
        let running = self.running.take().unwrap();
        Some(match running.join.join() {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("FPS worker panicked after native cleanup")),
        })
    }
}
impl Drop for FpsSampler {
    fn drop(&mut self) {
        self.stop();
    }
}

/// All timestamp/identity/freshness decisions are shared by native and fake IO.
#[derive(Default)]
struct Stream {
    target: Option<FpsProcess>,
    generation: u64,
    min_qpc: i64,
    frames: math::Accumulator,
    last_qpc: Option<i64>,
    losses: u64,
    max_gap_qpc: Option<u64>,
}
impl Stream {
    fn select(&mut self, target: Option<FpsProcess>, generation: u64, min_qpc: i64) {
        self.target = target;
        self.generation = generation;
        self.min_qpc = min_qpc;
        self.clear();
    }
    fn clear(&mut self) {
        self.frames.reset();
        self.last_qpc = None;
    }
    fn lost(&mut self, total: u64) -> bool {
        if total != self.losses {
            self.losses = total;
            self.clear();
            true
        } else {
            false
        }
    }
    fn present(&mut self, event: Present) -> bool {
        if self.target.as_ref().is_none_or(|p| p.pid != event.pid)
            || event.generation != self.generation
            || event.qpc < self.min_qpc
        {
            return false;
        }
        if event.qpc <= 0 {
            self.clear();
            return false;
        }
        if self.last_qpc.is_some_and(|previous| event.qpc < previous) {
            self.clear();
            return false;
        }
        if self.last_qpc.is_some_and(|previous| {
            self.max_gap_qpc
                .is_some_and(|limit| (event.qpc - previous) as u64 > limit)
        }) {
            self.clear();
        }
        let changed = self.frames.present(event.qpc);
        self.last_qpc = Some(event.qpc);
        changed
    }
    fn sample(
        &mut self,
        now_ms: u64,
        now_qpc: i64,
        frequency: u64,
        stale_ms: u64,
    ) -> Option<FpsSample> {
        let process = self.target.clone()?;
        let last = self.last_qpc?;
        if frequency == 0 || last > now_qpc {
            self.clear();
            return None;
        }
        let age = ((now_qpc - last) as u128).saturating_mul(1000) / frequency as u128;
        if age > stale_ms as u128 {
            self.clear();
            return None;
        }
        let values = self.frames.estimate(frequency);
        Some(FpsSample {
            process,
            generation: self.generation,
            fps: values.fps,
            low_fps: values.low,
            frame_time_ms: values.frame_ms,
            intervals: values.intervals,
            window_capacity: math::WINDOW,
            sampled_tick_ms: now_ms,
            last_present_tick_ms: now_ms.saturating_sub(age as u64),
            events_dropped: self.losses,
            etw_buffers_lost: 0,
            source: SOURCE.into(),
            process_aggregate: true,
            displayed_fps_verified: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target(pid: u32, born: u64) -> FpsProcess {
        FpsProcess {
            pid,
            creation_filetime: born,
            name: "fixture".into(),
        }
    }
    #[test]
    fn construction_disabled_start_and_idle_poll_have_no_worker_or_native_io() {
        let mut s = FpsSampler::new();
        assert!(!s.is_active());
        assert!(s.poll().is_none());
        assert!(s.try_finish().is_none());
        assert!(!s.stop());
        assert!(s.start(FpsOptions::default()).is_err());
        assert!(!s.is_active());
    }
    #[test]
    fn identity_switch_and_late_generation_never_mix_frame_intervals() {
        let mut s = Stream::default();
        s.select(Some(target(10, 1)), 1, 100);
        for qpc in [100, 1100, 2100] {
            s.present(Present {
                pid: 10,
                generation: 1,
                qpc,
            });
        }
        assert_eq!(s.sample(1, 2100, 60_000, 2000).unwrap().fps, Some(60.));
        s.select(Some(target(11, 2)), 2, 3000);
        assert!(!s.present(Present {
            pid: 10,
            generation: 1,
            qpc: 4000
        }));
        assert!(!s.present(Present {
            pid: 11,
            generation: 1,
            qpc: 4000
        }));
        assert!(!s.present(Present {
            pid: 11,
            generation: 2,
            qpc: 2999
        }));
        s.present(Present {
            pid: 11,
            generation: 2,
            qpc: 3000,
        });
        assert_eq!(s.sample(2, 3000, 60_000, 2000).unwrap().fps, None);
        s.select(Some(target(11, 3)), 3, 5000);
        assert!(!s.present(Present {
            pid: 11,
            generation: 2,
            qpc: 6000
        }));
        assert!(s.sample(3, 6000, 60_000, 2000).is_none());
    }
    #[test]
    fn loss_and_staleness_clear_fps_low_and_interval_history() {
        let mut s = Stream::default();
        s.select(Some(target(10, 1)), 1, 1);
        for qpc in [1, 1001, 2001] {
            s.present(Present {
                pid: 10,
                generation: 1,
                qpc,
            });
        }
        assert!(s.sample(10, 2001, 60_000, 2000).unwrap().fps.is_some());
        assert!(s.lost(1));
        assert!(s.sample(11, 2001, 60_000, 2000).is_none());
        for qpc in [3001, 4001, 5001] {
            s.present(Present {
                pid: 10,
                generation: 1,
                qpc,
            });
        }
        assert_eq!(s.sample(12, 5001, 60_000, 2000).unwrap().events_dropped, 1);
        assert!(s.sample(3000, 130_002, 60_000, 2000).is_none());
        s.present(Present {
            pid: 10,
            generation: 1,
            qpc: 131_002,
        });
        assert_eq!(s.sample(3001, 131_002, 60_000, 2000).unwrap().fps, None);
    }
    #[test]
    fn mailbox_coalesces_samples_but_retains_terminal_fault() {
        let shared = Shared::new();
        for _ in 0..100 {
            shared.emit(FpsMessage::Unavailable {
                reason: "fixture".into(),
            });
        }
        assert_eq!(shared.mailbox.lock().unwrap().controls.len(), 16);
        shared.emit(FpsMessage::Fault {
            message: "cleanup pending".into(),
            cleanup_pending: true,
        });
        assert!(matches!(
            shared.mailbox.lock().unwrap().controls.back(),
            Some(FpsMessage::Fault {
                cleanup_pending: true,
                ..
            })
        ));
    }
    #[test]
    fn stop_admission_is_not_actual_thread_completion() {
        let shared = Arc::new(Shared::new());
        let (send, _read) = mpsc::sync_channel(1);
        let (release, wait) = mpsc::sync_channel(1);
        let worker_shared = shared.clone();
        let join = thread::spawn(move || {
            wait.recv().unwrap();
            assert!(worker_shared.stop.load(Ordering::Acquire));
            Ok(FpsStopReport {
                session_name: "pure-fixture".into(),
                session_stopped: true,
                consumer_joined: true,
                events_dropped: 0,
            })
        });
        let mut sampler = FpsSampler {
            running: Some(Running {
                shared,
                sender: send,
                join,
            }),
        };
        let first = sampler.stop_ticket().unwrap();
        assert_eq!(first, 1);
        sampler
            .running
            .as_ref()
            .unwrap()
            .shared
            .cleanup_failed("first attempt", first);
        let second = sampler.stop_ticket().unwrap();
        assert_eq!(second, 2);
        assert_eq!(sampler.status().unwrap().cleanup_attempt, Some(first));
        sampler
            .running
            .as_ref()
            .unwrap()
            .shared
            .cleanup_failed("second attempt", second);
        assert_eq!(sampler.status().unwrap().cleanup_attempt, Some(second));
        assert!(sampler.try_finish().is_none());
        assert!(sampler.is_active());
        release.send(()).unwrap();
        for _ in 0..1000 {
            if let Some(result) = sampler.try_finish() {
                let report = result.unwrap();
                assert!(report.session_stopped && report.consumer_joined);
                assert!(!sampler.is_active());
                return;
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("pure fixture did not finish");
    }
    #[test]
    fn long_gap_resets_history_even_when_no_periodic_sample_ran_during_pause() {
        let mut s = Stream {
            max_gap_qpc: Some(120_000),
            ..Default::default()
        };
        s.select(Some(target(10, 1)), 1, 1);
        for qpc in [1, 1001, 2001] {
            s.present(Present {
                pid: 10,
                generation: 1,
                qpc,
            });
        }
        assert_eq!(s.sample(1, 2001, 60_000, 2000).unwrap().fps, Some(60.));
        s.present(Present {
            pid: 10,
            generation: 1,
            qpc: 300_001,
        });
        assert_eq!(s.sample(5000, 300_001, 60_000, 2000).unwrap().fps, None);
        for qpc in [301_001, 302_001] {
            s.present(Present {
                pid: 10,
                generation: 1,
                qpc,
            });
        }
        let sample = s.sample(5034, 302_001, 60_000, 2000).unwrap();
        assert_eq!(sample.fps, Some(60.));
        assert_eq!(sample.intervals, 2);
    }
}
