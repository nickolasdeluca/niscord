//! Frame-rate limiting that holds a steady cadence.
//!
//! Accepting a frame only if enough time passed since the *last accepted*
//! frame halves the rate whenever the source runs slightly faster than the
//! target (a 35 fps source limited to 30 fps keeps every other frame: 17 fps).
//! Instead, keep a schedule of due times and accept the first frame at or
//! after each one.

use std::time::{Duration, Instant};

pub struct Pacer {
    interval: Duration,
    /// Frames this early are still accepted, to absorb timing jitter.
    slack: Duration,
    next_due: Option<Instant>,
}

impl Pacer {
    pub fn new(fps: u32) -> Self {
        let interval = Duration::from_secs(1) / fps.max(1);
        Self { interval, slack: interval / 10, next_due: None }
    }

    /// Whether a frame arriving at `now` should be kept.
    pub fn accept(&mut self, now: Instant) -> bool {
        if let Some(due) = self.next_due {
            if now + self.slack < due {
                return false;
            }
            // Stay on schedule, but after a long gap (static content) don't
            // try to "catch up" with a burst of frames.
            let next = due + self.interval;
            self.next_due = Some(if next + self.interval < now { now + self.interval } else { next });
        } else {
            self.next_due = Some(now + self.interval);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed frames every `source_ms` for one second; count accepted ones.
    fn accepted(target_fps: u32, source_ms: u64) -> usize {
        let mut pacer = Pacer::new(target_fps);
        let start = Instant::now();
        (0..1000 / source_ms).filter(|i| pacer.accept(start + Duration::from_millis(i * source_ms))).count()
    }

    #[test]
    fn slightly_faster_source_is_not_halved() {
        // 35 fps source, 30 fps target: the old "since last" logic gave ~17.
        let n = accepted(30, 28);
        assert!((28..=31).contains(&n), "{n}");
    }

    #[test]
    fn much_faster_source_is_limited() {
        assert!((29..=31).contains(&accepted(30, 4)));
        assert!((58..=61).contains(&accepted(60, 4)));
    }

    #[test]
    fn slower_source_passes_everything() {
        assert_eq!(accepted(60, 50), 20);
        assert_eq!(accepted(30, 40), 25);
    }

    #[test]
    fn no_burst_after_idle() {
        let mut pacer = Pacer::new(30);
        let t0 = Instant::now();
        assert!(pacer.accept(t0));
        // Content was static for a second, then frames arrive every 4 ms for
        // less than one interval: only the first may pass.
        let resume = t0 + Duration::from_secs(1);
        let burst = (0..8).filter(|i| pacer.accept(resume + Duration::from_millis(i * 4))).count();
        assert_eq!(burst, 1);
    }
}
