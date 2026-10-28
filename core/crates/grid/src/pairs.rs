//! Differential pairs routed coupled end to end, before single-ended routing: the two tracks of a
//! pair (such as USB D+ and D-) run side by side at the net class's differential width and gap,
//! change layers together through paired vias spaced at the class's differential via gap, and
//! converge symmetrically from spread pads into the coupled run at each end. Every piece kept is
//! locked against later single-ended rip-up, ploughing and tightening, then handed to length
//! tuning at the end of the pipeline.

use std::collections::HashSet;

use board::{nm, BBox, Board, Item, ItemId, Kind, Nm, Point, Shape};

use crate::grid::{ObstacleMap, Profile, RouteGrid, ViaSpec, FREE, SEVERAL};
use crate::report::{tell, Focus, Log};
use crate::route::{local_cluster, pieces, track, Terminal};
use crate::search::{route_astar, GridPath, STRAIGHT};

/// Pads of a pair farther apart than this are not taken as one end of it.
const PIN_PAIR_SPAN: Nm = 3_000_000;
/// Pin pairs closer than this are joined by matched bridges alone; there is no run worth coupling.
const SHORTEST_RUN: Nm = 1_500_000;
/// Legacy start rings, kept for `--diff-pairs-legacy`.
const LEGACY_RINGS: [(Nm, Nm); 4] = [(0, 300_000), (400_000, 900_000), (900_000, 1_600_000), (1_600_000, 2_500_000)];
/// Rings around each pin pair's midpoint where a coupled stretch may start or end, tried from
/// nearest to farthest so spread connector pins still converge cleanly.
const END_RINGS: [(Nm, Nm); 5] = [
    (0, 300_000),
    (300_000, 900_000),
    (900_000, 1_600_000),
    (1_600_000, 2_500_000),
    (2_500_000, 3_400_000),
];
/// A coupled run shorter than this is not worth laying.
const SHORTEST_COUPLED: f64 = 500_000.0;
/// What a paired layer change costs in search length, and what each paired body via vs uncoupled
/// leg via costs when ranking candidate runs.
const VIA_COST: f64 = 2_000_000.0;
const PER_VIA_COST: f64 = 1_500_000.0;
const PER_LEG_VIA_COST: f64 = 1_750_000.0;
/// A turn sharper than this (cosine of the angle between the pieces) is cut in two first.
const SHARP_TURN: f64 = 0.75;
/// Fine grid cell and window margin for routing matched end legs and short bridges.
const LEG_CELL: Nm = 25_000;
const LEG_WINDOW: Nm = 2_000_000;
/// Rounding slack kept above minimum copper and hole spacing on paired vias.
const VIA_SLACK: Nm = 4_000;
/// How far from a pad an escape of its own copper (a fan-out dogbone, say) may be for an end leg
/// to start from it, and for it to be locked with the pair.
const CLUSTER_REACH: Nm = 1_800_000;

/// A differential pair: its positive and negative nets, each track's width and the gap between
/// them, in nanometres.
#[derive(Debug, Clone, PartialEq)]
pub struct Pair {
    pub p: u32,
    pub n: u32,
    pub width: Nm,
    pub gap: Nm,
}

impl Pair {
    /// Centre to centre of the two tracks.
    pub fn pitch(&self) -> Nm {
        self.width + self.gap
    }
}

#[derive(Debug, Clone)]
pub struct PairOptions {
    pub cell: Nm,
    pub pours_block: bool,
    /// Run the single-layer, legless pair stage from before end-to-end coupling, for comparison.
    pub legacy: bool,
    /// How long end-to-end coupling may search, in seconds: runs still to do after it are
    /// coupled the legacy way, their legs left to the router.
    pub max_seconds: Option<f64>,
    pub log: Option<Log>,
}

impl Default for PairOptions {
    fn default() -> PairOptions {
        PairOptions { cell: 50_000, pours_block: false, legacy: false, max_seconds: None, log: None }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PairStats {
    pub pairs: usize,
    /// Runs between pin pairs worth coupling, and how many were laid.
    pub runs: usize,
    pub coupled: usize,
    /// Length of the coupled runs, along their centrelines.
    pub coupled_mm: f64,
    pub laid: Vec<ItemId>,
}

/// Whether two net names are a pair's positive and negative: alike but for one P (or +) against
/// N, M (or -), the letters standing alone or after a D, as in `USB_DP`/`USB_DM` or `CLK_P`/`CLK_N`.
fn pair_names(p: &str, n: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (p.chars().collect(), n.chars().collect());
    if a.len() != b.len() {
        return false;
    }
    let differ: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
    let [i] = differ[..] else { return false };
    let (x, y) = (a[i].to_ascii_uppercase(), b[i].to_ascii_uppercase());
    let signs = matches!((x, y), ('P', 'N') | ('P', 'M') | ('+', '-'));
    let alone = |c: Option<&char>| c.is_none_or(|c| !c.is_ascii_alphanumeric());
    let before = i.checked_sub(1).map(|k| &a[k]);
    let letter = x.is_ascii_alphabetic();
    signs && alone(a.get(i + 1)) && (!letter || alone(before) || before.is_some_and(|c| c.eq_ignore_ascii_case(&'D')))
}

/// The board's differential pairs, found by their net names, sized by the positive net's class.
pub fn diff_pairs(board: &Board) -> Vec<Pair> {
    let mut pairs = Vec::new();
    for (p, pn) in board.nets.iter().enumerate() {
        for (n, nn) in board.nets.iter().enumerate() {
            if p != n && !pn.is_empty() && pair_names(pn, nn) {
                let class = board.rules.class_of(pn);
                let profile = Profile::for_net(board, p as u32);
                let width = class.map(|c| nm(c.diff_pair_width)).filter(|w| *w > 0).unwrap_or(2 * profile.half_width);
                let gap = class.map(|c| nm(c.diff_pair_gap)).filter(|g| *g > 0).unwrap_or(profile.clearance);
                pairs.push(Pair { p: p as u32, n: n as u32, width: width.max(nm(board.rules.min_track_width_of(pn))), gap: gap.max(profile.clearance) });
            }
        }
    }
    pairs
}

fn centre(board: &Board, id: ItemId) -> Point {
    let b = board.bbox(id).unwrap_or(BBox::of([Point::new(0, 0)]));
    Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2)
}

fn pad_footprint(board: &Board, id: ItemId) -> Option<usize> {
    match board.item(id)?.kind {
        Kind::Pad { footprint, .. } => Some(footprint),
        _ => None,
    }
}

/// One end of a pair: a pad of each net.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PinPair {
    pub(crate) p: ItemId,
    pub(crate) n: ItemId,
    pub(crate) mid: Point,
    pub(crate) layers: u64,
}

/// Each positive pad with the nearest negative pad that has it as its own nearest.
pub(crate) fn pin_pairs(board: &Board, pair: &Pair) -> Vec<PinPair> {
    let pads = |net: u32| -> Vec<ItemId> {
        board.items().filter(|(_, it)| it.net == net && matches!(it.kind, Kind::Pad { .. }) && it.layers != 0).map(|(id, _)| id).collect()
    };
    let (ps, ns) = (pads(pair.p), pads(pair.n));
    let d = |a: ItemId, b: ItemId| centre(board, a).distance(centre(board, b));
    let nearest = |from: ItemId, among: &[ItemId]| among.iter().copied().min_by(|&x, &y| d(from, x).total_cmp(&d(from, y)));
    let mut out = Vec::new();
    for &p in &ps {
        let Some(n) = nearest(p, &ns) else { continue };
        if nearest(n, &ps) == Some(p) && d(p, n) <= PIN_PAIR_SPAN as f64 {
            let (a, b) = (centre(board, p), centre(board, n));
            let layers = board.item(p).map_or(0, |it| it.layers) & board.item(n).map_or(0, |it| it.layers);
            out.push(PinPair { p, n, mid: Point::new((a.x + b.x) / 2, (a.y + b.y) / 2), layers });
        }
    }
    out
}

/// Whether `pair` has at least two pin pairs and open connections still to route, so pre-fanout
/// should leave its fine-pitch pads for `route_pairs` to fan out and couple together.
pub fn pair_has_open_runs(board: &Board, pair: &Pair, pours_block: bool) -> bool {
    if pin_pairs(board, pair).len() < 2 {
        return false;
    }
    let conn = if pours_block { board::connectivity(board) } else { board::connectivity_without_pours(board) };
    conn.airlines.iter().any(|a| a.net == pair.p || a.net == pair.n)
}

/// The runs to couple: a spanning tree over the pin pairs, shortest edges first.
fn runs(ends: &[PinPair]) -> Vec<(usize, usize)> {
    let mut edges: Vec<(f64, usize, usize)> =
        (0..ends.len()).flat_map(|i| (i + 1..ends.len()).map(move |j| (i, j))).map(|(i, j)| (ends[i].mid.distance(ends[j].mid), i, j)).collect();
    edges.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut group: Vec<usize> = (0..ends.len()).collect();
    fn root(g: &mut [usize], mut i: usize) -> usize {
        while g[i] != i {
            g[i] = g[g[i]];
            i = g[i];
        }
        i
    }
    let mut out = Vec::new();
    for (_, i, j) in edges {
        let (a, b) = (root(&mut group, i), root(&mut group, j));
        if a != b {
            group[a] = b;
            out.push((i, j));
        }
    }
    out
}

/// The free cells of one layer within a ring around `at`.
fn ring_cells(grid: &RouteGrid, layer: usize, at: Point, (inner, outer): (Nm, Nm)) -> Vec<usize> {
    let mut out = Vec::new();
    let reach = outer / grid.h + 1;
    let Some((ci, cj)) = grid.cell_at(at) else { return out };
    for j in (cj as Nm - reach).max(0)..=(cj as Nm + reach).min(grid.ny as Nm - 1) {
        for i in (ci as Nm - reach).max(0)..=(ci as Nm + reach).min(grid.nx as Nm - 1) {
            let d = grid.centre(i as usize, j as usize).distance(at);
            let index = grid.index(layer, i as usize, j as usize);
            if d >= inner as f64 && d <= outer as f64 && !grid.blocked[index] {
                out.push(index);
            }
        }
    }
    out
}

/// Free cells within `ring` around `at` on every routing layer whose board layer is in `mask`.
fn ring_cells_mask(grid: &RouteGrid, mask: u64, at: Point, ring: (Nm, Nm)) -> Vec<usize> {
    let mut out = Vec::new();
    for (rl, &bl) in grid.layers.iter().enumerate() {
        if mask & (1 << bl) != 0 {
            out.extend(ring_cells(grid, rl, at, ring));
        }
    }
    out
}

/// Whether the straight line from `a` to `b` stays on free cells of `layer`.
fn clear_line(grid: &RouteGrid, layer: usize, a: Point, b: Point) -> bool {
    let steps = (a.distance(b) / (grid.h as f64 / 2.0)).ceil().max(1.0) as i64;
    (0..=steps).all(|k| {
        let t = k as f64 / steps as f64;
        let p = Point::new(a.x + ((b.x - a.x) as f64 * t).round() as Nm, a.y + ((b.y - a.y) as f64 * t).round() as Nm);
        grid.cell_at(p).is_some_and(|(i, j)| !grid.blocked[grid.index(layer, i, j)])
    })
}

/// Extra region or pour cost along the straight segment from `a` to `b` on `layer`, in nanometres
/// of equivalent track length.
fn line_region_cost(grid: &RouteGrid, layer: usize, a: Point, b: Point) -> f64 {
    if grid.region_cost.is_empty() {
        return 0.0;
    }
    let len = a.distance(b);
    if len <= 0.0 {
        return 0.0;
    }
    let steps = (len / grid.h as f64).ceil().max(1.0) as i64;
    let step_len = len / steps as f64;
    let mut extra = 0.0;
    for k in 0..steps {
        let t = (k as f64 + 0.5) / steps as f64;
        let p = Point::new(a.x + ((b.x - a.x) as f64 * t).round() as Nm, a.y + ((b.y - a.y) as f64 * t).round() as Nm);
        if let Some((i, j)) = grid.cell_at(p) {
            let c = grid.region_cost.get(grid.index(layer, i, j)).copied().unwrap_or(0);
            extra += (c as f64 / STRAIGHT as f64) * step_len;
        }
    }
    extra
}

/// The path's cell centres pulled straight wherever the grid allows: from each corner, on to
/// the farthest later one in plain view.
fn pull_straight(grid: &RouteGrid, layer: usize, points: &[Point]) -> Vec<Point> {
    if points.is_empty() {
        return Vec::new();
    }
    let mut out = vec![points[0]];
    let mut at = 0;
    while at + 1 < points.len() {
        let mut next = at + 1;
        for k in (at + 2..points.len()).rev() {
            if clear_line(grid, layer, points[at], points[k]) {
                next = k;
                break;
            }
        }
        out.push(points[next]);
        at = next;
    }
    out
}

fn unit(a: Point, b: Point) -> (f64, f64) {
    let (dx, dy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
    let l = dx.hypot(dy).max(1.0);
    (dx / l, dy / l)
}

/// Sharp turns cut in two, `by` back along each piece, so neither track's corner juts out.
fn ease(points: &[Point], by: Nm) -> Vec<Point> {
    if by <= 0 || points.len() < 3 {
        return points.to_vec();
    }
    let mut out = vec![points[0]];
    for k in 1..points.len().saturating_sub(1) {
        let (a, v, b) = (points[k - 1], points[k], points[k + 1]);
        let (u1, u2) = (unit(a, v), unit(v, b));
        let cos = u1.0 * u2.0 + u1.1 * u2.1;
        let room = (a.distance(v).min(v.distance(b)) / 2.0) as Nm;
        if cos < SHARP_TURN && room > 0 {
            let c = by.min(room) as f64;
            out.push(Point::new(v.x - (u1.0 * c).round() as Nm, v.y - (u1.1 * c).round() as Nm));
            out.push(Point::new(v.x + (u2.0 * c).round() as Nm, v.y + (u2.1 * c).round() as Nm));
        } else {
            out.push(v);
        }
    }
    if points.len() > 1 {
        out.push(points[points.len() - 1]);
    }
    out
}

/// The line `by` to the left of `points` (positive is left of the direction of travel, in the
/// board's coordinates), corners mitred so the two lines stay parallel.
fn offset(points: &[Point], by: f64) -> Vec<Point> {
    let normal = |a: Point, b: Point| {
        let (x, y) = unit(a, b);
        (-y, x)
    };
    let n = points.len();
    (0..n)
        .map(|k| {
            let (nx, ny) = if k == 0 {
                normal(points[0], points[1])
            } else if k == n - 1 {
                normal(points[n - 2], points[n - 1])
            } else {
                let (a, b) = (normal(points[k - 1], points[k]), normal(points[k], points[k + 1]));
                let s = (1.0 + a.0 * b.0 + a.1 * b.1).max(0.25);
                ((a.0 + b.0) / s, (a.1 + b.1) / s)
            };
            Point::new(points[k].x + (nx * by).round() as Nm, points[k].y + (ny * by).round() as Nm)
        })
        .collect()
}

/// Which side of the run's start `p` stands on: left is positive.
fn side(points: &[Point], p: Point) -> f64 {
    let (a, b) = (points[0], points[1]);
    ((b.x - a.x) as f64) * ((p.y - a.y) as f64) - ((b.y - a.y) as f64) * ((p.x - a.x) as f64)
}

/// Whether an item laid for a pair keeps every design rule. With `pours_block` off, refilled
/// board-wide pours are ignored while functional-block kept pours and plane keepouts are enforced.
fn legal_pair_item(board: &Board, id: ItemId, pours_block: bool) -> bool {
    if pours_block {
        board::check_item_with_pours(board, id).is_empty()
    } else {
        board::check_item(board, id).is_empty()
    }
}

/// Lays one track per piece of `line`; all or nothing.
fn lay_line(board: &mut Board, line: &[Point], net: u32, layer: usize, r: Nm, pours_block: bool, laid: &mut Vec<ItemId>) -> bool {
    for w in line.windows(2) {
        if w[0] == w[1] {
            continue;
        }
        let id = board.insert(Item {
            kind: Kind::Track,
            net,
            layers: 1 << layer,
            shape: Shape::Capsule { a: w[0], b: w[1], r },
            hole: None,
            locked: false,
            span: None,
        });
        laid.push(id);
        if !legal_pair_item(board, id, pours_block) {
            return false;
        }
    }
    true
}

fn discard_all(board: &mut Board, ids: Vec<ItemId>) {
    for id in ids.into_iter().rev() {
        board.discard(id);
    }
}

fn length(points: &[Point]) -> f64 {
    points.windows(2).map(|w| w[0].distance(w[1])).sum()
}

fn items_track_len(board: &Board, ids: &[ItemId]) -> f64 {
    ids.iter()
        .filter_map(|&id| board.item(id))
        .map(|it| match (&it.kind, &it.shape) {
            (Kind::Track, Shape::Capsule { a, b, .. }) => a.distance(*b),
            _ => 0.0,
        })
        .sum()
}

fn items_via_count(board: &Board, ids: &[ItemId]) -> usize {
    ids.iter().filter(|&&id| board.item(id).is_some_and(|it| it.kind == Kind::Via)).count()
}

/// Whether `from` reaches `to` on `net`'s copper, judged with the same pour rule as connectivity.
fn net_reaches(board: &Board, from: ItemId, to: ItemId, pours_block: bool) -> bool {
    if from == to {
        return true;
    }
    let Some(start) = board.item(from) else { return false };
    let net = start.net;
    let member = |it: &Item| it.is_copper() && it.net == net && (pours_block || !it.refilled() || board.is_plane(it));
    let mut seen = HashSet::from([from]);
    let mut stack = vec![from];
    while let Some(at) = stack.pop() {
        let Some(item) = board.item(at) else { continue };
        let bb = board.bbox(at).unwrap_or_else(|| item.shape.bbox());
        for l in (0..board.layers.len()).filter(|&l| item.on(l)) {
            for other in board.near(l, bb) {
                if !seen.contains(&other) && board.item(other).is_some_and(|o| o.on(l) && member(o)) && board::joined(board, at, other) {
                    if other == to {
                        return true;
                    }
                    seen.insert(other);
                    stack.push(other);
                }
            }
        }
    }
    false
}

fn ends_joined(conn: &board::Connectivity, a: PinPair, b: PinPair) -> bool {
    let same = |x: ItemId, y: ItemId| match (conn.cluster.get(&x), conn.cluster.get(&y)) {
        (Some(cx), Some(cy)) => cx == cy,
        _ => false,
    };
    same(a.p, b.p) && same(a.n, b.n)
}

/// Preferred and minimum centre-to-centre spacing for paired vias of `pair`: the class's
/// `diff_pair_via_gap` comes first where wider than the board's clearance and hole rules.
fn via_pitches(board: &Board, pair: &Pair, profile: &Profile) -> (Nm, Nm) {
    let clr = board.clearance(pair.p, pair.n);
    let mask = if board.via_openings.0 != 0 { board.via_openings.1 + board::MASK_MARGIN } else { 0 };
    let via_clr = clr.max(mask);
    let h2h = nm(board.rules.min_hole_to_hole);
    let min_vp = (2 * profile.via_radius + via_clr)
        .max(2 * profile.drill_radius + h2h)
        .max(pair.pitch())
        + VIA_SLACK;
    let class_gap = board
        .rules
        .class_of(board.net_name(pair.p))
        .map(|c| nm(c.diff_pair_via_gap))
        .filter(|&g| g > 0);
    let pref_vp = class_gap.map_or(min_vp, |g| (2 * profile.via_radius + g).max(min_vp));
    (pref_vp, min_vp)
}

/// One run between two pin pairs as `route_pairs_watch` tried it: the pin pairs' midpoints, and
/// if it was laid, its layer, centreline and the tracks and vias kept.
#[derive(Debug, Clone)]
pub struct PairRun<'a> {
    pub pair: &'a Pair,
    pub from: Point,
    pub to: Point,
    pub layer: Option<usize>,
    pub centreline: &'a [Point],
    pub laid: &'a [ItemId],
}

/// Routes every differential pair's runs coupled, as far as they will go; what is laid is
/// returned in the stats, for the caller to keep apart from copper it may rip.
pub fn route_pairs(board: &mut Board, options: &PairOptions, watch: &mut dyn FnMut(&Board)) -> PairStats {
    route_pairs_watch(board, options, &mut |b, run| {
        if run.layer.is_some() {
            watch(b);
        }
    })
}

/// `route_pairs`, telling `watch` of every run tried, laid or not.
pub fn route_pairs_watch(board: &mut Board, options: &PairOptions, watch: &mut dyn FnMut(&Board, &PairRun)) -> PairStats {
    if options.legacy {
        return route_pairs_legacy(board, options, watch);
    }
    let started = crate::clock::Instant::now();
    let mut stats = PairStats::default();
    for pair in diff_pairs(board) {
        stats.pairs += 1;
        let ends = pin_pairs(board, &pair);
        if ends.len() < 2 {
            continue;
        }
        let profile = Profile::for_net(board, pair.p);
        let mst = runs(&ends);
        let mut long_runs: Vec<(usize, usize)> = Vec::new();
        let mut short_bridges: Vec<(usize, usize)> = Vec::new();
        for (i, j) in mst {
            if ends[i].mid.distance(ends[j].mid) < SHORTEST_RUN as f64 {
                short_bridges.push((i, j));
            } else {
                long_runs.push((i, j));
            }
        }
        // Inter-footprint trunks take open corridors before intra-footprint pass-throughs (such as
        // across an ESD protector's pads), which can share or drop under them cleanly.
        long_runs.sort_by(|&(i1, j1), &(i2, j2)| {
            let same1 = pad_footprint(board, ends[i1].p) == pad_footprint(board, ends[j1].p);
            let same2 = pad_footprint(board, ends[i2].p) == pad_footprint(board, ends[j2].p);
            same1
                .cmp(&same2)
                .then_with(|| ends[i1].mid.distance(ends[j1].mid).total_cmp(&ends[i2].mid.distance(ends[j2].mid)))
        });

        for (i, j) in long_runs {
            let conn = if options.pours_block { board::connectivity(board) } else { board::connectivity_without_pours(board) };
            let (a, b) = (ends[i], ends[j]);
            if ends_joined(&conn, a, b) {
                continue;
            }
            stats.runs += 1;
            tell(
                &options.log,
                "pairs",
                || format!("coupling {} and {}", board.net_name(pair.p), board.net_name(pair.n)),
                Some(Focus::Connection { net: pair.p, from: a.mid, to: b.mid }),
            );
            // Where a connector has two pin pairs already jumpered together (such as USB-C A6/B6
            // and A7/B7), try both attachment sites and keep the cleaner run.
            let a_alts: Vec<PinPair> = ends
                .iter()
                .copied()
                .filter(|&e| pad_footprint(board, e.p) == pad_footprint(board, a.p) && ends_joined(&conn, a, e))
                .collect();
            let b_alts: Vec<PinPair> = ends
                .iter()
                .copied()
                .filter(|&e| pad_footprint(board, e.p) == pad_footprint(board, b.p) && ends_joined(&conn, b, e))
                .collect();
            let a_cands = if a_alts.is_empty() { vec![a] } else { a_alts };
            let b_cands = if b_alts.is_empty() { vec![b] } else { b_alts };

            let mut best: Option<CoupledCandidate> = None;
            let out_of_time = || late(options, started);
            for &ac in &a_cands {
                for &bc in &b_cands {
                    // Each try builds the board's map afresh, which on a large board is slow.
                    if out_of_time() {
                        break;
                    }
                    if let Some(cand) = couple_end_to_end(board, options, started, &pair, &profile, ac, bc) {
                        if best.as_ref().is_none_or(|prev| cand.score < prev.score) {
                            best = Some(cand);
                        }
                    }
                }
            }
            if let Some(chosen) = best {
                if let Some(committed) = commit_candidate(board, options, &pair, &profile, chosen) {
                    let layer = committed.main_layer;
                    let layer_count = committed.stretch_layers;
                    let paired_vias = committed.paired_vias;
                    let coupled_len = committed.coupled_len;
                    tell(
                        &options.log,
                        "pairs",
                        || {
                            if layer_count <= 1 {
                                format!(
                                    "coupled {} and {} on {} for {:.2} mm",
                                    board.net_name(pair.p),
                                    board.net_name(pair.n),
                                    board.layers[layer],
                                    coupled_len * 1e-6
                                )
                            } else {
                                format!(
                                    "coupled {} and {} across {} layers for {:.2} mm ({} paired vias)",
                                    board.net_name(pair.p),
                                    board.net_name(pair.n),
                                    layer_count,
                                    coupled_len * 1e-6,
                                    paired_vias
                                )
                            }
                        },
                        None,
                    );
                    stats.coupled += 1;
                    stats.coupled_mm += coupled_len * 1e-6;
                    stats.laid.extend(committed.laid.iter().copied());
                    watch(
                        board,
                        &PairRun {
                            pair: &pair,
                            from: a.mid,
                            to: b.mid,
                            layer: Some(layer),
                            centreline: &committed.centreline,
                            laid: &committed.laid,
                        },
                    );
                    continue;
                }
            }
            // No end-to-end run (or no time left to look for one): the legacy run, a coupled
            // centreline with its legs left to the router, unlocked, is better than none.
            let fat = Profile { class: format!("pair {}", board.net_name(pair.p)), half_width: pair.width + pair.gap / 2, ..profile.clone() };
            if let Some(laid) = legacy_run(board, options, &pair, &fat, a, b) {
                let layer = laid.0.first().and_then(|id| board.item(*id)).map_or(0, |it| it.layers.trailing_zeros() as usize);
                tell(
                    &options.log,
                    "pairs",
                    || format!("coupled {} and {} on {} for {:.2} mm, legs left to the router", board.net_name(pair.p), board.net_name(pair.n), board.layers[layer], laid.1 * 1e-6),
                    None,
                );
                stats.coupled += 1;
                stats.coupled_mm += laid.1 * 1e-6;
                stats.laid.extend(laid.0.iter().copied());
                watch(board, &PairRun { pair: &pair, from: a.mid, to: b.mid, layer: Some(layer), centreline: &laid.2, laid: &laid.0 });
                continue;
            }
            watch(board, &PairRun { pair: &pair, from: a.mid, to: b.mid, layer: None, centreline: &[], laid: &[] });
        }

        // Short spans between adjacent pin pairs (such as series resistors beside an ESD chip, or
        // stripped USB-C A/B pins) get matched pair-width bridges and are locked with the pair.
        for (i, j) in short_bridges {
            let conn = if options.pours_block { board::connectivity(board) } else { board::connectivity_without_pours(board) };
            let (a, b) = (ends[i], ends[j]);
            if ends_joined(&conn, a, b) {
                continue;
            }
            if let Some(bridged) = route_matched_bridge(board, &pair, &profile, a, b, options.pours_block) {
                for &id in &bridged {
                    if let Some(mut it) = board.remove(id) {
                        it.locked = true;
                        board.restore(id, it);
                    }
                }
                stats.laid.extend(bridged);
                watch(board, &PairRun { pair: &pair, from: a.mid, to: b.mid, layer: None, centreline: &[], laid: &[] });
            }
        }

        // Any unpaired same-footprint sibling pad within PIN_PAIR_SPAN is bridged locally too.
        let extra = bridge_siblings(board, &pair, &profile, options.pours_block);
        stats.laid.extend(extra);
    }
    tell(&options.log, "pairs", || format!("{} of {} pair runs coupled, {:.1} mm", stats.coupled, stats.runs, stats.coupled_mm), None);
    stats
}

#[derive(Clone)]
struct CoupledCandidate {
    a: PinPair,
    b: PinPair,
    stretches: Vec<(usize, Vec<Point>)>,
    p_left: bool,
    vp: Nm,
    ease_by: Nm,
    score: f64,
}

struct CommittedRun {
    laid: Vec<ItemId>,
    centreline: Vec<Point>,
    coupled_len: f64,
    main_layer: usize,
    stretch_layers: usize,
    paired_vias: usize,
}

/// Whether the end-to-end search, begun at `started`, has used the time `options` gives it.
fn late(options: &PairOptions, started: crate::clock::Instant) -> bool {
    options.max_seconds.is_some_and(|s| started.elapsed().as_secs_f64() >= s)
}

/// Builds the obstacle map and routing grid for the centreline of `pair` between `a` and `b`.
/// Every piece of existing copper on the board (including other runs and jumper vias of the pair's
/// own nets) claims `fat.half_width + clearance` so neither offset track can short to the partner
/// net; only the four endpoint pads themselves are stamped with the tighter margin that lets the
/// centreline start between them.
fn coupled_grid(board: &Board, options: &PairOptions, pair: &Pair, profile: &Profile, a: PinPair, b: PinPair) -> RouteGrid {
    let (pref_vp, min_vp) = via_pitches(board, pair, profile);
    let fat = Profile {
        class: format!("pair {}", board.net_name(pair.p)),
        half_width: pair.width + pair.gap / 2,
        via_radius: min_vp / 2 + profile.via_radius,
        drill_radius: min_vp / 2 + profile.drill_radius,
        ..profile.clone()
    };
    let end_pads = [a.p, a.n, b.p, b.n];
    let mut map = ObstacleMap::without(board, fat.clone(), options.cell, options.pours_block, &|id| {
        end_pads.contains(&id)
    });
    let cells = map.nx * map.ny;
    let reach = map.reach();
    let rules = &board.rules;
    for &pad_id in &end_pads {
        let Some(item) = board.item(pad_id) else { continue };
        let clr = map.clearance_to(board, item.net).max(board.pad_clearances.get(&pad_id).copied().unwrap_or(0));
        let mask_keep = board.mask_openings.get(&pad_id).map_or(0, |(_, by)| by + board::MASK_MARGIN);
        let eff_clr = clr.max(mask_keep);
        if item.is_copper() {
            let own_track = (pair.width / 2 + eff_clr - pair.pitch() / 2).max(0) + reach;
            let mut hit = Vec::new();
            map.cells_near(&item.shape, own_track, &mut |c| hit.push(c));
            for l in (0..map.layers).filter(|&l| item.on(l)) {
                for &c in &hit {
                    map.owner[l * cells + c] = SEVERAL;
                }
            }
            let own_via = (profile.via_radius + eff_clr - min_vp / 2).max(0);
            hit.clear();
            map.cells_near(&item.shape, own_via, &mut |c| hit.push(c));
            for c in hit {
                map.via_owner[c] = SEVERAL;
            }
        }
        if let Some((hc, hr)) = item.hole {
            let hole = Shape::Capsule { a: hc, b: hc, r: hr };
            let through_hole = matches!(item.kind, Kind::Pad { plated: true, .. });
            let hole_clr = if through_hole { nm(rules.min_hole_clearance.max(rules.pth_hole_clearance)) } else { nm(rules.min_hole_clearance) };
            let via_margin = (fat.drill_radius + nm(rules.min_hole_to_hole)).max(fat.via_radius + hole_clr);
            let mut hit = Vec::new();
            map.cells_near(&hole, via_margin, &mut |c| hit.push(c));
            for &c in &hit {
                map.via_owner[c] = SEVERAL;
            }
            let own_hole_track = (pair.width / 2 + hole_clr - pair.pitch() / 2).max(0) + reach;
            hit.clear();
            map.cells_near(&hole, own_hole_track, &mut |c| hit.push(c));
            for l in 0..map.layers {
                for &c in &hit {
                    map.owner[l * cells + c] = SEVERAL;
                }
            }
        }
    }

    let mut grid = RouteGrid {
        net: pair.p,
        x0: map.x0,
        y0: map.y0,
        h: map.h,
        nx: map.nx,
        ny: map.ny,
        layers: (0..map.layers).collect(),
        half_width: fat.half_width,
        via: Some(ViaSpec { radius: fat.via_radius, drill_radius: fat.drill_radius }),
        blocked: map.owner.iter().map(|&o| o != FREE).collect(),
        via_blocked: map.via_owner.iter().map(|&o| o != FREE).collect(),
        penalty: Vec::new(),
        via_penalty: map.via_avoid.clone(),
        region_cost: Vec::new(),
        pours_block: options.pours_block,
        directional: false,
    };
    map.apply_regions(pair.p, &mut grid);
    // Enforce any region rules specific to the negative net without double-counting shared pour costs.
    let mut grid_n = RouteGrid {
        net: pair.n,
        x0: map.x0,
        y0: map.y0,
        h: map.h,
        nx: map.nx,
        ny: map.ny,
        layers: (0..map.layers).collect(),
        half_width: fat.half_width,
        via: grid.via.clone(),
        blocked: vec![false; grid.blocked.len()],
        via_blocked: vec![false; grid.via_blocked.len()],
        penalty: Vec::new(),
        via_penalty: Vec::new(),
        region_cost: Vec::new(),
        pours_block: options.pours_block,
        directional: false,
    };
    map.apply_regions(pair.n, &mut grid_n);
    for (b, &bn) in grid.blocked.iter_mut().zip(&grid_n.blocked) {
        *b |= bn;
    }
    for (vb, &vbn) in grid.via_blocked.iter_mut().zip(&grid_n.via_blocked) {
        *vb |= vbn;
    }
    if !grid_n.region_cost.is_empty() {
        if grid.region_cost.len() != grid_n.region_cost.len() {
            grid.region_cost.resize(grid_n.region_cost.len(), 0);
        }
        for (rc, &rcn) in grid.region_cost.iter_mut().zip(&grid_n.region_cost) {
            *rc = (*rc).max(rcn);
        }
    }
    // Scale soft pour costs down on the coupled centreline grid so a board-wide inner pour layer
    // acts as a gentle tie-breaker rather than forcing pairs onto crowded outer layers.
    for rc in &mut grid.region_cost {
        *rc /= 8;
    }
    if !grid_n.via_penalty.is_empty() {
        if grid.via_penalty.len() != grid_n.via_penalty.len() {
            grid.via_penalty.resize(grid_n.via_penalty.len(), 0);
        }
        for (vp_cell, &vpn) in grid.via_penalty.iter_mut().zip(&grid_n.via_penalty) {
            *vp_cell = (*vp_cell).max(vpn);
        }
    }

    // Softly nudge paired vias away from tight walls where `min_vp` fits but `pref_vp` would rub.
    if pref_vp > min_vp {
        let pref_profile = Profile {
            via_radius: pref_vp / 2 + profile.via_radius,
            drill_radius: pref_vp / 2 + profile.drill_radius,
            ..fat
        };
        let pref_map = ObstacleMap::without(board, pref_profile, options.cell, options.pours_block, &|id| {
            end_pads.contains(&id)
        });
        if grid.via_penalty.len() != cells {
            grid.via_penalty.resize(cells, 0);
        }
        for (c, &vo) in pref_map.via_owner.iter().enumerate() {
            if vo != FREE && !grid.via_blocked[c] {
                grid.via_penalty[c] = grid.via_penalty[c].saturating_add(40);
            }
        }
    }
    grid
}

/// Converts a grid path into single-layer pulled-straight stretches `(board_layer, points)`,
/// collapsing redundant back-to-back layer hops on the same cell and dropping degenerate stubs at
/// either end so `route_pair_legs` handles end-pad layer transitions cleanly.
fn path_stretches(grid: &RouteGrid, path: &GridPath) -> Vec<(usize, Vec<Point>)> {
    if path.len() < 2 {
        return Vec::new();
    }
    let mut collapsed: GridPath = Vec::with_capacity(path.len());
    for &step in path {
        if collapsed.len() >= 2 {
            let prev = collapsed[collapsed.len() - 1];
            let prev2 = collapsed[collapsed.len() - 2];
            if prev.1 == step.1 && prev.2 == step.2 && prev2.1 == step.1 && prev2.2 == step.2 {
                *collapsed.last_mut().expect("len >= 2") = step;
                continue;
            }
        }
        collapsed.push(step);
    }
    let mut raw: Vec<(usize, Vec<Point>)> = Vec::new();
    let mut k = 0;
    while k < collapsed.len() {
        let rl = collapsed[k].0;
        let mut pts = vec![grid.centre(collapsed[k].1, collapsed[k].2)];
        k += 1;
        while k < collapsed.len() && collapsed[k].0 == rl {
            let p = grid.centre(collapsed[k].1, collapsed[k].2);
            if pts.last() != Some(&p) {
                pts.push(p);
            }
            k += 1;
        }
        let straight = pull_straight(grid, rl, &pts);
        raw.push((grid.layers[rl], straight));
    }
    while raw.first().is_some_and(|s| s.1.len() < 2) {
        raw.remove(0);
    }
    while raw.last().is_some_and(|s| s.1.len() < 2) {
        raw.pop();
    }
    if raw.iter().any(|s| s.1.len() < 2) {
        return Vec::new();
    }
    raw
}

/// Searches single-layer and multi-layer coupled centrelines between `a` and `b`, tests laying
/// coupled tracks, paired vias and matched end legs on `board` (rolling back after each trial),
/// and returns the lowest-cost end-to-end candidate.
fn couple_end_to_end(
    board: &mut Board,
    options: &PairOptions,
    started: crate::clock::Instant,
    pair: &Pair,
    profile: &Profile,
    a: PinPair,
    b: PinPair,
) -> Option<CoupledCandidate> {
    let mut grid = coupled_grid(board, options, pair, profile, a, b);
    let all_blocked = grid.blocked.clone();
    let saved_via = grid.via.clone();
    let paired_via_cost = ((VIA_COST / grid.h as f64) * STRAIGHT as f64).round().max(80.0) as u32;
    let (pref_vp, min_vp) = via_pitches(board, pair, profile);
    let vp_tries: Vec<Nm> = if pref_vp > min_vp { vec![pref_vp, min_vp] } else { vec![min_vp] };

    // Ring pairs ordered from closest to the pads outward (`ia + ib`), capped to stay within the
    // span between `a` and `b` so a short pass-through never jumps outside its component.
    let run_dist = a.mid.distance(b.mid);
    let mut ring_pairs: Vec<(usize, usize)> = (0..END_RINGS.len())
        .flat_map(|ia| (0..END_RINGS.len()).map(move |ib| (ia, ib)))
        .filter(|&(ia, ib)| {
            (ia == 0 && ib == 0)
                || (END_RINGS[ia].0 as f64 <= 0.78 * run_dist && END_RINGS[ib].0 as f64 <= 0.78 * run_dist)
        })
        .collect();
    ring_pairs.sort_by_key(|&(ia, ib)| (ia + ib, ia.max(ib), ia));

    let middle_pads: Vec<ItemId> = {
        let mut mp = middle_blocker_pads(board, pair, a, options.pours_block);
        for id in middle_blocker_pads(board, pair, b, options.pours_block) {
            if !mp.contains(&id) {
                mp.push(id);
            }
        }
        mp.retain(|&id| local_cluster(board, id).len() == 1 && middle_pad_can_escape(board, id, options.pours_block));
        mp
    };

    let mut best: Option<CoupledCandidate> = None;
    let late = || late(options, started);

    // 1. Single-layer candidates on every non-plane layer.
    let cells = grid.cell_count();
    grid.via = None;
    for rl in 0..grid.layers.len() {
        let bl = grid.layers[rl];
        if board.planes & (1 << bl) != 0 {
            continue;
        }
        for l in 0..grid.layers.len() {
            let on = l == rl;
            grid.blocked[l * cells..(l + 1) * cells]
                .iter_mut()
                .zip(&all_blocked[l * cells..(l + 1) * cells])
                .for_each(|(c, &o)| *c = o || !on);
        }
        let from_rings: Vec<Vec<usize>> = END_RINGS.iter().map(|&r| ring_cells(&grid, rl, a.mid, r)).collect();
        let to_rings: Vec<Vec<usize>> = END_RINGS.iter().map(|&r| ring_cells(&grid, rl, b.mid, r)).collect();
        let mut found_tier: Option<usize> = None;
        for &(ia, ib) in &ring_pairs {
            if found_tier.is_some_and(|t| ia + ib > t) || late() {
                break;
            }
            if from_rings[ia].is_empty() || to_rings[ib].is_empty() {
                continue;
            }
            let Some(path) = route_astar(&grid, &from_rings[ia], &to_rings[ib], 0) else { continue };
            let stretches = path_stretches(&grid, &path);
            let total_len: f64 = stretches.iter().map(|s| length(&s.1)).sum();
            if stretches.is_empty() || total_len < SHORTEST_COUPLED {
                continue;
            }
            if let Some(cand) = evaluate_stretches(board, &grid, pair, profile, a, b, stretches, &vp_tries, &middle_pads, options.pours_block) {
                found_tier = Some(ia + ib);
                if best.as_ref().is_none_or(|prev| cand.score < prev.score) {
                    best = Some(cand);
                }
            }
        }
    }

    // 2. Multi-layer candidates using paired vias between `a`'s pad layer and `b`'s pad layer.
    grid.blocked.copy_from_slice(&all_blocked);
    grid.via = saved_via;
    if grid.layers.len() >= 2 {
        let non_plane_mask: u64 = (0..board.layers.len())
            .filter(|&l| board.planes & (1 << l) == 0)
            .fold(0u64, |m, l| m | (1 << l));
        let a_mask = if a.layers & non_plane_mask != 0 { a.layers & non_plane_mask } else { non_plane_mask };
        let b_mask = if b.layers & non_plane_mask != 0 { b.layers & non_plane_mask } else { non_plane_mask };
        let from_rings: Vec<Vec<usize>> = END_RINGS.iter().map(|&r| ring_cells_mask(&grid, a_mask, a.mid, r)).collect();
        let to_rings: Vec<Vec<usize>> = END_RINGS.iter().map(|&r| ring_cells_mask(&grid, b_mask, b.mid, r)).collect();
        let mut found_tier: Option<usize> = None;
        for &(ia, ib) in &ring_pairs {
            if found_tier.is_some_and(|t| ia + ib > t) || late() {
                break;
            }
            if from_rings[ia].is_empty() || to_rings[ib].is_empty() {
                continue;
            }
            let Some(path) = route_astar(&grid, &from_rings[ia], &to_rings[ib], paired_via_cost) else { continue };
            let stretches = path_stretches(&grid, &path);
            let total_len: f64 = stretches.iter().map(|s| length(&s.1)).sum();
            if stretches.len() < 2 || total_len < SHORTEST_COUPLED {
                continue;
            }
            if let Some(cand) = evaluate_stretches(board, &grid, pair, profile, a, b, stretches, &vp_tries, &middle_pads, options.pours_block) {
                found_tier = Some(ia + ib);
                if best.as_ref().is_none_or(|prev| cand.score < prev.score) {
                    best = Some(cand);
                }
            }
        }
    }

    best
}

/// Pads of other nets sitting directly between `end.p` and `end.n` on `end.layers` (such as a
/// middle GND or VBUS pin on a 6-pin ESD array) that still have open connections to route.
fn middle_blocker_pads(board: &Board, pair: &Pair, end: PinPair, pours_block: bool) -> Vec<ItemId> {
    let span = centre(board, end.p).distance(centre(board, end.n));
    if span <= pair.pitch() as f64 + 100_000.0 {
        return Vec::new();
    }
    let conn = if pours_block { board::connectivity(board) } else { board::connectivity_without_pours(board) };
    let open_nets: HashSet<u32> = conn.airlines.iter().map(|a| a.net).collect();
    let rad = 0.48 * span;
    board
        .items()
        .filter_map(|(id, it)| match it.kind {
            Kind::Pad { .. }
                if it.net != 0
                    && it.net != pair.p
                    && it.net != pair.n
                    && (it.layers & end.layers) != 0
                    && open_nets.contains(&it.net)
                    && centre(board, id).distance(end.mid) <= rad =>
            {
                Some(id)
            }
            _ => None,
        })
        .collect()
}

/// Unit vector pointing from the pad centroid of `pad_id`'s footprint outward through `pad_id`.
fn pad_outward_dir(board: &Board, pad_id: ItemId) -> Option<(f64, f64)> {
    let fp = pad_footprint(board, pad_id)?;
    let mc = centre(board, pad_id);
    let mut sx = 0i64;
    let mut sy = 0i64;
    let mut n = 0i64;
    for (id, it) in board.items() {
        if matches!(it.kind, Kind::Pad { .. }) && pad_footprint(board, id) == Some(fp) {
            let c = centre(board, id);
            sx += c.x as i64;
            sy += c.y as i64;
            n += 1;
        }
    }
    if n < 2 {
        return None;
    }
    let fp_mid = Point::new((sx / n) as Nm, (sy / n) as Nm);
    (fp_mid.distance(mc) > 50_000.0).then(|| unit(fp_mid, mc))
}

/// Whether `pad_id` (a middle pad between two spread pins of a pair) still has an unobstructed
/// outward escape corridor on its layer or through a nearby outward dogbone via after a candidate
/// pair run is laid.
fn middle_pad_can_escape(board: &mut Board, pad_id: ItemId, pours_block: bool) -> bool {
    let Some(item) = board.item(pad_id).cloned() else { return true };
    let mc = centre(board, pad_id);
    let prof = Profile::for_net(board, item.net);
    let r = prof.neck_into(Some(&item)).min(prof.half_width);
    let all_layers_mask = (1u64 << board.layers.len()) - 1;
    let planes = board.planes;
    let out_dirs: Vec<(f64, f64)> = if let Some(u) = pad_outward_dir(board, pad_id) {
        vec![u]
    } else {
        vec![(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)]
    };
    for layer in (0..board.layers.len()).filter(|&l| item.on(l) && (planes & (1 << l) == 0)) {
        for &(ux, uy) in &out_dirs {
            let (px, py) = (-uy, ux);
            for lat in [0.0, -100_000.0, 100_000.0] {
                let far = Point::new(
                    mc.x + (ux * 1_500_000.0 + px * lat).round() as Nm,
                    mc.y + (uy * 1_500_000.0 + py * lat).round() as Nm,
                );
                let tid = board.insert(track(item.net, layer, mc, far, r));
                let ok_track = legal_pair_item(board, tid, pours_block);
                board.discard(tid);
                if ok_track {
                    return true;
                }
                for step_nm in [400_000.0, 580_000.0, 780_000.0, 1_000_000.0] {
                    let vpt = Point::new(
                        mc.x + (ux * step_nm + px * lat).round() as Nm,
                        mc.y + (uy * step_nm + py * lat).round() as Nm,
                    );
                    let d_track = board.insert(track(item.net, layer, mc, vpt, r));
                    let d_via = board.insert(Item {
                        kind: Kind::Via,
                        net: item.net,
                        layers: all_layers_mask,
                        shape: Shape::Capsule { a: vpt, b: vpt, r: prof.via_radius },
                        hole: Some((vpt, prof.drill_radius)),
                        locked: false,
                        span: None,
                    });
                    let ok_dogbone = legal_pair_item(board, d_track, pours_block)
                        && legal_pair_item(board, d_via, pours_block);
                    let mut ok_inner = false;
                    if ok_dogbone {
                        for inner_l in (0..board.layers.len()).filter(|&l| l != layer && (planes & (1 << l) == 0)) {
                            for &(ix, iy) in &[(ux, uy), (px, py), (-px, -py)] {
                                let ifar = Point::new(
                                    vpt.x + (ix * 1_100_000.0).round() as Nm,
                                    vpt.y + (iy * 1_100_000.0).round() as Nm,
                                );
                                let itid = board.insert(track(item.net, inner_l, vpt, ifar, r));
                                let ok = legal_pair_item(board, itid, pours_block);
                                board.discard(itid);
                                if ok {
                                    ok_inner = true;
                                    break;
                                }
                            }
                            if ok_inner {
                                break;
                            }
                        }
                    }
                    board.discard(d_via);
                    board.discard(d_track);
                    if ok_dogbone && ok_inner {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Tries laying `stretches` with both initial polarities, via spacings and corner easings, plus
/// matched end legs at `a` and `b`, rolling everything back and returning the best valid spec.
#[allow(clippy::too_many_arguments)]
fn evaluate_stretches(
    board: &mut Board,
    grid: &RouteGrid,
    pair: &Pair,
    profile: &Profile,
    a: PinPair,
    b: PinPair,
    stretches: Vec<(usize, Vec<Point>)>,
    vp_tries: &[Nm],
    middle_pads: &[ItemId],
    pours_block: bool,
) -> Option<CoupledCandidate> {
    let first_line = &stretches.first()?.1;
    let pref_p_left = side(first_line, centre(board, a.p)) >= side(first_line, centre(board, a.n));
    let ease_tries = [pair.pitch(), pair.pitch() / 2, 0];
    let vps: &[Nm] = if stretches.len() >= 2 { vp_tries } else { &vp_tries[..1] };

    // Region and pour penalty integrated along all coupled centrelines.
    let mut region_pen = 0.0;
    for &(bl, ref pts) in &stretches {
        if let Some(rl) = grid.layers.iter().position(|&l| l == bl) {
            for w in pts.windows(2) {
                region_pen += line_region_cost(grid, rl, w[0], w[1]);
            }
        }
    }
    let coupled_len: f64 = stretches.iter().map(|s| length(&s.1)).sum();

    let mut best: Option<CoupledCandidate> = None;
    for &vp in vps {
        for p_left in [pref_p_left, !pref_p_left] {
            for ease_by in ease_tries {
                let Some(laid_body) = lay_coupled_stretches(board, pair, profile, &stretches, p_left, vp, ease_by, pours_block) else {
                    continue;
                };
                let legs_a = route_pair_legs(board, pair, profile, a, laid_body.start_p, laid_body.start_n, laid_body.start_layer, pours_block);
                let Some((ids_a, len_ap, len_an, vias_a)) = legs_a else {
                    discard_all(board, laid_body.items);
                    continue;
                };
                let legs_b = route_pair_legs(board, pair, profile, b, laid_body.end_p, laid_body.end_n, laid_body.end_layer, pours_block);
                let Some((ids_b, len_bp, len_bn, vias_b)) = legs_b else {
                    discard_all(board, ids_a);
                    discard_all(board, laid_body.items);
                    continue;
                };
                // Multi-layer coupled runs already change layers through paired vias and must start
                // and end directly on the pad layers without stacking extra uncoupled leg vias.
                let clean_multi = stretches.len() == 1 || (vias_a == 0 && vias_b == 0);
                let connected = clean_multi
                    && net_reaches(board, a.p, b.p, pours_block)
                    && net_reaches(board, a.n, b.n, pours_block)
                    && middle_pads.iter().all(|&mp| middle_pad_can_escape(board, mp, pours_block));
                let body_vias = items_via_count(board, &laid_body.items);
                discard_all(board, ids_b);
                discard_all(board, ids_a);
                discard_all(board, laid_body.items);
                if !connected {
                    continue;
                }
                let leg_vias = (vias_a + vias_b) as f64;
                let leg_p = len_ap + len_bp;
                let leg_n = len_an + len_bn;
                let avg_leg = 0.5 * (leg_p + leg_n);
                let skew = (leg_p - leg_n).abs();
                let vp_pen = if vp < vp_tries[0] { 250_000.0 } else { 0.0 };
                let transition_pen = (stretches.len().saturating_sub(1) as f64) * 350_000.0;
                let score = coupled_len
                    + 0.4 * region_pen
                    + body_vias as f64 * PER_VIA_COST
                    + leg_vias * PER_LEG_VIA_COST
                    + transition_pen
                    + 2.5 * avg_leg
                    + 3.0 * skew
                    + vp_pen;
                if best.as_ref().is_none_or(|prev| score < prev.score) {
                    best = Some(CoupledCandidate {
                        a,
                        b,
                        stretches: stretches.clone(),
                        p_left,
                        vp,
                        ease_by,
                        score,
                    });
                }
                // Once the sharpest corner-easing at this polarity and via pitch works, looser
                // corner-easings of the exact same centreline will not improve it.
                break;
            }
        }
        if best.is_some() {
            break;
        }
    }
    best
}

/// Re-lays the winning candidate on `board`, marks every track and via locked so subsequent
/// single-ended stages route around the coupled pair, and returns its summary for `PairStats`.
fn commit_candidate(
    board: &mut Board,
    options: &PairOptions,
    pair: &Pair,
    profile: &Profile,
    cand: CoupledCandidate,
) -> Option<CommittedRun> {
    let body = lay_coupled_stretches(board, pair, profile, &cand.stretches, cand.p_left, cand.vp, cand.ease_by, options.pours_block)?;
    let Some((ids_a, _, _, _)) = route_pair_legs(board, pair, profile, cand.a, body.start_p, body.start_n, body.start_layer, options.pours_block) else {
        discard_all(board, body.items);
        return None;
    };
    let Some((ids_b, _, _, _)) = route_pair_legs(board, pair, profile, cand.b, body.end_p, body.end_n, body.end_layer, options.pours_block) else {
        discard_all(board, ids_a);
        discard_all(board, body.items);
        return None;
    };
    if !net_reaches(board, cand.a.p, cand.b.p, options.pours_block) || !net_reaches(board, cand.a.n, cand.b.n, options.pours_block) {
        discard_all(board, ids_b);
        discard_all(board, ids_a);
        discard_all(board, body.items);
        return None;
    }
    let paired_vias = items_via_count(board, &body.items) / 2;
    let mut all_items = body.items;
    all_items.extend(ids_a);
    all_items.extend(ids_b);
    for &pad_id in &[cand.a.p, cand.a.n, cand.b.p, cand.b.n] {
        let pad_pt = centre(board, pad_id);
        // Only the router's own escapes (no span in the file): the board's own copper is not the
        // router's to lock, and anything handed back as laid is written out again as new copper.
        for member in local_cluster(board, pad_id) {
            if member != pad_id
                && !all_items.contains(&member)
                && board.item(member).is_some_and(|it| !matches!(it.kind, Kind::Pad { .. }) && it.span.is_none())
                && centre(board, member).distance(pad_pt) <= CLUSTER_REACH as f64
            {
                all_items.push(member);
            }
        }
    }
    for &id in &all_items {
        if let Some(mut it) = board.remove(id) {
            it.locked = true;
            board.restore(id, it);
        }
    }
    let main_layer = cand
        .stretches
        .iter()
        .max_by(|x, y| length(&x.1).total_cmp(&length(&y.1)))
        .map_or(0, |s| s.0);
    let mut centreline = Vec::new();
    for (_, pts) in &cand.stretches {
        if centreline.is_empty() {
            centreline.extend_from_slice(pts);
        } else {
            centreline.extend_from_slice(&pts[1..]);
        }
    }
    let coupled_len: f64 = cand.stretches.iter().map(|s| length(&s.1)).sum();
    Some(CommittedRun {
        laid: all_items,
        centreline,
        coupled_len,
        main_layer,
        stretch_layers: cand.stretches.len(),
        paired_vias,
    })
}

struct LaidStretches {
    items: Vec<ItemId>,
    start_p: Point,
    start_n: Point,
    start_layer: usize,
    end_p: Point,
    end_n: Point,
    end_layer: usize,
}

/// Trims `dist` from the start of `pts` along its segments.
fn trim_start(pts: &[Point], mut dist: f64) -> Option<Vec<Point>> {
    if pts.len() < 2 {
        return None;
    }
    if dist <= 0.0 {
        return Some(pts.to_vec());
    }
    for k in 0..pts.len() - 1 {
        let seg = pts[k].distance(pts[k + 1]);
        if dist < seg - 1_000.0 {
            let t = dist / seg;
            let p = Point::new(
                pts[k].x + ((pts[k + 1].x - pts[k].x) as f64 * t).round() as Nm,
                pts[k].y + ((pts[k + 1].y - pts[k].y) as f64 * t).round() as Nm,
            );
            let mut out = vec![p];
            out.extend_from_slice(&pts[k + 1..]);
            return (out.len() >= 2).then_some(out);
        }
        dist -= seg;
    }
    None
}

/// Trims `dist` from the end of `pts` along its segments.
fn trim_end(pts: &[Point], dist: f64) -> Option<Vec<Point>> {
    let mut rev: Vec<Point> = pts.iter().copied().rev().collect();
    rev = trim_start(&rev, dist)?;
    rev.reverse();
    Some(rev)
}

/// Lays one or more coupled stretches and the paired vias joining consecutive stretches. Across
/// each paired via, `p_left` stays unchanged on forward turns and flips cleanly on foldbacks
/// (`dot(n_in, n_out) < 0`), so neither track crosses its partner on either layer.
#[allow(clippy::too_many_arguments)]
fn lay_coupled_stretches(
    board: &mut Board,
    pair: &Pair,
    profile: &Profile,
    stretches: &[(usize, Vec<Point>)],
    initial_p_left: bool,
    vp: Nm,
    ease_by: Nm,
    pours_block: bool,
) -> Option<LaidStretches> {
    if stretches.is_empty() {
        return None;
    }
    let clr = board.clearance(pair.p, pair.n);
    let mask = if board.via_openings.0 != 0 { board.via_openings.1 + board::MASK_MARGIN } else { 0 };
    let via_clr = clr.max(mask);
    let need = (profile.via_radius + pair.width / 2 + via_clr) as f64;
    let base = (vp + pair.pitch()) as f64 / 2.0;
    let min_lead = if need > base { (need * need - base * base).sqrt().ceil() as Nm } else { 0 };
    let lead = min_lead.max((vp - pair.pitch()).max(0) / 2 + profile.via_radius).max(pair.pitch() / 2);

    let all_layers_mask = (1u64 << board.layers.len()) - 1;
    let vp_half = vp as f64 / 2.0;
    let half_pitch = pair.pitch() as f64 / 2.0;
    let s2 = std::f64::consts::FRAC_1_SQRT_2;
    let compass: [(f64, f64); 8] = [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0), (s2, s2), (-s2, s2), (s2, -s2), (-s2, -s2)];

    let mut laid: Vec<ItemId> = Vec::new();
    let mut via_pairs: Vec<(Point, Point)> = Vec::with_capacity(stretches.len().saturating_sub(1));
    let mut polarities: Vec<bool> = Vec::with_capacity(stretches.len());
    polarities.push(initial_p_left);

    for t in 0..stretches.len() - 1 {
        let pts_in = &stretches[t].1;
        let pts_out = &stretches[t + 1].1;
        let v = *pts_in.last()?;
        let a_in = pts_in[pts_in.len().checked_sub(2)?];
        let b_out = *pts_out.get(1)?;
        let (u_in, u_out) = (unit(a_in, v), unit(v, b_out));
        let (n_in, n_out) = ((-u_in.1, u_in.0), (-u_out.1, u_out.0));
        let cur_p_left = polarities[t];
        let p_side_in = if cur_p_left { n_in } else { (-n_in.0, -n_in.1) };
        let default_next = if n_in.0 * n_out.0 + n_in.1 * n_out.1 >= 0.0 { cur_p_left } else { !cur_p_left };
        let p_side_out_def = if default_next { n_out } else { (-n_out.0, -n_out.1) };
        let (sx, sy) = (p_side_in.0 + p_side_out_def.0, p_side_in.1 + p_side_out_def.1);
        let sl = sx.hypot(sy);
        let ideal_dir = if sl > 1e-6 { (sx / sl, sy / sl) } else { p_side_in };

        let mut dir_cands: Vec<((f64, f64), bool)> = vec![(ideal_dir, default_next)];
        let mut extras: Vec<((f64, f64), bool, f64)> = Vec::new();
        for d in compass {
            if d.0 * p_side_in.0 + d.1 * p_side_in.1 <= 0.25 {
                continue;
            }
            let next_pol = d.0 * n_out.0 + d.1 * n_out.1 >= 0.0;
            let pso = if next_pol { n_out } else { (-n_out.0, -n_out.1) };
            if d.0 * pso.0 + d.1 * pso.1 <= 0.25 {
                continue;
            }
            extras.push((d, next_pol, d.0 * ideal_dir.0 + d.1 * ideal_dir.1));
        }
        extras.sort_by(|x, y| y.2.total_cmp(&x.2));
        for (d, np, _) in extras {
            dir_cands.push((d, np));
        }

        let mut chosen_pair = None;
        for (p_dir, next_pol) in dir_cands {
            let vp_pt = Point::new(v.x + (p_dir.0 * vp_half).round() as Nm, v.y + (p_dir.1 * vp_half).round() as Nm);
            let vn_pt = Point::new(v.x - (p_dir.0 * vp_half).round() as Nm, v.y - (p_dir.1 * vp_half).round() as Nm);
            let id_p = board.insert(Item {
                kind: Kind::Via,
                net: pair.p,
                layers: all_layers_mask,
                shape: Shape::Capsule { a: vp_pt, b: vp_pt, r: profile.via_radius },
                hole: Some((vp_pt, profile.drill_radius)),
                locked: false,
                span: None,
            });
            let id_n = board.insert(Item {
                kind: Kind::Via,
                net: pair.n,
                layers: all_layers_mask,
                shape: Shape::Capsule { a: vn_pt, b: vn_pt, r: profile.via_radius },
                hole: Some((vn_pt, profile.drill_radius)),
                locked: false,
                span: None,
            });
            if legal_pair_item(board, id_p, pours_block) && legal_pair_item(board, id_n, pours_block) {
                laid.push(id_p);
                laid.push(id_n);
                polarities.push(next_pol);
                chosen_pair = Some((vp_pt, vn_pt));
                break;
            }
            board.discard(id_n);
            board.discard(id_p);
        }
        let Some(pair_pts) = chosen_pair else {
            discard_all(board, laid);
            return None;
        };
        via_pairs.push(pair_pts);
    }

    let mut start_ends = None;
    let mut end_ends = None;
    let last_idx = stretches.len() - 1;

    for (s, &(layer, ref pts)) in stretches.iter().enumerate() {
        let total = length(pts);
        let has_start_via = s > 0;
        let has_end_via = s < last_idx;
        let max_share = if has_start_via && has_end_via { 0.42 } else { 0.72 };
        let lead_s = if has_start_via { (lead as f64).min(total * max_share) } else { 0.0 };
        let lead_e = if has_end_via { (lead as f64).min(total * max_share) } else { 0.0 };

        let Some(after_start) = trim_start(pts, lead_s) else {
            discard_all(board, laid);
            return None;
        };
        let Some(trimmed) = trim_end(&after_start, lead_e) else {
            discard_all(board, laid);
            return None;
        };
        if trimmed.len() < 2 || trimmed[0].distance(*trimmed.last().expect("len >= 2")) < 25_000.0 {
            discard_all(board, laid);
            return None;
        }
        let body = ease(&trimmed, ease_by);
        let pol = polarities[s];
        let (mut p_line, mut n_line) = if pol {
            (offset(&body, half_pitch), offset(&body, -half_pitch))
        } else {
            (offset(&body, -half_pitch), offset(&body, half_pitch))
        };
        if has_start_via {
            let (vp_pt, vn_pt) = via_pairs[s - 1];
            p_line.insert(0, vp_pt);
            n_line.insert(0, vn_pt);
        }
        if has_end_via {
            let (vp_pt, vn_pt) = via_pairs[s];
            p_line.push(vp_pt);
            n_line.push(vn_pt);
        }
        if s == 0 {
            start_ends = Some((p_line[0], n_line[0], layer));
        }
        if s == last_idx {
            end_ends = Some((*p_line.last().expect("non-empty"), *n_line.last().expect("non-empty"), layer));
        }
        if !lay_line(board, &p_line, pair.p, layer, pair.width / 2, pours_block, &mut laid)
            || !lay_line(board, &n_line, pair.n, layer, pair.width / 2, pours_block, &mut laid)
        {
            discard_all(board, laid);
            return None;
        }
    }

    let (start_p, start_n, start_layer) = start_ends?;
    let (end_p, end_n, end_layer) = end_ends?;
    Some(LaidStretches { items: laid, start_p, start_n, start_layer, end_p, end_n, end_layer })
}

/// Candidate 45-degree chamfered and direct paths from `from` to `to`, trying the straight-out
/// elbow first so spread pads leave the bay between them open before converging into the pair.
fn chamfer_paths(from: Point, to: Point) -> Vec<Vec<Point>> {
    if from == to {
        return vec![vec![from]];
    }
    let mut out = Vec::new();
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    let d = dx.abs().min(dy.abs());
    if d > 0 && dx.abs() != dy.abs() {
        let elbow1 = Point::new(from.x + d * dx.signum(), from.y + d * dy.signum());
        let elbow2 = Point::new(to.x - d * dx.signum(), to.y - d * dy.signum());
        out.push(vec![from, elbow2, to]);
        if elbow1 != elbow2 {
            out.push(vec![from, elbow1, to]);
        }
    }
    out.push(vec![from, to]);
    out
}

/// Routes matched legs from `end.p` and `end.n` to `(entry_p, entry_n)` on `entry_layer`.
/// Same-layer direct and 45-degree chamfered legs at `pair.width` are tried first; when a layer
/// change or obstacle detour is needed, a fine local `25 um` window routes both legs (trying zero
/// vias before one via, and both net orders) and pulls them tight.
#[allow(clippy::too_many_arguments)]
fn route_pair_legs(
    board: &mut Board,
    pair: &Pair,
    profile: &Profile,
    end: PinPair,
    entry_p: Point,
    entry_n: Point,
    entry_layer: usize,
    pours_block: bool,
) -> Option<(Vec<ItemId>, f64, f64, usize)> {
    let (pc, nc) = (centre(board, end.p), centre(board, end.n));
    let max_leg = pc.distance(entry_p).max(nc.distance(entry_n)) * 2.2 + 1_100_000.0;
    let r_full = pair.width / 2;
    let r_neck = profile
        .neck_into(board.item(end.p))
        .min(profile.neck_into(board.item(end.n)))
        .min(r_full);
    let radii: Vec<Nm> = if r_neck < r_full { vec![r_full, r_neck] } else { vec![r_full] };

    // Tier 1: direct or symmetric 45-degree chamfer on `entry_layer` when both pads sit on it.
    if end.layers & (1 << entry_layer) != 0 {
        let p_paths = chamfer_paths(pc, entry_p);
        let n_paths = chamfer_paths(nc, entry_n);
        for &r in &radii {
            let mut best_t1: Option<(f64, Vec<Point>, Vec<Point>)> = None;
            for lp in &p_paths {
                for ln in &n_paths {
                    let (len_p, len_n) = (length(lp), length(ln));
                    if len_p > max_leg || len_n > max_leg || (len_p - len_n).abs() > 1_200_000.0 {
                        continue;
                    }
                    let mut ids = Vec::new();
                    let ok = lay_line(board, lp, pair.p, entry_layer, r, pours_block, &mut ids)
                        && lay_line(board, ln, pair.n, entry_layer, r, pours_block, &mut ids);
                    discard_all(board, ids);
                    if ok {
                        let cost = len_p + len_n + 3.0 * (len_p - len_n).abs();
                        if best_t1.as_ref().is_none_or(|b| cost < b.0) {
                            best_t1 = Some((cost, lp.clone(), ln.clone()));
                        }
                    }
                }
            }
            if let Some((_, lp, ln)) = best_t1 {
                let mut ids = Vec::new();
                if lay_line(board, &lp, pair.p, entry_layer, r, pours_block, &mut ids)
                    && lay_line(board, &ln, pair.n, entry_layer, r, pours_block, &mut ids)
                {
                    return Some((ids, length(&lp), length(&ln), 0));
                }
                discard_all(board, ids);
            }
        }
    }

    // Tier 2: fine local window A* (0 vias tried first so existing inner-layer escapes and vias of
    // the pad's cluster are reused without redundant outer-layer wrap-arounds, else 1+ vias).
    let bounds = BBox::of([pc, nc, entry_p, entry_n]).grow(LEG_WINDOW);
    let allow_vias_modes: &[bool] = &[false, true];
    // The window's map is built once for each width: every try below lays its legs and takes
    // them up again, so the board under the window is the same each time, and what a try lays
    // (the first leg, the middle pads' reserved escapes) is stamped onto a copy.
    let mut bases: Vec<Option<ObstacleMap>> = vec![None; radii.len()];

    for &allow_vias in allow_vias_modes {
        for (ri, &r) in radii.iter().enumerate() {
            let leg_prof = Profile { half_width: r, ..profile.clone() };
            let base: &ObstacleMap = bases[ri].get_or_insert_with(|| ObstacleMap::window(board, leg_prof.clone(), LEG_CELL, bounds, pours_block));
            let mut best_order: Option<(usize, f64, bool)> = None;
            for p_first in [true, false] {
                let Some((ids, lp, ln, vias)) = try_window_leg_pair(
                    board,
                    pair,
                    &leg_prof,
                    base,
                    end,
                    entry_p,
                    entry_n,
                    entry_layer,
                    allow_vias,
                    p_first,
                    pours_block,
                ) else {
                    continue;
                };
                discard_all(board, ids);
                if lp > max_leg || ln > max_leg {
                    continue;
                }
                let cost = lp + len_skew_cost(lp, ln);
                if best_order.is_none_or(|(bv, bc, _)| (vias, cost) < (bv, bc)) {
                    best_order = Some((vias, cost, p_first));
                }
            }
            if let Some((_, _, p_first)) = best_order {
                if let Some(res) = try_window_leg_pair(
                    board,
                    pair,
                    &leg_prof,
                    base,
                    end,
                    entry_p,
                    entry_n,
                    entry_layer,
                    allow_vias,
                    p_first,
                    pours_block,
                ) {
                    return Some(res);
                }
            }
        }
    }
    None
}

fn len_skew_cost(lp: f64, ln: f64) -> f64 {
    ln + 3.0 * (lp - ln).abs()
}

/// Temporarily inserts outward escape tracks or dogbone vias for any middle blocker pads of `end`
/// so window leg routing keeps their escape corridor clear; the caller discards the returned items.
fn reserve_middle_pad_escapes(
    board: &mut Board,
    pair: &Pair,
    leg_prof: &Profile,
    end: PinPair,
    entry_layer: usize,
    pours_block: bool,
) -> Vec<ItemId> {
    let span = centre(board, end.p).distance(centre(board, end.n));
    let half_span = (span * 0.5) as Nm;
    let all_layers_mask = (1u64 << board.layers.len()) - 1;
    let planes = board.planes;
    let mut reserved = Vec::new();
    for mp in middle_blocker_pads(board, pair, end, pours_block) {
        if local_cluster(board, mp).len() > 1 {
            continue;
        }
        let Some(mp_item) = board.item(mp).cloned() else { continue };
        let Some((ux, uy)) = pad_outward_dir(board, mp) else { continue };
        let mc = centre(board, mp);
        let mp_prof = Profile::for_net(board, mp_item.net);
        let mp_r = mp_prof.neck_into(Some(&mp_item)).min(mp_prof.half_width);
        let clr = board.clearance(mp_item.net, pair.p).max(board.clearance(mp_item.net, pair.n));
        for layer in (0..board.layers.len()).filter(|&l| mp_item.on(l) && (planes & (1 << l) == 0)) {
            let mut placed_track = false;
            if layer != entry_layer && mp_r + clr + leg_prof.half_width + 20_000 <= half_span {
                let b = Point::new(mc.x + (ux * 1_450_000.0).round() as Nm, mc.y + (uy * 1_450_000.0).round() as Nm);
                let tid = board.insert(track(mp_item.net, layer, mc, b, mp_r));
                if legal_pair_item(board, tid, pours_block) {
                    reserved.push(tid);
                    placed_track = true;
                } else {
                    board.discard(tid);
                }
            }
            if !placed_track && mp_prof.via_radius + clr + leg_prof.half_width + 20_000 <= half_span {
                let vpt = Point::new(mc.x + (ux * 410_000.0).round() as Nm, mc.y + (uy * 410_000.0).round() as Nm);
                let tid = board.insert(track(mp_item.net, layer, mc, vpt, mp_r));
                let vid = board.insert(Item {
                    kind: Kind::Via,
                    net: mp_item.net,
                    layers: all_layers_mask,
                    shape: Shape::Capsule { a: vpt, b: vpt, r: mp_prof.via_radius },
                    hole: Some((vpt, mp_prof.drill_radius)),
                    locked: false,
                    span: None,
                });
                if legal_pair_item(board, tid, pours_block) && legal_pair_item(board, vid, pours_block) {
                    reserved.push(tid);
                    reserved.push(vid);
                } else {
                    board.discard(vid);
                    board.discard(tid);
                }
            }
        }
    }
    reserved
}

#[allow(clippy::too_many_arguments)]
fn try_window_leg_pair(
    board: &mut Board,
    pair: &Pair,
    leg_prof: &Profile,
    base: &ObstacleMap,
    end: PinPair,
    entry_p: Point,
    entry_n: Point,
    entry_layer: usize,
    allow_vias: bool,
    p_first: bool,
    pours_block: bool,
) -> Option<(Vec<ItemId>, f64, f64, usize)> {
    let reserved = reserve_middle_pad_escapes(board, pair, leg_prof, end, entry_layer, pours_block);
    let has_reserved = !reserved.is_empty();
    let mut map = base.clone();
    for &id in &reserved {
        map.stamp(board, id);
    }
    let res = try_window_leg_pair_inner(
        board,
        pair,
        leg_prof,
        &map,
        end,
        entry_p,
        entry_n,
        entry_layer,
        allow_vias,
        p_first,
        pours_block,
    );
    discard_all(board, reserved);
    if res.is_some() || !has_reserved {
        return res;
    }
    try_window_leg_pair_inner(
        board,
        pair,
        leg_prof,
        base,
        end,
        entry_p,
        entry_n,
        entry_layer,
        allow_vias,
        p_first,
        pours_block,
    )
}

#[allow(clippy::too_many_arguments)]
fn try_window_leg_pair_inner(
    board: &mut Board,
    pair: &Pair,
    leg_prof: &Profile,
    map: &ObstacleMap,
    end: PinPair,
    entry_p: Point,
    entry_n: Point,
    entry_layer: usize,
    allow_vias: bool,
    p_first: bool,
    pours_block: bool,
) -> Option<(Vec<ItemId>, f64, f64, usize)> {
    let first = if p_first { (pair.p, end.p, entry_p) } else { (pair.n, end.n, entry_n) };
    let second = if p_first { (pair.n, end.n, entry_n) } else { (pair.p, end.p, entry_p) };

    let mut ids1 = route_single_leg_to_point(board, map, first.0, first.1, first.2, entry_layer, allow_vias, pours_block)?;
    tighten_leg_items(board, &mut ids1, pours_block);
    // The partner leg routes round the first.
    let mut map2 = map.clone();
    for &id in &ids1 {
        map2.stamp(board, id);
    }
    // Where the first leg placed a via, nudge the partner leg's via outside `diff_pair_via_gap`
    // whenever room exists, falling back cleanly if the package escape is tighter.
    if allow_vias {
        let (pref_vp, _) = via_pitches(board, pair, leg_prof);
        let avoid_r = (pref_vp - leg_prof.via_radius).max(0);
        if avoid_r > 0 {
            let v1_pts: Vec<Point> = ids1
                .iter()
                .filter_map(|&id| match board.item(id).map(|it| (&it.kind, &it.shape)) {
                    Some((Kind::Via, Shape::Capsule { a, .. })) => Some(*a),
                    _ => None,
                })
                .collect();
            for pt in v1_pts {
                map2.add_via_avoid(board, &Shape::Capsule { a: pt, b: pt, r: avoid_r });
            }
        }
    }
    let res2 = route_single_leg_to_point(board, &map2, second.0, second.1, second.2, entry_layer, allow_vias, pours_block);
    let Some(mut ids2) = res2 else {
        discard_all(board, ids1);
        return None;
    };
    tighten_leg_items(board, &mut ids2, pours_block);

    let (l1, l2) = (items_track_len(board, &ids1), items_track_len(board, &ids2));
    let vias = items_via_count(board, &ids1) + items_via_count(board, &ids2);
    let (lp, ln) = if p_first { (l1, l2) } else { (l2, l1) };
    ids1.extend(ids2);
    Some((ids1, lp, ln, vias))
}

/// Routes a single leg on a local `LEG_CELL` window from `pad_id` (falling back to its local
/// cluster only if the pad itself is unreachable) to the exact point `entry_pt` on `entry_layer`.
#[allow(clippy::too_many_arguments)]
fn route_single_leg_to_point(
    board: &mut Board,
    map: &ObstacleMap,
    net: u32,
    pad_id: ItemId,
    entry_pt: Point,
    entry_layer: usize,
    allow_vias: bool,
    pours_block: bool,
) -> Option<Vec<ItemId>> {
    if board.item(pad_id).is_some_and(|it| it.on(entry_layer)) && centre(board, pad_id) == entry_pt {
        return Some(Vec::new());
    }
    let mut grid = map.view(net);
    if !allow_vias {
        grid.via = None;
    }
    let targets = point_terminals(board, &grid, net, entry_pt, entry_layer, pours_block);
    if targets.is_empty() {
        return None;
    }
    let target_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
    let via_cost = 120;

    // Prefer terminating directly on `pad_id` so pre-jumpered connector vias farther away do not
    // skew the leg length; fall back only to nearby non-pad escape items of `pad_id`'s cluster.
    let direct_sources = pad_only_terminals(board, &grid, pad_id, pours_block);
    if !direct_sources.is_empty() {
        let src_cells: Vec<usize> = direct_sources.iter().map(|t| t.0).collect();
        if let Some(path) = route_astar(&grid, &src_cells, &target_cells, via_cost) {
            if let Some(laid) = lay_leg_pieces(board, &grid, &path, &direct_sources, &targets, pours_block) {
                return Some(laid);
            }
        }
    }
    let pad_pt = centre(board, pad_id);
    let mut cluster_sources = Vec::new();
    for member in local_cluster(board, pad_id) {
        if member != pad_id
            && board.item(member).is_some_and(|it| !matches!(it.kind, Kind::Pad { .. }))
            && centre(board, member).distance(pad_pt) <= CLUSTER_REACH as f64
        {
            cluster_sources.extend(pad_only_terminals(board, &grid, member, pours_block));
        }
    }
    cluster_sources.extend(direct_sources);
    if cluster_sources.is_empty() {
        return None;
    }
    let src_cells: Vec<usize> = cluster_sources.iter().map(|t| t.0).collect();
    let path = route_astar(&grid, &src_cells, &target_cells, via_cost)?;
    lay_leg_pieces(board, &grid, &path, &cluster_sources, &targets, pours_block)
}

/// Terminal cells on `grid` reaching the exact coordinate `pt` on `board_layer`.
fn point_terminals(board: &mut Board, grid: &RouteGrid, net: u32, pt: Point, board_layer: usize, pours_block: bool) -> Vec<Terminal> {
    let Some(rl) = grid.layers.iter().position(|&l| l == board_layer) else { return Vec::new() };
    let Some((ci, cj)) = grid.cell_at(pt) else { return Vec::new() };
    let mut out = Vec::new();
    for j in cj.saturating_sub(2)..=(cj + 2).min(grid.ny - 1) {
        for i in ci.saturating_sub(2)..=(ci + 2).min(grid.nx - 1) {
            let idx = grid.index(rl, i, j);
            if grid.blocked[idx] {
                continue;
            }
            let c = grid.centre(i, j);
            if c == pt {
                out.push((idx, None));
            } else {
                let id = board.insert(track(net, board_layer, c, pt, grid.half_width));
                let ok = legal_pair_item(board, id, pours_block);
                board.discard(id);
                if ok {
                    out.push((idx, Some((pt, grid.half_width))));
                }
            }
        }
    }
    out
}

/// Terminal cells on `grid` reaching `id` (checked with `legal_pair_item` so block pours do not
/// refuse a valid pad leg), necking at the pad only if `grid.half_width` cannot leave it.
fn pad_only_terminals(board: &mut Board, grid: &RouteGrid, id: ItemId, pours_block: bool) -> Vec<Terminal> {
    let Some(item) = board.item(id).cloned() else { return Vec::new() };
    let narrow = Profile::for_net(board, grid.net).neck_into(Some(&item)).min(grid.half_width);
    let tries: &[(Nm, Nm, usize)] = if narrow < grid.half_width {
        &[(grid.half_width, 4, 20), (narrow, 16, 32)]
    } else {
        &[(grid.half_width, 4, 20)]
    };
    let bb = item.shape.bbox();
    let anchors: Vec<Point> = match (&item.kind, &item.shape) {
        (Kind::Track, Shape::Capsule { a, b, .. }) if a != b => vec![*a, *b],
        _ => vec![Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2)],
    };
    for &(hw, leg_cells, cap) in tries {
        let mut out: Vec<Terminal> = Vec::new();
        for (rl, &bl) in grid.layers.iter().enumerate().filter(|(_, &bl)| item.on(bl)) {
            if board.planes & (1 << bl) != 0 {
                continue;
            }
            for &from in &anchors {
                let Some(c) = grid.cell_at(from) else { continue };
                let extent = match &item.kind {
                    Kind::Track => 0,
                    _ => (bb.max.x - bb.min.x).max(bb.max.y - bb.min.y) / 2,
                };
                let reach = (extent + leg_cells * grid.h) as f64;
                let span = (reach / grid.h as f64).ceil() as usize;
                let mut cands: Vec<(f64, usize, Point)> = Vec::new();
                for j in c.1.saturating_sub(span)..=(c.1 + span).min(grid.ny - 1) {
                    for i in c.0.saturating_sub(span)..=(c.0 + span).min(grid.nx - 1) {
                        let idx = grid.index(rl, i, j);
                        let pt = grid.centre(i, j);
                        let d = pt.distance(from);
                        if !grid.blocked[idx] && d <= reach {
                            cands.push((d, idx, pt));
                        }
                    }
                }
                cands.sort_by(|a, b| a.0.total_cmp(&b.0));
                for (_, idx, pt) in cands.into_iter().take(cap) {
                    if pt == from {
                        out.push((idx, None));
                    } else {
                        let tid = board.insert(track(item.net, bl, from, pt, hw));
                        let ok = legal_pair_item(board, tid, pours_block);
                        board.discard(tid);
                        if ok {
                            out.push((idx, Some((from, hw))));
                        }
                    }
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    Vec::new()
}

fn lay_leg_pieces(
    board: &mut Board,
    grid: &RouteGrid,
    path: &GridPath,
    sources: &[Terminal],
    targets: &[Terminal],
    pours_block: bool,
) -> Option<Vec<ItemId>> {
    let pcs = pieces(grid, path, sources, targets)?;
    let mut laid = Vec::new();
    let holes = board.layers.len();
    for piece in pcs {
        if piece.kind == Kind::Via {
            if let Shape::Capsule { a, r, .. } = piece.shape {
                let hole_r = piece.hole.map_or(0, |h| h.1);
                let reuse = board.near(holes, BBox::of([a])).into_iter().any(|id| {
                    board.item(id).is_some_and(|it| {
                        it.kind == Kind::Via
                            && it.net == piece.net
                            && (it.layers & piece.layers) == piece.layers
                            && matches!(it.shape, Shape::Capsule { a: p, b: q, r: vr } if p == a && q == a && vr == r)
                            && it.hole == Some((a, hole_r))
                    })
                });
                if reuse {
                    continue;
                }
            }
        }
        let id = board.insert(piece);
        laid.push(id);
        if !legal_pair_item(board, id, pours_block) {
            discard_all(board, laid);
            return None;
        }
    }
    Some(laid)
}

/// Pulls consecutive same-layer, same-width track segments in `ids` into straight lines wherever
/// `legal_pair_item` allows, keeping endpoints and vias fixed.
fn tighten_leg_items(board: &mut Board, ids: &mut Vec<ItemId>, pours_block: bool) {
    let mut k = 0;
    while k < ids.len() {
        let Some(first) = board.item(ids[k]).cloned() else {
            k += 1;
            continue;
        };
        let (Kind::Track, Shape::Capsule { a: a0, b: b0, r }) = (&first.kind, &first.shape) else {
            k += 1;
            continue;
        };
        let (net, layers, r) = (first.net, first.layers, *r);
        let mut pts = vec![*a0, *b0];
        let mut end = k + 1;
        while end < ids.len() {
            let Some(next) = board.item(ids[end]) else { break };
            let (Kind::Track, Shape::Capsule { a, b, r: nr }) = (&next.kind, &next.shape) else { break };
            if next.net != net || next.layers != layers || *nr != r || *a != *pts.last().expect("pts") {
                break;
            }
            pts.push(*b);
            end += 1;
        }
        if end > k + 1 {
            let run_ids = ids[k..end].to_vec();
            let mut new_run = Vec::new();
            let mut i = 0;
            let n = pts.len() - 1;
            while i < n {
                let mut j = n;
                let mut replaced_id = None;
                while j > i + 1 {
                    let removed: Vec<(ItemId, Item)> = run_ids[i..j].iter().filter_map(|&tid| Some((tid, board.remove(tid)?))).collect();
                    let cand = board.insert(Item {
                        kind: Kind::Track,
                        net,
                        layers,
                        shape: Shape::Capsule { a: pts[i], b: pts[j], r },
                        hole: None,
                        locked: false,
                        span: None,
                    });
                    if legal_pair_item(board, cand, pours_block) {
                        replaced_id = Some(cand);
                        break;
                    }
                    board.discard(cand);
                    for (tid, item) in removed {
                        board.restore(tid, item);
                    }
                    j -= 1;
                }
                if let Some(cid) = replaced_id {
                    new_run.push(cid);
                    i = j;
                } else {
                    new_run.push(run_ids[i]);
                    i += 1;
                }
            }
            let len_new = new_run.len();
            ids.splice(k..end, new_run);
            k += len_new;
        } else {
            k = end;
        }
    }
}

/// Routes a short matched bridge (`< SHORTEST_RUN`) between two nearby pin pairs at the pair's
/// differential width, trying symmetric zero-via tracks first and local fine-grid paths second.
fn route_matched_bridge(
    board: &mut Board,
    pair: &Pair,
    profile: &Profile,
    a: PinPair,
    b: PinPair,
    pours_block: bool,
) -> Option<Vec<ItemId>> {
    let (ap, an, bp, bn) = (centre(board, a.p), centre(board, a.n), centre(board, b.p), centre(board, b.n));
    let r_full = pair.width / 2;
    let r_neck = profile
        .neck_into(board.item(a.p))
        .min(profile.neck_into(board.item(b.p)))
        .min(r_full);
    let radii: Vec<Nm> = if r_neck < r_full { vec![r_full, r_neck] } else { vec![r_full] };

    let shared_layers = a.layers & b.layers & !board.planes;
    if shared_layers != 0 {
        let p_paths = chamfer_paths(ap, bp);
        let n_paths = chamfer_paths(an, bn);
        for layer in (0..board.layers.len()).filter(|&l| shared_layers & (1 << l) != 0) {
            for &r in &radii {
                for lp in &p_paths {
                    for ln in &n_paths {
                        let mut ids = Vec::new();
                        let ok = lay_line(board, lp, pair.p, layer, r, pours_block, &mut ids)
                            && lay_line(board, ln, pair.n, layer, r, pours_block, &mut ids)
                            && net_reaches(board, a.p, b.p, pours_block)
                            && net_reaches(board, a.n, b.n, pours_block);
                        if ok {
                            return Some(ids);
                        }
                        discard_all(board, ids);
                    }
                }
            }
        }
    }

    let bounds = BBox::of([ap, an, bp, bn]).grow(LEG_WINDOW);
    for allow_vias in [false, true] {
        for &r in &radii {
            let prof = Profile { half_width: r, ..profile.clone() };
            for p_first in [true, false] {
                let first = if p_first { (pair.p, a.p, b.p) } else { (pair.n, a.n, b.n) };
                let second = if p_first { (pair.n, a.n, b.n) } else { (pair.p, a.p, b.p) };
                let Some(mut ids1) = route_pad_to_pad_window(board, &prof, bounds, first.0, first.1, first.2, allow_vias, pours_block) else {
                    continue;
                };
                tighten_leg_items(board, &mut ids1, pours_block);
                let Some(mut ids2) = route_pad_to_pad_window(board, &prof, bounds, second.0, second.1, second.2, allow_vias, pours_block) else {
                    discard_all(board, ids1);
                    continue;
                };
                tighten_leg_items(board, &mut ids2, pours_block);
                if net_reaches(board, a.p, b.p, pours_block) && net_reaches(board, a.n, b.n, pours_block) {
                    ids1.extend(ids2);
                    return Some(ids1);
                }
                discard_all(board, ids2);
                discard_all(board, ids1);
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn route_pad_to_pad_window(
    board: &mut Board,
    prof: &Profile,
    bounds: BBox,
    net: u32,
    from_pad: ItemId,
    to_pad: ItemId,
    allow_vias: bool,
    pours_block: bool,
) -> Option<Vec<ItemId>> {
    if net_reaches(board, from_pad, to_pad, pours_block) {
        return Some(Vec::new());
    }
    let map = ObstacleMap::window(board, prof.clone(), LEG_CELL, bounds, pours_block);
    let mut grid = map.view(net);
    if !allow_vias {
        grid.via = None;
    }
    let sources = pad_only_terminals(board, &grid, from_pad, pours_block);
    let targets = pad_only_terminals(board, &grid, to_pad, pours_block);
    if sources.is_empty() || targets.is_empty() {
        return None;
    }
    let src_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
    let tgt_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
    let path = route_astar(&grid, &src_cells, &tgt_cells, 120)?;
    lay_leg_pieces(board, &grid, &path, &sources, &targets, pours_block)
}

/// Bridges any still-open same-footprint duplicate pads of `pair.p` or `pair.n` within
/// `PIN_PAIR_SPAN`, locking what is laid.
fn bridge_siblings(board: &mut Board, pair: &Pair, profile: &Profile, pours_block: bool) -> Vec<ItemId> {
    let mut out = Vec::new();
    for net in [pair.p, pair.n] {
        let pads: Vec<(ItemId, usize)> = board
            .items()
            .filter_map(|(id, it)| match it.kind {
                Kind::Pad { footprint, .. } if it.net == net && it.layers != 0 => Some((id, footprint)),
                _ => None,
            })
            .collect();
        for idx in 0..pads.len() {
            for jdx in idx + 1..pads.len() {
                if pads[idx].1 != pads[jdx].1 {
                    continue;
                }
                let (p1, p2) = (pads[idx].0, pads[jdx].0);
                let (c1, c2) = (centre(board, p1), centre(board, p2));
                if c1.distance(c2) > PIN_PAIR_SPAN as f64 || net_reaches(board, p1, p2, pours_block) {
                    continue;
                }
                let bounds = BBox::of([c1, c2]).grow(LEG_WINDOW);
                let prof = Profile { half_width: pair.width / 2, ..profile.clone() };
                for allow_vias in [false, true] {
                    if let Some(mut ids) = route_pad_to_pad_window(board, &prof, bounds, net, p1, p2, allow_vias, pours_block) {
                        tighten_leg_items(board, &mut ids, pours_block);
                        for &id in &ids {
                            if let Some(mut it) = board.remove(id) {
                                it.locked = true;
                                board.restore(id, it);
                            }
                        }
                        out.extend(ids);
                        break;
                    }
                }
            }
        }
    }
    out
}

/// Legacy single-layer `route_pairs_watch`, preserved for `--diff-pairs-legacy` comparisons.
fn route_pairs_legacy(board: &mut Board, options: &PairOptions, watch: &mut dyn FnMut(&Board, &PairRun)) -> PairStats {
    let mut stats = PairStats::default();
    for pair in diff_pairs(board) {
        stats.pairs += 1;
        let ends = pin_pairs(board, &pair);
        let profile = Profile::for_net(board, pair.p);
        let fat = Profile { class: format!("pair {}", board.net_name(pair.p)), half_width: pair.width + pair.gap / 2, ..profile };
        for (i, j) in runs(&ends) {
            let (a, b) = (ends[i], ends[j]);
            if a.mid.distance(b.mid) < SHORTEST_RUN as f64 {
                continue;
            }
            stats.runs += 1;
            tell(
                &options.log,
                "pairs",
                || format!("coupling {} and {}", board.net_name(pair.p), board.net_name(pair.n)),
                Some(Focus::Connection { net: pair.p, from: a.mid, to: b.mid }),
            );
            if let Some(laid) = legacy_run(board, options, &pair, &fat, a, b) {
                let layer = laid.0.first().and_then(|id| board.item(*id)).map_or(0, |it| it.layers.trailing_zeros() as usize);
                tell(
                    &options.log,
                    "pairs",
                    || format!("coupled {} and {} on {} for {:.2} mm", board.net_name(pair.p), board.net_name(pair.n), board.layers[layer], laid.1 * 1e-6),
                    None,
                );
                stats.coupled += 1;
                stats.coupled_mm += laid.1 * 1e-6;
                stats.laid.extend(laid.0.iter().copied());
                watch(board, &PairRun { pair: &pair, from: a.mid, to: b.mid, layer: Some(layer), centreline: &laid.2, laid: &laid.0 });
            } else {
                watch(board, &PairRun { pair: &pair, from: a.mid, to: b.mid, layer: None, centreline: &[], laid: &[] });
            }
        }
    }
    tell(&options.log, "pairs", || format!("{} of {} pair runs coupled, {:.1} mm", stats.coupled, stats.runs, stats.coupled_mm), None);
    stats
}

/// The legacy run between two pin pairs: a coupled centreline on one layer, no legs.
fn legacy_run(board: &mut Board, options: &PairOptions, pair: &Pair, fat: &Profile, a: PinPair, b: PinPair) -> Option<(Vec<ItemId>, f64, Vec<Point>)> {
    let ours: HashSet<u32> = [pair.p, pair.n].into();
    let map = ObstacleMap::without(board, fat.clone(), options.cell, options.pours_block, &|id| {
        board.item(id).is_some_and(|it| (ours.contains(&it.net) && !matches!(it.kind, Kind::Keepout { .. })) || (matches!(it.kind, Kind::Pour { .. }) && !it.drawn()))
    });
    let mut grid = map.view(pair.p);
    grid.via = None;
    couple_legacy(board, &mut grid, pair, a, b)
}

fn couple_legacy(board: &mut Board, grid: &mut RouteGrid, pair: &Pair, a: PinPair, b: PinPair) -> Option<(Vec<ItemId>, f64, Vec<Point>)> {
    let all = grid.blocked.clone();
    let mut best: Option<(f64, usize, Vec<Point>)> = None;
    for layer in 0..grid.layers.len() {
        let board_layer = grid.layers[layer];
        let vias = [a.layers, b.layers].iter().filter(|&&m| m & (1 << board_layer) == 0).count() as f64;
        for ring in LEGACY_RINGS {
            let Some(line) = centreline_legacy(grid, &all, layer, a, b, ring).filter(|l| length(l) >= SHORTEST_COUPLED) else { continue };
            let Some(laid) = lay_pair_legacy(board, pair, a, &line, board_layer) else { continue };
            discard_all(board, laid);
            let cost = length(&line) + vias * VIA_COST + 2.0 * ring.0 as f64;
            if best.as_ref().is_none_or(|b| cost < b.0) {
                best = Some((cost, layer, line));
            }
            break;
        }
    }
    grid.blocked.copy_from_slice(&all);
    let (_, layer, line) = best?;
    let laid = lay_pair_legacy(board, pair, a, &line, grid.layers[layer])?;
    Some((laid, length(&line), line))
}

fn centreline_legacy(grid: &mut RouteGrid, all: &[bool], layer: usize, a: PinPair, b: PinPair, ring: (Nm, Nm)) -> Option<Vec<Point>> {
    let cells = grid.cell_count();
    for l in 0..grid.layers.len() {
        let on = l == layer;
        grid.blocked[l * cells..(l + 1) * cells].iter_mut().zip(&all[l * cells..(l + 1) * cells]).for_each(|(c, &o)| *c = o || !on);
    }
    let (from, to) = (ring_cells(grid, layer, a.mid, ring), ring_cells(grid, layer, b.mid, ring));
    if from.is_empty() || to.is_empty() {
        return None;
    }
    let path = route_astar(grid, &from, &to, 0)?;
    let points: Vec<Point> = path.iter().map(|&(_, i, j)| grid.centre(i, j)).collect();
    if points.len() < 2 {
        return None;
    }
    let line = pull_straight(grid, layer, &points);
    (line.len() >= 2).then_some(line)
}

fn lay_pair_legacy(board: &mut Board, pair: &Pair, a: PinPair, line: &[Point], layer: usize) -> Option<Vec<ItemId>> {
    let line = ease(line, pair.pitch());
    let half = pair.pitch() as f64 / 2.0;
    let p_left = side(&line, centre(board, a.p)) >= side(&line, centre(board, a.n));
    let (p_line, n_line) = if p_left { (offset(&line, half), offset(&line, -half)) } else { (offset(&line, -half), offset(&line, half)) };
    let mut laid = Vec::new();
    if lay_line(board, &p_line, pair.p, layer, pair.width / 2, false, &mut laid)
        && lay_line(board, &n_line, pair.n, layer, pair.width / 2, false, &mut laid)
    {
        return Some(laid);
    }
    discard_all(board, laid);
    None
}

/// Whether every end of the pair has pads on a layer they all share, so its runs may reach the
/// pads without a via: then a fine-pitch part's pins of the pair are better left unescaped.
pub fn shares_layer(board: &Board, pair: &Pair) -> bool {
    let ends = pin_pairs(board, pair);
    ends.len() >= 2 && ends.iter().fold(u64::MAX, |m, e| m & e.layers) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_board(layers_sexpr: &str, rules_json: &str) -> Board {
        let text = format!(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers {layers_sexpr} (44 \"Edge.Cuts\" user)) \
             (net 0 \"\") (net 1 \"USB_DP\") (net 2 \"USB_DM\") (net 3 \"BLOCKER\") (net 4 \"SIG\") \
             (gr_rect (start 0 0) (end 30 20) (layer \"Edge.Cuts\") (width 0.1)))"
        );
        let (pcb, copper) = kicad_pcb::parse_board(&text).unwrap();
        Board::new(&pcb, &copper, kicad_pcb::Rules::parse(rules_json).unwrap())
    }

    fn default_pair_rules() -> &'static str {
        r#"{
            "board": {"design_settings": {"rules": {
                "min_clearance": 0.127, "min_track_width": 0.127,
                "min_via_diameter": 0.4, "min_through_hole_diameter": 0.2,
                "min_via_annular_width": 0.05, "min_copper_edge_clearance": 0.2,
                "min_hole_clearance": 0.2, "min_hole_to_hole": 0.25
            }}},
            "net_settings": {"classes": [{
                "name": "Default", "clearance": 0.127, "track_width": 0.15,
                "via_diameter": 0.4, "via_drill": 0.2,
                "diff_pair_width": 0.2, "diff_pair_gap": 0.2, "diff_pair_via_gap": 0.25, "priority": 0
            }]}
        }"#
    }

    fn insert_smd_pad(board: &mut Board, fp: usize, num: &str, net: u32, layer: usize, x: Nm, y: Nm, r: Nm) -> ItemId {
        board.insert(Item {
            kind: Kind::Pad { footprint: fp, number: num.into(), plated: false },
            net,
            layers: 1 << layer,
            shape: Shape::Capsule { a: Point::new(x, y), b: Point::new(x, y), r },
            hole: None,
            locked: true,
            span: None,
        })
    }

    #[test]
    fn pairs_are_named_by_one_sign_standing_alone_or_after_a_d() {
        for (p, n) in [("USB_DP", "USB_DM"), ("USB_C_DP", "USB_C_DN"), ("USB_DP_C", "USB_DM_C"), ("CLK_P", "CLK_N"), ("LVDS+", "LVDS-")] {
            assert!(pair_names(p, n), "{p} {n}");
        }
        for (p, n) in [("BTN_P", "BTN_Q"), ("PDM_CLK", "PDM_CLN"), ("SPI", "SNI"), ("USB_DP", "USB_DP"), ("PG_P", "PG_NN")] {
            assert!(!pair_names(p, n), "{p} {n}");
        }
    }

    #[test]
    fn offsets_stay_parallel_round_a_corner() {
        let line = [Point::new(0, 0), Point::new(1_000_000, 0), Point::new(2_000_000, 1_000_000)];
        let (l, r) = (offset(&line, 100_000.0), offset(&line, -100_000.0));
        for k in 0..line.len() - 1 {
            for (u, v) in [(&l, &r)] {
                let mid = |s: &[Point]| Point::new((s[k].x + s[k + 1].x) / 2, (s[k].y + s[k + 1].y) / 2);
                let d = board::point_segment(mid(u), v[k], v[k + 1]);
                assert!((d - 200_000.0).abs() < 2.0, "piece {k}: {d}");
            }
        }
    }

    #[test]
    fn spread_pins_converge_into_an_end_to_end_coupled_run_with_matched_legs() {
        let mut board = synthetic_board("(0 \"F.Cu\" signal) (31 \"B.Cu\" signal)", default_pair_rules());
        // Left connector has pins 1.6 mm apart with a blocker pad at the midpoint; right IC has
        // 0.4 mm pitch pins 16 mm away.
        insert_smd_pad(&mut board, 0, "1", 1, 0, 3_000_000, 9_200_000, 150_000);
        insert_smd_pad(&mut board, 0, "2", 3, 0, 3_000_000, 10_000_000, 150_000);
        insert_smd_pad(&mut board, 0, "3", 2, 0, 3_000_000, 10_800_000, 150_000);
        insert_smd_pad(&mut board, 1, "1", 1, 0, 19_000_000, 9_800_000, 100_000);
        insert_smd_pad(&mut board, 1, "2", 2, 0, 19_000_000, 10_200_000, 100_000);

        let stats = route_pairs(&mut board, &PairOptions::default(), &mut |_| {});
        assert_eq!((stats.runs, stats.coupled), (1, 1), "{stats:?}");
        assert!(board::connectivity(&board).airlines.is_empty(), "end-to-end connected");
        for &id in &stats.laid {
            assert!(board::check_item(&board, id).is_empty());
            assert!(board.item(id).unwrap().locked);
        }
        let pair = &diff_pairs(&board)[0];
        let ((lp, _), (ln, _)) = crate::tune::pair_lengths(&board, pair).expect("lengths");
        assert!((lp - ln).abs() < 100_000.0, "skew {:.3} mm", (lp - ln).abs() * 1e-6);
    }

    #[test]
    fn coupled_run_changes_layers_through_paired_vias_and_respects_via_gap() {
        let mut board = synthetic_board("(0 \"F.Cu\" signal) (31 \"B.Cu\" signal)", default_pair_rules());
        // Source pads on F.Cu, destination pads on B.Cu, with an F.Cu obstacle wall across the
        // middle so the coupled pair transitions through a paired via onto B.Cu.
        insert_smd_pad(&mut board, 0, "1", 1, 0, 3_000_000, 9_800_000, 100_000);
        insert_smd_pad(&mut board, 0, "2", 2, 0, 3_000_000, 10_200_000, 100_000);
        insert_smd_pad(&mut board, 1, "1", 1, 1, 22_000_000, 9_800_000, 100_000);
        insert_smd_pad(&mut board, 1, "2", 2, 1, 22_000_000, 10_200_000, 100_000);
        board.insert(Item {
            kind: Kind::Track,
            net: 3,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(11_000_000, 1_000_000), b: Point::new(11_000_000, 19_000_000), r: 200_000 },
            hole: None,
            locked: true,
            span: None,
        });

        let stats = route_pairs(&mut board, &PairOptions::default(), &mut |_| {});
        assert_eq!((stats.runs, stats.coupled), (1, 1), "{stats:?}");
        assert!(board::connectivity(&board).airlines.is_empty());
        let vias_p: Vec<Point> = board
            .items()
            .filter_map(|(_, it)| match (&it.kind, &it.shape) {
                (Kind::Via, Shape::Capsule { a, .. }) if it.net == 1 => Some(*a),
                _ => None,
            })
            .collect();
        let vias_n: Vec<Point> = board
            .items()
            .filter_map(|(_, it)| match (&it.kind, &it.shape) {
                (Kind::Via, Shape::Capsule { a, .. }) if it.net == 2 => Some(*a),
                _ => None,
            })
            .collect();
        assert_eq!((vias_p.len(), vias_n.len()), (1, 1), "one paired via transition");
        // Via diameter 0.4 mm + diff_pair_via_gap 0.25 mm = 0.65 mm centre-to-centre.
        let dist = vias_p[0].distance(vias_n[0]);
        assert!(dist >= 645_000.0, "expected >= 0.65 mm paired via pitch, got {:.3} mm", dist * 1e-6);
        for &id in &stats.laid {
            assert!(board::check_item(&board, id).is_empty());
        }
    }

    #[test]
    fn already_connected_pair_lays_no_duplicate_copper() {
        let mut board = synthetic_board("(0 \"F.Cu\" signal) (31 \"B.Cu\" signal)", default_pair_rules());
        insert_smd_pad(&mut board, 0, "1", 1, 0, 3_000_000, 9_800_000, 100_000);
        insert_smd_pad(&mut board, 0, "2", 2, 0, 3_000_000, 10_200_000, 100_000);
        insert_smd_pad(&mut board, 1, "1", 1, 0, 15_000_000, 9_800_000, 100_000);
        insert_smd_pad(&mut board, 1, "2", 2, 0, 15_000_000, 10_200_000, 100_000);
        board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(3_000_000, 9_800_000), b: Point::new(15_000_000, 9_800_000), r: 100_000 },
            hole: None,
            locked: true,
            span: None,
        });
        board.insert(Item {
            kind: Kind::Track,
            net: 2,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(3_000_000, 10_200_000), b: Point::new(15_000_000, 10_200_000), r: 100_000 },
            hole: None,
            locked: true,
            span: None,
        });
        let pair = &diff_pairs(&board)[0];
        assert!(!pair_has_open_runs(&board, pair, false));
        let stats = route_pairs(&mut board, &PairOptions::default(), &mut |_| {});
        assert_eq!((stats.runs, stats.coupled, stats.laid.len()), (0, 0, 0));
    }

    #[test]
    fn locked_pair_keeps_coupling_through_rip_up_shove_and_tightening() {
        let mut board = synthetic_board("(0 \"F.Cu\" signal) (31 \"B.Cu\" signal)", default_pair_rules());
        insert_smd_pad(&mut board, 0, "1", 1, 0, 4_000_000, 9_800_000, 100_000);
        insert_smd_pad(&mut board, 0, "2", 2, 0, 4_000_000, 10_200_000, 100_000);
        insert_smd_pad(&mut board, 1, "1", 1, 0, 20_000_000, 9_800_000, 100_000);
        insert_smd_pad(&mut board, 1, "2", 2, 0, 20_000_000, 10_200_000, 100_000);
        // Orthogonal single-ended net crossing the pair's path so it must drop under the locked pair.
        insert_smd_pad(&mut board, 2, "1", 4, 0, 12_000_000, 5_000_000, 120_000);
        insert_smd_pad(&mut board, 3, "1", 4, 0, 12_000_000, 15_000_000, 120_000);

        let pair_stats = route_pairs(&mut board, &PairOptions::default(), &mut |_| {});
        assert_eq!(pair_stats.coupled, 1);
        let snapshot: Vec<Shape> = pair_stats.laid.iter().map(|&id| board.item(id).unwrap().shape.clone()).collect();

        let mut solver = crate::route::CpuSolver;
        let mut soft = crate::route::route_open(&mut board, &mut solver, &crate::route::Options::default(), &mut |_| {}).laid;
        crate::ripup::rip_up(&mut board, &mut solver, &crate::ripup::RipOptions::default(), &mut soft, &mut |_| {});
        crate::shove::shove_escape(
            &mut board,
            &mut solver,
            &crate::shove::ShoveOptions { plough: crate::shove::Plough::Region, ..Default::default() },
            &mut soft,
            &mut |_, _| {},
        );
        crate::cleanup::pull_tight(&mut board, &mut soft);

        assert!(board::connectivity(&board).airlines.is_empty());
        let after: Vec<Shape> = pair_stats.laid.iter().map(|&id| board.item(id).expect("pair item intact").shape.clone()).collect();
        assert_eq!(snapshot, after, "pair copper was preserved untouched");
    }

    #[test]
    fn both_earring_usb_pairs_route_coupled_end_to_end() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../jobs/earring-kicad-locked/input/earring.kicad_pcb");
        let Ok((mut board, _)) = Board::load(&path) else { return };
        let loose: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked).map(|(id, _)| id).collect();
        for id in loose {
            board.remove(id);
        }
        let pairs = diff_pairs(&board);
        let names: Vec<(&str, &str)> = pairs.iter().map(|p| (board.net_name(p.p), board.net_name(p.n))).collect();
        assert_eq!(names, [("USB_C_DP", "USB_C_DN"), ("USB_DP", "USB_DM")]);
        let stats = route_pairs(&mut board, &PairOptions::default(), &mut |_| {});
        assert_eq!((stats.runs, stats.coupled), (3, 3), "{stats:?}");
        for id in &stats.laid {
            assert!(legal_pair_item(&board, *id, false));
        }
        for pair in &pairs {
            assert!(!pair_has_open_runs(&board, pair, false), "pair {:?} still has open connections", (board.net_name(pair.p), board.net_name(pair.n)));
        }
    }

    #[test]
    fn both_bluebuzz_usb_pairs_route_coupled_end_to_end() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../jobs/bluebuzz-kicad/input/bluebuzz-rm2.kicad_pcb");
        let Ok((mut board, _)) = Board::load(&path) else { return };
        let all_copper: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via)).map(|(id, _)| id).collect();
        for id in all_copper {
            board.remove(id);
        }
        let pairs = diff_pairs(&board);
        assert_eq!(pairs.len(), 2);
        let stats = route_pairs(&mut board, &PairOptions::default(), &mut |_| {});
        assert_eq!((stats.runs, stats.coupled), (4, 4), "{stats:?}");
        for id in &stats.laid {
            assert!(legal_pair_item(&board, *id, false));
        }
        for pair in &pairs {
            assert!(!pair_has_open_runs(&board, pair, false), "pair {:?} still has open connections", (board.net_name(pair.p), board.net_name(pair.n)));
        }
    }
}
