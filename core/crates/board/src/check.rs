//! The design-rule checks a router must not break: copper clearance between nets, copper to the
//! board edge, copper to holes, hole to hole, and keepouts.

use std::collections::HashSet;

use crate::model::{Board, Item, ItemId, Kind};
use crate::geom::{inside, nm, point_segment, Nm, Point, Shape};

/// Shortfalls this small are not violations: KiCad's DRC epsilon, 0.5 µm by default.
const SLACK: f64 = 500.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Rule {
    Clearance,
    EdgeClearance,
    HoleClearance,
    HoleToHole,
    Keepout,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    pub rule: Rule,
    pub a: ItemId,
    pub b: Option<ItemId>,
    pub layer: Option<usize>,
    pub gap: f64,
    pub required: Nm,
}

fn same_pad(a: &Item, b: &Item) -> bool {
    matches!((&a.kind, &b.kind), (Kind::Pad { footprint: f, number: n, .. }, Kind::Pad { footprint: g, number: m, .. }) if f == g && n == m)
}

/// Whether the hole of `holder` must be kept clear of `copper`, as KiCad 9 checks it: a plated
/// hole of all other nets' copper, an unplated one of every track and via, its own net's too,
/// but not of pads.
fn holds_clear(holder: &Item, copper: &Item) -> bool {
    holder.hole.is_some() && (holder.plated_hole() || matches!(copper.kind, Kind::Track | Kind::Via))
}

fn hole_shape(hole: (Point, Nm)) -> Shape {
    Shape::Capsule { a: hole.0, b: hole.0, r: hole.1 }
}

struct Limits {
    max_clearance: Nm,
    edge: Nm,
    hole_clearance: Nm,
    hole_to_hole: Nm,
}

impl Limits {
    fn of(board: &Board) -> Limits {
        let rules = &board.rules;
        Limits {
            max_clearance: rules
                .classes
                .iter()
                .map(|c| nm(c.clearance))
                .chain([nm(rules.min_clearance), nm(rules.min_hole_clearance), nm(rules.min_hole_to_hole)])
                .max()
                .unwrap_or(0),
            edge: nm(rules.min_copper_edge_clearance),
            hole_clearance: nm(rules.min_hole_clearance),
            hole_to_hole: nm(rules.min_hole_to_hole),
        }
    }
}

pub fn check(board: &Board) -> Vec<Violation> {
    let limits = Limits::of(board);
    let mut found = Vec::new();
    let mut failed_pairs = HashSet::new();
    for (id, _) in board.items() {
        item_checks(board, id, false, false, &limits, &mut failed_pairs, &mut found);
    }
    found
}

/// What one item breaks against everything else: how a router tests copper before keeping it.
/// Pours are left out, as they are refilled around new copper.
pub fn check_item(board: &Board, id: ItemId) -> Vec<Violation> {
    let mut found = Vec::new();
    item_checks(board, id, true, true, &Limits::of(board), &mut HashSet::new(), &mut found);
    found
}

/// What one item breaks, pours included: for routing that keeps pours as planes.
pub fn check_item_with_pours(board: &Board, id: ItemId) -> Vec<Violation> {
    let mut found = Vec::new();
    item_checks(board, id, true, false, &Limits::of(board), &mut HashSet::new(), &mut found);
    found
}

/// With `everyone` false, pairs are taken once, from their lower id, as the whole-board check
/// walks every item.
fn item_checks(
    board: &Board,
    id: ItemId,
    everyone: bool,
    skip_pours: bool,
    limits: &Limits,
    failed_pairs: &mut HashSet<(ItemId, ItemId)>,
    found: &mut Vec<Violation>,
) {
    let Some(item) = board.item(id) else { return };
    let item_bbox = board.bbox(id).unwrap_or_else(|| item.shape.bbox());
    let partner = |other: ItemId| other != id && (everyone || other > id);
    let holes = board.layers.len();
    if item.is_copper() {
        for layer in (0..board.layers.len()).filter(|l| item.on(*l)) {
            for other in board.near(layer, item_bbox.grow(limits.max_clearance)) {
                let Some(o) = board.item(other) else { continue };
                if !o.on(layer) || (skip_pours && matches!(o.kind, Kind::Pour { .. })) {
                    continue;
                }
                match &o.kind {
                    Kind::Keepout { forbids, footprint } => {
                        let banned = match &item.kind {
                            Kind::Track => forbids.tracks,
                            Kind::Via => forbids.vias,
                            Kind::Pad { footprint: f, .. } => forbids.pads && *footprint != Some(*f),
                            Kind::Pour { .. } => forbids.copperpour,
                            Kind::Keepout { .. } => false,
                        };
                        if banned && intrudes(&item.shape, &o.shape) {
                            found.push(Violation { rule: Rule::Keepout, a: id, b: Some(other), layer: Some(layer), gap: 0.0, required: 0 });
                        }
                    }
                    _ if partner(other) && o.is_copper() && (o.net != item.net || item.net == 0) && !same_pad(item, o) => {
                        let required = board.clearance(item.net, o.net);
                        let other_bbox = board.bbox(other).unwrap_or_else(|| o.shape.bbox());
                        if !item_bbox.grow(required).overlaps(other_bbox) {
                            continue;
                        }
                        let gap = item.shape.gap_within(&o.shape, required as f64);
                        if gap + SLACK < required as f64 {
                            failed_pairs.insert((id.min(other), id.max(other)));
                            found.push(Violation { rule: Rule::Clearance, a: id, b: Some(other), layer: Some(layer), gap, required });
                        }
                    }
                    _ => {}
                }
            }
        }
        if limits.edge > 0 {
            let gap = edge_gap(board, limits.edge, &item.shape, item_bbox);
            if gap + SLACK < limits.edge as f64 {
                found.push(Violation { rule: Rule::EdgeClearance, a: id, b: None, layer: None, gap, required: limits.edge });
            }
        }
        // Copper against the plated holes of others, when they are not walked themselves.
        if everyone {
            for other in board.near(holes, item_bbox.grow(limits.hole_clearance)) {
                let Some(o) = board.item(other) else { continue };
                let Some(h) = o.hole else { continue };
                if other == id || !holds_clear(o, item) || same_pad(item, o) || (o.plated_hole() && o.net == item.net && item.net != 0) {
                    continue;
                }
                if failed_pairs.contains(&(id.min(other), id.max(other))) {
                    continue;
                }
                let gap = item.shape.gap_within(&hole_shape(h), limits.hole_clearance as f64);
                if gap + SLACK < limits.hole_clearance as f64 {
                    found.push(Violation { rule: Rule::HoleClearance, a: id, b: Some(other), layer: None, gap, required: limits.hole_clearance });
                }
            }
        }
    }
    if let Some(hole) = item.hole {
        let shape = hole_shape(hole);
        let reach = shape.bbox().grow(limits.hole_clearance.max(limits.hole_to_hole));
        for other in board.near(holes, reach) {
            let Some(o) = board.item(other) else { continue };
            if !partner(other) || same_pad(item, o) {
                continue;
            }
            if let Some(h) = o.hole {
                let gap = shape.gap(&hole_shape(h));
                if gap + SLACK < limits.hole_to_hole as f64 {
                    found.push(Violation { rule: Rule::HoleToHole, a: id, b: Some(other), layer: None, gap, required: limits.hole_to_hole });
                }
            }
        }
        for layer in 0..board.layers.len() {
            for other in board.near(layer, reach) {
                let Some(o) = board.item(other) else { continue };
                if !holds_clear(item, o) || (skip_pours && matches!(o.kind, Kind::Pour { .. })) {
                    continue;
                }
                // One violation per pair, as KiCad reports them: a pair already too close is not
                // reported again for the hole.
                if other == id
                    || !o.is_copper()
                    || !o.on(layer)
                    || same_pad(item, o)
                    || (item.plated_hole() && o.net == item.net && item.net != 0)
                    || failed_pairs.contains(&(id.min(other), id.max(other)))
                {
                    continue;
                }
                let gap = shape.gap_within(&o.shape, limits.hole_clearance as f64);
                if gap + SLACK < limits.hole_clearance as f64 {
                    failed_pairs.insert((id.min(other), id.max(other)));
                    found.push(Violation { rule: Rule::HoleClearance, a: id, b: Some(other), layer: Some(layer), gap, required: limits.hole_clearance });
                }
            }
        }
    }
}

/// Whether copper reaches into a keepout by more than the slack; touching its edge is allowed,
/// as KiCad's zone filler pours right up to it.
pub fn intrudes(copper: &Shape, keepout: &Shape) -> bool {
    let Shape::Polygon(area) = keepout else {
        return copper.gap(keepout) == 0.0;
    };
    let depth = |p: Point| -> f64 {
        if !inside(p, area) {
            return 0.0;
        }
        (0..area.len()).map(|i| point_segment(p, area[i], area[(i + 1) % area.len()])).fold(f64::INFINITY, f64::min)
    };
    match copper {
        Shape::Capsule { a, b, r } => {
            depth(*a) > 0.0 || depth(*b) > 0.0 || crate::geom::segment_polygon(*a, *b, area) + SLACK < *r as f64
        }
        Shape::Polygon(poly) => {
            poly.iter().any(|p| depth(*p) > SLACK)
                || area.iter().any(|p| inside(*p, poly) && (0..poly.len()).map(|i| point_segment(*p, poly[i], poly[(i + 1) % poly.len()])).fold(f64::INFINITY, f64::min) > SLACK)
        }
    }
}

/// The shortest distance from a shape to the board edge; 0 when it crosses it.
fn edge_gap(board: &Board, limit: Nm, shape: &Shape, b: crate::geom::BBox) -> f64 {
    let mut best = (limit + 1) as f64;
    if !board.outline_edges.is_empty() {
        for &(p, q, eb) in &board.outline_edges {
            let far = (eb.min.x - b.max.x).max(b.min.x - eb.max.x).max(eb.min.y - b.max.y).max(b.min.y - eb.max.y);
            if far as f64 >= best {
                continue;
            }
            let edge = Shape::Capsule { a: p, b: q, r: 0 };
            best = best.min(shape.gap_within(&edge, best));
        }
        return best;
    }
    for poly in &board.outline {
        for i in 0..poly.len() {
            let (p, q) = (poly[i], poly[(i + 1) % poly.len()]);
            let eb = crate::geom::BBox::of([p, q]);
            let far = (eb.min.x - b.max.x).max(b.min.x - eb.max.x).max(eb.min.y - b.max.y).max(b.min.y - eb.max.y);
            if far as f64 >= best {
                continue;
            }
            let edge = Shape::Capsule { a: p, b: q, r: 0 };
            best = best.min(shape.gap_within(&edge, best));
        }
    }
    best
}

/// The distance from a point to the board edge, for anything that needs it without a shape.
pub fn edge_distance(board: &Board, p: Point) -> f64 {
    board
        .outline
        .iter()
        .flat_map(|poly| (0..poly.len()).map(move |i| point_segment(p, poly[i], poly[(i + 1) % poly.len()])))
        .fold(f64::INFINITY, f64::min)
}
