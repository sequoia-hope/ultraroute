//! Pads of a plane's net joined to the plane where they are not already: a via beside the pad,
//! dropped into the plane, and a short track to it, so the plane carries the net and no track
//! wires it pad to pad.

use std::collections::HashSet;

use board::{check_item, connectivity_without_pours, Board, Item, ItemId, Kind, Nm, Point, Shape};

use crate::grid::Profile;
use crate::report::{tell, Log};

/// Rings round a pad's edge a via is tried on, in nanometres beyond the pad.
const RINGS: [Nm; 9] = [100_000, 200_000, 350_000, 500_000, 700_000, 950_000, 1_300_000, 1_800_000, 2_500_000];
/// Directions tried on each ring.
const DIRECTIONS: usize = 36;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlaneStats {
    /// Plane-net pads that were not on their plane, and how many now are.
    pub pads: usize,
    pub joined: usize,
    pub laid: Vec<ItemId>,
}

fn centre(item: &Item) -> Point {
    let b = item.shape.bbox();
    Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2)
}

/// Joins every pad of a plane's net to its plane with a via beside it, where one fits.
pub fn join_planes(board: &mut Board, log: &Option<Log>) -> PlaneStats {
    let mut stats = PlaneStats::default();
    if board.planes == 0 {
        return stats;
    }
    let planes: Vec<(ItemId, u32, u64)> =
        board.items().filter(|(_, it)| board.is_plane(it) && it.net != 0).map(|(id, it)| (id, it.net, it.layers)).collect();
    let plane_nets: HashSet<u32> = planes.iter().map(|p| p.1).collect();
    // A pad is on its plane when its cluster holds the plane's pour.
    let conn = connectivity_without_pours(board);
    let on_plane: HashSet<usize> = planes.iter().filter_map(|(id, _, _)| conn.cluster.get(id).copied()).collect();
    let pads: Vec<ItemId> = board
        .items()
        .filter(|(id, it)| {
            matches!(it.kind, Kind::Pad { .. }) && plane_nets.contains(&it.net) && conn.cluster.get(id).is_some_and(|c| !on_plane.contains(c))
        })
        .map(|(id, _)| id)
        .collect();
    stats.pads = pads.len();
    for pad in pads {
        // An earlier via may have joined this pad's cluster already.
        let conn = connectivity_without_pours(board);
        if conn.cluster.get(&pad).is_some_and(|c| planes.iter().any(|(id, _, _)| conn.cluster.get(id) == Some(c))) {
            stats.joined += 1;
            continue;
        }
        let Some(item) = board.item(pad).cloned() else { continue };
        let fills: Vec<Shape> = planes.iter().filter(|p| p.1 == item.net).filter_map(|(id, _, _)| board.item(*id).map(|it| it.shape.clone())).collect();
        if let Some(laid) = drop_via(board, &item, &fills) {
            tell(log, "planes", || format!("{} joined to its plane", crate::report::item_name(board, pad)), None);
            stats.joined += 1;
            stats.laid.extend(laid);
        }
    }
    stats
}

/// The nearest via beside `pad` that lands in one of `fills`, the plane's copper the router
/// counts as joining it, and lays cleanly, with its track.
fn drop_via(board: &mut Board, pad: &Item, fills: &[Shape]) -> Option<Vec<ItemId>> {
    let profile = Profile::for_net(board, pad.net);
    let at = centre(pad);
    let b = pad.shape.bbox();
    let reach = ((b.max.x - b.min.x).max(b.max.y - b.min.y) / 2) + profile.via_radius;
    let layer = pad.layers.trailing_zeros() as usize;
    for ring in RINGS {
        let r = (reach + ring) as f64;
        for k in 0..DIRECTIONS {
            let t = std::f64::consts::TAU * k as f64 / DIRECTIONS as f64;
            let p = Point::new(at.x + (r * t.cos()).round() as Nm, at.y + (r * t.sin()).round() as Nm);
            if !fills.iter().any(|f| f.contains(p)) {
                continue;
            }
            let via = board.insert(Item {
                kind: Kind::Via,
                net: pad.net,
                layers: (1u64 << board.layers.len()) - 1,
                shape: Shape::Capsule { a: p, b: p, r: profile.via_radius },
                hole: Some((p, profile.drill_radius)),
                locked: false,
                span: None,
            });
            if !check_item(board, via).is_empty() {
                board.discard(via);
                continue;
            }
            let track = board.insert(Item {
                kind: Kind::Track,
                net: pad.net,
                layers: 1 << layer,
                shape: Shape::Capsule { a: at, b: p, r: profile.half_width },
                hole: None,
                locked: false,
                span: None,
            });
            if check_item(board, track).is_empty() {
                return Some(vec![via, track]);
            }
            board.discard(track);
            board.discard(via);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pads_drop_vias_to_their_plane_which_joins_them_and_keeps_tracks_off() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (1 \"In1.Cu\" signal) (31 \"B.Cu\" signal)) (net 0 \"\") (net 1 \"GND\")
             (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1)))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let pad = |x| Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(x, 10_000_000), b: Point::new(x, 10_000_000), r: 300_000 },
            hole: None,
            locked: true,
            span: None,
        };
        board.insert(pad(5_000_000));
        board.insert(pad(15_000_000));
        let fill = vec![Point::new(1_000_000, 1_000_000), Point::new(19_000_000, 1_000_000), Point::new(19_000_000, 19_000_000), Point::new(1_000_000, 19_000_000)];
        board.insert(Item { kind: Kind::Pour { zone: 0, kept: false }, net: 1, layers: 2, shape: Shape::Polygon(fill), hole: None, locked: false, span: None });
        assert_eq!(connectivity_without_pours(&board).airlines.len(), 1, "two pads, the pour not counted yet");
        board.set_planes(2);
        let mut stats = join_planes(&mut board, &None);
        assert_eq!((stats.pads, stats.joined), (2, 2), "{stats:?}");
        assert!(connectivity_without_pours(&board).airlines.is_empty(), "the plane joins the two vias");
        // The cleanup keeps vias whose only other copper is the plane.
        crate::prune_dangling(&mut board, &mut stats.laid);
        assert_eq!(stats.laid.len(), 4);
        assert!(connectivity_without_pours(&board).airlines.is_empty());
        // No track may cross the plane layer.
        let track = board.insert(Item { kind: Kind::Track, net: 1, layers: 2, shape: Shape::Capsule { a: Point::new(5_000_000, 5_000_000), b: Point::new(6_000_000, 5_000_000), r: 100_000 }, hole: None, locked: false, span: None });
        assert!(check_item(&board, track).iter().any(|v| v.rule == board::Rule::Keepout));
    }
}
