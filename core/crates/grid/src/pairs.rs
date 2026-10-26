//! Differential pairs routed coupled, before anything else: the two tracks of a pair (USB's D+
//! and D-) run side by side at the class's pair width and gap. A pair's ends are pin pairs, a
//! pad of each net close together; between two pin pairs one centreline is routed on a single
//! layer, wide enough for both tracks and the gap, and the two tracks are laid along it, one each
//! side. The short legs from each pad to its track are left to the router, as open connections.

use std::collections::HashSet;

use board::{nm, BBox, Board, Item, ItemId, Kind, Nm, Point, Shape};

use crate::grid::{ObstacleMap, Profile, RouteGrid};
use crate::report::{tell, Focus, Log};
use crate::search::route_astar;

/// Pads of a pair farther apart than this are not taken as one end of it.
const PIN_PAIR_SPAN: Nm = 3_000_000;
/// Pin pairs closer than this are joined by legs alone; there is no run worth coupling.
const SHORTEST_RUN: Nm = 1_500_000;
/// Where a coupled run may start around a pin pair: rings of (inner, outer) radius, tried in turn.
const START_RINGS: [(Nm, Nm); 4] = [(0, 300_000), (400_000, 900_000), (900_000, 1_600_000), (1_600_000, 2_500_000)];
/// What a run that does not start on its pads' layer pays per end, as in grid steps: a via.
const OFF_LAYER: u32 = 80;
/// A turn sharper than this (cosine of the angle between the pieces) is cut in two first.
const SHARP_TURN: f64 = 0.75;

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
    pub log: Option<Log>,
}

impl Default for PairOptions {
    fn default() -> PairOptions {
        PairOptions { cell: 50_000, pours_block: false, log: None }
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
                pairs.push(Pair { p: p as u32, n: n as u32, width: width.max(nm(board.rules.min_track_width)), gap: gap.max(profile.clearance) });
            }
        }
    }
    pairs
}

fn centre(board: &Board, id: ItemId) -> Point {
    let b = board.bbox(id).unwrap_or(BBox::of([Point::new(0, 0)]));
    Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2)
}

/// One end of a pair: a pad of each net.
#[derive(Debug, Clone, Copy)]
struct PinPair {
    p: ItemId,
    n: ItemId,
    mid: Point,
    layers: u64,
}

/// Each positive pad with the nearest negative pad that has it as its own nearest.
fn pin_pairs(board: &Board, pair: &Pair) -> Vec<PinPair> {
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

/// Whether the straight line from `a` to `b` stays on free cells of `layer`.
fn clear_line(grid: &RouteGrid, layer: usize, a: Point, b: Point) -> bool {
    let steps = (a.distance(b) / (grid.h as f64 / 2.0)).ceil().max(1.0) as i64;
    (0..=steps).all(|k| {
        let t = k as f64 / steps as f64;
        let p = Point::new(a.x + ((b.x - a.x) as f64 * t).round() as Nm, a.y + ((b.y - a.y) as f64 * t).round() as Nm);
        grid.cell_at(p).is_some_and(|(i, j)| !grid.blocked[grid.index(layer, i, j)])
    })
}

/// The path's cell centres pulled straight wherever the grid allows: from each corner, on to
/// the farthest later one in plain view.
fn pull_straight(grid: &RouteGrid, layer: usize, points: &[Point]) -> Vec<Point> {
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
                let s = 1.0 + a.0 * b.0 + a.1 * b.1;
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

/// Lays one track per piece of `line`; all or nothing.
fn lay_line(board: &mut Board, line: &[Point], net: u32, layer: usize, r: Nm, laid: &mut Vec<ItemId>) -> bool {
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
        if !board::check_item(board, id).is_empty() {
            return false;
        }
    }
    true
}

fn length(points: &[Point]) -> f64 {
    points.windows(2).map(|w| w[0].distance(w[1])).sum()
}

/// Routes every differential pair's runs coupled, as far as they will go; what is laid is
/// returned in the stats, for the caller to keep apart from copper it may rip.
pub fn route_pairs(board: &mut Board, options: &PairOptions, watch: &mut dyn FnMut(&Board)) -> PairStats {
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
            // Fresh each run: the pair's own copper is left off the map, so the run may start
            // right at its pads; a track that would meet the other net's pad is refused below.
            let ours: HashSet<u32> = [pair.p, pair.n].into();
            let map = ObstacleMap::without(board, fat.clone(), options.cell, options.pours_block, &|id| {
                board.item(id).is_some_and(|it| ours.contains(&it.net) && !matches!(it.kind, Kind::Keepout { .. }))
            });
            let mut grid = map.view(pair.p);
            grid.via = None;
            if let Some(laid) = couple(board, &mut grid, &pair, a, b) {
                stats.coupled += 1;
                stats.coupled_mm += laid.1 * 1e-6;
                stats.laid.extend(laid.0);
                watch(board);
            }
        }
    }
    tell(&options.log, "pairs", || format!("{} of {} pair runs coupled, {:.1} mm", stats.coupled, stats.runs, stats.coupled_mm), None);
    stats
}

/// The best coupled run between two pin pairs that lays cleanly: each start ring in turn, on
/// each layer, the shortest kept. Returns what was laid and the centreline's length.
fn couple(board: &mut Board, grid: &mut RouteGrid, pair: &Pair, a: PinPair, b: PinPair) -> Option<(Vec<ItemId>, f64)> {
    let cells = grid.cell_count();
    let all = grid.blocked.clone();
    let pitch = pair.pitch() as f64;
    let r = pair.width / 2;
    for ring in START_RINGS {
        let mut tries: Vec<(u32, usize, Vec<Point>)> = Vec::new();
        for layer in 0..grid.layers.len() {
            // One layer at a time: the others are closed.
            for l in 0..grid.layers.len() {
                let on = l == layer;
                grid.blocked[l * cells..(l + 1) * cells].iter_mut().zip(&all[l * cells..(l + 1) * cells]).for_each(|(c, &o)| *c = o || !on);
            }
            let (from, to) = (ring_cells(grid, layer, a.mid, ring), ring_cells(grid, layer, b.mid, ring));
            if from.is_empty() || to.is_empty() {
                continue;
            }
            let Some(path) = route_astar(grid, &from, &to, 0) else { continue };
            let points: Vec<Point> = path.iter().map(|&(_, i, j)| grid.centre(i, j)).collect();
            if points.len() < 2 {
                continue;
            }
            let board_layer = grid.layers[layer];
            let off = [a.layers, b.layers].iter().filter(|&&m| m & (1 << board_layer) == 0).count() as u32;
            let cost = (length(&points) / grid.h as f64 * 10.0) as u32 + off * OFF_LAYER;
            tries.push((cost, layer, pull_straight(grid, layer, &points)));
        }
        tries.sort_by_key(|t| t.0);
        for (_, layer, line) in tries {
            let line = ease(&line, pair.pitch());
            if line.len() < 2 {
                continue;
            }
            let p_left = side(&line, centre(board, a.p)) >= side(&line, centre(board, a.n));
            let half = pitch / 2.0;
            let (p_line, n_line) = if p_left { (offset(&line, half), offset(&line, -half)) } else { (offset(&line, -half), offset(&line, half)) };
            let board_layer = grid.layers[layer];
            let mut laid = Vec::new();
            if lay_line(board, &p_line, pair.p, board_layer, r, &mut laid) && lay_line(board, &n_line, pair.n, board_layer, r, &mut laid) {
                grid.blocked.copy_from_slice(&all);
                return Some((laid, length(&line)));
            }
            for id in laid.into_iter().rev() {
                board.discard(id);
            }
        }
    }
    grid.blocked.copy_from_slice(&all);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn both_earring_usb_pairs_route_coupled() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../jobs/earring-kicad/input/earring.kicad_pcb");
        let Ok((mut board, _)) = Board::load(&path) else { return };
        let loose: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via)).map(|(id, _)| id).collect();
        for id in loose {
            board.remove(id);
        }
        let pairs = diff_pairs(&board);
        let names: Vec<(&str, &str)> = pairs.iter().map(|p| (board.net_name(p.p), board.net_name(p.n))).collect();
        assert_eq!(names, [("USB_C_DP", "USB_C_DN"), ("USB_DP", "USB_DM")]);
        let stats = route_pairs(&mut board, &PairOptions::default(), &mut |_| {});
        assert!(stats.coupled >= 2, "{stats:?}");
        for id in &stats.laid {
            assert!(board::check_item(&board, *id).is_empty());
        }
    }
}
