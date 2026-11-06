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

const STEPS_PLAIN: [u32; 8] = [STRAIGHT, STRAIGHT, STRAIGHT, STRAIGHT, DIAGONAL, DIAGONAL, DIAGONAL, DIAGONAL];
const STEPS_EVEN: [u32; 8] = [10, 10, 12, 12, 15, 15, 15, 15];
const STEPS_ODD: [u32; 8] = [12, 12, 10, 10, 15, 15, 15, 15];

/// A 4-ary min-heap of packed `u64` keys `(priority << 32) | node`, used as bucket 0 of
/// [`NodeHeap`] for keys at or below the current radix floor. Because nearly every push lands in a
/// higher radix bucket as a plain `Vec::push`, this heap stays tiny (one `f` contour, in L1 cache)
/// while breaking ties by ascending `node` identically to a binary heap of pairs.
#[derive(Default)]
struct SmallHeap {
    keys: Vec<u64>,
}

impl SmallHeap {
    const fn new() -> SmallHeap {
        SmallHeap { keys: Vec::new() }
    }

    fn clear(&mut self) {
        self.keys.clear();
    }

    #[inline]
    fn push_key(&mut self, key: u64) {
        let mut i = self.keys.len();
        self.keys.push(key);
        let ptr = self.keys.as_mut_ptr();
        unsafe {
            while i > 0 {
                let parent = (i - 1) >> 2;
                let pk = *ptr.add(parent);
                if pk <= key {
                    break;
                }
                *ptr.add(i) = pk;
                i = parent;
            }
            *ptr.add(i) = key;
        }
    }

    #[inline]
    fn pop_key(&mut self) -> Option<u64> {
        let n = self.keys.len();
        if n == 0 {
            return None;
        }
        unsafe {
            let top = *self.keys.get_unchecked(0);
            let last = self.keys.pop().unwrap_unchecked();
            let rem = n - 1;
            if rem > 0 {
                let ptr = self.keys.as_mut_ptr();
                let mut i = 0;
                while 4 * i + 4 < rem {
                    let c = 4 * i + 1;
                    let k0 = *ptr.add(c);
                    let k1 = *ptr.add(c + 1);
                    let k2 = *ptr.add(c + 2);
                    let k3 = *ptr.add(c + 3);
                    let (m01, i01) = if k1 < k0 { (k1, c + 1) } else { (k0, c) };
                    let (m23, i23) = if k3 < k2 { (k3, c + 3) } else { (k2, c + 2) };
                    let (least_k, least) = if m23 < m01 { (m23, i23) } else { (m01, i01) };
                    if least_k >= last {
                        *ptr.add(i) = last;
                        return Some(top);
                    }
                    *ptr.add(i) = least_k;
                    i = least;
                }
                let first = 4 * i + 1;
                if first < rem {
                    let mut least = first;
                    let mut least_k = *ptr.add(first);
                    for c in first + 1..rem {
                        let kc = *ptr.add(c);
                        if kc < least_k {
                            least_k = kc;
                            least = c;
                        }
                    }
                    if least_k < last {
                        *ptr.add(i) = least_k;
                        i = least;
                    }
                }
                *ptr.add(i) = last;
            }
            Some(top)
        }
    }
}

/// A 33-bucket radix min-queue of `(priority, node)` pairs packed one to a `u64`, with [`SmallHeap`]
/// holding keys at or below `current` and 32 XOR-MSB buckets holding keys above `current`. Pushes
/// above `current` append straight to a `Vec`, avoiding 8-level heap sifts across hundreds of
/// thousands of open nodes on large boards, while popping in the exact `(priority, node)` order of
/// a binary min-heap even if non-monotone priorities arrive.
pub(crate) struct NodeHeap {
    current: u32,
    non_empty: u32,
    top: SmallHeap,
    buckets: [Vec<u64>; 32],
}

impl Default for NodeHeap {
    fn default() -> Self {
        NodeHeap::new()
    }
}

impl NodeHeap {
    pub(crate) const fn new() -> NodeHeap {
        NodeHeap {
            current: 0,
            non_empty: 0,
            top: SmallHeap::new(),
            buckets: [const { Vec::new() }; 32],
        }
    }

    pub(crate) fn clear(&mut self) {
        self.top.clear();
        let mut mask = self.non_empty;
        while mask != 0 {
            let b = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            self.buckets[b].clear();
        }
        self.non_empty = 0;
        self.current = 0;
    }

    #[inline(always)]
    pub(crate) fn push(&mut self, priority: u32, node: usize) {
        debug_assert!(u32::try_from(node).is_ok(), "node {node} does not fit the heap's key");
        let key = (u64::from(priority) << 32) | node as u64;
        if priority <= self.current {
            self.top.push_key(key);
        } else {
            let b = (31 - (priority ^ self.current).leading_zeros()) as usize;
            unsafe { self.buckets.get_unchecked_mut(b) }.push(key);
            self.non_empty |= 1u32 << b;
        }
    }

    #[inline(always)]
    pub(crate) fn pop(&mut self) -> Option<(u32, usize)> {
        if let Some(top) = self.top.pop_key() {
            return Some(((top >> 32) as u32, (top & 0xffff_ffff) as usize));
        }
        if self.non_empty == 0 {
            return None;
        }
        self.pull_bucket()
    }

    #[cold]
    #[inline(never)]
    fn pull_bucket(&mut self) -> Option<(u32, usize)> {
        let b = self.non_empty.trailing_zeros() as usize;
        self.non_empty &= !(1u32 << b);
        let mut buf = std::mem::take(unsafe { self.buckets.get_unchecked_mut(b) });
        if buf.len() == 1 {
            let key = buf[0];
            buf.clear();
            unsafe { *self.buckets.get_unchecked_mut(b) = buf };
            let cur = (key >> 32) as u32;
            self.current = cur;
            return Some((cur, (key & 0xffff_ffff) as usize));
        }
        let mut min_idx = 0;
        let mut min_key = buf[0];
        for (idx, &k) in buf.iter().enumerate().skip(1) {
            if k < min_key {
                min_key = k;
                min_idx = idx;
            }
        }
        let cur = (min_key >> 32) as u32;
        self.current = cur;
        for (idx, &k) in buf.iter().enumerate() {
            if idx == min_idx {
                continue;
            }
            let p = (k >> 32) as u32;
            if p == cur {
                self.top.push_key(k);
            } else {
                let nb = (31 - (p ^ cur).leading_zeros()) as usize;
                unsafe { self.buckets.get_unchecked_mut(nb) }.push(k);
                self.non_empty |= 1u32 << nb;
            }
        }
        buf.clear();
        unsafe { *self.buckets.get_unchecked_mut(b) = buf };
        Some((cur, (min_key & 0xffff_ffff) as usize))
    }
}

/// Cost from the nearest source to every cell (`UNREACHED` where none, and everywhere when the
/// grid's deadline passes before the field is done).
pub fn cost_field(grid: &RouteGrid, sources: &[usize], via_cost: u32) -> Vec<u32> {
    board::work::in_bucket(board::work::Bucket::Astar, || cost_field_inner(grid, sources, via_cost))
}

fn cost_field_inner(grid: &RouteGrid, sources: &[usize], via_cost: u32) -> Vec<u32> {
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
    board::work::in_bucket(board::work::Bucket::Astar, || trace_back_inner(grid, cost, targets, via_cost))
}

fn trace_back_inner(
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

#[repr(C)]
#[derive(Clone, Copy)]
struct NodeState {
    gen: u32,
    cost: u32,
}

/// Exact unsigned 32-bit division and remainder by a runtime divisor `d >= 1`, replacing hardware
/// integer divisions per popped A* node with single-multiply reciprocals.
#[derive(Clone, Copy)]
struct FastDiv32 {
    d: u32,
    m: u64,
}

impl FastDiv32 {
    #[inline]
    fn new(d: u32) -> Self {
        debug_assert!(d > 0);
        Self { d, m: u64::MAX / u64::from(d) }
    }

    #[inline(always)]
    fn div_rem(self, n: u32) -> (u32, u32) {
        let q = ((u128::from(u64::from(n) + 1) * u128::from(self.m)) >> 64) as u32;
        (q, n - q * self.d)
    }
}

struct AstarCtx<'a> {
    grid: &'a RouteGrid,
    epoch: u32,
    via_cost: u32,
    lo_i: u32,
    hi_i: u32,
    lo_j: u32,
    hi_j: u32,
    div_cells: FastDiv32,
    div_nx: FastDiv32,
}

impl AstarCtx<'_> {
    #[inline(always)]
    fn est_ij(&self, i: u32, j: u32) -> u32 {
        let dx = self.lo_i.saturating_sub(i).max(i.saturating_sub(self.hi_i));
        let dy = self.lo_j.saturating_sub(j).max(j.saturating_sub(self.hi_j));
        STRAIGHT * dx.max(dy) + (DIAGONAL - STRAIGHT) * dx.min(dy)
    }
}

#[inline(never)]
fn run_astar_loop<const PEN: u8>(
    ctx: &AstarCtx<'_>,
    state: &mut [NodeState],
    flags: &[u8],
    heap: &mut NodeHeap,
) -> (Option<usize>, usize) {
    const TARGET: u8 = 1;
    let grid = ctx.grid;
    let nx = grid.nx;
    let ny = grid.ny;
    let cells = nx * ny;
    let n_layers = grid.layers.len();
    let nx32 = nx as u32;
    let ny32 = ny as u32;
    let epoch = ctx.epoch;
    let via_cost = ctx.via_cost;
    let net = grid.net;
    let directional = grid.directional;
    let has_vias = grid.via.is_some();
    let has_via_pen = grid.via_penalty.len() >= cells;
    let div_cells = ctx.div_cells;
    let div_nx = ctx.div_nx;

    let blocked_ptr = grid.blocked.as_ptr();
    let via_blocked_ptr = grid.via_blocked.as_ptr();
    let penalty_ptr = grid.penalty.as_ptr();
    let via_pen_ptr = grid.via_penalty.as_ptr();
    let state_ptr = state.as_mut_ptr();
    let flags_ptr = flags.as_ptr();
    let (spokes_ptr, spoke_track_ptr, spoke_via_ptr) = match grid.spokes.as_deref() {
        Some(sp) => (sp as *const crate::spokes::SpokePrices, sp.track.as_ptr(), sp.via.as_ptr()),
        None => (std::ptr::null(), std::ptr::null(), std::ptr::null()),
    };

    let node_pen = |next: usize| -> u32 {
        unsafe {
            match PEN {
                0 => 0,
                1 => {
                    if *spoke_track_ptr.add(next) {
                        (*spokes_ptr).track_hit(next, net)
                    } else {
                        0
                    }
                }
                2 => *penalty_ptr.add(next),
                3 => {
                    let base = *penalty_ptr.add(next);
                    if *spoke_track_ptr.add(next) {
                        base + (*spokes_ptr).track_hit(next, net)
                    } else {
                        base
                    }
                }
                _ => grid.penalty_at(next),
            }
        }
    };

    let mut expanded = 0usize;
    let mut reached = None;
    while let Some((f, index)) = heap.pop() {
        let st = unsafe { *state_ptr.add(index) };
        let c = if st.gen == epoch { st.cost } else { UNREACHED };
        let (layer32, cell32) = div_cells.div_rem(index as u32);
        let (j32, i32_u) = div_nx.div_rem(cell32);
        let h_curr = ctx.est_ij(i32_u, j32);
        if f > c.saturating_add(h_curr) {
            continue;
        }
        if unsafe { *flags_ptr.add(index) } & TARGET != 0 {
            reached = Some(index);
            break;
        }
        expanded += 1;
        if expanded.is_multiple_of(CHECK_EVERY) && grid.deadline.expired() {
            break;
        }
        let layer = layer32 as usize;
        let cell = cell32 as usize;
        let steps = if !directional {
            &STEPS_PLAIN
        } else if layer & 1 == 0 {
            &STEPS_EVEN
        } else {
            &STEPS_ODD
        };
        if i32_u > 0 && i32_u + 1 < nx32 && j32 > 0 && j32 + 1 < ny32 {
            let mut relax_nb = |next: usize, ni: u32, nj: u32, step: u32| unsafe {
                if !*blocked_ptr.add(next) {
                    let candidate = c.saturating_add(step).saturating_add(node_pen(next));
                    let st_n = *state_ptr.add(next);
                    let cur = if st_n.gen == epoch { st_n.cost } else { UNREACHED };
                    if candidate < cur {
                        *state_ptr.add(next) = NodeState { gen: epoch, cost: candidate };
                        heap.push(candidate.saturating_add(ctx.est_ij(ni, nj)), next);
                    }
                }
            };
            relax_nb(index + 1, i32_u + 1, j32, steps[0]);
            relax_nb(index - 1, i32_u - 1, j32, steps[1]);
            relax_nb(index + nx, i32_u, j32 + 1, steps[2]);
            relax_nb(index - nx, i32_u, j32 - 1, steps[3]);
            relax_nb(index + nx + 1, i32_u + 1, j32 + 1, steps[4]);
            relax_nb(index - nx + 1, i32_u + 1, j32 - 1, steps[5]);
            relax_nb(index + nx - 1, i32_u - 1, j32 + 1, steps[6]);
            relax_nb(index - nx - 1, i32_u - 1, j32 - 1, steps[7]);
        } else {
            let (si, sj) = (i32_u as i32, j32 as i32);
            for (k, &(di, dj, _)) in NEIGHBOURS.iter().enumerate() {
                let (ni, nj) = (si + di, sj + dj);
                if ni < 0 || nj < 0 || ni >= nx32 as i32 || nj >= ny32 as i32 {
                    continue;
                }
                let next = layer * cells + (nj as usize) * nx + (ni as usize);
                unsafe {
                    if !*blocked_ptr.add(next) {
                        let candidate = c.saturating_add(steps[k]).saturating_add(node_pen(next));
                        let st_n = *state_ptr.add(next);
                        let cur = if st_n.gen == epoch { st_n.cost } else { UNREACHED };
                        if candidate < cur {
                            *state_ptr.add(next) = NodeState { gen: epoch, cost: candidate };
                            heap.push(candidate.saturating_add(ctx.est_ij(ni as u32, nj as u32)), next);
                        }
                    }
                }
            }
        }
        if has_vias && unsafe { !*via_blocked_ptr.add(cell) } {
            let via_base = if PEN < 4 {
                let vp = if has_via_pen { unsafe { *via_pen_ptr.add(cell) } } else { 0 };
                let sv = if (PEN == 1 || PEN == 3) && unsafe { *spoke_via_ptr.add(cell) } {
                    unsafe { (*spokes_ptr).via_hit(cell, net) }
                } else {
                    0
                };
                c.saturating_add(via_cost).saturating_add(vp + sv)
            } else {
                c.saturating_add(via_cost).saturating_add(grid.via_penalty_at(cell))
            };
            let mut base_layer = 0usize;
            for other in 0..n_layers {
                if other != layer {
                    let next = base_layer + cell;
                    unsafe {
                        if !*blocked_ptr.add(next) {
                            let candidate = via_base.saturating_add(node_pen(next));
                            let st_n = *state_ptr.add(next);
                            let cur = if st_n.gen == epoch { st_n.cost } else { UNREACHED };
                            if candidate < cur {
                                *state_ptr.add(next) = NodeState { gen: epoch, cost: candidate };
                                heap.push(candidate.saturating_add(h_curr), next);
                            }
                        }
                    }
                }
                base_layer += cells;
            }
        }
    }
    (reached, expanded)
}

/// The cheapest route from any source to any target by A*, stopping at the first target
/// reached: the octile distance to the targets' box (never more than the true cost) steers it,
/// so a short connection explores a small patch instead of the whole board. It gives up (`None`)
/// when the grid's deadline passes.
pub fn route_astar(grid: &RouteGrid, sources: &[usize], targets: &[usize], via_cost: u32) -> Option<GridPath> {
    /// Per node: `(gen, cost)` fused in one 8-byte slot (`state`) so each relaxation touches one
    /// cache line, plus `flags` (target, source, in the pocket) cleared via `touched`: 9 bytes a node.
    struct Buffers {
        state: Vec<NodeState>,
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
                state: Vec::new(),
                flags: Vec::new(),
                touched: Vec::new(),
                pocket_queue: Vec::new(),
                epoch: 0,
                heap: NodeHeap::new(),
            })
        };
    }
    work::add(&work::ASTAR, 1);
    board::work::in_bucket(board::work::Bucket::Astar, || {
        BUFS.with(|bufs| {
        let mut b = bufs.borrow_mut();
        let nodes = grid.blocked.len();
        let cells = grid.cell_count();
        if cells == 0 || nodes == 0 {
            return None;
        }
        if b.state.len() < nodes {
            b.state.resize(nodes, NodeState { gen: 0, cost: UNREACHED });
            b.flags.resize(nodes, 0);
        }
        b.epoch = b.epoch.wrapping_add(1);
        if b.epoch == 0 {
            b.state.fill(NodeState { gen: 0, cost: UNREACHED });
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
        assert!(
            nodes == grid.layers.len().saturating_mul(cells)
                && nodes <= i32::MAX as usize
                && (grid.via.is_none() || grid.via_blocked.len() >= cells)
                && b.state.len() >= nodes
                && b.flags.len() >= nodes
        );
        let ctx = AstarCtx {
            grid,
            epoch,
            via_cost,
            lo_i: lo_i as u32,
            hi_i: hi_i as u32,
            lo_j: lo_j as u32,
            hi_j: hi_j as u32,
            div_cells: FastDiv32::new(cells as u32),
            div_nx: FastDiv32::new(grid.nx as u32),
        };
        b.heap.clear();
        for &s in sources {
            if !grid.blocked[s] {
                b.state[s] = NodeState { gen: epoch, cost: 0 };
                let (_, cell32) = ctx.div_cells.div_rem(s as u32);
                let (j32, i32_u) = ctx.div_nx.div_rem(cell32);
                b.heap.push(ctx.est_ij(i32_u, j32), s);
            }
        }
        let via_pen_dense = grid.via_penalty.is_empty() || grid.via_penalty.len() >= cells;
        let spokes_ok = match grid.spokes.as_deref() {
            None => Some(false),
            Some(sp) if sp.track.len() >= nodes && sp.via.len() >= cells => Some(true),
            Some(_) => None,
        };
        let (reached, expanded) = match (
            grid.region_cost.is_empty() && via_pen_dense,
            grid.penalty.is_empty(),
            grid.penalty.len() >= nodes,
            spokes_ok,
        ) {
            (true, true, _, Some(false)) => run_astar_loop::<0>(&ctx, &mut b.state, &b.flags, &mut b.heap),
            (true, true, _, Some(true)) => run_astar_loop::<1>(&ctx, &mut b.state, &b.flags, &mut b.heap),
            (true, false, true, Some(false)) => run_astar_loop::<2>(&ctx, &mut b.state, &b.flags, &mut b.heap),
            (true, false, true, Some(true)) => run_astar_loop::<3>(&ctx, &mut b.state, &b.flags, &mut b.heap),
            _ => run_astar_loop::<4>(&ctx, &mut b.state, &b.flags, &mut b.heap),
        };
        work::add(&work::ASTAR_EXPANDED, expanded);
        let mut at = reached?;
        let cost_at = |state: &[NodeState], n: usize| if state[n].gen == epoch { state[n].cost } else { UNREACHED };
        let mut path = vec![at];
        while cost_at(&b.state, at) > 0 {
            let c_at = cost_at(&b.state, at);
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
                let cn = cost_at(&b.state, n);
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
                        let cn = cost_at(&b.state, n);
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

    #[test]
    fn radix_node_heap_matches_binary_heap_across_monotone_and_random_streams() {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut draw = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let (mut ours, mut theirs) = (NodeHeap::new(), BinaryHeap::new());
        for round in 0..4u32 {
            ours.clear();
            theirs.clear();
            assert_eq!(ours.pop(), None);
            let mut floor = round * 1_000;
            for step in 0..50_000u32 {
                let roll = draw() % 10;
                if roll < 6 {
                    let p = match step % 8 {
                        // Monotone A* steps with frequent exact ties.
                        0..=4 => floor.saturating_add((draw() % 20) as u32),
                        // Large bucket jumps across powers of two.
                        5 => floor.saturating_add(1u32 << (draw() % 28)),
                        // Non-monotone drops below `current` into `SmallHeap`.
                        6 => (draw() % (floor.saturating_add(1) as u64 + 32)) as u32,
                        // Extreme u32 priorities.
                        _ => u32::MAX - (draw() % 4) as u32,
                    };
                    let node = match draw() % 16 {
                        0 => u32::MAX as usize,
                        _ => (draw() % 512) as usize,
                    };
                    ours.push(p, node);
                    theirs.push(Reverse((p, node)));
                } else {
                    let want = theirs.pop().map(|Reverse(k)| k);
                    if let Some((p, _)) = want {
                        floor = floor.max(p);
                    }
                    assert_eq!(ours.pop(), want, "round {round} step {step}");
                }
            }
            while let Some(Reverse(k)) = theirs.pop() {
                assert_eq!(ours.pop(), Some(k), "drain round {round}");
            }
            assert_eq!(ours.pop(), None);
        }
    }

    #[test]
    fn fast_div32_is_exact_across_divisors_and_extremes() {
        let mut divisors = Vec::new();
        for d in 1..=64u32 {
            divisors.push(d);
        }
        for k in 6..32u32 {
            let p = 1u32 << k;
            divisors.extend([p - 1, p, p.saturating_add(1)]);
        }
        divisors.extend([997, 65_537, 1_000_000, 12_345_678, u32::MAX - 1, u32::MAX]);
        let mut state = 0xd1b5_4a32_d192_ed03u64;
        let mut draw = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u32
        };
        for d in divisors {
            let fd = FastDiv32::new(d);
            let mut probes = vec![0, 1, d.saturating_sub(1), d, d.saturating_add(1), u32::MAX - 1, u32::MAX];
            for mult in [2u64, 3, 17, 1024] {
                if let Ok(v) = u32::try_from(u64::from(d) * mult) {
                    probes.extend([v - 1, v, v.saturating_add(1)]);
                }
            }
            for _ in 0..64 {
                probes.push(draw());
            }
            for n in probes {
                assert_eq!(fd.div_rem(n), (n / d, n % d), "n={n} d={d}");
            }
        }
    }

    fn reference_route_astar(grid: &RouteGrid, sources: &[usize], targets: &[usize], via_cost: u32) -> Option<GridPath> {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        let nodes = grid.blocked.len();
        let cells = grid.cell_count();
        if cells == 0 || nodes == 0 {
            return None;
        }
        let (mut lo_i, mut lo_j, mut hi_i, mut hi_j) = (usize::MAX, usize::MAX, 0, 0);
        let mut is_target = vec![false; nodes];
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
        let mut cost = vec![UNREACHED; nodes];
        let mut heap = BinaryHeap::new();
        let mut any = false;
        for &s in sources {
            if !grid.blocked[s] {
                any = true;
                cost[s] = 0;
                heap.push(Reverse((estimate(s), s)));
            }
        }
        if !any {
            return None;
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
            for (di, dj, step) in NEIGHBOURS {
                let (ni, nj) = (i + di, j + dj);
                if ni < 0 || nj < 0 || ni >= grid.nx as i32 || nj >= grid.ny as i32 {
                    continue;
                }
                let next = grid.index(layer, ni as usize, nj as usize);
                if !grid.blocked[next] {
                    let cand = c.saturating_add(step_cost(grid.directional, layer, di, dj, step)).saturating_add(grid.penalty_at(next));
                    if cand < cost[next] {
                        cost[next] = cand;
                        heap.push(Reverse((cand.saturating_add(estimate(next)), next)));
                    }
                }
            }
            if grid.via.is_some() && !grid.via_blocked[cell] {
                for other in 0..grid.layers.len() {
                    if other != layer {
                        let next = other * cells + cell;
                        if !grid.blocked[next] {
                            let cand = c.saturating_add(via_cost).saturating_add(grid.via_penalty_at(cell)).saturating_add(grid.penalty_at(next));
                            if cand < cost[next] {
                                cost[next] = cand;
                                heap.push(Reverse((cand.saturating_add(estimate(next)), next)));
                            }
                        }
                    }
                }
            }
        }
        let mut at = reached?;
        let mut path = vec![at];
        while cost[at] > 0 {
            let c_at = cost[at];
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
                if cost[n] != UNREACHED && cost[n].saturating_add(actual).saturating_add(grid.penalty_at(at)) == c_at {
                    previous = Some(n);
                    break;
                }
            }
            if previous.is_none() && !grid.via_blocked[cell] {
                previous = (0..grid.layers.len())
                    .filter(|&other| other != layer)
                    .map(|other| other * cells + cell)
                    .find(|&n| {
                        cost[n] != UNREACHED && cost[n].saturating_add(via_cost).saturating_add(grid.via_penalty_at(cell)).saturating_add(grid.penalty_at(at)) == c_at
                    });
            }
            at = previous?;
            path.push(at);
        }
        path.reverse();
        Some(path.into_iter().map(|idx| (idx / cells, (idx % cells) % grid.nx, (idx % cells) / grid.nx)).collect())
    }

    #[test]
    fn route_astar_matches_reference_cell_for_cell_across_all_modes() {
        use std::sync::Arc;
        use crate::spokes::SpokePrices;
        for (mode, &(nx, ny, layers)) in [(1, 18, 2), (18, 1, 2), (3, 3, 1), (28, 22, 2), (24, 20, 4)].iter().enumerate() {
            for seed in 1..=25u64 {
                let mut state = (seed * 13_1071) ^ (mode as u64 + 1).wrapping_mul(0x517c_c1b7_2722_0a95);
                let mut next = || {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state
                };
                let cells = nx * ny;
                let nodes = cells * layers;
                let pen_mode = (seed as usize + mode) % 5;
                let spokes = if pen_mode == 1 || pen_mode == 3 || pen_mode == 4 {
                    let mut th = Vec::new();
                    let mut vh = Vec::new();
                    for idx in 0..nodes {
                        if next() % 11 == 0 {
                            th.push((idx, (next() % 3 + 1) as u32));
                        }
                    }
                    for c in 0..cells {
                        if next() % 13 == 0 {
                            vh.push((c, (next() % 3 + 1) as u32));
                        }
                    }
                    Some(Arc::new(SpokePrices::for_test(nodes, cells, &th, &vh, 10, 40)))
                } else {
                    None
                };
                let penalty = match pen_mode {
                    0 | 1 => Vec::new(),
                    2 | 3 => (0..nodes).map(|_| (next() % 9) as u32 * 3).collect(),
                    _ => (0..nodes / 2).map(|_| (next() % 7) as u32 * 5).collect(),
                };
                let region_cost = if pen_mode == 4 {
                    (0..nodes).map(|_| if next() % 9 == 0 { 17 } else { 0 }).collect()
                } else {
                    Vec::new()
                };
                let via_penalty = if seed % 2 == 0 {
                    (0..cells).map(|_| (next() % 6) as u32 * 8).collect()
                } else {
                    Vec::new()
                };
                let grid = RouteGrid {
                    net: 1,
                    x0: 0,
                    y0: 0,
                    h: 50_000,
                    nx,
                    ny,
                    layers: (0..layers).collect(),
                    half_width: 50_000,
                    via: (layers > 1 && seed % 7 != 0).then_some(ViaSpec { radius: 100_000, drill_radius: 50_000 }),
                    blocked: (0..nodes).map(|_| next() % 5 == 0).collect(),
                    via_blocked: (0..cells).map(|_| next() % 4 == 0).collect(),
                    penalty,
                    via_penalty,
                    region_cost,
                    pours_block: false,
                    directional: seed % 3 == 0,
                    deadline: Default::default(),
                    spokes,
                };
                let sources: Vec<usize> = (0..3).map(|_| (next() as usize) % nodes).collect();
                let targets: Vec<usize> = (0..3).map(|_| (next() as usize) % nodes).collect();
                let via_cost = 25 + (seed as u32 % 4) * 15;
                let got = route_astar(&grid, &sources, &targets, via_cost);
                let want = reference_route_astar(&grid, &sources, &targets, via_cost);
                assert_eq!(got, want, "mode={pen_mode} dims=({nx},{ny},{layers}) seed={seed}");
            }
        }
    }
}
