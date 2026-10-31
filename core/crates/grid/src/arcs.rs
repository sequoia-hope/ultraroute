//! Corners of the router's copper rounded into arcs, as KiCad draws them: where two of its tracks
//! meet at an angle and nothing else does, the corner is cut by the widest arc up to a given
//! radius that keeps every rule, the tracks either side shortened to meet it. On the board an arc
//! stands as its chords, as one read from a file does; it is written as one `(arc …)`.

use std::collections::HashSet;

use board::{arc_chords, check_item, joined, Board, Item, ItemId, Kind, Nm, Point, Shape};

use crate::cleanup::chains;

/// An arc laid in place of a corner: its chords on the board, and the arc they stand for.
#[derive(Debug, Clone, PartialEq)]
pub struct RoundArc {
    pub chords: Vec<ItemId>,
    pub start: Point,
    pub mid: Point,
    pub end: Point,
    /// The track's width, not the chords' (each grown by its sag).
    pub width: Nm,
    pub layers: u64,
    pub net: u32,
}

/// Corners turning by less than this are left as they are: there is nothing to round.
const LEAST_TURN: f64 = 1.0 * std::f64::consts::PI / 180.0;
/// Nor sharper than a 135° turn (a hairpin), whose arc would be as long as the tracks.
const MOST_TURN: f64 = 0.75 * std::f64::consts::PI + 1e-6;

/// Rounds the corners of the router's tracks in `soft` (unlocked, between fixed points as
/// tightening has them) with arcs of at most `radius`, halving it where the largest that fits
/// breaks a rule, down to the track's width. `soft` is updated with the tracks and chords that
/// replace them; the arcs laid are returned.
pub fn round_corners(board: &mut Board, soft: &mut Vec<ItemId>, radius: Nm) -> Vec<RoundArc> {
    let soft_set: HashSet<ItemId> = soft.iter().copied().filter(|id| board.item(*id).is_some_and(|it| !it.locked)).collect();
    let mut arcs = Vec::new();
    let mut replaced: HashSet<ItemId> = HashSet::new();
    let mut added: Vec<ItemId> = Vec::new();
    for chain in chains(board, &soft_set) {
        let n = chain.tracks.len();
        let pts = &chain.points;
        // The piece standing for each track of the chain, and where it starts: a corner rounded
        // before it moves its start along.
        let mut piece: Vec<Option<ItemId>> = chain.tracks.iter().map(|&t| Some(t)).collect();
        let mut from: Vec<Point> = pts[..n].to_vec();
        for k in 1..n {
            let (Some(before), Some(after)) = (piece[k - 1], piece[k]) else { continue };
            let (p, q) = (pts[k], pts[k + 1]);
            let s = from[k - 1];
            let unit = |a: Point, b: Point| {
                let l = a.distance(b);
                [(b.x - a.x) as f64 / l, (b.y - a.y) as f64 / l]
            };
            if s == p || p == q {
                continue;
            }
            let (u, v) = (unit(pts[k - 1], p), unit(p, q));
            let cross = u[0] * v[1] - u[1] * v[0];
            let turn = cross.abs().atan2(u[0] * v[0] + u[1] * v[1]);
            if !(LEAST_TURN..=MOST_TURN).contains(&turn) {
                continue;
            }
            // The track after is shared with the next corner, unless it ends the chain.
            let room = s.distance(p).min(if k + 1 < n { p.distance(q) / 2.0 } else { p.distance(q) });
            let tan = (turn / 2.0).tan();
            let mut r = (radius as f64).min(room / tan);
            // Whatever joined the two tracks must still join what replaces them.
            let partners = partners(board, chain.net, &[before, after]);
            while r >= chain.half_width as f64 {
                if let Some((laid, arc)) = try_arc(board, &chain, [s, p, q], [u, v], cross, r, [before, after], &partners) {
                    replaced.extend([before, after]);
                    // The piece after may be replaced in turn by the next corner's.
                    added.extend(laid.straight_before.into_iter().chain(arc.chords.iter().copied()).chain(laid.straight_after));
                    piece[k - 1] = None;
                    piece[k] = laid.straight_after;
                    from[k] = arc.end;
                    arcs.push(arc);
                    break;
                }
                r /= 2.0;
            }
        }
    }
    soft.retain(|id| !replaced.contains(id));
    soft.extend(added.into_iter().filter(|id| !replaced.contains(id)));
    soft.retain(|id| board.item(*id).is_some());
    arcs
}

struct Laid {
    straight_before: Option<ItemId>,
    straight_after: Option<ItemId>,
}

/// The copper of `net` joined to any of `ids`, not among them.
fn partners(board: &Board, net: u32, ids: &[ItemId]) -> Vec<ItemId> {
    let mut out = Vec::new();
    for &t in ids {
        let Some(track) = board.item(t) else { continue };
        for l in (0..board.layers.len()).filter(|l| track.on(*l)) {
            for other in board.near(l, track.shape.bbox()) {
                if !ids.contains(&other) && board.item(other).is_some_and(|o| o.net == net) && joined(board, t, other) {
                    out.push(other);
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Whether `other` is joined to the pieces (centre lines, of half width `r`) as KiCad joins
/// copper: an end of a piece inside it, or, for a track, an end of it inside a piece. Overlap
/// alone, which [`joined`] allows, is not enough: a track ending inside a corner comes loose
/// when an arc cuts the corner away from its end, so what was anchored to the corner's tracks
/// must be anchored to what replaces them.
fn anchored(board: &Board, other: ItemId, pieces: &[(Point, Point)], r: Nm) -> bool {
    let Some(o) = board.item(other) else { return true };
    let inside_a_piece = |e: Point| pieces.iter().any(|&(p, q)| board::point_segment(e, p, q) <= r as f64);
    pieces.iter().any(|&(a, b)| o.shape.contains(a) || o.shape.contains(b))
        || matches!((&o.kind, &o.shape), (Kind::Track, Shape::Capsule { a, b, .. }) if inside_a_piece(*a) || inside_a_piece(*b))
}

/// The tracks `old` (from `s` to the corner `p`, and from `p` to `q`) taken up and laid again with
/// an arc of radius `r` cutting the corner, if nothing it lays breaks a rule and every partner
/// stays joined; else the board as it was.
#[allow(clippy::too_many_arguments)]
fn try_arc(board: &mut Board, chain: &crate::cleanup::Chain, [s, p, q]: [Point; 3], [u, v]: [[f64; 2]; 2], cross: f64, r: f64, old: [ItemId; 2], partners: &[ItemId]) -> Option<(Laid, RoundArc)> {
    let turn = cross.abs().atan2(u[0] * v[0] + u[1] * v[1]);
    let d = r * (turn / 2.0).tan();
    let at = |x: f64, y: f64| Point { x: x.round() as Nm, y: y.round() as Nm };
    let start = at(p.x as f64 - u[0] * d, p.y as f64 - u[1] * d);
    let end = at(p.x as f64 + v[0] * d, p.y as f64 + v[1] * d);
    // The centre lies off the track before, on the side it turns to.
    let side = if cross > 0.0 { [-u[1], u[0]] } else { [u[1], -u[0]] };
    let c = [p.x as f64 - u[0] * d + side[0] * r, p.y as f64 - u[1] * d + side[1] * r];
    let to_p = [p.x as f64 - c[0], p.y as f64 - c[1]];
    let l = to_p[0].hypot(to_p[1]);
    let mid = at(c[0] + to_p[0] / l * r, c[1] + to_p[1] / l * r);
    if start == end || start == mid || mid == end {
        return None;
    }
    let metres = |p: Point| [p.x as f64 * 1e-9, p.y as f64 * 1e-9];
    let (chord_points, sag) = arc_chords(metres(start), metres(mid), metres(end), chain.half_width as f64 * 1e-9);
    let track = |a: Point, b: Point, r: Nm| Item { kind: Kind::Track, net: chain.net, layers: chain.layer, shape: Shape::Capsule { a, b, r }, hole: None, locked: false, span: None };
    let taken: Vec<(ItemId, Item)> = old.iter().filter_map(|&t| Some((t, board.remove(t)?))).collect();
    let straight_before = (s != start).then(|| board.insert(track(s, start, chain.half_width)));
    let chords: Vec<ItemId> = chord_points.windows(2).map(|w| board.insert(track(w[0], w[1], chain.half_width + (sag * 1e9).ceil() as Nm))).collect();
    let straight_after = (end != q).then(|| board.insert(track(end, q, chain.half_width)));
    let new: Vec<ItemId> = straight_before.into_iter().chain(chords.iter().copied()).chain(straight_after).collect();
    let pieces: Vec<(Point, Point)> =
        straight_before.map(|_| (s, start)).into_iter().chain(chord_points.windows(2).map(|w| (w[0], w[1]))).chain(straight_after.map(|_| (end, q))).collect();
    let legal = new.iter().all(|&id| check_item(board, id).is_empty())
        && partners.iter().all(|&o| new.iter().any(|&id| joined(board, o, id)) && (!anchored(board, o, &[(s, p), (p, q)], chain.half_width) || anchored(board, o, &pieces, chain.half_width)));
    if !legal {
        for id in new.into_iter().rev() {
            board.discard(id);
        }
        for (id, item) in taken {
            board.restore(id, item);
        }
        return None;
    }
    let arc = RoundArc { chords, start, mid, end, width: 2 * chain.half_width, layers: chain.layer, net: chain.net };
    Some((Laid { straight_before, straight_after }, arc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_board() -> Board {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal)) (net 0 \"\") (net 1 \"A\") (net 2 \"B\"))",
        )
        .unwrap();
        Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap())
    }

    fn mm(x: f64, y: f64) -> Point {
        Point { x: (x * 1e6) as Nm, y: (y * 1e6) as Nm }
    }

    fn track(net: u32, a: Point, b: Point) -> Item {
        Item { kind: Kind::Track, net, layers: 1, shape: Shape::Capsule { a, b, r: 100_000 }, hole: None, locked: false, span: None }
    }

    #[test]
    fn a_right_angle_becomes_an_arc_tangent_to_both_tracks() {
        let mut b = empty_board();
        let mut soft = vec![b.insert(track(1, mm(10.0, 10.0), mm(20.0, 10.0))), b.insert(track(1, mm(20.0, 10.0), mm(20.0, 20.0)))];
        let arcs = round_corners(&mut b, &mut soft, 1_000_000);
        assert_eq!(arcs.len(), 1);
        let a = &arcs[0];
        assert_eq!((a.start, a.end), (mm(19.0, 10.0), mm(20.0, 11.0)));
        // The mid point lies on the circle round (19, 11), half way round the turn.
        let off = 1e6 * (1.0 - std::f64::consts::FRAC_1_SQRT_2);
        assert!((a.mid.x as f64 - (20e6 - off)).abs() <= 1.0 && (a.mid.y as f64 - (10e6 + off)).abs() <= 1.0, "{:?}", a.mid);
        assert!(a.chords.iter().all(|c| soft.contains(c)));
        assert_eq!(soft.len(), 2 + a.chords.len());
        assert!(board::check(&b).is_empty());
    }

    #[test]
    fn a_corner_hard_by_another_net_gets_a_smaller_arc_or_none() {
        let mut b = empty_board();
        let mut soft = vec![b.insert(track(1, mm(10.0, 10.0), mm(20.0, 10.0))), b.insert(track(1, mm(20.0, 10.0), mm(20.0, 20.0)))];
        // Another net's track inside the corner, clear of the tracks but in the way of a 2 mm arc.
        b.insert(track(2, mm(19.2, 10.8), mm(19.0, 11.0)));
        let arcs = round_corners(&mut b, &mut soft, 2_000_000);
        assert_eq!(arcs.iter().map(|a| a.start).collect::<Vec<_>>(), [mm(19.0, 10.0)]);
        assert!(board::check(&b).is_empty());
    }

    #[test]
    fn a_track_ending_beside_the_corner_keeps_its_end_in_copper() {
        // As on Biscay_Blueeye_sipm-comp: a track whose end only overlaps the corner, outside it.
        let mut b = empty_board();
        let mut soft = vec![b.insert(track(1, mm(20.0, 5.0), mm(20.0, 10.0))), b.insert(track(1, mm(20.0, 10.0), mm(10.0, 10.0)))];
        let end = mm(20.05, 10.05);
        let beside = b.insert(track(1, end, mm(20.05, 15.0)));
        soft.push(beside);
        round_corners(&mut b, &mut soft, 500_000);
        let other = b.item(beside).unwrap().shape.clone();
        let held = soft.iter().filter(|&&id| id != beside).filter_map(|id| b.item(*id)).any(|it| match it.shape {
            Shape::Capsule { a, b: e, .. } => board::point_segment(end, a, e) <= 100_000.0 || other.contains(a) || other.contains(e),
            _ => false,
        });
        assert!(held);
    }

    #[test]
    fn a_corner_at_a_via_or_a_locked_track_is_left() {
        let mut b = empty_board();
        let mut soft = vec![b.insert(track(1, mm(10.0, 10.0), mm(20.0, 10.0))), b.insert(Item { locked: true, ..track(1, mm(20.0, 10.0), mm(20.0, 20.0)) })];
        assert!(round_corners(&mut b, &mut soft, 1_000_000).is_empty());
        let mut soft = vec![b.insert(track(1, mm(10.0, 30.0), mm(20.0, 30.0))), b.insert(track(1, mm(20.0, 30.0), mm(20.0, 38.0)))];
        let at = mm(20.0, 30.0);
        b.insert(Item { kind: Kind::Via, net: 1, layers: 3, shape: Shape::Capsule { a: at, b: at, r: 300_000 }, hole: Some((at, 150_000)), locked: false, span: None });
        assert!(round_corners(&mut b, &mut soft, 1_000_000).is_empty());
    }
}
