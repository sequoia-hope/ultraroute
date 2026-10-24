//! The design-rule checks a router must not break: copper clearance between nets, copper to the
//! board edge, copper to holes, hole to hole, and keepouts.

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

fn hole_shape(hole: (Point, Nm)) -> Shape {
    Shape::Capsule { a: hole.0, b: hole.0, r: hole.1 }
}

pub fn check(board: &Board) -> Vec<Violation> {
    let mut found: Vec<Violation> = Vec::new();
    let mut failed_pairs = std::collections::HashSet::new();
    let max_clearance = board
        .rules
        .classes
        .iter()
        .map(|c| nm(c.clearance))
        .chain([nm(board.rules.min_clearance), nm(board.rules.min_hole_clearance), nm(board.rules.min_hole_to_hole)])
        .max()
        .unwrap_or(0);
    let edge = nm(board.rules.min_copper_edge_clearance);
    let hole_clearance = nm(board.rules.min_hole_clearance);
    let hole_to_hole = nm(board.rules.min_hole_to_hole);
    let holes = board.layers.len();

    for (id, item) in board.items() {
        if item.is_copper() {
            for layer in (0..board.layers.len()).filter(|l| item.on(*l)) {
                for other in board.near(layer, item.shape.bbox().grow(max_clearance)) {
                    let Some(o) = board.item(other) else { continue };
                    if !o.on(layer) {
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
                        _ if other > id && o.is_copper() && (o.net != item.net || item.net == 0) && !same_pad(item, o) => {
                            let required = board.clearance(item.net, o.net);
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
            if edge > 0 {
                let gap = edge_gap(board, &item.shape);
                if gap + SLACK < edge as f64 {
                    found.push(Violation { rule: Rule::EdgeClearance, a: id, b: None, layer: None, gap, required: edge });
                }
            }
        }
        if let Some(hole) = item.hole {
            let shape = hole_shape(hole);
            let reach = shape.bbox().grow(hole_clearance.max(hole_to_hole));
            for other in board.near(holes, reach) {
                let Some(o) = board.item(other) else { continue };
                if other <= id || same_pad(item, o) {
                    continue;
                }
                if let Some(h) = o.hole {
                    let gap = shape.gap(&hole_shape(h));
                    if gap + SLACK < hole_to_hole as f64 {
                        found.push(Violation { rule: Rule::HoleToHole, a: id, b: Some(other), layer: None, gap, required: hole_to_hole });
                    }
                }
            }
            // KiCad 9 keeps copper clear of plated holes only; an unplated hole is not checked.
            for layer in (0..board.layers.len()).filter(|_| item.plated_hole()) {
                for other in board.near(layer, reach) {
                    let Some(o) = board.item(other) else { continue };
                    // One violation per pair, as KiCad reports them: a pair already too close is not
                    // reported again for the hole.
                    if other == id
                        || !o.is_copper()
                        || !o.on(layer)
                        || same_pad(item, o)
                        || (o.net == item.net && item.net != 0)
                        || failed_pairs.contains(&(id.min(other), id.max(other)))
                    {
                        continue;
                    }
                    let gap = shape.gap_within(&o.shape, hole_clearance as f64);
                    if gap + SLACK < hole_clearance as f64 {
                        found.push(Violation { rule: Rule::HoleClearance, a: id, b: Some(other), layer: Some(layer), gap, required: hole_clearance });
                    }
                }
            }
        }
    }
    found
}

/// Whether copper reaches into a keepout by more than the slack; touching its edge is allowed,
/// as KiCad's zone filler pours right up to it.
fn intrudes(copper: &Shape, keepout: &Shape) -> bool {
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
fn edge_gap(board: &Board, shape: &Shape) -> f64 {
    let b = shape.bbox();
    let mut best = f64::INFINITY;
    for poly in &board.outline {
        for i in 0..poly.len() {
            let (p, q) = (poly[i], poly[(i + 1) % poly.len()]);
            let edge = Shape::Capsule { a: p, b: q, r: 0 };
            let eb = edge.bbox();
            let far = (eb.min.x - b.max.x).max(b.min.x - eb.max.x).max(eb.min.y - b.max.y).max(b.min.y - eb.max.y);
            if far as f64 >= best {
                continue;
            }
            best = best.min(shape.gap(&edge));
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
