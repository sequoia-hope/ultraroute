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
    cost_field_via(grid, sources, via_cost, &[])
}

/// As [`cost_field`], adding `via_penalty[cell]` whenever a route changes layer at 2D `cell`.
pub fn cost_field_via(grid: &RouteGrid, sources: &[usize], via_cost: u32, via_penalty: &[u32]) -> Vec<u32> {
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
            let v_pen = via_penalty.get(cell).copied().unwrap_or(0);
            for other in 0..grid.layers.len() {
                if other != layer {
                    relax(other * cells + cell, via_cost + v_pen);
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
    trace_back_via(grid, cost, targets, via_cost, &[])
}

/// As [`trace_back`], matching [`cost_field_via`] layer-change prices.
pub fn trace_back_via(
    grid: &RouteGrid,
    cost: &[u32],
    targets: &[usize],
    via_cost: u32,
    via_penalty: &[u32],
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
            let v_pen = via_penalty.get(cell).copied().unwrap_or(0);
            previous = (0..grid.layers.len())
                .filter(|&other| other != layer)
                .map(|other| other * cells + cell)
                .find(|&n| {
                    cost[n] != UNREACHED && cost[n] + via_cost + v_pen + grid.penalty_at(at) == cost[at]
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
    route_astar_via(grid, sources, targets, via_cost, &[])
}

/// As [`route_astar`], charging `via_penalty[cell]` when changing layer at 2D `cell` so vias see
/// congestion and rip-up costs beyond a trace's narrower footprint.
pub fn route_astar_via(
    grid: &RouteGrid,
    sources: &[usize],
    targets: &[usize],
    via_cost: u32,
    via_penalty: &[u32],
) -> Option<GridPath> {
    struct Buffers {
        cost: Vec<u32>,
        gen: Vec<u32>,
        is_target: Vec<u32>,
        is_source: Vec<u32>,
        pocket_gen: Vec<u32>,
        pocket_queue: Vec<usize>,
        epoch: u32,
        heap: BinaryHeap<Reverse<(u32, usize)>>,
    }
    thread_local! {
        static BUFS: std::cell::RefCell<Buffers> = const {
            std::cell::RefCell::new(Buffers {
                cost: Vec::new(),
                gen: Vec::new(),
                is_target: Vec::new(),
                is_source: Vec::new(),
                pocket_gen: Vec::new(),
                pocket_queue: Vec::new(),
                epoch: 0,
                heap: BinaryHeap::new(),
            })
        };
    }
    BUFS.with(|bufs| {
        let mut b = bufs.borrow_mut();
        let nodes = grid.blocked.len();
        if b.cost.len() < nodes {
            b.cost.resize(nodes, UNREACHED);
            b.gen.resize(nodes, 0);
            b.is_target.resize(nodes, 0);
            b.is_source.resize(nodes, 0);
            b.pocket_gen.resize(nodes, 0);
        }
        b.epoch = b.epoch.wrapping_add(1);
        if b.epoch == 0 {
            b.gen.fill(0);
            b.is_target.fill(0);
            b.is_source.fill(0);
            b.pocket_gen.fill(0);
            b.epoch = 1;
        }
        let epoch = b.epoch;
        let cells = grid.cell_count();
        let (mut lo_i, mut lo_j, mut hi_i, mut hi_j) = (usize::MAX, usize::MAX, 0, 0);
        b.pocket_queue.clear();
        for &t in targets {
            if grid.blocked[t] {
                continue;
            }
            if b.is_target[t] != epoch {
                b.is_target[t] = epoch;
                b.pocket_gen[t] = epoch;
                b.pocket_queue.push(t);
            }
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
        let mut has_source = false;
        let mut overlap = false;
        for &s in sources {
            if !grid.blocked[s] {
                has_source = true;
                b.is_source[s] = epoch;
                if b.is_target[s] == epoch {
                    overlap = true;
                }
            }
        }
        if !has_source {
            return None;
        }
        // When the target end is boxed in inside a small pocket of free cells while the source end
        // is open to the board, a bounded reachability check from the targets proves there is no
        // way in without expanding the whole board from the sources.
        const POCKET_LIMIT: usize = 1024;
        if !overlap && b.pocket_queue.len() < POCKET_LIMIT {
            let mut head = 0;
            let mut found_source = false;
            while head < b.pocket_queue.len() && b.pocket_queue.len() < POCKET_LIMIT {
                let index = b.pocket_queue[head];
                head += 1;
                let layer = index / cells;
                let cell = index % cells;
                let (i, j) = ((cell % grid.nx) as i32, (cell / grid.nx) as i32);
                for (di, dj, _) in NEIGHBOURS {
                    let (ni, nj) = (i + di, j + dj);
                    if ni < 0 || nj < 0 || ni >= grid.nx as i32 || nj >= grid.ny as i32 {
                        continue;
                    }
                    let next = grid.index(layer, ni as usize, nj as usize);
                    if !grid.blocked[next] && b.pocket_gen[next] != epoch {
                        if b.is_source[next] == epoch {
                            found_source = true;
                            break;
                        }
                        b.pocket_gen[next] = epoch;
                        b.pocket_queue.push(next);
                    }
                }
                if found_source {
                    break;
                }
                if grid.via.is_some() && !grid.via_blocked[cell] {
                    for other in 0..grid.layers.len() {
                        if other != layer {
                            let next = other * cells + cell;
                            if !grid.blocked[next] && b.pocket_gen[next] != epoch {
                                if b.is_source[next] == epoch {
                                    found_source = true;
                                    break;
                                }
                                b.pocket_gen[next] = epoch;
                                b.pocket_queue.push(next);
                            }
                        }
                    }
                    if found_source {
                        break;
                    }
                }
            }
            if !found_source && b.pocket_queue.len() < POCKET_LIMIT {
                return None;
            }
        }
        let estimate = |index: usize| -> u32 {
            let c = index % cells;
            let (i, j) = (c % grid.nx, c / grid.nx);
            let dx = lo_i.saturating_sub(i).max(i.saturating_sub(hi_i)) as u32;
            let dy = lo_j.saturating_sub(j).max(j.saturating_sub(hi_j)) as u32;
            STRAIGHT * dx.max(dy) + (DIAGONAL - STRAIGHT) * dx.min(dy)
        };
        b.heap.clear();
        for &s in sources {
            if !grid.blocked[s] {
                b.gen[s] = epoch;
                b.cost[s] = 0;
                b.heap.push(Reverse((estimate(s), s)));
            }
        }
        let mut reached = None;
        while let Some(Reverse((f, index))) = b.heap.pop() {
            let c = if b.gen[index] == epoch { b.cost[index] } else { UNREACHED };
            if f > c.saturating_add(estimate(index)) {
                continue;
            }
            if b.is_target[index] == epoch {
                reached = Some(index);
                break;
            }
            let layer = index / cells;
            let cell = index % cells;
            let (i, j) = ((cell % grid.nx) as i32, (cell / grid.nx) as i32);
            for (di, dj, step) in NEIGHBOURS {
                let (ni, nj) = (i + di, j + dj);
                if ni < 0 || nj < 0 || ni >= grid.nx as i32 || nj >= grid.ny as i32 {
                    continue;
                }
                let next = grid.index(layer, ni as usize, nj as usize);
                if !grid.blocked[next] {
                    let candidate = c + step + grid.penalty_at(next);
                    let cur = if b.gen[next] == epoch { b.cost[next] } else { UNREACHED };
                    if candidate < cur {
                        b.gen[next] = epoch;
                        b.cost[next] = candidate;
                        b.heap.push(Reverse((candidate + estimate(next), next)));
                    }
                }
            }
            if grid.via.is_some() && !grid.via_blocked[cell] {
                let v_pen = via_penalty.get(cell).copied().unwrap_or(0);
                for other in 0..grid.layers.len() {
                    if other != layer {
                        let next = other * cells + cell;
                        if !grid.blocked[next] {
                            let candidate = c + via_cost + v_pen + grid.penalty_at(next);
                            let cur = if b.gen[next] == epoch { b.cost[next] } else { UNREACHED };
                            if candidate < cur {
                                b.gen[next] = epoch;
                                b.cost[next] = candidate;
                                b.heap.push(Reverse((candidate + estimate(next), next)));
                            }
                        }
                    }
                }
            }
        }
        let mut at = reached?;
        let cost_at = |b: &Buffers, n: usize| if b.gen[n] == epoch { b.cost[n] } else { UNREACHED };
        let mut path = vec![at];
        while cost_at(&b, at) > 0 {
            let c_at = cost_at(&b, at);
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
                let cn = cost_at(&b, n);
                if cn != UNREACHED && cn + step + grid.penalty_at(at) == c_at {
                    previous = Some(n);
                    break;
                }
            }
            if previous.is_none() && !grid.via_blocked[cell] {
                let v_pen = via_penalty.get(cell).copied().unwrap_or(0);
                previous = (0..grid.layers.len())
                    .filter(|&other| other != layer)
                    .map(|other| other * cells + cell)
                    .find(|&n| {
                        let cn = cost_at(&b, n);
                        cn != UNREACHED && cn + via_cost + v_pen + grid.penalty_at(at) == c_at
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
    })
}

/// When `sources` and `targets` are disconnected on `grid`, returns the board bounding box of the
/// smaller reachable component (grown by one grid cell). Returns `None` if a path exists between
/// `sources` and `targets`.
pub fn pocket_bbox(grid: &RouteGrid, sources: &[usize], targets: &[usize]) -> Option<board::BBox> {
    let nodes = grid.blocked.len();
    let cells = grid.cell_count();
    let mut mark = vec![0u8; nodes];
    let mut qs = Vec::new();
    let mut qt = Vec::new();
    for &s in sources {
        if !grid.blocked[s] && mark[s] == 0 {
            mark[s] = 1;
            qs.push(s);
        }
    }
    for &t in targets {
        if !grid.blocked[t] {
            if mark[t] == 1 {
                return None;
            }
            if mark[t] == 0 {
                mark[t] = 2;
                qt.push(t);
            }
        }
    }
    let (mut hs, mut ht) = (0usize, 0usize);
    let exhausted = loop {
        if hs >= qs.len() {
            break &qs;
        }
        if ht >= qt.len() {
            break &qt;
        }
        let (q, h, me, other) = if qs.len() <= qt.len() {
            (&mut qs, &mut hs, 1u8, 2u8)
        } else {
            (&mut qt, &mut ht, 2u8, 1u8)
        };
        let index = q[*h];
        *h += 1;
        let layer = index / cells;
        let cell = index % cells;
        let (i, j) = ((cell % grid.nx) as i32, (cell / grid.nx) as i32);
        for (di, dj, _) in NEIGHBOURS {
            let (ni, nj) = (i + di, j + dj);
            if ni < 0 || nj < 0 || ni >= grid.nx as i32 || nj >= grid.ny as i32 {
                continue;
            }
            let next = grid.index(layer, ni as usize, nj as usize);
            if !grid.blocked[next] {
                if mark[next] == other {
                    return None;
                }
                if mark[next] == 0 {
                    mark[next] = me;
                    q.push(next);
                }
            }
        }
        if grid.via.is_some() && !grid.via_blocked[cell] {
            for other_layer in 0..grid.layers.len() {
                if other_layer != layer {
                    let next = other_layer * cells + cell;
                    if !grid.blocked[next] {
                        if mark[next] == other {
                            return None;
                        }
                        if mark[next] == 0 {
                            mark[next] = me;
                            q.push(next);
                        }
                    }
                }
            }
        }
    };
    if exhausted.is_empty() {
        return None;
    }
    let (mut lo_i, mut lo_j, mut hi_i, mut hi_j) = (usize::MAX, usize::MAX, 0usize, 0usize);
    for &idx in exhausted {
        let c = idx % cells;
        let (i, j) = (c % grid.nx, c / grid.nx);
        lo_i = lo_i.min(i);
        lo_j = lo_j.min(j);
        hi_i = hi_i.max(i);
        hi_j = hi_j.max(j);
    }
    Some(board::BBox::of([grid.centre(lo_i, lo_j), grid.centre(hi_i, hi_j)]).grow(grid.h))
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
            pours_block: false,
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

    fn path_cost_via(grid: &RouteGrid, path: &GridPath, via_cost: u32, via_pen: &[u32]) -> u32 {
        path.windows(2)
            .map(|w| {
                let (a, b) = (w[0], w[1]);
                let step = if a.0 != b.0 {
                    via_cost + via_pen.get(b.2 * grid.nx + b.1).copied().unwrap_or(0)
                } else if a.1 != b.1 && a.2 != b.2 {
                    DIAGONAL
                } else {
                    STRAIGHT
                };
                step + grid.penalty_at(grid.index(b.0, b.1, b.2))
            })
            .sum()
    }

    #[test]
    fn a_star_via_respects_via_penalties_and_matches_the_full_field() {
        for seed in 1..40u64 {
            let grid = random_grid(seed * 104_729);
            let mut state = seed * 31_337 + 1;
            let via_pen: Vec<u32> = (0..grid.cell_count())
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    (state % 120) as u32
                })
                .collect();
            let sources = vec![grid.index(0, 1, 1), grid.index(1, 2, 1)];
            let targets = vec![grid.index(0, 37, 27), grid.index(1, 30, 5)];
            let full = trace_back_via(&grid, &cost_field_via(&grid, &sources, 50, &via_pen), &targets, 50, &via_pen);
            let quick = route_astar_via(&grid, &sources, &targets, 50, &via_pen);
            assert_eq!(full.is_some(), quick.is_some(), "seed {seed}");
            if let (Some(f), Some(q)) = (full, quick) {
                assert_eq!(path_cost_via(&grid, &f, 50, &via_pen), path_cost_via(&grid, &q, 50, &via_pen), "seed {seed}");
            }
        }
    }
}
