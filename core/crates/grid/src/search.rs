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

/// The cheapest route from any source to any target by A*, stopping at the first target
/// reached: the octile distance to the targets' box (never more than the true cost) steers it,
/// so a short connection explores a small patch instead of the whole board.
pub fn route_astar(grid: &RouteGrid, sources: &[usize], targets: &[usize], via_cost: u32) -> Option<GridPath> {
    let cells = grid.cell_count();
    let mut is_target = vec![false; grid.blocked.len()];
    let (mut lo_i, mut lo_j, mut hi_i, mut hi_j) = (usize::MAX, usize::MAX, 0, 0);
    for &t in targets {
        if grid.blocked[t] {
            continue;
        }
        is_target[t] = true;
        let c = t % cells;
        let (i, j) = (c % grid.nx, c / grid.nx);
        lo_i = lo_i.min(i);
        lo_j = lo_j.min(j);
        hi_i = hi_i.max(i);
        hi_j = hi_j.max(j);
    }
    if lo_i == usize::MAX {
        return None;
    }
    let estimate = |index: usize| -> u32 {
        let c = index % cells;
        let (i, j) = (c % grid.nx, c / grid.nx);
        let dx = lo_i.saturating_sub(i).max(i.saturating_sub(hi_i)) as u32;
        let dy = lo_j.saturating_sub(j).max(j.saturating_sub(hi_j)) as u32;
        STRAIGHT * dx.max(dy) + (DIAGONAL - STRAIGHT) * dx.min(dy)
    };
    let mut cost = vec![UNREACHED; grid.blocked.len()];
    let mut heap = BinaryHeap::new();
    for &s in sources {
        if !grid.blocked[s] {
            cost[s] = 0;
            heap.push(Reverse((estimate(s), s)));
        }
    }
    let mut reached = None;
    while let Some(Reverse((f, index))) = heap.pop() {
        let c = cost[index];
        if f > c.saturating_add(estimate(index)) {
            continue;
        }
        if is_target[index] {
            reached = Some(index);
            break;
        }
        let layer = index / cells;
        let cell = index % cells;
        let (i, j) = ((cell % grid.nx) as i32, (cell / grid.nx) as i32);
        let mut relax = |next: usize, step: u32| {
            let candidate = c + step + grid.penalty_at(next);
            if candidate < cost[next] && !grid.blocked[next] {
                cost[next] = candidate;
                heap.push(Reverse((candidate + estimate(next), next)));
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
    trace_back(grid, &cost, &[reached?], via_cost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::ViaSpec;

    fn random_grid(seed: u64) -> RouteGrid {
        let (nx, ny, layers) = (40, 30, 2);
        let mut state = seed;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let nodes = nx * ny * layers;
        RouteGrid {
            net: 1,
            x0: 0,
            y0: 0,
            h: 100,
            nx,
            ny,
            layers: (0..layers).collect(),
            half_width: 10,
            via: Some(ViaSpec { radius: 20, drill_radius: 10 }),
            blocked: (0..nodes).map(|_| next() % 4 == 0).collect(),
            via_blocked: (0..nx * ny).map(|_| next() % 3 == 0).collect(),
            penalty: (0..nodes).map(|_| (next() % 5) as u32).collect(),
        }
    }

    fn path_cost(grid: &RouteGrid, path: &GridPath, via_cost: u32) -> u32 {
        path.windows(2)
            .map(|w| {
                let (a, b) = (w[0], w[1]);
                let step = if a.0 != b.0 { via_cost } else if a.1 != b.1 && a.2 != b.2 { DIAGONAL } else { STRAIGHT };
                step + grid.penalty_at(grid.index(b.0, b.1, b.2))
            })
            .sum()
    }

    #[test]
    fn a_star_finds_routes_as_cheap_as_the_full_field() {
        for seed in 1..60u64 {
            let grid = random_grid(seed * 7919);
            let sources = vec![grid.index(0, 1, 1), grid.index(1, 2, 1)];
            let targets = vec![grid.index(0, 37, 27), grid.index(1, 30, 5)];
            let full = trace_back(&grid, &cost_field(&grid, &sources, 50), &targets, 50);
            let quick = route_astar(&grid, &sources, &targets, 50);
            assert_eq!(full.is_some(), quick.is_some(), "seed {seed}");
            if let (Some(f), Some(q)) = (full, quick) {
                assert_eq!(path_cost(&grid, &f, 50), path_cost(&grid, &q, 50), "seed {seed}");
            }
        }
    }
}
