//! Counts of the work the board model does, for a run's stats: how often a piece of copper is
//! checked against the rules, and how often the board's connectivity is worked out. The counts
//! are the process's, kept with relaxed atomics; they only ever go up, and nothing reads them
//! to decide anything.

use std::sync::atomic::{AtomicU64, Ordering};

/// `check_item` and `check_item_with_pours` calls.
pub static CHECKS: AtomicU64 = AtomicU64::new(0);
/// Connectivity worked out, for the whole board or one net.
pub static CONNECTIVITY: AtomicU64 = AtomicU64::new(0);

/// One more of `counter`.
pub fn count(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}
