//! A global router, the coarse level of a two-level router (`--global`, experimental): every open
//! connection routed at once on a grid of GCells (1 mm squares, one plane of them per signal
//! layer) under the capacity of each GCell's sides in tracks, by negotiated congestion
//! (PathFinder): crossing a side costs more the more tracks it carries over its capacity, now and
//! in the rounds before. Each net's GCell paths become corridor regions
//! ([`RegionKind::Corridor`]), one GCell either side of its path on each layer it was given,
//! which the fine grids price through the board's routing regions like any planner's: the
//! detailed router itself is unchanged, and keeps to a net's corridor and layers where it can.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

use board::{Airline, Board, Nm, Point, Region, RegionKind, RegionRule, Shape};

use crate::clock::Deadline;
use crate::grid::{ObstacleMap, Profile, FREE, SEVERAL};
use crate::search::STRAIGHT;

#[derive(Debug, Clone)]
pub struct GlobalOptions {
    /// A GCell's side.
    pub cell: Nm,
    /// The most rounds of negotiation.
    pub rounds: usize,
    /// What a via costs, in millimetres of track.
    pub via_cost_mm: f64,
    /// What a net's track pays per length outside its corridor on a layer, on top of the length.
    pub corridor_weight: f64,
    pub pours_block: bool,
    pub deadline: Deadline,
}

impl Default for GlobalOptions {
    fn default() -> Self {
        GlobalOptions { cell: 1_000_000, rounds: 20, via_cost_mm: 5.0, corridor_weight: 3.0, pours_block: false, deadline: Deadline::default() }
    }
}

/// Fine cells along a GCell's side in the map its capacity is counted on.
const FINE_PER_GCELL: Nm = 10;
/// A side's price per track over its capacity, in GCell steps, at first and growing each round;
/// and what each round's overflow adds to its history.
const PRESENT_START: f64 = 0.5;
const PRESENT_GROWTH: f64 = 1.5;
const HISTORY_GAIN: f64 = 0.3;
/// A step across a plane's own direction, on a two-plane board, in tenths of a GCell, as the
/// fine grid's directional steps price it.
const ACROSS: u32 = 12;
/// A search looks at its deadline once in this many expansions.
const CHECK_EVERY: usize = 4096;

/// The GCells: the obstacle map's frame cut into squares, one plane of them per signal layer.
/// A node is `(plane * ny + j) * nx + i`; the side from it to the next GCell along x is edge
/// `2 * node`, along y `2 * node + 1`.
#[derive(Debug, Clone)]
pub struct GCells {
    pub x0: Nm,
    pub y0: Nm,
    /// A GCell's side.
    pub g: Nm,
    pub nx: usize,
    pub ny: usize,
    /// The board layer of each plane.
    pub layers: Vec<usize>,
    /// Per edge: how many tracks of the class cross that side.
    pub capacity: Vec<u16>,
    /// Per node: nobody may route anywhere in it (off the board, a keepout, a plane).
    pub blocked: Vec<bool>,
}

impl GCells {
    pub fn cells(&self) -> usize {
        self.nx * self.ny
    }

    /// The GCell `p` falls in, the nearest on the grid if it falls off it.
    pub fn cell_at(&self, p: Point) -> (usize, usize) {
        let at = |v: Nm, o: Nm, n: usize| (v - o).div_euclid(self.g).clamp(0, n as Nm - 1) as usize;
        (at(p.x, self.x0, self.nx), at(p.y, self.y0, self.ny))
    }

    /// The edge between two nodes next to each other on a plane.
    fn edge(&self, a: usize, b: usize) -> usize {
        let (lo, hi) = (a.min(b), a.max(b));
        2 * lo + usize::from(hi - lo != 1)
    }
}

/// The GCells of side `g` over `map` on `layers`, and the capacity of each side between two of
/// them: the rows (or columns) of fine cells free to every net on both sides of it, where a
/// track's centre may cross, times the cell, over a track's pitch (`pitch`, its width and
/// clearance). A GCell with no cell any net may use is blocked.
pub fn capacities(map: &ObstacleMap, g: Nm, layers: &[usize], pitch: Nm) -> GCells {
    let k = (g / map.h).max(1) as usize;
    let (nx, ny) = (map.nx.div_ceil(k), map.ny.div_ceil(k));
    let (cells, fine) = (nx * ny, map.nx * map.ny);
    let mut capacity = vec![0u16; 2 * layers.len() * cells];
    let mut usable = vec![false; layers.len() * cells];
    let lanes = |free: usize| (free as f64 * map.h as f64 / pitch.max(1) as f64).floor().min(f64::from(u16::MAX)) as u16;
    for (p, &l) in layers.iter().enumerate() {
        let free = |fi: usize, fj: usize| map.owner[l * fine + fj * map.nx + fi] == FREE;
        for fj in 0..map.ny {
            for fi in 0..map.nx {
                usable[p * cells + (fj / k) * nx + fi / k] |= map.owner[l * fine + fj * map.nx + fi] != SEVERAL;
            }
        }
        for j in 0..ny {
            for i in 0..nx {
                let node = p * cells + j * nx + i;
                let (rows, cols) = (j * k..((j + 1) * k).min(map.ny), i * k..((i + 1) * k).min(map.nx));
                // The side along x: the last column of this GCell and the first of the next.
                let x = (i + 1) * k;
                if i + 1 < nx && x < map.nx {
                    capacity[2 * node] = lanes(rows.clone().filter(|&fj| free(x - 1, fj) && free(x, fj)).count());
                }
                let y = (j + 1) * k;
                if j + 1 < ny && y < map.ny {
                    capacity[2 * node + 1] = lanes(cols.filter(|&fi| free(fi, y - 1) && free(fi, y)).count());
                }
            }
        }
    }
    GCells { x0: map.x0, y0: map.y0, g: map.h * k as Nm, nx, ny, layers: layers.to_vec(), capacity, blocked: usable.iter().map(|u| !u).collect() }
}

/// One open connection on the GCells.
struct Conn {
    net: u32,
    from: (usize, usize),
    to: (usize, usize),
    /// The planes each end may start on.
    from_on: u64,
    to_on: u64,
    /// Its nodes, end to end, once routed.
    path: Option<Vec<usize>>,
}

/// What the global stage found.
pub struct GlobalPlan {
    /// Each net's corridors, for the board's regions.
    pub regions: Vec<Region>,
    pub cells: GCells,
    /// Per edge: how many nets' paths cross it.
    pub demand: Vec<u16>,
    pub connections: usize,
    pub routed: usize,
    /// Tracks over capacity, summed over the edges, after each round.
    pub overflow: Vec<u32>,
    /// Nets given corridors: every connection of theirs routed.
    pub nets: usize,
    pub vias: usize,
    pub length_mm: f64,
}

/// Routes `airlines` on GCells, and returns with them the corridors of each net whose
/// connections all found a way. `None` when the board could not be mapped in time.
pub fn global_route(board: &Board, airlines: &[Airline], options: &GlobalOptions) -> Option<GlobalPlan> {
    let profile = class_profile(board, airlines);
    let pitch = 2 * profile.half_width + profile.clearance;
    let h = (options.cell / FINE_PER_GCELL).max(1);
    let map = ObstacleMap::without_by(board, profile, h, options.pours_block, &|_| false, &options.deadline)?;
    let layers: Vec<usize> = (0..board.layers.len()).filter(|l| board.planes & (1 << l) == 0).collect();
    let gc = capacities(&map, options.cell, &layers, pitch);
    let (cells, planes) = (gc.cells(), layers.len());
    let on = |id| {
        let it = board.item(id).map_or(0, |it| it.layers);
        let mask = (0..planes).filter(|&p| it & (1 << layers[p]) != 0).fold(0u64, |m, p| m | 1 << p);
        if mask == 0 { (1u64 << planes) - 1 } else { mask }
    };
    let mut conns: Vec<Conn> = airlines
        .iter()
        .map(|a| Conn { net: a.net, from: gc.cell_at(a.from), to: gc.cell_at(a.to), from_on: on(a.from_item), to_on: on(a.to_item), path: None })
        .collect();
    // Shorter connections first, as their ways are fewer.
    let span = |c: &Conn| c.from.0.abs_diff(c.to.0) + c.from.1.abs_diff(c.to.1);
    let mut order: Vec<usize> = (0..conns.len()).collect();
    order.sort_by_key(|&k| (span(&conns[k]), k));
    let mut of_net: HashMap<u32, Vec<usize>> = HashMap::new();
    for (k, c) in conns.iter().enumerate() {
        of_net.entry(c.net).or_default().push(k);
    }
    let via = (options.via_cost_mm * 1e6 / gc.g as f64 * f64::from(STRAIGHT)).round() as u32;
    let mut neg = Negotiation { demand: vec![0; gc.capacity.len()], uses: HashMap::new(), history: vec![0.0; gc.capacity.len()], present: PRESENT_START, price: vec![0; gc.capacity.len()] };
    let (mut overflow, mut failed) = (Vec::new(), HashSet::new());
    for round in 0..options.rounds.max(1) {
        if options.deadline.expired() {
            break;
        }
        neg.reprice(&gc.capacity);
        for &k in &order {
            if options.deadline.expired() {
                break;
            }
            // After the first round, only connections over a side over its capacity move.
            if failed.contains(&k) || round > 0 && conns[k].path.as_ref().is_some_and(|p| !edges(&gc, p, &conns[k]).any(|e| neg.over(e, &gc.capacity) > 0)) {
                continue;
            }
            if let Some(path) = conns[k].path.take() {
                neg.lay(&gc, &path, &conns[k], -1);
            }
            let c = &conns[k];
            // Where the net's other connections already pass, it adds no track.
            let mut priced = Vec::new();
            for &o in of_net[&c.net].iter().filter(|&&o| o != k) {
                for e in conns[o].path.as_ref().map(|p| edges(&gc, p, &conns[o]).collect::<Vec<_>>()).unwrap_or_default() {
                    priced.push((e, neg.price[e]));
                    neg.price[e] = neg.cost(e, 0, &gc.capacity);
                }
            }
            let ends = |(i, j): (usize, usize), m: u64| (0..planes).filter(move |p| m & (1 << p) != 0).map(move |p| p * cells + j * gc.nx + i).collect::<Vec<_>>();
            let found = search(&gc, &ends(c.from, c.from_on), &ends(c.to, c.to_on), &neg.price, via, &options.deadline);
            for &(e, v) in priced.iter().rev() {
                neg.price[e] = v;
            }
            match found {
                Some(path) => {
                    neg.lay(&gc, &path, &conns[k], 1);
                    conns[k].path = Some(path);
                }
                None if !options.deadline.expired() => {
                    failed.insert(k);
                }
                None => {}
            }
        }
        let over: Vec<u32> = (0..gc.capacity.len()).map(|e| neg.over(e, &gc.capacity)).collect();
        overflow.push(over.iter().sum());
        if overflow.last() == Some(&0) {
            break;
        }
        for (e, o) in over.iter().enumerate() {
            neg.history[e] += HISTORY_GAIN * f64::from(*o);
        }
        neg.present *= PRESENT_GROWTH;
    }
    let (mut regions, mut nets, mut vias, mut length) = (Vec::new(), 0, 0, 0.0);
    let mut ids: Vec<u32> = of_net.keys().copied().collect();
    ids.sort_unstable();
    for net in ids {
        let ks = &of_net[&net];
        if ks.iter().any(|&k| conns[k].path.is_none()) {
            continue;
        }
        nets += 1;
        let mut on_plane: Vec<Vec<(usize, usize)>> = vec![Vec::new(); planes];
        let mut ends = Vec::new();
        for &k in ks {
            let path = conns[k].path.as_deref().unwrap_or_default();
            for w in path.windows(2) {
                if w[0] / cells != w[1] / cells {
                    vias += 1;
                } else {
                    length += gc.g as f64 * 1e-6;
                }
            }
            for &n in path {
                on_plane[n / cells].push((n % cells % gc.nx, n % cells / gc.nx));
            }
            ends.extend([conns[k].from, conns[k].to]);
        }
        regions.extend(corridor_regions(&gc, net, &format!("global {}", board.net_name(net)), &on_plane, &ends, options.corridor_weight));
    }
    let routed = conns.iter().filter(|c| c.path.is_some()).count();
    Some(GlobalPlan { regions, demand: neg.demand, cells: gc, connections: conns.len(), routed, overflow, nets, vias, length_mm: length })
}

/// The sides a connection's path crosses on a plane, but those of the GCells of its own ends,
/// where its pads stand and no other net passes.
fn edges<'a>(gc: &'a GCells, path: &'a [usize], c: &Conn) -> impl Iterator<Item = usize> + 'a {
    let ends = [c.from.1 * gc.nx + c.from.0, c.to.1 * gc.nx + c.to.0];
    let cells = gc.cells();
    path.windows(2).filter(move |w| w[0] / cells == w[1] / cells && !ends.contains(&(w[0] % cells)) && !ends.contains(&(w[1] % cells))).map(|w| gc.edge(w[0], w[1]))
}

/// The demand on each side and its price.
struct Negotiation {
    /// Per edge: how many nets' paths cross it.
    demand: Vec<u16>,
    /// How many of a net's connections cross each edge it crosses.
    uses: HashMap<(u32, usize), u16>,
    history: Vec<f64>,
    present: f64,
    /// Per edge: what a path adding a track there pays to cross it, on top of the step.
    price: Vec<u32>,
}

impl Negotiation {
    /// Tracks over the edge's capacity.
    fn over(&self, e: usize, capacity: &[u16]) -> u32 {
        u32::from(self.demand[e].saturating_sub(capacity[e]))
    }

    /// What crossing edge `e` costs a path adding `extra` tracks there.
    fn cost(&self, e: usize, extra: u16, capacity: &[u16]) -> u32 {
        let over = (self.demand[e] + extra).saturating_sub(capacity[e]);
        (f64::from(STRAIGHT) * (self.history[e] + self.present * f64::from(over))).round() as u32
    }

    fn reprice(&mut self, capacity: &[u16]) {
        for e in 0..self.price.len() {
            self.price[e] = self.cost(e, 1, capacity);
        }
    }

    /// Adds (`by` 1) or takes away (`by` -1) a connection's path from the demand, pricing again
    /// the edges whose demand it changed.
    fn lay(&mut self, gc: &GCells, path: &[usize], c: &Conn, by: i32) {
        let mut seen = HashSet::new();
        for e in edges(gc, path, c) {
            if !seen.insert(e) {
                continue;
            }
            let uses = self.uses.entry((c.net, e)).or_insert(0);
            let was = *uses;
            *uses = (i32::from(was) + by).max(0) as u16;
            if (was == 0) != (*uses == 0) {
                self.demand[e] = (i32::from(self.demand[e]) + by).max(0) as u16;
                self.price[e] = self.cost(e, 1, &gc.capacity);
            }
        }
    }
}

/// The cheapest path of nodes from a source to a target by A*: a step to a neighbour on a plane
/// costs a GCell (more across the plane's direction on a two-plane board) and the edge's
/// `price`, a via `via`. Blocked GCells, and sides no track crosses, are passed only in the
/// GCells of the connection's ends, among its own pads. `None` if none is found before
/// `deadline`.
fn search(gc: &GCells, sources: &[usize], targets: &[usize], price: &[u32], via: u32, deadline: &Deadline) -> Option<Vec<usize>> {
    let (cells, planes) = (gc.cells(), gc.layers.len());
    let directional = planes == 2;
    let target: HashSet<usize> = targets.iter().copied().collect();
    let (lo_i, hi_i) = targets.iter().map(|t| t % cells % gc.nx).fold((usize::MAX, 0), |(lo, hi), i| (lo.min(i), hi.max(i)));
    let (lo_j, hi_j) = targets.iter().map(|t| t % cells / gc.nx).fold((usize::MAX, 0), |(lo, hi), j| (lo.min(j), hi.max(j)));
    let estimate = |n: usize| {
        let (i, j) = (n % cells % gc.nx, n % cells / gc.nx);
        STRAIGHT * (lo_i.saturating_sub(i) + i.saturating_sub(hi_i) + lo_j.saturating_sub(j) + j.saturating_sub(hi_j)) as u32
    };
    let ends: HashSet<usize> = sources.iter().chain(targets).map(|n| n % cells).collect();
    let open = |n: usize| !gc.blocked[n] || target.contains(&n);
    let side = |a: usize, b: usize| {
        let e = gc.edge(a, b);
        (gc.capacity[e] > 0 || ends.contains(&(a % cells)) || ends.contains(&(b % cells))).then(|| price[e])
    };
    let mut cost = vec![u32::MAX; planes * cells];
    let mut from = vec![usize::MAX; planes * cells];
    let mut heap = BinaryHeap::new();
    for &s in sources {
        cost[s] = 0;
        heap.push(Reverse((estimate(s), s)));
    }
    let mut expanded = 0;
    while let Some(Reverse((f, n))) = heap.pop() {
        if f > cost[n].saturating_add(estimate(n)) {
            continue;
        }
        if target.contains(&n) {
            let mut path = vec![n];
            while from[*path.last()?] != usize::MAX {
                path.push(from[*path.last()?]);
            }
            path.reverse();
            return Some(path);
        }
        expanded += 1;
        if expanded % CHECK_EVERY == 0 && deadline.expired() {
            return None;
        }
        let (p, i, j) = (n / cells, n % cells % gc.nx, n % cells / gc.nx);
        let mut steps: Vec<(usize, u32)> = Vec::with_capacity(4 + planes);
        let along = |x: bool| if directional && (p % 2 == 0) != x { ACROSS } else { STRAIGHT };
        for (m, x) in [(i + 1 < gc.nx, n + 1, true), (i > 0, n.wrapping_sub(1), true), (j + 1 < gc.ny, n + gc.nx, false), (j > 0, n.wrapping_sub(gc.nx), false)]
            .into_iter()
            .filter_map(|(on, m, x)| on.then_some((m, x)))
        {
            if let Some(p) = side(n, m) {
                steps.push((m, along(x) + p));
            }
        }
        steps.extend((0..planes).filter(|&q| q != p).map(|q| (q * cells + n % cells, via)));
        for (m, step) in steps {
            let c = cost[n].saturating_add(step);
            if open(m) && c < cost[m] {
                cost[m] = c;
                from[m] = n;
                heap.push(Reverse((c.saturating_add(estimate(m)), m)));
            }
        }
    }
    None
}

/// The profile of the class with the most open connections, whose tracks the capacities count.
fn class_profile(board: &Board, airlines: &[Airline]) -> Profile {
    let mut count: HashMap<String, (usize, u32)> = HashMap::new();
    for a in airlines {
        let class = board.rules.class_of(board.net_name(a.net)).map_or_else(String::new, |c| c.name.clone());
        count.entry(class).or_insert((0, a.net)).0 += 1;
    }
    let best = count.into_iter().max_by(|a, b| a.1 .0.cmp(&b.1 .0).then(b.0.cmp(&a.0)));
    Profile::for_net(board, best.map_or(0, |(_, (_, net))| net))
}

/// A net's corridor regions: on each plane, the GCells its paths take there and those of its
/// ends (a net may change layer at its pads), with one GCell round them, as rectangles; each
/// holds for the net alone on that layer, and its track pays `weight` per length outside them.
pub fn corridor_regions(gc: &GCells, net: u32, name: &str, on_plane: &[Vec<(usize, usize)>], ends: &[(usize, usize)], weight: f64) -> Vec<Region> {
    let mut out = Vec::new();
    for (p, cells) in on_plane.iter().enumerate() {
        let mut mask = vec![false; gc.cells()];
        for &(i, j) in cells.iter().chain(ends) {
            for jj in j.saturating_sub(1)..=(j + 1).min(gc.ny - 1) {
                for ii in i.saturating_sub(1)..=(i + 1).min(gc.nx - 1) {
                    mask[jj * gc.nx + ii] = true;
                }
            }
        }
        for (i0, j0, i1, j1) in rectangles(&mask, gc.nx, gc.ny) {
            let at = |i: usize, j: usize| Point::new(gc.x0 + i as Nm * gc.g, gc.y0 + j as Nm * gc.g);
            out.push(Region {
                name: name.to_string(),
                shape: Shape::Polygon(vec![at(i0, j0), at(i1, j0), at(i1, j1), at(i0, j1)]),
                layers: 1 << gc.layers[p],
                rules: vec![RegionRule { nets: Some([net].into()), kind: RegionKind::Corridor(weight), vias: false }],
            });
        }
    }
    out
}

/// The cells set in `mask` as rectangles `(i0, j0, i1, j1)`, ends exclusive: each row's runs,
/// a run carried down while the rows below repeat it.
fn rectangles(mask: &[bool], nx: usize, ny: usize) -> Vec<(usize, usize, usize, usize)> {
    let (mut done, mut open): (Vec<_>, Vec<(usize, usize, usize)>) = (Vec::new(), Vec::new());
    for j in 0..=ny {
        let mut runs = Vec::new();
        let mut i = 0;
        while j < ny && i < nx {
            if mask[j * nx + i] {
                let start = i;
                while i < nx && mask[j * nx + i] {
                    i += 1;
                }
                runs.push((start, i));
            } else {
                i += 1;
            }
        }
        let mut next = Vec::new();
        for (i0, i1, j0) in open {
            if let Some(k) = runs.iter().position(|&r| r == (i0, i1)) {
                runs.remove(k);
                next.push((i0, i1, j0));
            } else {
                done.push((i0, j0, i1, j));
            }
        }
        next.extend(runs.into_iter().map(|(i0, i1)| (i0, i1, j)));
        open = next;
    }
    done.sort_unstable_by_key(|&(i0, j0, ..)| (j0, i0));
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(layers: &str) -> Board {
        let (pcb, copper) = kicad_pcb::parse_board(&format!(
            "(kicad_pcb (version 20240108) (generator \"pcbnew\") (general (thickness 1.6))
             (layers {layers} (44 \"Edge.Cuts\" user))
             (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
             (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1)))"
        ))
        .unwrap();
        Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap())
    }

    fn two_layers() -> Board {
        board("(0 \"F.Cu\" signal) (31 \"B.Cu\" signal)")
    }

    fn item(kind: board::Kind, net: u32, layer: usize, a: (f64, f64), b: (f64, f64), r: f64) -> board::Item {
        let pt = |(x, y): (f64, f64)| Point::new((x * 1e6).round() as Nm, (y * 1e6).round() as Nm);
        board::Item { kind, net, layers: 1 << layer, shape: Shape::Capsule { a: pt(a), b: pt(b), r: (r * 1e6) as Nm }, hole: None, locked: true, span: None }
    }

    fn pad(net: u32, layer: usize, x: f64, y: f64) -> board::Item {
        item(board::Kind::Pad { footprint: 0, number: "1".into(), plated: false }, net, layer, (x, y), (x, y), 0.25)
    }

    #[test]
    fn a_side_holds_the_tracks_that_cross_it_where_nothing_stands() {
        let mut b = two_layers();
        // A wall of net 2 along y = 10 mm, from x = 2 to 12 mm, on F.Cu alone.
        b.insert(item(board::Kind::Track, 2, 0, (2.0, 10.0), (12.0, 10.0), 0.1));
        let map = ObstacleMap::new(&b, Profile::for_net(&b, 1), 100_000, false);
        // Default class: 0.2 mm track and clearance, a pitch of 0.4 mm.
        let gc = capacities(&map, 1_000_000, &[0, 1], 400_000);
        assert_eq!((gc.nx, gc.ny, gc.g), (20, 20, 1_000_000));
        let x_side = |p: usize, i: usize, j: usize| gc.capacity[2 * (p * gc.cells() + j * gc.nx + i)];
        let y_side = |p: usize, i: usize, j: usize| gc.capacity[2 * (p * gc.cells() + j * gc.nx + i) + 1];
        // Ten free rows of 0.1 mm across a side in the open: two and a half tracks.
        assert_eq!(x_side(0, 15, 15), 2);
        assert_eq!(y_side(1, 5, 9), 2);
        // The wall runs along the side between rows 9 and 10: nothing crosses it on F.Cu, and
        // the sides along it carry fewer tracks beside it.
        assert_eq!(y_side(0, 5, 9), 0);
        assert!(x_side(0, 5, 10) < 2 && x_side(0, 5, 9) < 2);
        assert_eq!(y_side(0, 15, 9), 2, "past the wall's end");
        // The board's edge: no side leads off it, and the GCells beyond the outline's band are
        // blocked.
        assert_eq!(x_side(0, 19, 5), 0);
        assert!(!gc.blocked[5 * gc.nx + 5]);
    }

    #[test]
    fn a_corridor_is_its_path_and_its_ends_with_a_gcell_round_them() {
        let gc = GCells { x0: 0, y0: 0, g: 1_000_000, nx: 12, ny: 12, layers: vec![0, 1], capacity: Vec::new(), blocked: Vec::new() };
        let path: Vec<(usize, usize)> = (2..=8).map(|i| (i, 5)).collect();
        let regions = corridor_regions(&gc, 7, "global A", &[path, Vec::new()], &[(2, 5), (8, 5)], 3.0);
        let rect = |r: &Region| match &r.shape {
            Shape::Polygon(p) => (p[0].x / 1_000_000, p[0].y / 1_000_000, p[2].x / 1_000_000, p[2].y / 1_000_000),
            _ => panic!("a rectangle"),
        };
        let on = |layers: u64| regions.iter().filter(|r| r.layers == layers).map(rect).collect::<Vec<_>>();
        assert_eq!(on(1), vec![(1, 4, 10, 7)], "F.Cu: the path, a GCell either side");
        assert_eq!(on(2), vec![(1, 4, 4, 7), (7, 4, 10, 7)], "B.Cu: its ends alone");
        for r in &regions {
            assert_eq!(r.rules, vec![RegionRule { nets: Some([7].into()), kind: RegionKind::Corridor(3.0), vias: false }]);
        }
    }

    #[test]
    fn rectangles_cover_the_mask_once() {
        let (nx, ny) = (9, 7);
        let mut mask = vec![false; nx * ny];
        for (i, j) in [(1, 1), (2, 1), (3, 1), (1, 2), (2, 2), (3, 2), (3, 3), (4, 3), (6, 5), (7, 5), (6, 6)] {
            mask[j * nx + i] = true;
        }
        let mut covered = vec![0; nx * ny];
        for (i0, j0, i1, j1) in rectangles(&mask, nx, ny) {
            for j in j0..j1 {
                for i in i0..i1 {
                    covered[j * nx + i] += 1;
                }
            }
        }
        assert_eq!(covered, mask.iter().map(|&m| u32::from(m)).collect::<Vec<_>>());
    }

    #[test]
    fn a_connection_walled_off_on_its_layer_is_given_the_other() {
        let mut b = two_layers();
        b.insert(pad(1, 0, 3.0, 10.0));
        b.insert(pad(1, 0, 17.0, 10.0));
        // A wall of net 2 across F.Cu from edge to edge.
        b.insert(item(board::Kind::Track, 2, 0, (10.0, -1.0), (10.0, 21.0), 0.1));
        let airlines = board::connectivity_without_pours(&b).airlines;
        assert_eq!(airlines.len(), 1);
        let plan = global_route(&b, &airlines, &GlobalOptions::default()).unwrap();
        assert_eq!((plan.connections, plan.routed, plan.nets), (1, 1, 1));
        assert_eq!(plan.vias, 2, "down under the wall and up again");
        assert_eq!(plan.overflow, vec![0]);
        // F.Cu's corridor runs end to end; B.Cu's is the crossing under the wall and the ends.
        let covers = |layers: u64, x: i64| plan.regions.iter().filter(|r| r.layers == layers).any(|r| r.shape.contains(Point::new(x, 10_500_000)));
        assert!((3..=17).all(|x| covers(1, x * 1_000_000 + 500_000)));
        assert!(covers(2, 9_500_000) && covers(2, 10_500_000) && covers(2, 3_500_000));
        assert!(!covers(2, 6_500_000) && !covers(2, 13_500_000));
    }
}
