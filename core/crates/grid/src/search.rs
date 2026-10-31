//! Shortest paths on a `RouteGrid`, as integer costs so the CPU reference and the GPU kernel
//! agree exactly: 10 per straight step, 14 per diagonal, `via_cost` per layer change.


use crate::grid::RouteGrid;
use crate::work;

pub const STRAIGHT: u32 = 10;
pub const DIAGONAL: u32 = 14;
pub const UNREACHED: u32 = u32::MAX;

/// A search looks at its grid's deadline once in this many expansions. A search is bounded by
/// time, not by a count of expansions: on the fixtures, searches that arrive expand up to four
/// fifths of the grid, so any count that saved time on the searches that fail lost routes.
const CHECK_EVERY: usize = 4096;

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

/// Cost of a layer change equivalent to `mm` millimetres of straight trace on a grid of step `cell`.
pub fn via_cost_mm(cell: board::Nm, mm: f64) -> u32 {
    ((mm * 1e6 / cell.max(1) as f64) * f64::from(STRAIGHT)).round().max(f64::from(STRAIGHT)) as u32
}

/// Step cost on `layer` along `(di, dj)`. When `directional` is set, even routing layers prefer
/// horizontal travel (`10` vs `12`) and odd routing layers prefer vertical travel (`10` vs `12`),
/// with `15` on diagonals so 45-degree turns still beat staircasing, diagonal zig-zags never beat
/// off-axis straights (`30 > 24`), and the octile A* heuristic remains admissible.
#[inline]
fn step_cost(directional: bool, layer: usize, di: i32, dj: i32, base: u32) -> u32 {
    if !directional {
        base
    } else if di != 0 && dj != 0 {
        15
    } else if (layer & 1 == 0) == (dj == 0) {
        10
    } else {
        12
    }
}

/// A min-heap of (priority, node) pairs packed one to a `u64`, the priority in the high half,
/// with four children to a parent. It pops in exactly the order a binary heap of the pairs does,
/// from entries half the size in a tree half as deep: the searches' queue was a third of the
/// router's time.
#[derive(Default)]
pub(crate) struct NodeHeap {
    keys: Vec<u64>,
}

impl NodeHeap {
    pub(crate) const fn new() -> NodeHeap {
        NodeHeap { keys: Vec::new() }
    }

    pub(crate) fn clear(&mut self) {
        self.keys.clear();
    }

    pub(crate) fn push(&mut self, priority: u32, node: usize) {
        debug_assert!(u32::try_from(node).is_ok(), "node {node} does not fit the heap's key");
        let key = (u64::from(priority) << 32) | node as u64;
        let mut i = self.keys.len();
        self.keys.push(key);
        while i > 0 {
            let parent = (i - 1) / 4;
            if self.keys[parent] <= key {
                break;
            }
            self.keys[i] = self.keys[parent];
            i = parent;
        }
        self.keys[i] = key;
    }

    pub(crate) fn pop(&mut self) -> Option<(u32, usize)> {
        let top = *self.keys.first()?;
        let last = self.keys.pop().expect("a first key is a last one");
        let n = self.keys.len();
        if n > 0 {
            let mut i = 0;
            loop {
                let first = 4 * i + 1;
                if first >= n {
                    break;
                }
                let mut least = first;
                for c in first + 1..(first + 4).min(n) {
                    if self.keys[c] < self.keys[least] {
                        least = c;
                    }
                }
                if self.keys[least] >= last {
                    break;
                }
                self.keys[i] = self.keys[least];
                i = least;
            }
            self.keys[i] = last;
        }
        Some(((top >> 32) as u32, (top & 0xffff_ffff) as usize))
    }
}

/// Cost from the nearest source to every cell (`UNREACHED` where none, and everywhere when the
/// grid's deadline passes before the field is done).
pub fn cost_field(grid: &RouteGrid, sources: &[usize], via_cost: u32) -> Vec<u32> {
    let cells = grid.cell_count();
    let mut cost = vec![UNREACHED; grid.blocked.len()];
    let mut heap = NodeHeap::new();
    for &s in sources {
        if !grid.blocked[s] {
            cost[s] = 0;
            heap.push(0, s);
        }
    }
    let mut popped = 0usize;
    while let Some((c, index)) = heap.pop() {
        if c > cost[index] {
            continue;
        }
        popped += 1;
        if popped.is_multiple_of(CHECK_EVERY) && grid.deadline.expired() {
            cost.fill(UNREACHED);
            work::add(&work::FIELDS, 1);
            work::add(&work::FIELD_SETTLED, popped);
            return cost;
        }
        let layer = index / cells;
        let cell = index % cells;
        let (i, j) = ((cell % grid.nx) as i32, (cell / grid.nx) as i32);
        let mut relax = |next: usize, step: u32| {
            let candidate = c.saturating_add(step).saturating_add(grid.penalty_at(next));
            if candidate < cost[next] && !grid.blocked[next] {
                cost[next] = candidate;
                heap.push(candidate, next);
            }
        };
        for (di, dj, step) in NEIGHBOURS {
            let (ni, nj) = (i + di, j + dj);
            if ni < 0 || nj < 0 || ni >= grid.nx as i32 || nj >= grid.ny as i32 {
                continue;
            }
            relax(grid.index(layer, ni as usize, nj as usize), step_cost(grid.directional, layer, di, dj, step));
        }
        if grid.via.is_some() && !grid.via_blocked[cell] {
            for other in 0..grid.layers.len() {
                if other != layer {
                    relax(other * cells + cell, via_cost + grid.via_penalty_at(cell));
                }
            }
        }
    }
    work::add(&work::FIELDS, 1);
    work::add(&work::FIELD_SETTLED, popped);
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
            let actual = step_cost(grid.directional, layer, di, dj, step);
            if cost[n] != UNREACHED && cost[n].saturating_add(actual).saturating_add(grid.penalty_at(at)) == cost[at] {
                previous = Some(n);
                break;
            }
        }
        if previous.is_none() && !grid.via_blocked[cell] {
            previous = (0..grid.layers.len())
                .filter(|&other| other != layer)
                .map(|other| other * cells + cell)
                .find(|&n| {
                    cost[n] != UNREACHED && cost[n].saturating_add(via_cost).saturating_add(grid.via_penalty_at(cell)).saturating_add(grid.penalty_at(at)) == cost[at]
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

/// The most cells [`walled_in`] looks at before it gives up.
const POCKET_LIMIT: usize = 1024;

/// Whether the free cells reachable from those in `queue` (the free target cells, each once)
/// number fewer than `POCKET_LIMIT` and hold no source: a target boxed into a small pocket of free
/// cells with no way in, found without expanding the whole board from the sources. `visit` is
/// asked of each free cell reached: `None` for a source, `Some(true)` for a cell seen for the
/// first time (which it marks seen), `Some(false)` for one seen before. `queue` grows with every
/// cell seen.
pub(crate) fn walled_in(grid: &RouteGrid, queue: &mut Vec<usize>, visit: &mut dyn FnMut(usize) -> Option<bool>) -> bool {
    let cells = grid.cell_count();
    let mut head = 0;
    while head < queue.len() && queue.len() < POCKET_LIMIT {
        let index = queue[head];
        head += 1;
        let layer = index / cells;
        let cell = index % cells;
        let (i, j) = ((cell % grid.nx) as i32, (cell / grid.nx) as i32);
        let beside = NEIGHBOURS.iter().filter_map(|&(di, dj, _)| {
            let (ni, nj) = (i + di, j + dj);
            (ni >= 0 && nj >= 0 && ni < grid.nx as i32 && nj < grid.ny as i32).then(|| grid.index(layer, ni as usize, nj as usize))
        });
        let through = (grid.via.is_some() && !grid.via_blocked[cell]).then(|| (0..grid.layers.len()).filter(move |&l| l != layer).map(move |l| l * cells + cell));
        for next in beside.chain(through.into_iter().flatten()) {
            if grid.blocked[next] {
                continue;
            }
            match visit(next) {
                None => return false,
                Some(true) => queue.push(next),
                Some(false) => {}
            }
        }
    }
    queue.len() < POCKET_LIMIT
}

/// The cheapest route from any source to any target by A*, stopping at the first target
/// reached: the octile distance to the targets' box (never more than the true cost) steers it,
/// so a short connection explores a small patch instead of the whole board. It gives up (`None`)
/// when the grid's deadline passes.
pub fn route_astar(grid: &RouteGrid, sources: &[usize], targets: &[usize], via_cost: u32) -> Option<GridPath> {
    /// Per node: `cost`, valid where `gen` is this search's epoch, and `flags` (target, source,
    /// in the pocket), cleared after each search by `touched`, the nodes given any: 9 bytes a node.
    struct Buffers {
        cost: Vec<u32>,
        gen: Vec<u32>,
        flags: Vec<u8>,
        touched: Vec<usize>,
        pocket_queue: Vec<usize>,
        epoch: u32,
        heap: NodeHeap,
    }
    const TARGET: u8 = 1;
    const SOURCE: u8 = 2;
    const POCKET: u8 = 4;
    thread_local! {
        static BUFS: std::cell::RefCell<Buffers> = const {
            std::cell::RefCell::new(Buffers {
                cost: Vec::new(),
                gen: Vec::new(),
                flags: Vec::new(),
                touched: Vec::new(),
                pocket_queue: Vec::new(),
                epoch: 0,
                heap: NodeHeap::new(),
            })
        };
    }
    work::add(&work::ASTAR, 1);
    BUFS.with(|bufs| {
        let mut b = bufs.borrow_mut();
        let nodes = grid.blocked.len();
        if b.cost.len() < nodes {
            b.cost.resize(nodes, UNREACHED);
            b.gen.resize(nodes, 0);
            b.flags.resize(nodes, 0);
        }
        b.epoch = b.epoch.wrapping_add(1);
        if b.epoch == 0 {
            b.gen.fill(0);
            b.epoch = 1;
        }
        let b = &mut *b;
        for &n in &b.touched {
            b.flags[n] = 0;
        }
        b.touched.clear();
        let mark = |flags: &mut [u8], touched: &mut Vec<usize>, n: usize, bit: u8| {
            if flags[n] == 0 {
                touched.push(n);
            }
            flags[n] |= bit;
        };
        let epoch = b.epoch;
        let cells = grid.cell_count();
        let (mut lo_i, mut lo_j, mut hi_i, mut hi_j) = (usize::MAX, usize::MAX, 0, 0);
        b.pocket_queue.clear();
        for &t in targets {
            if grid.blocked[t] {
                continue;
            }
            if b.flags[t] & TARGET == 0 {
                mark(&mut b.flags, &mut b.touched, t, TARGET | POCKET);
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
                mark(&mut b.flags, &mut b.touched, s, SOURCE);
                if b.flags[s] & TARGET != 0 {
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
        if !overlap && b.pocket_queue.len() < POCKET_LIMIT {
            let Buffers { flags, touched, pocket_queue, .. } = &mut *b;
            let mut visit = |n: usize| {
                if flags[n] & POCKET != 0 {
                    Some(false)
                } else if flags[n] & SOURCE != 0 {
                    None
                } else {
                    mark(flags, touched, n, POCKET);
                    Some(true)
                }
            };
            if walled_in(grid, pocket_queue, &mut visit) {
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
                b.heap.push(estimate(s), s);
            }
        }
        let mut expanded = 0usize;
        let mut reached = None;
        while let Some((f, index)) = b.heap.pop() {
            let c = if b.gen[index] == epoch { b.cost[index] } else { UNREACHED };
            if f > c.saturating_add(estimate(index)) {
                continue;
            }
            if b.flags[index] & TARGET != 0 {
                reached = Some(index);
                break;
            }
            expanded += 1;
            if expanded.is_multiple_of(CHECK_EVERY) && grid.deadline.expired() {
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
                    let candidate = c.saturating_add(step_cost(grid.directional, layer, di, dj, step)).saturating_add(grid.penalty_at(next));
                    let cur = if b.gen[next] == epoch { b.cost[next] } else { UNREACHED };
                    if candidate < cur {
                        b.gen[next] = epoch;
                        b.cost[next] = candidate;
                        b.heap.push(candidate.saturating_add(estimate(next)), next);
                    }
                }
            }
            if grid.via.is_some() && !grid.via_blocked[cell] {
                for other in 0..grid.layers.len() {
                    if other != layer {
                        let next = other * cells + cell;
                        if !grid.blocked[next] {
                            let candidate = c.saturating_add(via_cost).saturating_add(grid.via_penalty_at(cell)).saturating_add(grid.penalty_at(next));
                            let cur = if b.gen[next] == epoch { b.cost[next] } else { UNREACHED };
                            if candidate < cur {
                                b.gen[next] = epoch;
                                b.cost[next] = candidate;
                                b.heap.push(candidate.saturating_add(estimate(next)), next);
                            }
                        }
                    }
                }
            }
        }
        work::add(&work::ASTAR_EXPANDED, expanded);
        let mut at = reached?;
        let cost_at = |b: &Buffers, n: usize| if b.gen[n] == epoch { b.cost[n] } else { UNREACHED };
        let mut path = vec![at];
        while cost_at(b, at) > 0 {
            let c_at = cost_at(b, at);
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
                let cn = cost_at(b, n);
                let actual = step_cost(grid.directional, layer, di, dj, step);
                if cn != UNREACHED && cn.saturating_add(actual).saturating_add(grid.penalty_at(at)) == c_at {
                    previous = Some(n);
                    break;
                }
            }
            if previous.is_none() && !grid.via_blocked[cell] {
                previous = (0..grid.layers.len())
                    .filter(|&other| other != layer)
                    .map(|other| other * cells + cell)
                    .find(|&n| {
                        let cn = cost_at(b, n);
                        cn != UNREACHED && cn.saturating_add(via_cost).saturating_add(grid.via_penalty_at(cell)).saturating_add(grid.penalty_at(at)) == c_at
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_node_heap_pops_as_a_binary_heap_of_pairs_does() {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut draw = |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let (mut ours, mut theirs) = (NodeHeap::new(), BinaryHeap::new());
        for _ in 0..20_000 {
            if draw(3) < 2 {
                // Few priorities and nodes, so ties and repeats are common.
                let (p, n) = (draw(50) as u32, draw(200) as usize);
                ours.push(p, n);
                theirs.push(Reverse((p, n)));
            } else {
                assert_eq!(ours.pop(), theirs.pop().map(|Reverse(k)| k));
            }
        }
        while let Some(Reverse(k)) = theirs.pop() {
            assert_eq!(ours.pop(), Some(k));
        }
        assert_eq!(ours.pop(), None);
    }
    use crate::grid::ViaSpec;

    #[test]
    fn a_via_avoids_a_dear_area_but_may_stand_in_it() {
        // Two layers, 20 by 1 cells: from the left end on layer 0 to the right end on layer 1.
        let (nx, ny) = (20, 1);
        let mut grid = RouteGrid {
            net: 1,
            x0: 0,
            y0: 0,
            h: 50_000,
            nx,
            ny,
            layers: vec![0, 1],
            half_width: 50_000,
            via: Some(ViaSpec { radius: 100_000, drill_radius: 50_000 }),
            blocked: vec![false; 2 * nx * ny],
            via_blocked: vec![false; nx * ny],
            penalty: Vec::new(),
            via_penalty: Vec::new(),
            region_cost: Vec::new(),
            pours_block: false,
            directional: false,
            deadline: Default::default(),
            spokes: None,
        };
        let via_at = |path: &GridPath| path.windows(2).find(|w| w[0].0 != w[1].0).map(|w| w[1].1);
        let (src, tgt) = (vec![0], vec![nx + nx - 1]);
        let free = route_astar(&grid, &src, &tgt, 50).unwrap();
        assert_eq!(via_at(&free), Some(0), "with no area the via goes first, at the source");
        // Vias cost dearly on the left half: the via moves right of it.
        grid.via_penalty = (0..nx).map(|i| if i < 10 { 10_000 } else { 0 }).collect();
        let path = route_astar(&grid, &src, &tgt, 50).unwrap();
        assert!(via_at(&path).is_some_and(|i| i >= 10), "{path:?}");
        assert_eq!(cost_field(&grid, &src, 50)[tgt[0]], path_cost(&grid, &path, 50), "the field and the search agree");
        // Where every via site is dear, one still stands.
        grid.via_penalty = vec![10_000; nx];
        assert!(route_astar(&grid, &src, &tgt, 50).is_some());
    }

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
            via_penalty: (0..nx * ny).map(|_| (next() % 7) as u32).collect(),
            region_cost: Vec::new(),
            pours_block: false,
            directional: false,
            deadline: Default::default(),
            spokes: None,
        }
    }

    fn path_cost(grid: &RouteGrid, path: &GridPath, via_cost: u32) -> u32 {
        path.windows(2)
            .map(|w| {
                let (a, b) = (w[0], w[1]);
                let step = if a.0 != b.0 {
                    via_cost + grid.via_penalty_at(b.2 * grid.nx + b.1)
                } else {
                    let (di, dj) = (b.1 as i32 - a.1 as i32, b.2 as i32 - a.2 as i32);
                    let base = if di != 0 && dj != 0 { DIAGONAL } else { STRAIGHT };
                    step_cost(grid.directional, b.0, di, dj, base)
                };
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

    #[test]
    fn directional_two_layer_grid_separates_axes_and_matches_full_field() {
        let (nx, ny) = (25, 25);
        let grid = RouteGrid {
            net: 1,
            x0: 0,
            y0: 0,
            h: 50_000,
            nx,
            ny,
            layers: vec![0, 1],
            half_width: 50_000,
            via: Some(ViaSpec { radius: 100_000, drill_radius: 50_000 }),
            blocked: vec![false; 2 * nx * ny],
            via_blocked: vec![false; nx * ny],
            penalty: Vec::new(),
            via_penalty: Vec::new(),
            region_cost: Vec::new(),
            pours_block: false,
            directional: true,
            deadline: Default::default(),
            spokes: None,
        };
        let pth = |i: usize, j: usize| vec![grid.index(0, i, j), grid.index(1, i, j)];
        let horiz = route_astar(&grid, &pth(2, 12), &pth(22, 12), 500).unwrap();
        assert!(horiz.iter().all(|&(l, _, _)| l == 0), "horizontal PTH run stays on layer 0: {horiz:?}");
        let vert = route_astar(&grid, &pth(12, 2), &pth(12, 22), 500).unwrap();
        assert!(vert.iter().all(|&(l, _, _)| l == 1), "vertical PTH run stays on layer 1: {vert:?}");

        for seed in 1..60u64 {
            let mut g = random_grid(seed * 6151);
            g.directional = true;
            let sources = vec![g.index(0, 1, 1), g.index(1, 2, 1)];
            let targets = vec![g.index(0, 37, 27), g.index(1, 30, 5)];
            let full = trace_back(&g, &cost_field(&g, &sources, 50), &targets, 50);
            let quick = route_astar(&g, &sources, &targets, 50);
            assert_eq!(full.is_some(), quick.is_some(), "directional seed {seed}");
            if let (Some(f), Some(q)) = (full, quick) {
                assert_eq!(path_cost(&g, &f, 50), path_cost(&g, &q, 50), "directional seed {seed}");
            }
        }
    }
}
