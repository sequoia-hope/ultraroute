//! The clock the router keeps time by: the standard one, except in a browser, where the standard
//! one panics and the page's own is used instead. And the deadline every stage keeps.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;
#[cfg(target_arch = "wasm32")]
pub use web_time::Instant;

/// When a stage must stop. The run makes one from `--timeout` and hands it down; a stage gives the
/// stages it calls a share of what is left of its own, never more, so no stage can outlast the run.
#[derive(Clone, Debug)]
pub struct Deadline {
    /// When this stage must stop; `None` for no limit.
    pub at: Option<Instant>,
    /// When the whole run must stop: never later than `at`.
    pub hard: Option<Instant>,
    /// Set from outside (the studio's stop button) to stop everything at the next check.
    pub cancel: Arc<AtomicBool>,
}

impl Default for Deadline {
    fn default() -> Self {
        Deadline::unlimited(Arc::new(AtomicBool::new(false)))
    }
}

impl Deadline {
    pub fn unlimited(cancel: Arc<AtomicBool>) -> Self {
        Deadline { at: None, hard: None, cancel }
    }

    /// A deadline for the whole run, ending at `end`.
    pub fn until(end: Instant, cancel: Arc<AtomicBool>) -> Self {
        Deadline { at: Some(end), hard: Some(end), cancel }
    }

    /// `seconds` from now, or no limit; for tests and callers that have no run to share.
    pub fn max_seconds(seconds: Option<f64>) -> Self {
        let d = Deadline::default();
        match seconds {
            Some(s) => d.seconds(s),
            None => d,
        }
    }

    /// Whether this stage's time or the run's has gone, or the run was cancelled.
    pub fn expired(&self) -> bool {
        self.cancelled() || self.hard_passed() || self.at.is_some_and(|at| Instant::now() >= at)
    }

    /// Whether the run as a whole must stop: its hard deadline passed, or it was cancelled.
    pub fn hard_passed(&self) -> bool {
        self.cancelled() || self.hard.is_some_and(|hard| Instant::now() >= hard)
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// The time left to this stage's deadline.
    pub fn remaining(&self) -> Option<Duration> {
        self.at.map(|at| at.saturating_duration_since(Instant::now()))
    }

    /// A sub-stage's deadline: `fraction` of what is left of this one, the run's unchanged.
    pub fn share(&self, fraction: f64) -> Deadline {
        let now = Instant::now();
        let at = self.remaining().map(|left| now + left.mul_f64(fraction.clamp(0.0, 1.0)));
        Deadline { at, hard: self.hard, cancel: self.cancel.clone() }
    }

    /// A sub-stage's deadline `s` seconds from now, or this one's if it comes sooner.
    pub fn seconds(&self, s: f64) -> Deadline {
        let mut end = Instant::now() + Duration::from_secs_f64(s.max(0.0));
        if let Some(at) = self.at.or(self.hard) {
            end = end.min(at);
        }
        Deadline { at: Some(end), hard: self.hard, cancel: self.cancel.clone() }
    }

    /// A sub-stage's deadline at `end`, or this one's if it comes sooner.
    pub fn until_at(&self, end: Instant) -> Deadline {
        let end = self.at.map_or(end, |at| at.min(end));
        Deadline { at: Some(end), hard: self.hard, cancel: self.cancel.clone() }
    }

    /// This deadline with the stage's own end taken off: only the run's hard end and the cancel flag
    /// stop it.
    pub fn to_hard(&self) -> Deadline {
        Deadline { at: self.hard, hard: self.hard, cancel: self.cancel.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_deadlines_never_outlast_their_parent() {
        let cancel = Arc::new(AtomicBool::new(false));
        let run = Deadline::until(Instant::now() + Duration::from_secs(10), cancel.clone());
        let half = run.share(0.5);
        assert!(half.at.unwrap() <= run.at.unwrap());
        assert!(half.at.unwrap() > Instant::now() + Duration::from_secs(4));
        assert_eq!(half.hard, run.hard);
        // A share over one is a share of one.
        assert!(run.share(3.0).at.unwrap() <= run.at.unwrap());
        // Seconds past the parent's end end with it.
        let long = half.seconds(60.0);
        assert!(long.at.unwrap() <= half.at.unwrap());
        assert!(half.seconds(1.0).at.unwrap() < half.at.unwrap());
        assert!(half.until_at(Instant::now() + Duration::from_secs(60)).at.unwrap() <= half.at.unwrap());
        // An unlimited stage given seconds still stops at the run's hard end.
        let open = Deadline { at: None, ..run.clone() };
        assert!(open.seconds(60.0).at.unwrap() <= run.hard.unwrap());
        assert!(open.share(0.5).at.is_none());
        assert!(!run.expired());
        cancel.store(true, Ordering::Relaxed);
        assert!(half.expired() && long.hard_passed());
    }

    #[test]
    fn a_passed_deadline_is_expired() {
        let d = Deadline::max_seconds(Some(0.0));
        assert!(d.expired());
        assert!(!d.hard_passed());
        assert!(!Deadline::max_seconds(None).expired());
        assert!(Deadline::max_seconds(None).remaining().is_none());
    }
}
