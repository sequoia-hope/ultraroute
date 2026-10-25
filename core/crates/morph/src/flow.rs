//! Moving parts and letting the copper flow with them: one warp field, rigid on every moving
//! part, still on every part that stays and on locked copper, harmonic in between. Each free
//! track is cut into short pieces, and at every point where other copper joins it partway, so it
//! bends with the field; every point then moves by the field there. A point on a moving part's
//! pad moves exactly as the part does and a point on copper that stays does not move at all, so
//! every contact holds.

use std::collections::{BTreeSet, HashMap, HashSet};

use board::{BBox, Board, Item, ItemId, Kind, Point, Shape};
use kicad_pcb::Span;

use crate::warp::WarpField;

/// Field cell and how far its domain reaches past the board, in nanometres.
const CELL: f64 = 150_000.0;
const MARGIN: f64 = 3_000_000.0;
/// Tracks are cut into pieces no longer than this before they flow, so they can bend.
const PIECE: f64 = 500_000.0;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlowStats {
    /// Copper items moved.
    pub flowed: usize,
    /// Pieces cut from tracks so they could bend.
    pub pieces: usize,
    /// Copper from the board file that moved, for removing from the file; it is in `soft` now.
    pub removed_spans: Vec<Span>,
    /// Bounding box covering the old and new positions of all copper items that flowed.
    pub dirty_bbox: Option<BBox>,
}

/// Moves each footprint in `moves` by its vector and every unlocked track and via by the warp
/// field those moves make, held still by every other part and by locked copper. New and moved
/// copper is added to `soft`, the copper to write back.
pub fn flow(board: &mut Board, moves: &HashMap<usize, Point>, soft: &mut Vec<ItemId>) -> FlowStats {
    let mut stats = FlowStats::default();
    if moves.values().all(|v| *v == Point::new(0, 0)) {
        return stats;
    }
    let field = warp_for(board, moves);
    let free = |it: &Item| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked;
    // Where a point of copper is fastened, judged where everything stands now.
    let fastened = |board: &Board, p: Point, net: u32, layers: u64| -> Option<Point> {
        for layer in (0..board.layers.len()).filter(|l| layers & (1 << l) != 0) {
            for other in board.near(layer, BBox::of([p])) {
                let Some(o) = board.item(other) else { continue };
                if o.net != net || !o.on(layer) || !o.shape.contains(p) {
                    continue;
                }
                if let Kind::Pad { footprint: f, .. } = o.kind {
                    return Some(moves.get(&f).copied().unwrap_or(Point::new(0, 0)));
                }
                if matches!(o.kind, Kind::Track | Kind::Via) && o.locked {
                    return Some(Point::new(0, 0));
                }
            }
        }
        None
    };
    let mut moving_pads_bb: Option<BBox> = None;
    for (_, it) in board.items() {
        let f_opt = match it.kind {
            Kind::Pad { footprint, .. } => Some(footprint),
            Kind::Keepout { footprint, .. } => footprint,
            _ => None,
        };
        if let Some(footprint) = f_opt {
            if let Some(&by) = moves.get(&footprint).filter(|v| **v != Point::new(0, 0)) {
                let mut b = it.shape.bbox();
                if let Some((c, r)) = it.hole {
                    b = b.union(BBox::of([c]).grow(r));
                }
                let b2 = BBox { min: Point::new(b.min.x + by.x, b.min.y + by.y), max: Point::new(b.max.x + by.x, b.max.y + by.y) };
                let u = b.union(b2);
                moving_pads_bb = Some(moving_pads_bb.map_or(u, |acc| acc.union(u)));
            }
        }
    }
    stats.dirty_bbox = moving_pads_bb;
    let mut ids: Vec<ItemId> = Vec::new();
    let mut need_junctions = false;
    for (id, it) in board.items() {
        if !free(it) {
            continue;
        }
        let Shape::Capsule { a, b, .. } = it.shape else { continue };
        let seg_bb = BBox::of([a, b]);
        if !field.may_move(seg_bb) && !moving_pads_bb.is_some_and(|mb| mb.overlaps(seg_bb)) {
            continue;
        }
        if a != b {
            need_junctions = true;
        }
        ids.push(id);
    }
    ids.sort_unstable();
    let junctions = if need_junctions { junctions(board) } else { HashMap::new() };
    let mut plan: Vec<(ItemId, Vec<(Point, Point)>)> = Vec::new();
    for id in ids {
        let Some(it) = board.item(id) else { continue };
        let Shape::Capsule { a, b, .. } = it.shape else { continue };
        let points = if a == b { vec![a] } else { cut(a, b, junctions.get(&layer_of(it)).map_or(&[][..], Vec::as_slice), PIECE) };
        let moved: Vec<(Point, Point)> = points
            .iter()
            .map(|&p| {
                let by = fastened(board, p, it.net, it.layers).unwrap_or_else(|| {
                    let (dx, dy) = field.displacement(p);
                    Point::new(dx.round() as i64, dy.round() as i64)
                });
                (p, Point::new(p.x + by.x, p.y + by.y))
            })
            .collect();
        if moved.iter().any(|(from, to)| from != to) {
            plan.push((id, moved));
        }
    }
    let mut removed_ids: HashSet<ItemId> = HashSet::with_capacity(plan.len());
    for (id, moved) in plan {
        let Some(mut item) = board.remove(id) else { continue };
        let old_bb = item.shape.bbox();
        stats.dirty_bbox = Some(stats.dirty_bbox.map_or(old_bb, |b| b.union(old_bb)));
        if let Some(span) = item.span.take() {
            stats.removed_spans.push(span);
        }
        stats.flowed += 1;
        removed_ids.insert(id);
        match item.shape {
            Shape::Capsule { r, .. } if moved.len() == 1 => {
                let to = moved[0].1;
                item.shape = Shape::Capsule { a: to, b: to, r };
                item.hole = item.hole.map(|(_, hr)| (to, hr));
                let new_bb = item.shape.bbox();
                stats.dirty_bbox = Some(stats.dirty_bbox.map_or(new_bb, |b| b.union(new_bb)));
                board.restore(id, item);
                removed_ids.remove(&id);
                if !soft.contains(&id) {
                    soft.push(id);
                }
            }
            Shape::Capsule { r, .. } => {
                stats.pieces += moved.len().saturating_sub(2);
                for w in moved.windows(2) {
                    let piece = Item { shape: Shape::Capsule { a: w[0].1, b: w[1].1, r }, ..item.clone() };
                    let new_bb = piece.shape.bbox();
                    stats.dirty_bbox = Some(stats.dirty_bbox.map_or(new_bb, |b| b.union(new_bb)));
                    soft.push(board.insert(piece));
                }
            }
            _ => {
                removed_ids.remove(&id);
                board.restore(id, item);
            }
        }
    }
    if !removed_ids.is_empty() {
        soft.retain(|s| !removed_ids.contains(s));
    }
    for (&f, &by) in moves {
        if by != Point::new(0, 0) {
            board.shift_footprint(f, by);
        }
    }
    stats
}

pub(crate) fn layer_of(it: &Item) -> usize {
    it.layers.trailing_zeros() as usize
}

/// The warp field the moves make: rigid on each moving part (its pads and the space between
/// them), still on every other pad, keepout and piece of locked copper.
pub fn warp_for(board: &Board, moves: &HashMap<usize, Point>) -> WarpField {
    let bounds = board
        .outline
        .iter()
        .flatten()
        .copied()
        .fold(None, |b: Option<BBox>, p| Some(b.map_or(BBox::of([p]), |b| b.union(BBox::of([p])))))
        .unwrap_or_else(|| BBox::of(board.items().map(|(_, it)| it.shape.bbox().min)));
    let mut field = WarpField::new(bounds, CELL, MARGIN);
    let mut moving: Vec<(&usize, &Point)> = moves.iter().filter(|(_, v)| **v != Point::new(0, 0)).collect();
    moving.sort_unstable_by_key(|(f, _)| **f);
    for (&f, by) in moving {
        let shapes: Vec<Shape> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Pad { footprint, .. } if footprint == f)).map(|(_, it)| it.shape.clone()).collect();
        if !shapes.is_empty() {
            field.add_inclusion(&shapes, (by.x as f64, by.y as f64), true);
        }
    }
    let still: Vec<Shape> = board
        .items()
        .filter(|(_, it)| match it.kind {
            Kind::Pad { footprint, .. } => moves.get(&footprint).is_none_or(|v| *v == Point::new(0, 0)),
            Kind::Keepout { .. } => true,
            Kind::Track | Kind::Via => it.locked,
            Kind::Pour { kept, .. } => kept,
        })
        .map(|(_, it)| it.shape.clone())
        .collect();
    if !still.is_empty() {
        field.add_inclusion(&still, (0.0, 0.0), false);
    }
    field.solve();
    field
}

/// Track ends and via centres on each layer: where other copper may join a track partway along.
pub(crate) fn junctions(board: &Board) -> HashMap<usize, Vec<Point>> {
    let mut out: HashMap<usize, BTreeSet<Point>> = HashMap::new();
    for (_, it) in board.items() {
        let Shape::Capsule { a, b, .. } = it.shape else { continue };
        match it.kind {
            Kind::Track => {
                out.entry(layer_of(it)).or_default().extend([a, b]);
            }
            Kind::Via => {
                for l in (0..64).filter(|l| it.layers & (1u64 << l) != 0) {
                    out.entry(l).or_default().insert(a);
                }
            }
            _ => {}
        }
    }
    out.into_iter().map(|(l, s)| (l, s.into_iter().collect())).collect()
}

/// The points a track from `a` to `b` is cut at: its ends, every `piece` between, and wherever a
/// junction lies on it.
pub(crate) fn cut(a: Point, b: Point, junctions: &[Point], piece: f64) -> Vec<Point> {
    let (dx, dy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
    let len2 = dx * dx + dy * dy;
    let pieces = (len2.sqrt() / piece).ceil().max(1.0) as usize;
    let mut stops: Vec<f64> = (0..=pieces).map(|k| k as f64 / pieces as f64).collect();
    let bb = BBox::of([a, b]).grow(2);
    let i0 = junctions.partition_point(|p| p.x < bb.min.x);
    let i1 = junctions.partition_point(|p| p.x <= bb.max.x);
    for p in &junctions[i0..i1] {
        if p.y < bb.min.y || p.y > bb.max.y {
            continue;
        }
        let t = ((p.x - a.x) as f64 * dx + (p.y - a.y) as f64 * dy) / len2;
        if t > 0.0 && t < 1.0 {
            let (qx, qy) = (a.x as f64 + dx * t, a.y as f64 + dy * t);
            if (qx - p.x as f64).hypot(qy - p.y as f64) <= 1.0 {
                stops.push(t);
            }
        }
    }
    stops.sort_by(f64::total_cmp);
    stops.dedup_by(|x, y| (*x - *y).abs() < 1e-9);
    stops
        .into_iter()
        .map(|t| if t == 0.0 { a } else if t == 1.0 { b } else { Point::new((a.x as f64 + dx * t).round() as i64, (a.y as f64 + dy * t).round() as i64) })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_track_is_cut_short_and_at_every_junction_on_it() {
        let (a, b) = (Point::new(0, 0), Point::new(1_200_000, 0));
        let at = cut(a, b, &[Point::new(100_000, 0), Point::new(100_000, 5_000), b], PIECE);
        assert_eq!(at.first(), Some(&a));
        assert_eq!(at.last(), Some(&b));
        assert!(at.contains(&Point::new(100_000, 0)), "the junction on it");
        assert!(!at.contains(&Point::new(100_000, 5_000)), "not one beside it");
        assert!(at.windows(2).all(|w| w[0].distance(w[1]) <= PIECE + 1.0));
    }
}
