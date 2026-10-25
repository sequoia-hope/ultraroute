//! What stands in the way of an open connection, and how far each part in the way must move to
//! clear it.
//!
//! The connection is routed on its net's grid with the copper of parts that may move made
//! passable, at a price that grows with how deeply the path would cut into its clearance. The
//! cheapest path then shows what is in the way: the parts whose clearance it cuts become
//! displacements that would clear it, while other nets' routed copper and parts that may not
//! move are passable at a fixed price, so the report can name them without anything moving.
//! Only copper that belongs to no part and is not routed (the outline, keepouts, kept pours) is a
//! wall.

use std::collections::{BTreeMap, BTreeSet};

use board::{Board, ItemId, Kind, Nm, Point, Shape};

use crate::grid::{ObstacleMap, Profile, RouteGrid};
use crate::route::terminals;
use crate::search::{cost_field, trace_back, STRAIGHT};

/// Cost of entering a cell inside a movable part's clearance, plus a share per cell of intrusion,
/// so a path brushes past a part rather than cutting through its middle.
const INTRUSION_BASE: u32 = 2 * STRAIGHT;
const INTRUSION_PER_CELL: f64 = 2.0 * STRAIGHT as f64;
/// Cost of entering the clearance of a part that may not move, or that owns an end: passable so
/// a path can leave a crowded pad, but dear, since nothing moving can clear it.
const PINNED_PART: u32 = 8 * STRAIGHT;
/// Rounds of projection before a part's pushes are judged impossible to meet together.
const SOLVER_SWEEPS: usize = 50;

#[derive(Debug, Clone, PartialEq)]
pub struct BlameOptions {
    pub cell: Nm,
    pub via_cost: u32,
    /// Clearance kept beyond the rules, so a part moved by its displacement leaves room for a
    /// grid router, whose cell centres keep further off than the rules.
    pub margin: Nm,
    /// Displacements longer than this are not offered.
    pub max_move: Nm,
    /// Cost of entering a cell other nets' routed copper claims, which the router may rip up;
    /// `None` makes routed copper a wall, so only moving parts can open the path.
    pub routed_copper: Option<u32>,
    /// Keep other nets' pours as walls instead of copper to be refilled around the path.
    pub pours_block: bool,
}

impl Default for BlameOptions {
    fn default() -> BlameOptions {
        BlameOptions {
            cell: 50_000,
            via_cost: 80,
            margin: 45_000,
            max_move: 1_500_000,
            routed_copper: Some(6 * STRAIGHT),
            pours_block: false,
        }
    }
}

/// A movable part in the way, and the displacement that clears the path, in nanometres.
#[derive(Debug, Clone, PartialEq)]
pub struct Blame {
    pub footprint: usize,
    pub dx: f64,
    pub dy: f64,
}

/// What stands between the two ends of an open connection on its cheapest path.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BlameReport {
    /// No path at all: walls alone separate the ends.
    pub walled_off: bool,
    /// Free cells each end offers a path, before any search; none means the end is buried.
    pub end_cells: (usize, usize),
    pub path_cells: usize,
    /// Path cells inside other nets' routed copper clearance, and those nets.
    pub routed_cells: usize,
    pub routed_nets: Vec<u32>,
    /// Path cells inside the clearance of parts that may not move or that own an end, and those
    /// parts by footprint index.
    pub pinned_cells: usize,
    pub pinned_parts: Vec<usize>,
    /// Path cells inside movable parts' clearance.
    pub intruded_cells: usize,
    /// Movable parts in the way, with the displacement that clears the path.
    pub blamed: Vec<Blame>,
    /// Movable parts in the way that no translation within `max_move` clears: the path cuts
    /// them from both sides, or would need them moved too far.
    pub unmovable: Vec<usize>,
}

impl BlameReport {
    /// Whether anything at all was found in the way.
    pub fn blocked(&self) -> bool {
        self.walled_off || self.routed_cells > 0 || self.pinned_cells > 0 || self.intruded_cells > 0
    }
}

/// One shape of a part's copper on one layer, and how near a trace's centre may come to it.
struct Intruder {
    footprint: usize,
    movable: bool,
    layer: usize,
    shape: Shape,
    /// Trace half width plus clearance.
    reach: f64,
}

/// The footprint an item belongs to: a pad's part, or the part a rule area came with.
fn part_of(kind: &Kind) -> Option<usize> {
    match kind {
        Kind::Pad { footprint, .. } => Some(*footprint),
        Kind::Keepout { footprint, .. } => *footprint,
        _ => None,
    }
}

/// The parts `movable` allows to move that block the cheapest route between `from` and `to`,
/// with the displacement each needs, and whatever else the route had to pass. Parts owning
/// either end are never blamed: moving them moves the end too. The board is left as it was.
pub fn blame(board: &mut Board, from: ItemId, to: ItemId, options: &BlameOptions, movable: &dyn Fn(usize) -> bool) -> BlameReport {
    let mut report = BlameReport::default();
    let Some(net) = board.item(from).map(|it| it.net).filter(|n| *n != 0) else {
        report.walled_off = true;
        return report;
    };
    let ends: BTreeSet<usize> = [from, to].iter().filter_map(|id| board.item(*id).and_then(|it| part_of(&it.kind))).collect();
    let moves = |f: usize| movable(f) && !ends.contains(&f);
    let routed = |kind: &Kind| matches!(kind, Kind::Track | Kind::Via);
    let profile = Profile::for_net(board, net);

    // Three maps over the same grid: the walls, the routed copper, the parts that stay put.
    // Movable parts are on none of them; their clearance is measured exactly below instead.
    let kind_of = |board: &Board, id: ItemId| board.item(id).map(|it| it.kind.clone());
    let hard_map = ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| match kind_of(board, id) {
        Some(k) => part_of(&k).is_some() || (routed(&k) && options.routed_copper.is_some()),
        None => true,
    });
    let routed_map = ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| {
        !kind_of(board, id).is_some_and(|k| routed(&k))
    });
    let pinned_map = ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| {
        !kind_of(board, id).and_then(|k| part_of(&k)).is_some_and(|f| !moves(f))
    });
    let mut grid = hard_map.view(net);
    let routed_grid = routed_map.view(net);
    let pinned_grid = pinned_map.view(net);

    let intruders = intruders(board, &hard_map, &grid, net, &moves);
    let mut depth = vec![f64::NEG_INFINITY; grid.blocked.len()];
    for intruder in intruders.iter().filter(|x| x.movable) {
        stamp_depth(&grid, intruder, options.margin as f64, &mut depth);
    }
    grid.penalty = (0..grid.blocked.len())
        .map(|n| {
            let mut penalty = 0;
            if routed_grid.blocked[n] {
                penalty += options.routed_copper.unwrap_or(0);
            }
            if pinned_grid.blocked[n] {
                penalty += PINNED_PART;
            }
            if depth[n] > 0.0 {
                penalty += INTRUSION_BASE + (INTRUSION_PER_CELL * depth[n] / grid.h as f64).round() as u32;
            }
            penalty
        })
        .collect();

    let sources: Vec<usize> = terminals(board, &grid, from).into_iter().map(|t| t.0).collect();
    let targets: Vec<usize> = terminals(board, &grid, to).into_iter().map(|t| t.0).collect();
    report.end_cells = (sources.len(), targets.len());
    let cost = cost_field(&grid, &sources, options.via_cost);
    let Some(path) = trace_back(&grid, &cost, &targets, options.via_cost) else {
        report.walled_off = true;
        return report;
    };
    report.path_cells = path.len();

    let h = grid.h as f64;
    let mut pushes: BTreeMap<usize, Vec<((f64, f64), f64)>> = BTreeMap::new();
    let (mut pinned_parts, mut routed_nets) = (BTreeSet::new(), BTreeSet::new());
    for &(layer, i, j) in &path {
        let n = grid.index(layer, i, j);
        let at = grid.centre(i, j);
        if routed_grid.blocked[n] {
            report.routed_cells += 1;
            routed_nets.extend(routed_nets_near(board, &hard_map, grid.layers[layer], at, net));
        }
        if pinned_grid.blocked[n] {
            report.pinned_cells += 1;
            pinned_parts.extend(
                intruders
                    .iter()
                    .filter(|x| !x.movable && x.layer == layer)
                    .filter(|x| nearest(&x.shape, at).0 < x.reach + h)
                    .map(|x| x.footprint),
            );
        }
        if depth[n] <= 0.0 {
            continue;
        }
        report.intruded_cells += 1;
        for intruder in intruders.iter().filter(|x| x.movable && x.layer == layer) {
            if let Some(push) = clearing_push(intruder, at, options.margin as f64) {
                pushes.entry(intruder.footprint).or_default().push(push);
            }
        }
    }
    report.pinned_parts = pinned_parts.into_iter().collect();
    report.routed_nets = routed_nets.into_iter().collect();
    for (footprint, pushes) in pushes {
        let (dx, dy) = satisfy(&pushes);
        if dx.hypot(dy) <= options.max_move as f64 {
            report.blamed.push(Blame { footprint, dx, dy });
        } else {
            report.unmovable.push(footprint);
        }
    }
    report
}

/// Every shape of every part's copper on the grid's layers, other than the net's own. A drilled
/// hole counts on every layer, keeping the hole clearance; a part's rule area keeps a trace's
/// half width.
fn intruders(board: &Board, map: &ObstacleMap, grid: &RouteGrid, net: u32, moves: &dyn Fn(usize) -> bool) -> Vec<Intruder> {
    let half = grid.half_width as f64;
    let hole_reach = half + board::nm(board.rules.min_hole_clearance) as f64;
    let mut out = Vec::new();
    for (_, item) in board.items() {
        let Some(footprint) = part_of(&item.kind) else { continue };
        let movable = moves(footprint);
        let reach = match &item.kind {
            Kind::Keepout { forbids, .. } if !forbids.tracks => continue,
            Kind::Keepout { .. } => half,
            _ if item.net == net && net != 0 => continue,
            _ => half + map.clearance_to(board, item.net) as f64,
        };
        for (layer, &board_layer) in grid.layers.iter().enumerate() {
            if item.on(board_layer) && (item.is_copper() || matches!(item.kind, Kind::Keepout { .. })) {
                out.push(Intruder { footprint, movable, layer, shape: item.shape.clone(), reach });
            }
            if let Some((centre, r)) = item.hole {
                out.push(Intruder { footprint, movable, layer, shape: Shape::Capsule { a: centre, b: centre, r }, reach: hole_reach });
            }
        }
    }
    out
}

/// Other nets' routed copper on `layer` whose clearance a trace centred at `at` would cut.
fn routed_nets_near(board: &Board, map: &ObstacleMap, layer: usize, at: Point, net: u32) -> Vec<u32> {
    // Wide enough for the largest clearance any class keeps; each candidate is then measured.
    let widest = board.rules.classes.iter().map(|c| board::nm(c.clearance)).max().unwrap_or(0);
    let probe = board::BBox::of([at]).grow(map.profile.half_width + map.profile.clearance.max(widest) + map.h);
    board
        .near(layer, probe)
        .into_iter()
        .filter_map(|id| board.item(id))
        .filter(|it| matches!(it.kind, Kind::Track | Kind::Via) && it.net != net && it.on(layer))
        .filter(|it| nearest(&it.shape, at).0 < (map.profile.half_width + map.clearance_to(board, it.net) + map.h) as f64)
        .map(|it| it.net)
        .collect()
}

/// Records, per cell, the deepest intrusion of a trace centred there into `intruder`'s clearance.
fn stamp_depth(grid: &RouteGrid, intruder: &Intruder, margin: f64, depth: &mut [f64]) {
    let reach = intruder.reach + margin;
    let bb = intruder.shape.bbox();
    let h = grid.h as f64;
    let lo = |v: Nm, origin: Nm| ((v as f64 - reach - origin as f64) / h).floor().max(0.0) as usize;
    let hi = |v: Nm, origin: Nm, n: usize| (((v as f64 + reach - origin as f64) / h).ceil().max(0.0) as usize).min(n - 1);
    for j in lo(bb.min.y, grid.y0)..=hi(bb.max.y, grid.y0, grid.ny) {
        for i in lo(bb.min.x, grid.x0)..=hi(bb.max.x, grid.x0, grid.nx) {
            let d = reach - nearest(&intruder.shape, grid.centre(i, j)).0;
            let n = grid.index(intruder.layer, i, j);
            depth[n] = depth[n].max(d);
        }
    }
}

/// The distance from `p` to the shape, 0 inside it, and the nearest point of the shape's core:
/// a capsule's spine, or a polygon's boundary.
fn nearest(shape: &Shape, p: Point) -> (f64, (f64, f64)) {
    let on_segment = |a: Point, b: Point| {
        let (dx, dy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
        let (px, py) = ((p.x - a.x) as f64, (p.y - a.y) as f64);
        let len2 = dx * dx + dy * dy;
        let t = if len2 == 0.0 { 0.0 } else { ((px * dx + py * dy) / len2).clamp(0.0, 1.0) };
        let q = (a.x as f64 + t * dx, a.y as f64 + t * dy);
        ((p.x as f64 - q.0).hypot(p.y as f64 - q.1), q)
    };
    match shape {
        Shape::Capsule { a, b, r } => {
            let (d, q) = on_segment(*a, *b);
            ((d - *r as f64).max(0.0), q)
        }
        Shape::Polygon(poly) => {
            let best = (0..poly.len())
                .map(|k| on_segment(poly[k], poly[(k + 1) % poly.len()]))
                .min_by(|x, y| x.0.total_cmp(&y.0))
                .unwrap_or((f64::INFINITY, (p.x as f64, p.y as f64)));
            if board::inside(p, poly) {
                (0.0, best.1)
            } else {
                best
            }
        }
    }
}

/// The direction to move `intruder` and how far along it, so a trace centred at `at` clears it.
/// From outside the shape it is pushed straight away from the trace; from inside, away towards
/// the side its middle lies, far enough that its trailing corner passes the trace.
fn clearing_push(intruder: &Intruder, at: Point, margin: f64) -> Option<((f64, f64), f64)> {
    let reach = intruder.reach + margin;
    let (distance, near) = nearest(&intruder.shape, at);
    if distance >= reach {
        return None;
    }
    let (ax, ay) = (at.x as f64, at.y as f64);
    if distance > 0.0 {
        return Some((unit(near.0 - ax, near.1 - ay)?, reach - distance));
    }
    let bb = intruder.shape.bbox();
    let n = unit((bb.min.x + bb.max.x) as f64 / 2.0 - ax, (bb.min.y + bb.max.y) as f64 / 2.0 - ay)?;
    let behind = [(bb.min.x, bb.min.y), (bb.min.x, bb.max.y), (bb.max.x, bb.min.y), (bb.max.x, bb.max.y)]
        .into_iter()
        .map(|(x, y)| (ax - x as f64) * n.0 + (ay - y as f64) * n.1)
        .fold(0.0, f64::max);
    Some((n, reach + behind))
}

fn unit(x: f64, y: f64) -> Option<(f64, f64)> {
    let length = x.hypot(y);
    (length > 0.0).then(|| (x / length, y / length))
}

/// A displacement `v` with `v · n >= d` for every push `(n, d)`, by cyclic projection. Pushes
/// from both sides of the path cannot all be met; then the projection never settles, and the
/// displacement is infinite.
fn satisfy(pushes: &[((f64, f64), f64)]) -> (f64, f64) {
    let (mut x, mut y) = (0.0, 0.0);
    for _ in 0..SOLVER_SWEEPS {
        let mut moved = false;
        for ((nx, ny), d) in pushes {
            let short = d - (x * nx + y * ny);
            if short > 1e-6 {
                x += short * nx;
                y += short * ny;
                moved = true;
            }
        }
        if !moved {
            return (x, y);
        }
    }
    (f64::INFINITY, f64::INFINITY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: Nm, y: Nm, half: Nm) -> Shape {
        Shape::Polygon(vec![
            Point::new(x - half, y - half),
            Point::new(x + half, y - half),
            Point::new(x + half, y + half),
            Point::new(x - half, y + half),
        ])
    }

    fn pad(shape: Shape, reach: f64) -> Intruder {
        Intruder { footprint: 0, movable: true, layer: 0, shape, reach }
    }

    #[test]
    fn opposing_pushes_have_no_displacement() {
        assert!(satisfy(&[((1.0, 0.0), 1.0), ((-1.0, 0.0), 1.0)]).0.is_infinite());
    }

    #[test]
    fn perpendicular_pushes_add_up() {
        let (x, y) = satisfy(&[((1.0, 0.0), 2.0), ((0.0, 1.0), 3.0)]);
        assert!((x - 2.0).abs() < 1e-6 && (y - 3.0).abs() < 1e-6);
    }

    #[test]
    fn no_pushes_ask_for_no_move() {
        assert_eq!(satisfy(&[]), (0.0, 0.0));
    }

    #[test]
    fn distances_are_measured_to_the_copper_and_are_zero_inside() {
        let disc = Shape::Capsule { a: Point::new(0, 0), b: Point::new(0, 0), r: 100 };
        assert_eq!(nearest(&disc, Point::new(300, 0)).0, 200.0);
        assert_eq!(nearest(&disc, Point::new(50, 0)).0, 0.0);
        let sq = square(0, 0, 100);
        assert_eq!(nearest(&sq, Point::new(0, 400)), (300.0, (0.0, 100.0)));
        assert_eq!(nearest(&sq, Point::new(10, 10)).0, 0.0);
    }

    #[test]
    fn a_part_beside_the_path_is_pushed_away_by_its_shortfall() {
        // A trace at the origin, a pad edge 300 off along +x: 500 of reach wants it 200 further.
        let p = pad(square(400, 0, 100), 500.0);
        let ((nx, ny), d) = clearing_push(&p, Point::new(0, 0), 0.0).unwrap();
        assert!((nx - 1.0).abs() < 1e-9 && ny.abs() < 1e-9);
        assert!((d - 200.0).abs() < 1e-9);
        // The margin is kept on top of the rules.
        assert!((clearing_push(&p, Point::new(0, 0), 50.0).unwrap().1 - 250.0).abs() < 1e-9);
        // Far enough off, nothing is asked.
        assert!(clearing_push(&p, Point::new(-300, 0), 0.0).is_none());
    }

    #[test]
    fn a_part_the_path_runs_through_is_pushed_past_it_entirely() {
        // A trace 50 left of a 200-wide pad's middle: the pad moves right until its left edge,
        // 50 behind the trace, is a full reach beyond it.
        let p = pad(square(0, 0, 100), 300.0);
        let ((nx, _), d) = clearing_push(&p, Point::new(-50, 0), 0.0).unwrap();
        assert!((nx - 1.0).abs() < 1e-9);
        assert!((d - (300.0 + 50.0)).abs() < 1e-9, "{d}");
    }

    #[test]
    fn depth_is_deepest_near_the_copper_and_absent_beyond_its_reach() {
        let grid = RouteGrid {
            net: 1,
            x0: 0,
            y0: 0,
            h: 100,
            nx: 20,
            ny: 20,
            layers: vec![0],
            half_width: 10,
            via: None,
            blocked: vec![false; 400],
            via_blocked: vec![false; 400],
            penalty: Vec::new(),
            pours_block: false,
        };
        let p = pad(Shape::Capsule { a: Point::new(1000, 1000), b: Point::new(1000, 1000), r: 100 }, 300.0);
        let mut depth = vec![f64::NEG_INFINITY; 400];
        stamp_depth(&grid, &p, 0.0, &mut depth);
        let at = |i: usize, j: usize| depth[grid.index(0, i, j)];
        // Cell (9, 9) is centred at (950, 950), inside the pad: the full reach.
        assert!((at(9, 9) - 300.0).abs() < 1e-9);
        assert!(at(12, 9) > 0.0 && at(12, 9) < at(10, 9));
        assert!(at(15, 9) <= 0.0);
        assert_eq!(at(0, 0), f64::NEG_INFINITY, "cells beyond the reach are never visited");
    }
}
