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

#[derive(Debug, Clone, Default)]
pub(crate) struct Limits {
    max_clearance: Nm,
    edge: Nm,
    hole_clearance: Nm,
    hole_to_hole: Nm,
    /// From custom rules: hole clearance where either item is a through-hole pad, hole-to-hole
    /// between two of them, and clearance between two SMD pads; never below the plain ones.
    pth_hole_clearance: Nm,
    pth_hole_to_hole: Nm,
    smd_pad_clearance: Nm,
}

/// A pad with a plated hole: KiCad's `Pad_Type == 'Through-hole'`.
fn through_hole(item: &Item) -> bool {
    matches!(item.kind, Kind::Pad { plated: true, .. }) && item.hole.is_some()
}

/// A pad without a hole: KiCad's `Pad_Type == 'SMD'`.
fn smd(item: &Item) -> bool {
    matches!(item.kind, Kind::Pad { .. }) && item.hole.is_none()
}

impl Limits {
    /// The hole clearance between two items, one of whose holes is measured.
    fn hole_clearance(&self, a: &Item, b: &Item) -> Nm {
        if through_hole(a) || through_hole(b) { self.pth_hole_clearance } else { self.hole_clearance }
    }

    /// How far copper keeps from the hole of `hole` (item `id`): its hole clearance, and for a
    /// track or via near an unplated hole, the hole's pad's own clearance where wider. (An
    /// unplated slot is board edge to KiCad; its keepout holds the edge clearance.)
    fn hole_keep(&self, board: &Board, id: ItemId, hole: &Item, other: &Item) -> Nm {
        let base = self.hole_clearance(hole, other);
        if hole.plated_hole() || !matches!(other.kind, Kind::Track | Kind::Via) {
            return base;
        }
        base.max(board.pad_clearances.get(&id).copied().unwrap_or(0))
    }

    fn hole_to_hole(&self, a: &Item, b: &Item) -> Nm {
        if through_hole(a) && through_hole(b) { self.pth_hole_to_hole } else { self.hole_to_hole }
    }
}

impl Limits {
    pub(crate) fn of(board: &Board) -> Limits {
        let rules = &board.rules;
        let (hole_clearance, hole_to_hole) = (nm(rules.min_hole_clearance), nm(rules.min_hole_to_hole));
        let pth_hole_clearance = hole_clearance.max(nm(rules.pth_hole_clearance));
        let pth_hole_to_hole = hole_to_hole.max(nm(rules.pth_hole_to_hole));
        let smd_pad_clearance = nm(rules.smd_pad_clearance);
        // The widest solder mask opening round a pad beyond its copper, twice over with the web
        // between two openings, or pad clearance.
        let widest = board.mask_openings.values().chain([&board.via_openings]).map(|o| o.1).max().unwrap_or(0);
        let mask = (2 * widest + MASK_MARGIN + board.mask_web).max(board.pad_clearances.values().copied().max().unwrap_or(0));
        Limits {
            max_clearance: rules
                .classes
                .iter()
                .map(|c| nm(c.clearance))
                .chain([nm(rules.min_clearance), pth_hole_clearance, pth_hole_to_hole, smd_pad_clearance, mask])
                .max()
                .unwrap_or(0),
            edge: nm(rules.min_copper_edge_clearance),
            hole_clearance,
            hole_to_hole,
            pth_hole_clearance,
            pth_hole_to_hole,
            smd_pad_clearance,
        }
    }
}

/// How far beyond a pad's solder mask opening other nets' copper keeps, so it lies clearly
/// outside the opening, not on its edge.
pub const MASK_MARGIN: Nm = 1_000;

/// The gap copper of another net must keep on `layer` from `own` (item `id`) so as not to be
/// bared in its solder mask opening, for solder to bridge the two: a pad's opening, or a bare
/// via's; 0 where that does not apply. Two pads are the footprints' affair.
pub fn mask_gap(board: &Board, id: ItemId, own: &Item, other: &Item, layer: usize) -> Nm {
    let routed = matches!(other.kind, Kind::Track | Kind::Via);
    let opening = match own.kind {
        Kind::Pad { .. } if routed => board.mask_openings.get(&id).copied(),
        Kind::Via if routed || matches!(other.kind, Kind::Pad { .. }) => Some(board.via_openings),
        _ => None,
    };
    match opening {
        Some((layers, by)) if layers & (1 << layer) != 0 => by + MASK_MARGIN,
        _ => 0,
    }
}

/// The gap two items' copper must keep on `layer` for neither to be bared in the other's solder
/// mask opening, and, where both have an opening there (a bare via beside a pad or another bare
/// via), for the two openings to keep the mask's narrowest web between them, as KiCad checks
/// opening against opening.
pub fn mask_pair_gap(board: &Board, a_id: ItemId, a: &Item, b_id: ItemId, b: &Item, layer: usize) -> Nm {
    let (x, y) = (mask_gap(board, a_id, a, b, layer), mask_gap(board, b_id, b, a, layer));
    if x > 0 && y > 0 {
        x + y - MASK_MARGIN + board.mask_web
    } else {
        x.max(y)
    }
}

pub fn check(board: &Board) -> Vec<Violation> {
    let limits = &board.limits;
    let mut found = Vec::new();
    let mut failed_pairs = HashSet::new();
    for (id, _) in board.items() {
        item_checks(board, id, false, false, limits, &mut failed_pairs, &mut found);
    }
    found
}

/// What one item breaks against everything else: how a router tests copper before keeping it.
/// Pours are left out, as they are refilled around new copper; a block's kept pours are not.
pub fn check_item(board: &Board, id: ItemId) -> Vec<Violation> {
    let mut found = Vec::new();
    item_checks(board, id, true, true, &board.limits, &mut HashSet::new(), &mut found);
    found
}

/// What one item breaks, pours included: for routing that keeps pours as planes.
pub fn check_item_with_pours(board: &Board, id: ItemId) -> Vec<Violation> {
    let mut found = Vec::new();
    item_checks(board, id, true, false, &board.limits, &mut HashSet::new(), &mut found);
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
    // A track or via of another net bared in an opening drawn in the mask would bridge there.
    if matches!(item.kind, Kind::Track | Kind::Via) {
        for (shape, layers, net) in &board.mask_areas {
            if layers & item.layers != 0 && (*net != item.net || *net == 0) && item.shape.gap_within(shape, MASK_MARGIN as f64) < MASK_MARGIN as f64 {
                found.push(Violation { rule: Rule::Keepout, a: id, b: None, layer: Some(layers.trailing_zeros() as usize), gap: 0.0, required: MASK_MARGIN });
            }
        }
    }
    if item.is_copper() {
        for layer in (0..board.layers.len()).filter(|l| item.on(*l)) {
            for other in board.near(layer, item_bbox.grow(limits.max_clearance)) {
                let Some(o) = board.item(other) else { continue };
                if !o.on(layer) || (skip_pours && o.refilled()) {
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
                        let mut required = board.clearance(item.net, o.net);
                        if smd(item) && smd(o) {
                            required = required.max(limits.smd_pad_clearance);
                        }
                        required = required.max(mask_pair_gap(board, id, item, other, o, layer));
                        for p in [id, other] {
                            required = required.max(board.pad_clearances.get(&p).copied().unwrap_or(0));
                        }
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
            for other in board.near(holes, item_bbox.grow(limits.pth_hole_clearance.max(limits.edge).max(limits.max_clearance))) {
                let Some(o) = board.item(other) else { continue };
                let Some(h) = o.hole else { continue };
                let required = limits.hole_keep(board, other, o, item);
                if other == id || !holds_clear(o, item) || same_pad(item, o) || (o.plated_hole() && o.net == item.net && item.net != 0) {
                    continue;
                }
                if failed_pairs.contains(&(id.min(other), id.max(other))) {
                    continue;
                }
                let gap = item.shape.gap_within(&hole_shape(h), required as f64);
                if gap + SLACK < required as f64 {
                    found.push(Violation { rule: Rule::HoleClearance, a: id, b: Some(other), layer: None, gap, required });
                }
            }
        }
    }
    if let Some(hole) = item.hole {
        let shape = hole_shape(hole);
        let reach = shape.bbox().grow(limits.pth_hole_clearance.max(limits.pth_hole_to_hole).max(limits.edge).max(limits.max_clearance));
        for other in board.near(holes, reach) {
            let Some(o) = board.item(other) else { continue };
            if !partner(other) || same_pad(item, o) {
                continue;
            }
            if let Some(h) = o.hole {
                let required = limits.hole_to_hole(item, o);
                let gap = shape.gap(&hole_shape(h));
                if gap + SLACK < required as f64 {
                    found.push(Violation { rule: Rule::HoleToHole, a: id, b: Some(other), layer: None, gap, required });
                }
            }
        }
        for layer in 0..board.layers.len() {
            for other in board.near(layer, reach) {
                let Some(o) = board.item(other) else { continue };
                if !holds_clear(item, o) || (skip_pours && o.refilled()) {
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
                let required = limits.hole_keep(board, id, item, o);
                let gap = shape.gap_within(&o.shape, required as f64);
                if gap + SLACK < required as f64 {
                    failed_pairs.insert((id.min(other), id.max(other)));
                    found.push(Violation { rule: Rule::HoleClearance, a: id, b: Some(other), layer: Some(layer), gap, required });
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
