//! Shortest paths on a `RouteGrid`, as integer costs so the CPU reference and the GPU kernel
//! agree exactly: 10 per straight step, 14 per diagonal, `via_cost` per layer change.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::grid::RouteGrid;

pub const STRAIGHT: u32 = 10;
pub const DIAGONAL: u32 = 14;
pub const UNREACHED: u32 = u32::MAX;

pub const NEIGHBOURS: [(i32, i32, u32); 8] = [
    (1, 0, STRAIGHT),
    (-1, 0, STRAIGHT),
    (0, 1, STRAIGHT),
    (0, -1, STRAIGHT),
    (1, 1, DIAGONAL),
    (1, -1, DIAGONAL),
    (-1, 1, DIAGONAL),
    (-1, -1, DIAGONAL),
];

/// A route as (routing layer, i, j) cells from a source cell to a target cell.
pub type GridPath = Vec<(usize, usize, usize)>;

/// Cost from the nearest source to every cell (`UNREACHED` where none).
pub fn cost_field(grid: &RouteGrid, sources: &[usize], via_cost: u32) -> Vec<u32> {
    let cells = grid.cell_count();
    let mut cost = vec![UNREACHED; grid.blocked.len()];
    let mut heap = BinaryHeap::new();
    for &s in sources {
        if !grid.blocked[s] {
            cost[s] = 0;
            heap.push(Reverse((0u32, s)));
        }
    }
    while let Some(Reverse((c, index))) = heap.pop() {
        if c > cost[index] {
            continue;
        }
        let layer = index / cells;
        let cell = index % cells;
        let (i, j) = ((cell % grid.nx) as i32, (cell / grid.nx) as i32);
        let mut relax = |next: usize, step: u32| {
            let candidate = c + step + grid.penalty_at(next);
            if candidate < cost[next] && !grid.blocked[next] {
                cost[next] = candidate;
                heap.push(Reverse((candidate, next)));
            }
        };
        for (di, dj, step) in NEIGHBOURS {
            let (ni, nj) = (i + di, j + dj);
            if ni < 0 || nj < 0 || ni >= grid.nx as i32 || nj >= grid.ny as i32 {
                continue;
            }
            relax(grid.index(layer, ni as usize, nj as usize), step);
        }
        if grid.via.is_some() && !grid.via_blocked[cell] {
            for other in 0..grid.layers.len() {
                if other != layer {
                    relax(other * cells + cell, via_cost);
                }
            }
        }
    }
    cost
}

/// The cheapest route from any source to any target, by walking the cost field downhill from the
/// cheapest target. Shared with the GPU path, which produces the same field.
pub fn trace_back(
    grid: &RouteGrid,
    cost: &[u32],
    targets: &[usize],
    via_cost: u32,
) -> Option<GridPath> {
    let cells = grid.cell_count();
    let mut at = *targets
        .iter()
        .filter(|&&t| cost[t] != UNREACHED)
        .min_by_key(|&&t| cost[t])?;
    let mut path = vec![at];
    while cost[at] > 0 {
        let layer = at / cells;
        let cell = at % cells;
        let (i, j) = ((cell % grid.nx) as i32, (cell / grid.nx) as i32);
        let mut previous = None;
        for (di, dj, step) in NEIGHBOURS {
            let (ni, nj) = (i + di, j + dj);
            if ni < 0 || nj < 0 || ni >= grid.nx as i32 || nj >= grid.ny as i32 {
                continue;
            }
            let n = grid.index(layer, ni as usize, nj as usize);
            if cost[n] != UNREACHED && cost[n] + step + grid.penalty_at(at) == cost[at] {
                previous = Some(n);
                break;
            }
        }
        if previous.is_none() && !grid.via_blocked[cell] {
            previous = (0..grid.layers.len())
                .filter(|&other| other != layer)
                .map(|other| other * cells + cell)
                .find(|&n| {
                    cost[n] != UNREACHED && cost[n] + via_cost + grid.penalty_at(at) == cost[at]
                });
        }
        at = previous?;
        path.push(at);
    }
    path.reverse();
    Some(
        path.into_iter()
            .map(|index| {
                let cell = index % cells;
                (index / cells, cell % grid.nx, cell / grid.nx)
            })
            .collect(),
    )
}
