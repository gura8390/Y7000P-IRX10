//! Bounded frame-interval statistics. No clocks, processes, ETW or other IO.
//! The legacy LLT PresentMonFps contract uses 500 intervals and averages the
//! slowest 1% of instantaneous FPS. Average FPS correctly counts every interval;
//! the old wrapper used n-1 for n intervals and underreported a steady stream.
use std::collections::VecDeque;

pub(super) const WINDOW: usize = 500;
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Estimate {
    pub fps: Option<f64>,
    pub low: Option<f64>,
    pub frame_ms: Option<f64>,
    pub intervals: usize,
}
#[derive(Debug, Default)]
pub(super) struct Accumulator {
    last: Option<i64>,
    intervals: VecDeque<u64>,
    sum: u128,
}
impl Accumulator {
    pub fn reset(&mut self) {
        self.last = None;
        self.intervals.clear();
        self.sum = 0;
    }
    /// A nonpositive/reversed timestamp invalidates continuity. A duplicate is
    /// ignored (it cannot represent an elapsed frame), rather than yielding inf.
    pub fn present(&mut self, qpc: i64) -> bool {
        if qpc <= 0 {
            self.reset();
            return false;
        }
        let Some(previous) = self.last else {
            self.last = Some(qpc);
            return false;
        };
        if qpc < previous {
            self.reset();
            return false;
        }
        if qpc == previous {
            return false;
        }
        let delta = (qpc - previous) as u64;
        self.last = Some(qpc);
        if self.intervals.len() == WINDOW {
            self.sum -= self.intervals.pop_front().unwrap() as u128;
        }
        self.intervals.push_back(delta);
        self.sum += delta as u128;
        true
    }
    pub fn estimate(&self, frequency: u64) -> Estimate {
        let count = self.intervals.len();
        if frequency == 0 || count == 0 {
            return Estimate {
                fps: None,
                low: None,
                frame_ms: None,
                intervals: count,
            };
        }
        let frame_ms = self
            .intervals
            .back()
            .map(|ticks| *ticks as f64 * 1000. / frequency as f64);
        let fps =
            (count >= 2 && self.sum > 0).then(|| count as f64 * frequency as f64 / self.sum as f64);
        let low = if count == WINDOW {
            let mut worst = self.intervals.iter().copied().collect::<Vec<_>>();
            worst.sort_unstable_by(|a, b| b.cmp(a));
            let n = (count / 100).max(1);
            Some(
                worst[..n]
                    .iter()
                    .map(|ticks| frequency as f64 / *ticks as f64)
                    .sum::<f64>()
                    / n as f64,
            )
        } else {
            None
        };
        Estimate {
            fps,
            low,
            frame_ms,
            intervals: count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn steady_stream_counts_all_intervals_not_n_minus_one() {
        let mut a = Accumulator::default();
        for i in 1..=501 {
            a.present(i * 1000);
        }
        let s = a.estimate(60_000);
        assert_eq!(s.intervals, 500);
        assert_eq!(s.fps, Some(60.));
        assert_eq!(s.low, Some(60.));
        assert!((s.frame_ms.unwrap() - 1000. / 60.).abs() < 1e-9);
    }
    #[test]
    fn low_requires_full_window_and_uses_worst_one_percent() {
        let mut a = Accumulator::default();
        let mut qpc = 1;
        a.present(qpc);
        for _ in 0..495 {
            qpc += 1000;
            a.present(qpc);
        }
        assert!(a.estimate(60_000).low.is_none());
        for _ in 0..5 {
            qpc += 2000;
            a.present(qpc);
        }
        let s = a.estimate(60_000);
        assert_eq!(s.low, Some(30.));
        assert!(s.fps.unwrap() < 60.);
    }
    #[test]
    fn eviction_removes_old_hitches_and_bounds_memory() {
        let mut a = Accumulator::default();
        let mut qpc = 1;
        a.present(qpc);
        for _ in 0..5 {
            qpc += 10_000;
            a.present(qpc);
        }
        for _ in 0..1000 {
            qpc += 1000;
            a.present(qpc);
        }
        assert_eq!(a.intervals.len(), 500);
        assert_eq!(a.sum, 500_000);
        assert_eq!(a.estimate(60_000).fps, Some(60.));
        assert_eq!(a.estimate(60_000).low, Some(60.));
    }
    #[test]
    fn invalid_clock_and_reset_never_publish_old_or_infinite_values() {
        let mut a = Accumulator::default();
        assert!(!a.present(1000));
        assert!(a.present(2000));
        assert!(a.present(3000));
        assert_eq!(a.estimate(60_000).fps, Some(60.));
        assert!(!a.present(2999));
        assert_eq!(a.estimate(60_000).fps, None);
        assert!(!a.present(0));
        assert!(a.intervals.is_empty());
        assert_eq!(a.estimate(0).frame_ms, None);
    }
    #[test]
    fn rejected_old_timestamp_cannot_anchor_a_new_window() {
        let mut a = Accumulator::default();
        for qpc in [1000, 2000, 3000] {
            a.present(qpc);
        }
        assert!(!a.present(2999));
        assert!(!a.present(4000));
        assert!(a.present(5000));
        assert!(a.present(6000));
        assert_eq!(a.estimate(60_000).fps, Some(60.));
        assert_eq!(a.estimate(60_000).intervals, 2);
    }
    #[test]
    fn duplicate_event_has_no_zero_duration_sample() {
        let mut a = Accumulator::default();
        a.present(1);
        a.present(1001);
        assert!(!a.present(1001));
        a.present(2001);
        assert_eq!(a.estimate(60_000).fps, Some(60.));
        assert_eq!(a.intervals.len(), 2);
    }
    #[test]
    fn large_qpc_does_not_overflow_interval_sum() {
        let mut a = Accumulator::default();
        a.present(1);
        a.present(i64::MAX / 2);
        a.present(i64::MAX - 1);
        let s = a.estimate(10_000_000);
        assert!(s.fps.unwrap().is_finite());
        assert!(s.frame_ms.unwrap().is_finite());
    }
}
