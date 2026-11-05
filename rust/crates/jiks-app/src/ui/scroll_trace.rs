//! Debug-only trace of a real Win32 message loop. No I/O runs while the window
//! is scrolling. The trace is saved after the window closes.
use serde::Serialize;
use std::{cell::RefCell, path::PathBuf, time::Instant};

#[derive(Serialize)]
struct Sample {
    elapsed_us: u64,
    event: &'static str,
    a: i64,
    b: i64,
}

struct Trace {
    start: Instant,
    output: PathBuf,
    samples: Vec<Sample>,
}

thread_local! {
    static TRACE: RefCell<Option<Trace>> = const { RefCell::new(None) };
}

pub(super) struct Session;

impl Session {
    pub(super) fn start() -> Self {
        if let Some(output) = std::env::var_os("JIKS_UI_SCROLL_TRACE") {
            TRACE.with(|slot| {
                *slot.borrow_mut() = Some(Trace {
                    start: Instant::now(),
                    output: output.into(),
                    samples: Vec::with_capacity(1024),
                });
            });
        }
        Self
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(trace) = TRACE.with(|slot| slot.borrow_mut().take())
            && let Ok(bytes) = serde_json::to_vec_pretty(&trace.samples)
        {
            let _ = std::fs::write(trace.output, bytes);
        }
    }
}

pub(super) fn record(event: &'static str, a: i64, b: i64) {
    TRACE.with(|slot| {
        if let Some(trace) = slot.borrow_mut().as_mut()
            && trace.samples.len() < 8192
        {
            trace.samples.push(Sample {
                elapsed_us: trace.start.elapsed().as_micros() as u64,
                event,
                a,
                b,
            });
        }
    });
}
