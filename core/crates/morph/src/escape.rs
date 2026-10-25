//! Combing a fine-pitch part's escapes into lanes. Each signal pad on the package's edge owns a
//! lane: the line straight out from the pad's centre, across a fanout zone in front of that edge.
//! Within the zone the router's copper is moved, never broken: a net's copper is drawn sideways
//! onto the lane of its own pad nearest it, fully from the pad row out and fading to nothing at
//! the zone's outer edge, while copper of nets with no pad on that edge is pushed outward past the
//! zone. A point's move depends only on where it is and whose it is, so pieces of copper that meet
//! at a point move together and stay joined; points on pads do not move at all. What the combing
//! leaves breaking a rule is for the caller to rip and route again, through the lanes it cleared.

use std::collections::{BTreeMap, HashMap};

use board::{BBox, Board, Item, ItemId, Kind, Nm, Point, Shape};
use kicad_pcb::Span;

use crate::flow::{cut, junctions, layer_of};

/// Pieces are cut this short inside the zone, so copper can bend onto its lane.
const PIECE: f64 = 150_000.0;

#[derive(Debug, Clone)]
pub struct EscapeOptions {
    /// How far the fanout zone reaches out from the package's pad rows, in nanometres.
    pub zone: Nm,
    /// Of the zone's depth, the share over which copper is drawn fully onto its lane; beyond it
    /// the pull fades to nothing at the zone's edge.
    pub full: f64,
    /// How far past the zone's edge foreign copper is pushed, in nanometres.
    pub clear: Nm,
}

impl Default for EscapeOptions {
    fn default() -> EscapeOptions {
        EscapeOptions { zone: 1_000_000, full: 0.6, clear: 150_000 }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EscapeStats {
    /// Lanes laid out, one per signal pad on the package's edges.
    pub lanes: usize,
    /// Copper items combed onto lanes, and foreign ones pushed out of the zone.
    pub combed: usize,
    pub pushed_out: usize,
    /// Copper from the board file that moved, for removing from the file; it is in `soft` now.
    pub removed_spans: Vec<Span>,
}

/// One edge of the package: the direction out of it and the direction along it, both unit,
/// where its pad row ends, and its pads' lanes by net as positions along the edge.
struct Edge {
    out: (f64, f64),
    along: (f64, f64),
    row: f64,
    from: f64,
    to: f64,
    lanes: BTreeMap<u32, Vec<f64>>,
}

impl Edge {
    fn depth(&self, p: Point) -> f64 {
        p.x as f64 * self.out.0 + p.y as f64 * self.out.1 - self.row
    }

    fn lateral(&self, p: Point) -> f64 {
        p.x as f64 * self.along.0 + p.y as f64 * self.along.1
    }
}

/// The package's edges with their lanes: each pad is assigned to the side of the pads' bounding
/// box it is nearest, and a pad far from every side (an exposed pad) to none.
fn edges(board: &Board, footprint: usize) -> Vec<Edge> {
    let pads: Vec<&Item> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Pad { footprint: f, .. } if f == footprint)).map(|(_, it)| it).collect();
    let Some(b) = pads.iter().map(|p| p.shape.bbox()).reduce(BBox::union) else { return Vec::new() };
    let centre = |p: &Item| {
        let bb = p.shape.bbox();
        Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2)
    };
    // A pad belongs to a side when its centre lies within a pad's own size of it.
    let sides: [((f64, f64), (f64, f64), f64); 4] = [
        ((-1.0, 0.0), (0.0, 1.0), -(b.min.x as f64)),
        ((1.0, 0.0), (0.0, 1.0), b.max.x as f64),
        ((0.0, -1.0), (1.0, 0.0), -(b.min.y as f64)),
        ((0.0, 1.0), (1.0, 0.0), b.max.y as f64),
    ];
    let mut out: Vec<Edge> = sides.iter().map(|&(o, a, row)| Edge { out: o, along: a, row, from: f64::INFINITY, to: f64::NEG_INFINITY, lanes: BTreeMap::new() }).collect();
    for p in pads {
        if p.net == 0 {
            continue;
        }
        let c = centre(p);
        let bb = p.shape.bbox();
        let size = (bb.max.x - bb.min.x).max(bb.max.y - bb.min.y) as f64;
        let nearest = out.iter().enumerate().map(|(i, e)| (-e.depth(c), i)).min_by(|a, b| a.0.total_cmp(&b.0));
        if let Some((gap, i)) = nearest {
            if gap <= size {
                let e = &mut out[i];
                let t = e.lateral(c);
                e.lanes.entry(p.net).or_default().push(t);
                e.from = e.from.min(t);
                e.to = e.to.max(t);
            }
        }
    }
    out.retain(|e| !e.lanes.is_empty());
    out
}

/// Combs the escapes of `footprint` into lanes. `soft` is the router's copper; copper from the
/// board file that moves joins it.
pub fn comb(board: &mut Board, footprint: usize, options: &EscapeOptions, soft: &mut Vec<ItemId>) -> EscapeStats {
    let mut stats = EscapeStats::default();
    let edges = edges(board, footprint);
    stats.lanes = edges.iter().map(|e| e.lanes.values().map(Vec::len).sum::<usize>()).sum();
    if edges.is_empty() {
        return stats;
    }
    let zone = options.zone as f64;
    let full = zone * options.full.clamp(0.0, 1.0);
    // A point stays put when copper there could touch a pad, on any layer: every piece meeting
    // at a point must get the same answer, whatever its own width or layers.
    let reach = board.items().filter_map(|(_, it)| match (&it.kind, &it.shape) {
        (Kind::Track | Kind::Via, Shape::Capsule { r, .. }) => Some(*r),
        _ => None,
    }).max().unwrap_or(0);
    let pads: Vec<Shape> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Pad { .. })).map(|(_, it)| it.shape.clone()).collect();
    let locked: Vec<(Shape, u32)> =
        board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && it.locked).map(|(_, it)| (it.shape.clone(), it.net)).collect();
    let touches = |s: &Shape, p: Point| {
        let dot = Shape::Capsule { a: p, b: p, r: reach };
        s.bbox().grow(reach).overlaps(BBox::of([p])) && s.gap(&dot) == 0.0
    };
    // Points fastened to something that stays: any pad, or locked copper of their own net.
    let held = |p: Point, net: u32| pads.iter().any(|s| touches(s, p)) || locked.iter().any(|(s, n)| *n == net && touches(s, p));
    // A free via moves as one piece: copper ending anywhere inside it follows its centre.
    let vias: Vec<(Point, Nm, u32)> = board
        .items()
        .filter_map(|(_, it)| match (&it.kind, &it.shape) {
            (Kind::Via, Shape::Capsule { a, r, .. }) if !it.locked => Some((*a, *r, it.net)),
            _ => None,
        })
        .collect();
    let via_at = |p: Point, net: u32| vias.iter().find(|(c, r, n)| *n == net && c.distance(p) <= *r as f64).map(|(c, _, _)| *c);
    // The move of a point of `net`'s copper: onto its nearest lane on an edge whose zone holds
    // it, or out of a zone where it has no lane. The nearest edge wins where zones overlap.
    let shift = |p: Point, net: u32| -> Option<(Point, bool)> {
        let p = via_at(p, net).unwrap_or(p);
        if held(p, net) {
            return None;
        }
        let e = edges
            .iter()
            .filter(|e| {
                let (d, t) = (e.depth(p), e.lateral(p));
                d >= 0.0 && d <= zone && t >= e.from - zone && t <= e.to + zone
            })
            .min_by(|a, b| a.depth(p).total_cmp(&b.depth(p)))?;
        let d = e.depth(p);
        match e.lanes.get(&net) {
            Some(lanes) => {
                let t = e.lateral(p);
                let lane = lanes.iter().copied().min_by(|a, b| (a - t).abs().total_cmp(&(b - t).abs()))?;
                let w = if d <= full { 1.0 } else { ((zone - d) / (zone - full).max(1.0)).clamp(0.0, 1.0) };
                let by = (lane - t) * w;
                Some((Point::new((e.along.0 * by).round() as i64, (e.along.1 * by).round() as i64), true))
            }
            None => {
                let by = zone + options.clear as f64 - d;
                Some((Point::new((e.out.0 * by).round() as i64, (e.out.1 * by).round() as i64), false))
            }
        }
    };
    let junctions = junctions(board);
    let mut ids: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked).map(|(id, _)| id).collect();
    ids.sort_unstable();
    let mut plan: Vec<(ItemId, Vec<Point>, bool)> = Vec::new();
    for id in ids {
        let Some(it) = board.item(id) else { continue };
        let Shape::Capsule { a, b, .. } = it.shape else { continue };
        let points = if a == b { vec![a] } else { cut(a, b, junctions.get(&layer_of(it)).map_or(&[][..], Vec::as_slice), PIECE) };
        let mut own = false;
        let mut moved = false;
        let to: Vec<Point> = points
            .iter()
            .map(|&p| match shift(p, it.net) {
                Some((by, lane)) if by != Point::new(0, 0) => {
                    moved = true;
                    own |= lane;
                    Point::new(p.x + by.x, p.y + by.y)
                }
                _ => p,
            })
            .collect();
        if moved {
            plan.push((id, to, own));
        }
    }
    for (id, to, own) in plan {
        let Some(mut item) = board.remove(id) else { continue };
        if let Some(span) = item.span.take() {
            stats.removed_spans.push(span);
        }
        if own {
            stats.combed += 1;
        } else {
            stats.pushed_out += 1;
        }
        soft.retain(|s| *s != id);
        let Shape::Capsule { r, .. } = item.shape else { continue };
        if to.len() == 1 {
            item.shape = Shape::Capsule { a: to[0], b: to[0], r };
            item.hole = item.hole.map(|(_, hr)| (to[0], hr));
            board.restore(id, item);
            soft.push(id);
        } else {
            for w in to.windows(2).filter(|w| w[0] != w[1]) {
                soft.push(board.insert(Item { shape: Shape::Capsule { a: w[0], b: w[1], r }, ..item.clone() }));
            }
        }
    }
    stats
}

/// Footprints whose closest pads of different nets sit no further apart than `pitch`, centre to
/// centre: the parts whose escapes are worth combing.
pub fn fine_pitch(board: &Board, pitch: Nm) -> Vec<usize> {
    let mut centres: HashMap<usize, Vec<(Point, u32)>> = HashMap::new();
    for (_, it) in board.items() {
        if let Kind::Pad { footprint, .. } = it.kind {
            let bb = it.shape.bbox();
            centres.entry(footprint).or_default().push((Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2), it.net));
        }
    }
    let mut out: Vec<usize> = centres
        .into_iter()
        .filter(|(_, pads)| pads.len() >= 8)
        .filter(|(_, pads)| {
            pads.iter().enumerate().any(|(i, (p, n))| pads[i + 1..].iter().any(|(q, m)| m != n && p.distance(*q) <= pitch as f64))
        })
        .map(|(f, _)| f)
        .collect();
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edge_measures_depth_out_of_its_row_and_position_along_it() {
        let e = Edge { out: (0.0, 1.0), along: (1.0, 0.0), row: 1000.0, from: 0.0, to: 0.0, lanes: BTreeMap::new() };
        assert_eq!(e.depth(Point::new(5, 1500)), 500.0);
        assert_eq!(e.lateral(Point::new(5, 1500)), 5.0);
    }
}
