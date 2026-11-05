//! Pads of a plane's net joined to the plane where they are not already: a via beside the pad,
//! dropped into the plane, and a short track to it, so the plane carries the net and no track
//! wires it pad to pad. They are laid as the router's own copper, which a later rip-up may take
//! up when another net needs the way and join the pad some other way.

use std::collections::HashSet;

use board::{check_item, connectivity_without_pours, Board, Item, ItemId, Kind, Nm, Point, Shape};

use crate::clock::Deadline;
use crate::grid::Profile;
use crate::report::{tell, Log};

/// Rings round a pad's edge a via is tried on, in nanometres beyond the pad.
const RINGS: [Nm; 9] = [100_000, 200_000, 350_000, 500_000, 700_000, 950_000, 1_300_000, 1_800_000, 2_500_000];
/// Directions tried on each ring.
const DIRECTIONS: usize = 36;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlaneStats {
    /// Pieces of a plane's net that were not on their plane, and how many now are.
    pub pads: usize,
    pub joined: usize,
    pub laid: Vec<ItemId>,
    /// With a reach, the plane nets' pads that must each have a via of their own within it, how
    /// many have one, and the copper laid for them: the router's own, which a signal that needs
    /// the room may rip, as an open signal is worse than a capacitor sharing its via.
    pub own: usize,
    pub owned: usize,
    pub own_laid: Vec<ItemId>,
}

/// A plane net's pad and the via of its own within the reach, if it has one.
#[derive(Debug, Clone, PartialEq)]
pub struct PadVia {
    pub pad: ItemId,
    pub via: Option<ItemId>,
    /// The nearest via of its copper's, edge to edge, in nanometres; infinite with none.
    pub nearest: f64,
}

/// The pads of a plane's net that reach no plane themselves: surface pads on layers no plane
/// takes, each wanting a via down to its plane, as a decoupling capacitor's ground does.
fn plane_pads(board: &Board) -> Vec<ItemId> {
    let nets: HashSet<u32> = board.items().filter(|(_, it)| board.is_plane(it) && it.net != 0).map(|(_, it)| it.net).collect();
    board
        .items()
        .filter(|(_, it)| matches!(it.kind, Kind::Pad { .. }) && nets.contains(&it.net) && it.hole.is_none() && it.layers & board.planes == 0 && it.layers != 0)
        .map(|(id, _)| id)
        .collect()
}

/// For every pad of a plane's net that reaches no plane itself, the via of its own its copper
/// joins within `reach` of it, edge to edge: each via counts for one pad, nearest pairs first,
/// so two pads sharing one via, or reaching it down another's track, have one between them.
pub fn pad_vias(board: &Board, reach: Nm) -> Vec<PadVia> {
    let conn = connectivity_without_pours(board);
    let pads = plane_pads(board);
    let mut pairs: Vec<(f64, ItemId, ItemId)> = Vec::new();
    let mut nearest: std::collections::HashMap<ItemId, f64> = Default::default();
    for &pad in &pads {
        let Some(p) = board.item(pad) else { continue };
        let Some(&cluster) = conn.cluster.get(&pad) else { continue };
        let around = p.shape.bbox().grow(reach.max(5_000_000));
        let layer = p.layers.trailing_zeros() as usize;
        for v in board.near(layer, around) {
            let Some(it) = board.item(v) else { continue };
            if it.kind != Kind::Via || it.net != p.net || conn.cluster.get(&v) != Some(&cluster) {
                continue;
            }
            let gap = p.shape.gap(&it.shape);
            let n = nearest.entry(pad).or_insert(f64::INFINITY);
            *n = n.min(gap);
            if gap <= reach as f64 {
                pairs.push((gap, pad, v));
            }
        }
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let (mut owner, mut used): (std::collections::HashMap<ItemId, ItemId>, HashSet<ItemId>) = Default::default();
    for (_, pad, v) in pairs {
        if !owner.contains_key(&pad) && used.insert(v) {
            owner.insert(pad, v);
        }
    }
    pads.into_iter().map(|pad| PadVia { pad, via: owner.get(&pad).copied(), nearest: nearest.get(&pad).copied().unwrap_or(f64::INFINITY) }).collect()
}

/// Gives each plane-net pad without one a via of its own within `reach`, as [`join_planes`] does
/// first: once routing is done, for the pads whose via a signal took, where one still fits.
pub fn own_planes_vias(board: &mut Board, log: &Option<Log>, deadline: &Deadline, reach: Nm) -> PlaneStats {
    let _b = board::work::enter(board::work::Bucket::Geometry);
    let mut stats = PlaneStats::default();
    if board.planes != 0 {
        own_vias(board, log, deadline, reach, &mut stats);
    }
    stats
}

/// Gives each plane-net pad without one a via of its own within `reach`: in the pad, where the
/// pad holds a whole via and the rules let one stand there (an exposed pad), else beside it on
/// the rings no further out than `reach`, with a short track to it.
fn own_vias(board: &mut Board, log: &Option<Log>, deadline: &Deadline, reach: Nm, stats: &mut PlaneStats) {
    let planes: Vec<(ItemId, u32)> = board.items().filter(|(_, it)| board.is_plane(it) && it.net != 0).map(|(id, it)| (id, it.net)).collect();
    let mut want: Vec<ItemId> = pad_vias(board, reach).into_iter().filter(|pv| pv.via.is_none()).map(|pv| pv.pad).collect();
    stats.own = plane_pads(board).len();
    stats.owned = stats.own - want.len();
    want.sort_unstable();
    for pad in want {
        if deadline.expired() {
            break;
        }
        let Some(item) = board.item(pad).cloned() else { continue };
        let fills: Vec<Shape> = planes.iter().filter(|p| p.1 == item.net).filter_map(|(p, _)| board.item(*p).map(|it| it.shape.clone())).collect();
        let profile = Profile::for_net(board, item.net);
        let layer = item.layers.trailing_zeros() as usize;
        let at = centre(&item);
        let in_pad = (item.shape.depth(at) >= profile.via_radius as f64 && fills.iter().any(|f| f.depth(at) >= profile.via_radius as f64))
            .then(|| {
                let via = board.insert(Item {
                    kind: Kind::Via,
                    net: item.net,
                    layers: (1u64 << board.layers.len()) - 1,
                    shape: Shape::Capsule { a: at, b: at, r: profile.via_radius },
                    hole: Some((at, profile.drill_radius)),
                    locked: false,
                    span: None,
                });
                if check_item(board, via).is_empty() {
                    Some(vec![via])
                } else {
                    board.discard(via);
                    None
                }
            })
            .flatten();
        let b = item.shape.bbox();
        let extent = (b.max.x - b.min.x).max(b.max.y - b.min.y) / 2;
        let laid = in_pad.or_else(|| drop_via_within(board, item.net, layer, at, extent + profile.via_radius, &fills, &profile, reach));
        let Some(laid) = laid else {
            tell(log, "planes", || format!("{} has no room for a via of its own within {:.2} mm", crate::report::item_name(board, pad), reach as f64 * 1e-6), None);
            continue;
        };
        tell(log, "planes", || format!("{} has a via of its own", crate::report::item_name(board, pad)), None);
        stats.owned += 1;
        stats.own_laid.extend(laid);
    }
}

fn centre(item: &Item) -> Point {
    let b = item.shape.bbox();
    Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2)
}

/// Joins every piece of a plane's net that is not on its plane, a pad or locked track, with a
/// via beside it, where one fits: beside a pad first, else at a track's end; until `deadline`.
///
/// With `reach`, every pad of a plane's net first gets a via of its own within it, where one
/// fits (`own_vias`); the pieces still off their plane are joined after.
pub fn join_planes(board: &mut Board, log: &Option<Log>, deadline: &Deadline, reach: Option<Nm>) -> PlaneStats {
    let _b = board::work::enter(board::work::Bucket::Geometry);
    let mut stats = PlaneStats::default();
    if board.planes == 0 {
        return stats;
    }
    if let Some(reach) = reach {
        own_vias(board, log, deadline, reach, &mut stats);
    }
    let planes: Vec<(ItemId, u32)> = board.items().filter(|(_, it)| board.is_plane(it) && it.net != 0).map(|(id, it)| (id, it.net)).collect();
    let plane_nets: HashSet<u32> = planes.iter().map(|p| p.1).collect();
    let on_plane = |board: &Board, id: ItemId| {
        let conn = connectivity_without_pours(board);
        conn.cluster.get(&id).is_some_and(|c| planes.iter().any(|(p, _)| conn.cluster.get(p) == Some(c)))
    };
    // The pieces off their plane: each cluster's items, pads before tracks.
    let conn = connectivity_without_pours(board);
    let mut pieces: std::collections::BTreeMap<usize, Vec<ItemId>> = Default::default();
    for (id, it) in board.items() {
        if plane_nets.contains(&it.net) && matches!(it.kind, Kind::Pad { .. } | Kind::Track) {
            if let Some(&c) = conn.cluster.get(&id) {
                pieces.entry(c).or_default().push(id);
            }
        }
    }
    let on: HashSet<usize> = planes.iter().filter_map(|(id, _)| conn.cluster.get(id).copied()).collect();
    pieces.retain(|c, _| !on.contains(c));
    stats.pads = pieces.len();
    for (_, mut members) in pieces {
        if deadline.expired() {
            break;
        }
        members.sort_by_key(|id| board.item(*id).is_some_and(|it| it.kind == Kind::Track));
        // An earlier via may have joined this piece already.
        if on_plane(board, members[0]) {
            stats.joined += 1;
            continue;
        }
        for id in members {
            let Some(item) = board.item(id).cloned() else { continue };
            let fills: Vec<Shape> = planes.iter().filter(|p| p.1 == item.net).filter_map(|(p, _)| board.item(*p).map(|it| it.shape.clone())).collect();
            let profile = Profile::for_net(board, item.net);
            let layer = item.layers.trailing_zeros() as usize;
            let anchors: Vec<(Point, Nm)> = match (&item.kind, &item.shape) {
                (Kind::Track, Shape::Capsule { a, b, r }) => vec![(*a, *r), (*b, *r)],
                _ => {
                    let b = item.shape.bbox();
                    vec![(centre(&item), (b.max.x - b.min.x).max(b.max.y - b.min.y) / 2)]
                }
            };
            // A track's end takes a via on itself, with nothing more to lay.
            if item.kind == Kind::Track {
                let at_end = anchors.iter().find_map(|&(at, _)| {
                    if !fills.iter().any(|f| f.depth(at) >= profile.via_radius as f64) {
                        return None;
                    }
                    let via = board.insert(Item {
                        kind: Kind::Via,
                        net: item.net,
                        layers: (1u64 << board.layers.len()) - 1,
                        shape: Shape::Capsule { a: at, b: at, r: profile.via_radius },
                        hole: Some((at, profile.drill_radius)),
                        locked: false,
                        span: None,
                    });
                    if check_item(board, via).is_empty() {
                        Some(vec![via])
                    } else {
                        board.discard(via);
                        None
                    }
                });
                if let Some(laid) = at_end {
                    tell(log, "planes", || format!("{} joined to its plane", crate::report::item_name(board, id)), None);
                    stats.joined += 1;
                    stats.laid.extend(laid);
                    break;
                }
            }
            let laid = anchors.into_iter().find_map(|(at, extent)| drop_via(board, item.net, layer, at, extent + profile.via_radius, &fills, &profile));
            if let Some(laid) = laid {
                tell(log, "planes", || format!("{} joined to its plane", crate::report::item_name(board, id)), None);
                stats.joined += 1;
                stats.laid.extend(laid);
                break;
            }
        }
    }
    stats
}

/// How far it is from `p` to the nearest copper of another net, up to a millimetre.
fn room_round(board: &Board, p: Point, net: u32) -> f64 {
    let probe = Shape::Capsule { a: p, b: p, r: 0 };
    let near = board::BBox::of([p]).grow(1_000_000);
    (0..board.layers.len())
        .flat_map(|l| board.near(l, near))
        .filter_map(|o| board.item(o))
        .filter(|o| o.is_copper() && o.net != net && !o.refilled())
        .map(|o| o.shape.gap_within(&probe, 1_000_000.0))
        .fold(1_000_000.0, f64::min)
}

/// A via of `net` at `p` and a track on `layer` to it from `at`, if both lay cleanly.
fn lay(board: &mut Board, net: u32, layer: usize, at: Point, p: Point, profile: &Profile) -> Option<Vec<ItemId>> {
    let via = board.insert(Item {
        kind: Kind::Via,
        net,
        layers: (1u64 << board.layers.len()) - 1,
        shape: Shape::Capsule { a: p, b: p, r: profile.via_radius },
        hole: Some((p, profile.drill_radius)),
        locked: false,
        span: None,
    });
    if !check_item(board, via).is_empty() {
        board.discard(via);
        return None;
    }
    let track = board.insert(Item {
        kind: Kind::Track,
        net,
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
    None
}

/// The nearest via round `at` (copper reaching `reach` from it on `layer`) that lands in one of
/// `fills`, the plane's copper the router counts as joining it, and lays cleanly, with its track.
fn drop_via(board: &mut Board, net: u32, layer: usize, at: Point, reach: Nm, fills: &[Shape], profile: &Profile) -> Option<Vec<ItemId>> {
    drop_via_within(board, net, layer, at, reach, fills, profile, Nm::MAX)
}

/// As [`drop_via`], on the rings no further out than `most` beyond the copper.
#[allow(clippy::too_many_arguments)]
fn drop_via_within(board: &mut Board, net: u32, layer: usize, at: Point, reach: Nm, fills: &[Shape], profile: &Profile, most: Nm) -> Option<Vec<ItemId>> {
    for ring in RINGS.into_iter().filter(|&r| r <= most) {
        // Of the sites on this ring that lay cleanly, the one with the most room round it, so a
        // plane's vias leave other nets' ways past the pad as open as they can.
        let r = (reach + ring) as f64;
        let mut best: Option<(f64, Point)> = None;
        for k in 0..DIRECTIONS {
            let t = std::f64::consts::TAU * k as f64 / DIRECTIONS as f64;
            let p = Point::new(at.x + (r * t.cos()).round() as Nm, at.y + (r * t.sin()).round() as Nm);
            if !fills.iter().any(|f| f.depth(p) >= profile.via_radius as f64) {
                continue;
            }
            let Some(laid) = lay(board, net, layer, at, p, profile) else { continue };
            // A via-avoid area counts as no room at all: a site there is the last resort.
            let room = if board.in_via_avoid(p, profile.via_radius) { -1.0 } else { room_round(board, p, net) };
            for id in laid.into_iter().rev() {
                board.discard(id);
            }
            if best.is_none_or(|b| room > b.0) {
                best = Some((room, p));
            }
        }
        if let Some((_, p)) = best {
            return lay(board, net, layer, at, p, profile);
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
        let mut stats = join_planes(&mut board, &None, &Default::default(), None);
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

    /// Two ground pads 3 mm apart, joined by a track: one via joins the pair to the plane, and
    /// with a reach each has a via of its own, as a decoupling capacitor's ground wants.
    #[test]
    fn with_a_reach_each_plane_pad_gets_a_via_of_its_own() {
        let make = || {
            let (pcb, copper) = kicad_pcb::parse_board(
                "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (1 \"In1.Cu\" signal) (31 \"B.Cu\" signal)) (net 0 \"\") (net 1 \"GND\")
                 (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1)))",
            )
            .unwrap();
            let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
            let pad = |x| Item { kind: Kind::Pad { footprint: 0, number: "2".into(), plated: false }, net: 1, layers: 1, shape: Shape::Capsule { a: Point::new(x, 10_000_000), b: Point::new(x, 10_000_000), r: 300_000 }, hole: None, locked: true, span: None };
            let pads = [board.insert(pad(8_000_000)), board.insert(pad(11_000_000))];
            board.insert(Item { kind: Kind::Track, net: 1, layers: 1, shape: Shape::Capsule { a: Point::new(8_000_000, 10_000_000), b: Point::new(11_000_000, 10_000_000), r: 125_000 }, hole: None, locked: false, span: None });
            let fill = vec![Point::new(1_000_000, 1_000_000), Point::new(19_000_000, 1_000_000), Point::new(19_000_000, 19_000_000), Point::new(1_000_000, 19_000_000)];
            board.insert(Item { kind: Kind::Pour { zone: 0, kept: false }, net: 1, layers: 2, shape: Shape::Polygon(fill), hole: None, locked: false, span: None });
            board.set_planes(2);
            (board, pads)
        };
        let vias = |b: &Board| b.items().filter(|(_, it)| it.kind == Kind::Via).count();
        let (mut shared, _) = make();
        join_planes(&mut shared, &None, &Default::default(), None);
        assert_eq!(vias(&shared), 1, "the pair shares one via");
        assert_eq!(pad_vias(&shared, 1_000_000).iter().filter(|pv| pv.via.is_some()).count(), 1, "the other reaches it 3 mm down the track");
        let (mut own, pads) = make();
        let stats = join_planes(&mut own, &None, &Default::default(), Some(1_000_000));
        assert_eq!((stats.own, stats.owned), (2, 2), "{stats:?}");
        assert_eq!(vias(&own), 2);
        let owned = pad_vias(&own, 1_000_000);
        assert!(pads.iter().all(|p| owned.iter().any(|pv| pv.pad == *p && pv.via.is_some_and(|v| own.item(v).unwrap().shape.gap(&own.item(*p).unwrap().shape) <= 1_000_000.0))), "{owned:?}");
        assert!(stats.own_laid.iter().all(|&id| !own.item(id).unwrap().locked), "the router's own copper, a signal may rip");
        // Taken by a signal, a pad's via is laid again once routing is done.
        let via = stats.own_laid.iter().copied().find(|&id| own.item(id).unwrap().kind == Kind::Via).unwrap();
        own.remove(via);
        assert_eq!(pad_vias(&own, 1_000_000).iter().filter(|pv| pv.via.is_none()).count(), 1);
        let again = own_planes_vias(&mut own, &None, &Default::default(), 1_000_000);
        assert_eq!((again.own, again.owned), (2, 2), "{again:?}");
    }
}
