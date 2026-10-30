//! Fine-pitch parts, whose escapes are hard, and moving the router's copper a point at a time
//! without breaking it. A point's move depends only on where it is and whose it is, so pieces of
//! copper that meet at a point move together and stay joined; points on pads do not move at all.
//! What a move leaves breaking a rule is for the caller to rip and route again.

use std::collections::HashMap;

use board::{BBox, Board, Item, ItemId, Kind, Nm, Point, Shape};
use kicad_pcb::Span;

use crate::flow::{cut, junctions, layer_of};

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
/// than `pitch`, centre to centre: the parts whose escapes are hard. Pads on opposite
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_copper_bends_a_free_track_while_pad_ends_hold_still() {
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
        // A repulsor above the track's middle: 0.2 mm out at 0.5 mm, falling off as 1/d beyond.
        let (centre, strength, radius) = (Point::new(1_000_000, -500_000), 200_000.0, 500_000.0);
        let away = |p: Point, _: u32, _: Option<usize>| {
            let (dx, dy) = ((p.x - centre.x) as f64, (p.y - centre.y) as f64);
            let d = dx.hypot(dy);
            let u = 2.0 * strength * (d / radius) / (1.0 + (d / radius).powi(2));
            let by = Point::new((u * dx / d).round() as i64, (u * dy / d).round() as i64);
            (by != Point::new(0, 0)).then_some((by, false))
        };
        let (_, moved, _) = shift_copper(&mut board, &mut soft, 200_000.0, &away);
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
