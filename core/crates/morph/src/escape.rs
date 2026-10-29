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
#[derive(Debug, Clone)]
pub struct Edge {
    pub out: (f64, f64),
    pub along: (f64, f64),
    pub row: f64,
    pub from: f64,
    pub to: f64,
    pub lanes: BTreeMap<u32, Vec<f64>>,
}

impl Edge {
    pub fn depth(&self, p: Point) -> f64 {
        p.x as f64 * self.out.0 + p.y as f64 * self.out.1 - self.row
    }

    pub fn lateral(&self, p: Point) -> f64 {
        p.x as f64 * self.along.0 + p.y as f64 * self.along.1
    }
}

/// The package's edges with their lanes: each pad is assigned to the side of the pads' bounding
/// box it is nearest, and a pad far from every side (an exposed pad) to none.
pub fn edges(board: &Board, footprint: usize) -> Vec<Edge> {
    let pads: Vec<&Item> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Pad { footprint: f, .. } if f == footprint)).map(|(_, it)| it).collect();
    let Some(b) = pads.iter().map(|p| p.shape.bbox()).reduce(BBox::union) else { return Vec::new() };
    let centre = |p: &Item| {
        let bb = p.shape.bbox();
        Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2)
    };
    // A pad belongs to a side when its centre lies within a pad's own size of it.
    type Side = ((f64, f64), (f64, f64), f64);
    let sides: [Side; 4] = [
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
    // The move of a point of `net`'s copper: onto its nearest lane on an edge whose zone holds
    // it, or out of a zone where it has no lane. The nearest edge wins where zones overlap.
    let rule = |p: Point, net: u32| -> Option<(Point, bool)> {
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
    let (own, foreign, spans) = move_copper(board, soft, &rule);
    (stats.combed, stats.pushed_out, stats.removed_spans) = (own, foreign, spans);
    stats
}

/// Clears the lane of one pad of a fine-pitch part: other nets' copper within half a pitch of
/// the line straight out from the pad, across the fanout zone, slides sideways to the nearer
/// side of that corridor, fully near the package and fading to nothing at the zone's edge. The
/// pad's own net and everything outside the corridor stay where they are. How many pieces moved.
pub fn clear_lane(board: &mut Board, footprint: usize, pad: ItemId, options: &EscapeOptions, soft: &mut Vec<ItemId>) -> (usize, Vec<Span>) {
    let Some(p) = board.item(pad) else { return (0, Vec::new()) };
    let net = p.net;
    let bb = p.shape.bbox();
    let c = Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2);
    let edges = edges(board, footprint);
    let Some(e) = edges.iter().filter(|e| e.lanes.get(&net).is_some_and(|l| l.iter().any(|t| (t - e.lateral(c)).abs() < 1.0))).min_by(|a, b| (-a.depth(c)).total_cmp(&-b.depth(c))) else {
        return (0, Vec::new());
    };
    let lane = e.lateral(c);
    // Half the distance to the nearest other lane on the edge, so neighbouring escapes stay put.
    let pitch = e.lanes.values().flatten().filter(|t| (*t - lane).abs() > 1.0).map(|t| (t - lane).abs()).fold(f64::INFINITY, f64::min);
    let half = if pitch.is_finite() { pitch / 2.0 } else { 200_000.0 };
    let (zone, full) = (options.zone as f64, options.zone as f64 * options.full.clamp(0.0, 1.0));
    let rule = |p: Point, n: u32| -> Option<(Point, bool)> {
        let (d, t) = (e.depth(p), e.lateral(p) - lane);
        if n == net || d < 0.0 || d > zone || t.abs() >= half {
            return None;
        }
        let w = if d <= full { 1.0 } else { ((zone - d) / (zone - full).max(1.0)).clamp(0.0, 1.0) };
        let side = if t >= 0.0 { 1.0 } else { -1.0 };
        let by = side * (half - t.abs()) * w;
        Some((Point::new((e.along.0 * by).round() as i64, (e.along.1 * by).round() as i64), false))
    };
    let (_, moved, spans) = move_copper(board, soft, &rule);
    (moved, spans)
}

fn move_copper(board: &mut Board, soft: &mut Vec<ItemId>, rule: &dyn Fn(Point, u32) -> Option<(Point, bool)>) -> (usize, usize, Vec<Span>) {
    shift_copper(board, soft, PIECE, &|p, net, _| rule(p, net))
}

/// Moves every unlocked track and via by `rule`, a point at a time, without breaking it: a point
/// touching a pad or locked copper of its net stays put, and copper ending inside a free via
/// moves as the via's centre does, so every piece meeting at a point gets the same move. The
/// rule is told the layer of a track's point, and no layer for a via's centre, which is on all
/// of them. Tracks are cut into pieces no longer than `piece` so they bend. Counts the pieces
/// moved by the rule's two kinds of move, and returns the board file's spans of what moved.
pub fn shift_copper(board: &mut Board, soft: &mut Vec<ItemId>, piece: f64, rule: &dyn Fn(Point, u32, Option<usize>) -> Option<(Point, bool)>) -> (usize, usize, Vec<Span>) {
    // A point stays put when copper there could touch a pad, on any layer: every piece meeting
    // at a point must get the same answer, whatever its own width or layers.
    let reach = board
        .items()
        .filter_map(|(_, it)| match (&it.kind, &it.shape) {
            (Kind::Track | Kind::Via, Shape::Capsule { r, .. }) => Some(*r),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let pads: Vec<Shape> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Pad { .. })).map(|(_, it)| it.shape.clone()).collect();
    let locked: Vec<(Shape, u32)> =
        board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && it.locked).map(|(_, it)| (it.shape.clone(), it.net)).collect();
    let touches = |s: &Shape, p: Point| {
        let dot = Shape::Capsule { a: p, b: p, r: reach };
        s.bbox().grow(reach).overlaps(BBox::of([p])) && s.gap(&dot) == 0.0
    };
    let held = |p: Point, net: u32| pads.iter().any(|s| touches(s, p)) || locked.iter().any(|(s, n)| *n == net && touches(s, p));
    let vias: Vec<(Point, Nm, u32)> = board
        .items()
        .filter_map(|(_, it)| match (&it.kind, &it.shape) {
            (Kind::Via, Shape::Capsule { a, r, .. }) if !it.locked => Some((*a, *r, it.net)),
            _ => None,
        })
        .collect();
    let via_at = |p: Point, net: u32| vias.iter().find(|(c, r, n)| *n == net && c.distance(p) <= *r as f64).map(|(c, _, _)| *c);
    let shift = |p: Point, net: u32, layer: Option<usize>| {
        let (p, layer) = via_at(p, net).map_or((p, layer), |c| (c, None));
        if held(p, net) { None } else { rule(p, net, layer) }
    };
    let junctions = junctions(board);
    let mut ids: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked).map(|(id, _)| id).collect();
    ids.sort_unstable();
    let mut plan: Vec<(ItemId, Vec<Point>, bool)> = Vec::new();
    for id in ids {
        let Some(it) = board.item(id) else { continue };
        let Shape::Capsule { a, b, .. } = it.shape else { continue };
        let points = if a == b { vec![a] } else { cut(a, b, junctions.get(&layer_of(it)).map_or(&[][..], Vec::as_slice), piece) };
        let layer = (it.kind == Kind::Track).then(|| layer_of(it));
        let mut first_kind = false;
        let mut moved = false;
        let to: Vec<Point> = points
            .iter()
            .map(|&p| match shift(p, it.net, layer) {
                Some((by, kind)) if by != Point::new(0, 0) => {
                    moved = true;
                    first_kind |= kind;
                    Point::new(p.x + by.x, p.y + by.y)
                }
                _ => p,
            })
            .collect();
        if moved {
            plan.push((id, to, first_kind));
        }
    }
    let (mut first, mut second, mut spans) = (0, 0, Vec::new());
    for (id, to, kind) in plan {
        let Some(mut item) = board.remove(id) else { continue };
        if let Some(span) = item.span.take() {
            spans.push(span);
        }
        if kind {
            first += 1;
        } else {
            second += 1;
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
    (first, second, spans)
}

/// Footprints whose closest pads of different nets on a shared copper layer sit no further apart
/// than `pitch`, centre to centre: the parts whose escapes are worth combing. Pads on opposite
/// sides of the board, such as the front and back fingers of a card-edge connector at the same
/// `(x, y)`, do not crowd each other's layer.
pub fn fine_pitch(board: &Board, pitch: Nm) -> Vec<usize> {
    let mut centres: HashMap<usize, Vec<(Point, u32, u64)>> = HashMap::new();
    for (_, it) in board.items() {
        if let Kind::Pad { footprint, .. } = it.kind {
            let bb = it.shape.bbox();
            centres
                .entry(footprint)
                .or_default()
                .push((Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2), it.net, it.layers));
        }
    }
    let mut out: Vec<usize> = centres
        .into_iter()
        // Chip-scale parts count too: a four-ball load switch at 0.4 mm escapes as hard as a QFN.
        .filter(|(_, pads)| pads.len() >= 4)
        .filter(|(_, pads)| {
            pads.iter()
                .enumerate()
                .any(|(i, (p, n, la))| pads[i + 1..].iter().any(|(q, m, lb)| m != n && la & lb != 0 && p.distance(*q) <= pitch as f64))
        })
        .map(|(f, _)| f)
        .collect();
    out.sort_unstable();
    out
}

/// One pole of a smooth radial field: positive `strength` pushes copper away from `center`,
/// negative draws it in, zero at `center` itself and peaking one `radius` out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pole {
    pub center: Point,
    /// Radial move one `radius` from `center`, in nanometres; negative attracts.
    pub strength: f64,
    /// Distance at which the move peaks and beyond which it decays, in nanometres.
    pub radius: f64,
}

/// Combined displacement `(dx, dy)` in nanometres at `p` from `poles`. Because each pole goes
/// linearly to zero at its centre, a small attractor step never pulls a point past the pole.
pub fn poles_at(poles: &[Pole], p: Point) -> (f64, f64) {
    let (mut sx, mut sy) = (0.0, 0.0);
    for pole in poles {
        let (dx, dy) = ((p.x - pole.center.x) as f64, (p.y - pole.center.y) as f64);
        let d = dx.hypot(dy);
        if d == 0.0 {
            continue;
        }
        let r = pole.radius.max(1.0);
        let u = 2.0 * pole.strength * (d / r) / (1.0 + (d / r).powi(2));
        sx += u * dx / d;
        sy += u * dy / d;
    }
    (sx, sy)
}

/// Moves every unlocked track and via one step along `poles`, holding pads and locked copper
/// still and keeping copper joined at junctions and vias.
pub fn shift_poles(board: &mut Board, soft: &mut Vec<ItemId>, poles: &[Pole], piece: f64) -> (usize, Vec<Span>) {
    let rule = |p: Point, _net: u32, _layer: Option<usize>| -> Option<(Point, bool)> {
        let (dx, dy) = poles_at(poles, p);
        let by = Point::new(dx.round() as i64, dy.round() as i64);
        (by != Point::new(0, 0)).then_some((by, false))
    };
    let (_, moved, spans) = shift_copper(board, soft, piece, &rule);
    (moved, spans)
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

    #[test]
    fn poles_repel_attract_and_superpose_smoothly() {
        let repel = Pole { center: Point::new(0, 0), strength: 1000.0, radius: 10_000.0 };
        assert_eq!(poles_at(&[repel], Point::new(0, 0)), (0.0, 0.0));
        let (dx, dy) = poles_at(&[repel], Point::new(10_000, 0));
        assert!((dx - 1000.0).abs() < 1e-9 && dy.abs() < 1e-9);

        let attract = Pole { strength: -600.0, ..repel };
        let (ax, _) = poles_at(&[attract], Point::new(10_000, 0));
        assert!((ax + 600.0).abs() < 1e-9);

        // Between a repulsor on the left and an attractor on the right, both push rightward.
        let pair = [
            Pole { center: Point::new(-10_000, 0), strength: 500.0, radius: 10_000.0 },
            Pole { center: Point::new(10_000, 0), strength: -500.0, radius: 10_000.0 },
        ];
        let (px, py) = poles_at(&pair, Point::new(0, 0));
        assert!((px - 1000.0).abs() < 1e-9 && py.abs() < 1e-9);
    }

    #[test]
    fn shift_poles_bends_a_free_track_while_pad_ends_hold_still() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"A\"))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        for x in [0, 2_000_000] {
            board.insert(Item {
                kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
                net: 1,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(x, 0), b: Point::new(x, 0), r: 100_000 },
                hole: None,
                locked: true,
                span: None,
            });
        }
        let tr = board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(2_000_000, 0), r: 50_000 },
            hole: None,
            locked: false,
            span: None,
        });
        let mut soft = vec![tr];
        let pole = Pole { center: Point::new(1_000_000, -500_000), strength: 200_000.0, radius: 500_000.0 };
        let (moved, _) = shift_poles(&mut board, &mut soft, &[pole], 200_000.0);
        assert_eq!(moved, 1);
        assert!(board::connectivity_without_pours(&board).airlines.is_empty(), "pads stay joined");
        let max_y = soft
            .iter()
            .filter_map(|&id| match board.item(id)?.shape {
                Shape::Capsule { a, b, .. } => Some(a.y.max(b.y)),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        assert!(max_y > 150_000, "middle of the track bowed away from the repulsor: {max_y}");
    }

    #[test]
    fn fine_pitch_ignores_opposite_side_edge_connector_fingers_at_the_same_xy() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal)) (net 0 \"\") (net 1 \"A\") (net 2 \"B\") (net 3 \"C\") (net 4 \"D\"))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        // Footprint 0: wide-pitch (2.54 mm) card-edge fingers paired on F.Cu and B.Cu at identical (x, y).
        for (k, (x, layer, net)) in [(0, 1u64, 1u32), (0, 2, 2), (2_540_000, 1, 3), (2_540_000, 2, 4)].into_iter().enumerate() {
            board.insert(Item {
                kind: Kind::Pad { footprint: 0, number: format!("{}", k + 1), plated: false },
                net,
                layers: layer,
                shape: Shape::Capsule { a: Point::new(x, 0), b: Point::new(x, 0), r: 300_000 },
                hole: None,
                locked: true,
                span: None,
            });
        }
        // Footprint 1: four 0.4 mm pitch pads all on F.Cu.
        for k in 0..4 {
            board.insert(Item {
                kind: Kind::Pad { footprint: 1, number: format!("{}", k + 1), plated: false },
                net: (k + 1) as u32,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(k * 400_000, 5_000_000), b: Point::new(k * 400_000, 5_000_000), r: 100_000 },
                hole: None,
                locked: true,
                span: None,
            });
        }
        assert_eq!(fine_pitch(&board, 500_000), vec![1]);
    }
}
