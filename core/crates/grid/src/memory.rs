//! What a run's grids will take, worked out from the board before anything is allocated, so a
//! board too large for the machine is refused or routed coarser instead of running it out of
//! memory.

use std::collections::BTreeSet;

use board::{Board, Nm};

use crate::grid::{map_extent, Profile};

/// A run's grids, sized: the figures the estimate is built from, and its total.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEstimate {
    /// Routing nodes, `layers × nx × ny`, and the cells of one layer.
    pub nodes: u64,
    pub cells: u64,
    /// Net classes with an obstacle map of their own.
    pub classes: u64,
    pub threads: u64,
    pub bytes: u64,
}

/// Bytes per node and per cell of each structure, as allocated.
/// An obstacle map: `owner` per node, `via_owner` per cell (`i32` each).
const MAP_NODE: u64 = 4;
const MAP_CELL: u64 = 4;
/// Per thread, one net's view (`blocked`, `penalty`, `region_cost`) and its search's buffers
/// (`cost`, `gen`, `flags`), and a price buffer.
const THREAD_NODE: u64 = 9 + 9 + 4;
/// A view's `via_blocked` and `via_penalty`, per cell.
const THREAD_CELL: u64 = 5;
/// Negotiation: each node's history, and each viewing class's count of claims.
const HISTORY_NODE: u64 = 4;
const CLAIMS_NODE: u64 = 2;
/// The board model per item, with its spatial index.
const ITEM: u64 = 400;
/// What the structures above leave out, per node: the maps the stages build beside the
/// negotiation's (with and without the pours, the shove's copies) and the claims' per-net maps.
/// Fitted to the peaks measured on the fixtures (earring 1.7 M nodes 236 MB, bluebuzz 3.5 M nodes
/// 406 MB, one thread), with room to spare.
const OTHER_NODE: u64 = 128;

impl MemoryEstimate {
    pub fn mb(&self) -> u64 {
        self.bytes.div_ceil(1 << 20)
    }
}

/// The memory a run on `board` with grid cell `cell` and `threads` threads, each holding a
/// view and search of the whole board, will take.
pub fn memory_estimate(board: &Board, cell: Nm, threads: usize) -> MemoryEstimate {
    let b = map_extent(board);
    let h = cell.max(1);
    let nx = ((b.max.x - b.min.x) / h).max(1) as u64;
    let ny = ((b.max.y - b.min.y) / h).max(1) as u64;
    let cells = nx * ny;
    let nodes = board.layers.len() as u64 * cells;
    let classes: BTreeSet<String> = (1..board.nets.len() as u32).map(|net| Profile::for_net(board, net).class).collect();
    let classes = classes.len().max(1) as u64;
    let threads = threads.max(1) as u64;
    let bytes = classes * (MAP_NODE * nodes + MAP_CELL * cells)
        + threads * (THREAD_NODE * nodes + THREAD_CELL * cells)
        + (HISTORY_NODE + CLAIMS_NODE * classes) * nodes
        + ITEM * board.items().count() as u64
        + OTHER_NODE * nodes;
    MemoryEstimate { nodes, cells, classes, threads, bytes }
}
