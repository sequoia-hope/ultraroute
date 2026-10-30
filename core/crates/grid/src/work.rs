//! Counts of the work the grid router does, for a run's stats: searches and the nodes they
//! expanded, cost fields solved, and obstacle maps built; with the board model's own counts
//! ([`board::work`]). The counts are the process's, kept with relaxed atomics and added once per
//! search, not per node; nothing reads them to decide anything, so they change no result.

use std::sync::atomic::{AtomicU64, Ordering};

/// A* searches, and the nodes they expanded.
pub static ASTAR: AtomicU64 = AtomicU64::new(0);
pub static ASTAR_EXPANDED: AtomicU64 = AtomicU64::new(0);
/// Cost fields solved on the CPU, and the nodes they settled.
pub static FIELDS: AtomicU64 = AtomicU64::new(0);
pub static FIELD_SETTLED: AtomicU64 = AtomicU64::new(0);
/// Obstacle maps built, whole-board or windows.
pub static MAPS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn add(counter: &AtomicU64, n: usize) {
    counter.fetch_add(n as u64, Ordering::Relaxed);
}

/// The counts at one moment; two of them subtract to the work done between.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Work {
    pub astar: u64,
    pub astar_expanded: u64,
    pub fields: u64,
    pub field_settled: u64,
    pub maps: u64,
    pub checks: u64,
    pub connectivity: u64,
}

impl Work {
    /// The counts now.
    pub fn now() -> Work {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Work {
            astar: get(&ASTAR),
            astar_expanded: get(&ASTAR_EXPANDED),
            fields: get(&FIELDS),
            field_settled: get(&FIELD_SETTLED),
            maps: get(&MAPS),
            checks: get(&board::work::CHECKS),
            connectivity: get(&board::work::CONNECTIVITY),
        }
    }

    /// The work done since `earlier`.
    pub fn since(&self, earlier: &Work) -> Work {
        Work {
            astar: self.astar.saturating_sub(earlier.astar),
            astar_expanded: self.astar_expanded.saturating_sub(earlier.astar_expanded),
            fields: self.fields.saturating_sub(earlier.fields),
            field_settled: self.field_settled.saturating_sub(earlier.field_settled),
            maps: self.maps.saturating_sub(earlier.maps),
            checks: self.checks.saturating_sub(earlier.checks),
            connectivity: self.connectivity.saturating_sub(earlier.connectivity),
        }
    }
}
