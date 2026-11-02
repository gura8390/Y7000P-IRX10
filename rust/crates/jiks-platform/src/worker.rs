//! Single physical worker. A timed-out ticket does not cancel or replace it.
use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
};
use std::time::Duration;

pub trait WorkerState: 'static {
    fn shutdown(&mut self) -> Result<()>;
}
type Work<T> = Box<dyn FnOnce(&mut T) -> Result<Value> + Send + 'static>;
struct Job<T> {
    operation: Option<Work<T>>, // None is an acknowledged shutdown barrier.
    reply: SyncSender<std::result::Result<Value, String>>,
}
#[derive(Clone, Debug)]
pub struct WorkerStatus {
    pub busy: bool,
    pub last_error: Option<String>,
    pub completed: u64,
}
struct Shared {
    busy: AtomicBool,
    poisoned: AtomicBool,
    shutting_down: AtomicBool,
    shutdown_confirmed: AtomicBool,
    orphaned_cleanup_failed: AtomicBool,
    last_error: Mutex<Option<String>>,
    completed: AtomicU64,
}
pub struct Ticket {
    pub id: u64,
    receiver: Receiver<std::result::Result<Value, String>>,
}
#[derive(Debug)]
pub enum Completion {
    Pending { operation_id: u64 },
    Completed(Value),
}
impl Ticket {
    pub fn wait(&mut self, timeout: Duration) -> Result<Completion> {
        match self.receiver.recv_timeout(timeout) {
            Ok(Ok(value)) => Ok(Completion::Completed(value)),
            Ok(Err(error)) => Err(anyhow!(error)),
            Err(RecvTimeoutError::Timeout) => Ok(Completion::Pending {
                operation_id: self.id,
            }),
            Err(RecvTimeoutError::Disconnected) => {
                bail!("Hardware worker ended without a verified result")
            }
        }
    }
}
pub struct BoundedWorker<T: WorkerState> {
    sender: SyncSender<Job<T>>,
    shared: Arc<Shared>,
    next: AtomicU64,
}
impl<T: WorkerState> BoundedWorker<T> {
    /// The factory runs on the worker, so apartment-bound state never crosses threads.
    pub fn spawn(factory: impl FnOnce() -> Result<T> + Send + 'static) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Job<T>>(1);
        let shared = Arc::new(Shared {
            busy: AtomicBool::new(false),
            poisoned: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            shutdown_confirmed: AtomicBool::new(false),
            orphaned_cleanup_failed: AtomicBool::new(false),
            last_error: Mutex::new(None),
            completed: AtomicU64::new(0),
        });
        let worker_shared = shared.clone();
        std::thread::Builder::new()
            .name("jiks-hardware".into())
            .spawn(move || {
                let mut state = match factory() {
                    Ok(state) => state,
                    Err(error) => {
                        *worker_shared
                            .last_error
                            .lock()
                            .unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
                        while let Ok(job) = receiver.recv() {
                            if job.operation.is_none(){
                                worker_shared.shutdown_confirmed.store(true,Ordering::Release);
                                worker_shared.completed.fetch_add(1,Ordering::Release);
                                worker_shared.busy.store(false,Ordering::Release);
                                let _=job.reply.send(Ok(serde_json::json!({"shutdown_confirmed":true,"initialized":false,"owner_released":true})));
                                return;
                            }
                            worker_shared.completed.fetch_add(1,Ordering::Release);
                            worker_shared.busy.store(false, Ordering::Release);
                            let _ = job.reply.send(Err(error.to_string()));
                        }
                        return;
                    }
                };
                while let Ok(job) = receiver.recv() {
                    let Some(operation)=job.operation else{
                        let cleanup=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||state.shutdown()))
                            .map_err(|_|anyhow!("Hardware cleanup panicked; restoration is unverified"))
                            .and_then(|result|result);
                        if let Err(error)=cleanup{
                            worker_shared.poisoned.store(true,Ordering::Release);
                            *worker_shared.last_error.lock().unwrap_or_else(|e|e.into_inner())=Some(format!("Restoration unverified: {error}"));
                            worker_shared.shutting_down.store(false,Ordering::Release);
                            worker_shared.completed.fetch_add(1,Ordering::Release);
                            worker_shared.busy.store(false,Ordering::Release);
                            let _=job.reply.send(Err(format!("Hardware shutdown was not confirmed: {error}")));
                            continue; // Retain state/owner after failure; no false acknowledgement.
                        }
                        // The barrier is acknowledged only AFTER all apartment-bound
                        // state (including every owner Rc) has physically been dropped.
                        let dropped=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||drop(state)));
                        worker_shared.completed.fetch_add(1,Ordering::Release);
                        if dropped.is_ok(){
                            worker_shared.shutdown_confirmed.store(true,Ordering::Release);
                            worker_shared.busy.store(false,Ordering::Release);
                            let _=job.reply.send(Ok(serde_json::json!({"shutdown_confirmed":true,"initialized":true,"owner_released":true})));
                        }else{
                            worker_shared.poisoned.store(true,Ordering::Release);
                            *worker_shared.last_error.lock().unwrap_or_else(|e|e.into_inner())=Some("Hardware state drop panicked; owner release is unverified".into());
                            worker_shared.busy.store(false,Ordering::Release);
                            let _=job.reply.send(Err("Hardware state drop panicked; shutdown was not confirmed".into()));
                        }
                        return;
                    };
                    if worker_shared.poisoned.load(Ordering::Acquire) {
                        worker_shared.busy.store(false, Ordering::Release);
                        let _ = job.reply.send(Err("Hardware worker is latched after a panic; queued operation was not executed".into()));
                        continue;
                    }
                    // No cancellation/drop branch: state, owner, buffers and gate live until I/O returns.
                    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        operation(&mut state)
                    }));
                    if caught.is_err() {
                        worker_shared.poisoned.store(true, Ordering::Release);
                    }
                    let result = caught
                        .map_err(|_| {
                            anyhow!("Hardware operation panicked; restoration must be verified")
                        })
                        .and_then(|r| r);
                    if let Err(error) = &result {
                        *worker_shared
                            .last_error
                            .lock()
                            .unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
                    }
                    let result = result.map_err(|e| e.to_string());
                    worker_shared.completed.fetch_add(1, Ordering::Release);
                    // Clear the busy flag only AFTER the physical operation is finished.
                    worker_shared.busy.store(false, Ordering::Release);
                    let _ = job.reply.send(result);
                }
                let cleanup=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||state.shutdown()))
                    .map_err(|_|anyhow!("Orphaned hardware cleanup panicked"))
                    .and_then(|result|result);
                if let Err(error) = cleanup {
                    *worker_shared
                        .last_error
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) =
                        Some(format!("Restoration unverified: {error}"));
                    worker_shared.poisoned.store(true,Ordering::Release);
                    worker_shared.orphaned_cleanup_failed.store(true,Ordering::Release);
                    // The coordinator vanished without a successful shutdown
                    // barrier. Keep this thread/state/kernel owner alive; dropping
                    // it would let another controller proceed after failed restore.
                    // No automatic setter retry is permitted after a latched fault.
                    loop {std::hint::black_box(&mut state);std::thread::park();}
                }
            })?;
        Ok(Self {
            sender,
            shared,
            next: AtomicU64::new(1),
        })
    }
    pub fn submit(
        &self,
        operation: impl FnOnce(&mut T) -> Result<Value> + Send + 'static,
    ) -> Result<Ticket> {
        self.enqueue(Some(Box::new(operation)))
    }
    pub fn shutdown_ticket(&self) -> Result<Ticket> {
        if self.shared.shutdown_confirmed.load(Ordering::Acquire) {
            let (reply, receiver) = mpsc::sync_channel(1);
            reply
                .send(Ok(
                    serde_json::json!({"shutdown_confirmed":true,"owner_released":true}),
                ))
                .map_err(|_| anyhow!("Cannot create completed shutdown ticket"))?;
            return Ok(Ticket {
                id: self.next.fetch_add(1, Ordering::Relaxed),
                receiver,
            });
        }
        self.enqueue(None)
    }
    fn enqueue(&self, operation: Option<Work<T>>) -> Result<Ticket> {
        let shutdown = operation.is_none();
        if !shutdown
            && (self.shared.shutting_down.load(Ordering::Acquire)
                || self.shared.shutdown_confirmed.load(Ordering::Acquire))
        {
            bail!("Hardware worker is closing/closed; no new operation was issued")
        }
        if !shutdown && self.shared.poisoned.load(Ordering::Acquire) {
            bail!("Hardware worker is latched after a panic; no new operation was issued")
        }
        if self
            .shared
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            bail!(
                "Previous physical operation is still pending; no extra worker or command was created"
            )
        }
        // A prior job can panic between the initial latch read and acquiring busy.
        if !shutdown
            && (self.shared.poisoned.load(Ordering::Acquire)
                || self.shared.shutting_down.load(Ordering::Acquire)
                || self.shared.shutdown_confirmed.load(Ordering::Acquire))
        {
            self.shared.busy.store(false, Ordering::Release);
            bail!("Hardware worker is latched after a panic; no new operation was issued")
        }
        if shutdown {
            self.shared.shutting_down.store(true, Ordering::Release);
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        if self.sender.try_send(Job { operation, reply }).is_err() {
            if shutdown {
                self.shared.shutting_down.store(false, Ordering::Release);
            }
            self.shared.busy.store(false, Ordering::Release);
            bail!("Hardware worker unavailable")
        }
        Ok(Ticket { id, receiver })
    }
    pub fn status(&self) -> WorkerStatus {
        WorkerStatus {
            busy: self.shared.busy.load(Ordering::Acquire),
            last_error: self
                .shared
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            completed: self.shared.completed.load(Ordering::Acquire),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        restored: Arc<AtomicBool>,
    }
    impl WorkerState for Fake {
        fn shutdown(&mut self) -> Result<()> {
            self.restored.store(true, Ordering::Release);
            Ok(())
        }
    }
    #[test]
    fn timeout_retains_one_worker_and_late_completion() {
        let restored = Arc::new(AtomicBool::new(false));
        let signal = restored.clone();
        let worker = BoundedWorker::spawn(move || Ok(Fake { restored: signal })).unwrap();
        let (release_tx, release_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let mut ticket = worker
            .submit(move |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(Value::from(7))
            })
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            ticket.wait(Duration::from_millis(1)).unwrap(),
            Completion::Pending { .. }
        ));
        assert!(worker.status().busy);
        assert!(worker.submit(|_| Ok(Value::Null)).is_err());
        assert!(!restored.load(Ordering::Acquire));
        release_tx.send(()).unwrap();
        assert!(
            matches!(ticket.wait(Duration::from_secs(2)).unwrap(),Completion::Completed(v) if v==Value::from(7))
        );
        assert!(!worker.status().busy);
        drop(worker);
        for _ in 0..100 {
            if restored.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("Worker did not run cleanup after the real operation completed");
    }
    #[test]
    fn panic_latch_also_rejects_a_job_accepted_by_a_racing_submitter() {
        let restored = Arc::new(AtomicBool::new(false));
        let signal = restored.clone();
        let worker = BoundedWorker::spawn(move || Ok(Fake { restored: signal })).unwrap();
        let mut panic_ticket = worker.submit(|_| panic!("fixture panic")).unwrap();
        assert!(panic_ticket.wait(Duration::from_secs(2)).is_err());
        assert!(worker.submit(|_| Ok(Value::Null)).is_err());
        // Reproduce an already-accepted enqueue after the panic, independent of
        // scheduler luck. The worker must recheck the latch before invocation.
        let executed = Arc::new(AtomicBool::new(false));
        let flag = executed.clone();
        let (reply, receiver) = mpsc::sync_channel(1);
        worker
            .sender
            .try_send(Job {
                operation: Some(Box::new(move |_| {
                    flag.store(true, Ordering::Release);
                    Ok(Value::Null)
                })),
                reply,
            })
            .unwrap();
        assert!(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        assert!(!executed.load(Ordering::Acquire));
    }
    #[test]
    fn shutdown_ack_waits_for_state_drop_and_is_idempotent() {
        struct Owned {
            released: Arc<AtomicBool>,
        }
        impl WorkerState for Owned {
            fn shutdown(&mut self) -> Result<()> {
                Ok(())
            }
        }
        impl Drop for Owned {
            fn drop(&mut self) {
                self.released.store(true, Ordering::Release)
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        let signal = released.clone();
        let worker = BoundedWorker::spawn(move || Ok(Owned { released: signal })).unwrap();
        let mut ticket = worker.shutdown_ticket().unwrap();
        assert!(
            matches!(ticket.wait(Duration::from_secs(2)).unwrap(),Completion::Completed(value)if value["shutdown_confirmed"]==true)
        );
        assert!(released.load(Ordering::Acquire));
        assert!(worker.submit(|_| Ok(Value::Null)).is_err());
        assert!(
            matches!(worker.shutdown_ticket().unwrap().wait(Duration::from_secs(2)).unwrap(),Completion::Completed(value)if value["owner_released"]==true)
        );
    }
    #[test]
    fn shutdown_failure_retains_owned_state_and_never_confirms() {
        struct Owned {
            released: Arc<AtomicBool>,
        }
        impl WorkerState for Owned {
            fn shutdown(&mut self) -> Result<()> {
                bail!("fixture restore failure")
            }
        }
        impl Drop for Owned {
            fn drop(&mut self) {
                self.released.store(true, Ordering::Release)
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        let signal = released.clone();
        let worker = BoundedWorker::spawn(move || Ok(Owned { released: signal })).unwrap();
        assert!(
            worker
                .shutdown_ticket()
                .unwrap()
                .wait(Duration::from_secs(2))
                .is_err()
        );
        assert!(!released.load(Ordering::Acquire));
        assert!(worker.submit(|_| Ok(Value::Null)).is_err());
        assert!(
            worker
                .shutdown_ticket()
                .unwrap()
                .wait(Duration::from_secs(2))
                .is_err()
        );
        assert!(!released.load(Ordering::Acquire));
    }
    #[test]
    fn initialization_failure_with_no_state_can_confirm_read_only_shutdown() {
        let worker = BoundedWorker::<Fake>::spawn(|| {
            bail!("fixture initialization failure before ownership")
        })
        .unwrap();
        assert!(
            worker
                .submit(|_| Ok(Value::Null))
                .unwrap()
                .wait(Duration::from_secs(2))
                .is_err()
        );
        let value = worker
            .shutdown_ticket()
            .unwrap()
            .wait(Duration::from_secs(2))
            .unwrap();
        assert!(
            matches!(value,Completion::Completed(value)if value["shutdown_confirmed"]==true&&value["initialized"]==false)
        );
    }
    #[test]
    fn channel_eof_cannot_drop_owner_after_failed_fallback_restore() {
        struct Owned {
            released: Arc<AtomicBool>,
        }
        impl WorkerState for Owned {
            fn shutdown(&mut self) -> Result<()> {
                bail!("fixture orphan restore failure")
            }
        }
        impl Drop for Owned {
            fn drop(&mut self) {
                self.released.store(true, Ordering::Release)
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        let signal = released.clone();
        let worker = BoundedWorker::spawn(move || Ok(Owned { released: signal })).unwrap();
        let shared = worker.shared.clone();
        drop(worker);
        for _ in 0..2000 {
            if shared.orphaned_cleanup_failed.load(Ordering::Acquire) {
                assert!(!released.load(Ordering::Acquire));
                assert!(!shared.shutdown_confirmed.load(Ordering::Acquire));
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("Worker did not enter the retained-owner fail-closed fallback");
    }
}
