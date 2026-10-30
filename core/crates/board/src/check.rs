//! The design-rule checks a router must not break: copper clearance between nets, copper to the
//! board edge, copper to holes, hole to hole, and keepouts.

use crate::custom::Thing;
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


#[derive(Debug, Clone, Default)]
pub(crate) struct Limits {
    max_clearance: Nm,
    edge: Nm,
    hole_clearance: Nm,
    hole_to_hole: Nm,
    /// How far a hole may hold copper or another hole away, custom rules included.
    hole_reach: Nm,
}

/// Whether `shape` lies inside the polygon `area`, more than `margin` from its edge: a point of
/// it inside, and no edge within `margin` (none touching, when `margin` is 0).
pub fn within(area: &Shape, shape: &Shape, margin: Nm) -> bool {
    let Shape::Polygon(poly) = area else { return false };
    let first = match shape {
        Shape::Capsule { a, .. } => *a,
        Shape::Polygon(p) => match p.first() {
            Some(q) => *q,
            None => return false,
        },
    };
    area.contains(first)
        && (0..poly.len()).all(|k| {
            let edge = Shape::Capsule { a: poly[k], b: poly[(k + 1) % poly.len()], r: 0 };
            shape.gap_within(&edge, margin as f64 + 1.0) > margin as f64
        })
}

/// Whether a net tie lets the two touch, as KiCad does: two members of one tie (its pads and the
/// copper drawn in it), or a member and a track of one of its nets that ends inside one of its
/// pads (checked with kicad-cli 9.0.8: such a track may cross the tie's bar; one ending short of
/// the pads may not). Against a pad of the tie the track must end inside that pad: one that
/// crosses the other net's pad to end in its own shorts the two (kicad-cli 9.0.8 again).
fn tied(board: &Board, a: ItemId, item: &Item, b: ItemId, other: &Item) -> bool {
    let joins = |member: &Item, track: &Item, f: usize, nets: &[u32]| match member.kind {
        Kind::Pad { .. } => ends_inside(track, member) && nets.contains(&track.net) && track.kind == Kind::Track,
        _ => ends_in_tie(board, track, f, nets),
    };
    match (board.ties.get(&a), board.ties.get(&b)) {
        (Some((f, _)), Some((g, _))) => f == g,
        (Some((f, nets)), None) => joins(item, other, *f, nets),
        (None, Some((f, nets))) => joins(other, item, *f, nets),
        (None, None) => false,
    }
}

/// Whether a track has an end inside `pad`.
fn ends_inside(track: &Item, pad: &Item) -> bool {
    matches!(track.shape, Shape::Capsule { a, b, .. } if pad.shape.contains(a) || pad.shape.contains(b))
}

/// A track of one of a tie's `nets` with an end inside one of the pads of the tie in footprint `fp`.
fn ends_in_tie(board: &Board, track: &Item, fp: usize, nets: &[u32]) -> bool {
    let Shape::Capsule { a, b, .. } = track.shape else { return false };
    if track.kind != Kind::Track || !nets.contains(&track.net) {
        return false;
    }
    board.ties.iter().any(|(&id, (f, _))| {
        *f == fp && board.item(id).is_some_and(|p| matches!(p.kind, Kind::Pad { .. }) && (p.shape.contains(a) || p.shape.contains(b)))
    })
}

/// The largest of `f` over the layers in `mask`: a rule asked on each layer two things share.
fn over_layers(board: &Board, mask: u64, f: impl Fn(usize) -> Nm) -> Nm {
    (0..board.layers.len()).filter(|l| mask & (1 << l) != 0).map(f).max().unwrap_or(0)
}

impl Limits {
    /// How far copper `other` keeps from the hole of `hole` (item `id`): the hole clearance the
    /// rules set between them, and for a track or via near an unplated hole, the hole's pad's
    /// own clearance where wider. (An unplated slot is board edge to KiCad; its keepout holds
    /// the edge clearance.)
    fn hole_keep(&self, board: &Board, id: ItemId, hole: &Item, other_id: ItemId, other: &Item) -> Nm {
        let (a, b) = (Thing::Item(board, id, hole), Thing::Item(board, other_id, other));
        let base = over_layers(board, other.layers, |l| board.rule_min("hole_clearance", &a, Some(&b), l, self.hole_clearance));
        if hole.plated_hole() || !matches!(other.kind, Kind::Track | Kind::Via) {
            return base;
        }
        base.max(board.pad_clearances.get(&id).copied().unwrap_or(0))
    }

    /// The physical hole clearance custom rules set between the hole of `hole` and copper
    /// `other`, whatever their nets; 0 where none does.
    fn physical_hole(&self, board: &Board, id: ItemId, hole: &Item, other_id: ItemId, other: &Item) -> Nm {
        let (a, b) = (Thing::Item(board, id, hole), Thing::Item(board, other_id, other));
        over_layers(board, other.layers, |l| board.rule_min("physical_hole_clearance", &a, Some(&b), l, 0))
    }

    fn hole_to_hole(&self, board: &Board, a_id: ItemId, a: &Item, b_id: ItemId, b: &Item) -> Nm {
        let (x, y) = (Thing::Item(board, a_id, a), Thing::Item(board, b_id, b));
        // A drill goes through every layer, whatever copper the two have.
        over_layers(board, u64::MAX, |l| board.rule_min("hole_to_hole", &x, Some(&y), l, self.hole_to_hole))
    }
}

impl Limits {
    pub(crate) fn of(board: &Board) -> Limits {
        let rules = &board.rules;
        let (hole_clearance, hole_to_hole) = (nm(rules.min_hole_clearance), nm(rules.min_hole_to_hole));
        let hole_reach = [hole_clearance, hole_to_hole, board.rule_reach("hole_clearance"), board.rule_reach("hole_to_hole"), board.rule_reach("physical_hole_clearance")].into_iter().max().unwrap_or(0);
        // The widest solder mask opening round a pad beyond its copper, twice over with the web
        // between two openings, or pad clearance.
        let widest = board.mask_openings.values().chain([&board.via_openings]).map(|o| o.1).max().unwrap_or(0);
        let mask = (2 * widest + board.mask_margin() + board.mask_web).max(board.pad_clearances.values().copied().max().unwrap_or(0));
        Limits {
            max_clearance: rules
                .classes
                .iter()
                .map(|c| nm(c.clearance))
                .chain([nm(rules.min_clearance), hole_reach, board.rule_reach("clearance"), mask])
                .max()
                .unwrap_or(0),
            edge: board.outline_edges.iter().map(|e| e.3).fold(nm(rules.min_copper_edge_clearance), Nm::max),
            hole_clearance,
            hole_to_hole,
            hole_reach,
        }
    }
}

/// How far beyond a pad's solder mask opening other nets' copper keeps, so it lies clearly
/// outside the opening, not on its edge.
pub const MASK_MARGIN: Nm = 1_000;

impl Board {
    /// How far beyond a solder mask opening other nets' copper keeps: the project's solder mask
    /// to copper clearance, and [`MASK_MARGIN`] past it.
    pub fn mask_margin(&self) -> Nm {
        MASK_MARGIN + nm(self.rules.solder_mask_to_copper_clearance)
    }
}

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
        Some((layers, by)) if layers & (1 << layer) != 0 => by + board.mask_margin(),
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
        x + y - board.mask_margin() + board.mask_web
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
    crate::work::count(&crate::work::CHECKS);
    let mut found = Vec::new();
    item_checks(board, id, true, true, &board.limits, &mut HashSet::new(), &mut found);
    found
}

/// What one item breaks, pours included: for routing that keeps pours as planes.
pub fn check_item_with_pours(board: &Board, id: ItemId) -> Vec<Violation> {
    crate::work::count(&crate::work::CHECKS);
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
    // A custom rule may forbid tracks or vias of a net on a layer or in a named area.
    if matches!(item.kind, Kind::Track | Kind::Via) && board.custom.has("disallow") {
        let word = if item.kind == Kind::Via { "via" } else { "track" };
        let me = Thing::Item(board, id, item);
        if let Some(layer) = (0..board.layers.len()).find(|&l| item.on(l) && board.disallowed(word, &me, l) == kicad_pcb::dru::Tri::True) {
            found.push(Violation { rule: Rule::Keepout, a: id, b: None, layer: Some(layer), gap: 0.0, required: 0 });
        }
    }
    // A track or via of another net bared in an opening drawn in the mask would bridge there.
    if matches!(item.kind, Kind::Track | Kind::Via) {
        for (shape, layers, net) in &board.mask_areas {
            let margin = board.mask_margin();
            if layers & item.layers != 0 && (*net != item.net || *net == 0) && item.shape.gap_within(shape, margin as f64) < margin as f64 {
                found.push(Violation { rule: Rule::Keepout, a: id, b: None, layer: Some(layers.trailing_zeros() as usize), gap: 0.0, required: margin });
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
                        let in_a_hole = board.keepout_holes.get(&other).is_some_and(|holes| holes.iter().any(|h| within(h, &item.shape, 0)));
                        if banned && intrudes(&item.shape, &o.shape) && !in_a_hole {
                            found.push(Violation { rule: Rule::Keepout, a: id, b: Some(other), layer: Some(layer), gap: 0.0, required: 0 });
                        }
                    }
                    _ if partner(other) && o.is_copper() && (o.net != item.net || item.net == 0) && !same_pad(item, o) && !tied(board, id, item, other, o) => {
                        let mut implicit = board.clearance(item.net, o.net);
                        for p in [id, other] {
                            implicit = implicit.max(board.pad_clearances.get(&p).copied().unwrap_or(0));
                        }
                        let (a, b) = (Thing::Item(board, id, item), Thing::Item(board, other, o));
                        let required = board.rule_min("clearance", &a, Some(&b), layer, implicit).max(mask_pair_gap(board, id, item, other, o, layer));
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
            let (gap, required) = edge_gap(board, limits.edge, &item.shape, item_bbox);
            if gap + SLACK < required as f64 {
                found.push(Violation { rule: Rule::EdgeClearance, a: id, b: None, layer: None, gap, required });
            }
        }
        // Copper against the plated holes of others, when they are not walked themselves.
        if everyone {
            for other in board.near(holes, item_bbox.grow(limits.hole_reach.max(limits.edge).max(limits.max_clearance))) {
                let Some(o) = board.item(other) else { continue };
                let Some(h) = board.hole_shape(other, o) else { continue };
                if other == id || !holds_clear(o, item) || same_pad(item, o) {
                    continue;
                }
                let required = hole_required(board, limits, other, o, id, item);
                if required == 0 {
                    continue;
                }
                if failed_pairs.contains(&(id.min(other), id.max(other))) {
                    continue;
                }
                let gap = item.shape.gap_within(&h, required as f64);
                if gap + SLACK < required as f64 {
                    found.push(Violation { rule: Rule::HoleClearance, a: id, b: Some(other), layer: None, gap, required });
                }
            }
        }
    }
    if let Some(shape) = board.hole_shape(id, item) {
        let reach = shape.bbox().grow(limits.hole_reach.max(limits.edge).max(limits.max_clearance));
        for other in board.near(holes, reach) {
            let Some(o) = board.item(other) else { continue };
            if !partner(other) || same_pad(item, o) {
                continue;
            }
            if let Some(h) = board.hole_shape(other, o) {
                let required = limits.hole_to_hole(board, id, item, other, o);
                let gap = shape.gap(&h);
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
                if other == id || !o.is_copper() || !o.on(layer) || same_pad(item, o) || failed_pairs.contains(&(id.min(other), id.max(other))) {
                    continue;
                }
                let required = hole_required(board, limits, id, item, other, o);
                if required == 0 {
                    continue;
                }
                let gap = shape.gap_within(&o.shape, required as f64);
                if gap + SLACK < required as f64 {
                    failed_pairs.insert((id.min(other), id.max(other)));
                    found.push(Violation { rule: Rule::HoleClearance, a: id, b: Some(other), layer: Some(layer), gap, required });
                }
            }
        }
    }
}

/// How far copper `other` keeps from the hole of `hole` (item `id`): the hole clearance where
/// their nets differ (or the hole is unplated), and any physical hole clearance a custom rule
/// sets, which holds whatever the nets, between holes and the copper of other holes (a via
/// beside a via of its own net), not the tracks that join them.
fn hole_required(board: &Board, limits: &Limits, id: ItemId, hole: &Item, other_id: ItemId, other: &Item) -> Nm {
    let joined = hole.plated_hole() && other.net == hole.net && hole.net != 0;
    let clearance = if joined { 0 } else { limits.hole_keep(board, id, hole, other_id, other) };
    if joined && other.hole.is_none() {
        return clearance;
    }
    clearance.max(limits.physical_hole(board, id, hole, other_id, other))
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

/// The distance from a shape to the board edge it falls furthest short of, and the clearance
/// that edge asks: the shortest distance and `limit` where no edge is short; 0 when it crosses
/// one.
fn edge_gap(board: &Board, limit: Nm, shape: &Shape, b: crate::geom::BBox) -> (f64, Nm) {
    let mut best = (limit + 1) as f64;
    if !board.outline_edges.is_empty() {
        // How far short of its clearance the shape is of the worst edge, and that edge's.
        let mut worst = (best - limit as f64, best, limit);
        for &(p, q, eb, clearance) in &board.outline_edges {
            let far = (eb.min.x - b.max.x).max(b.min.x - eb.max.x).max(eb.min.y - b.max.y).max(b.min.y - eb.max.y);
            if far as f64 >= worst.0 + clearance as f64 {
                continue;
            }
            let edge = Shape::Capsule { a: p, b: q, r: 0 };
            let gap = shape.gap_within(&edge, worst.0 + clearance as f64);
            if gap - (clearance as f64) < worst.0 {
                worst = (gap - clearance as f64, gap, clearance);
            }
        }
        return (worst.1, worst.2);
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
    (best, limit)
}

/// The distance from a point to the board edge, for anything that needs it without a shape.
pub fn edge_distance(board: &Board, p: Point) -> f64 {
    board
        .outline
        .iter()
        .flat_map(|poly| (0..poly.len()).map(move |i| point_segment(p, poly[i], poly[(i + 1) % poly.len()])))
        .fold(f64::INFINITY, f64::min)
}
