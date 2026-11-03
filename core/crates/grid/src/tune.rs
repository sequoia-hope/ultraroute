//! Length matching for differential pairs: each net's length along its copper from one end of
//! the pair to the other, and bumps (a meander) put into the shorter net's tracks until the two
//! agree within a tolerance.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use board::{check_item, joined, region_price, Board, Item, ItemId, Kind, Nm, Point, Shape};

use crate::clock::Deadline;
use crate::pairs::{pin_pairs, Pair};

/// Lengths within this of each other are matched.
pub const TOLERANCE: f64 = 100_000.0;
/// Straight track kept before the first bump and after the last, at the least: a bump also keeps
/// its own width from either end of the track it goes into, so it never jogs where the track
/// turns, and from the bump before it, so their sides stand a bump's width apart.
const LEAD: Nm = 150_000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TuneStats {
    pub pairs: usize,
    /// Pairs measured, the skew of each before and after, in millimetres.
    pub skews: Vec<(String, f64, f64)>,
    pub bumps: usize,
}

/// The two ends a pair's lengths are measured between: the pin pairs farthest apart.
fn ends(board: &Board, pair: &Pair) -> Option<((ItemId, ItemId), (ItemId, ItemId))> {
    let ends = pin_pairs(board, pair);
    let mut best: Option<(f64, usize, usize)> = None;
    for i in 0..ends.len() {
        for j in i + 1..ends.len() {
            let d = ends[i].mid.distance(ends[j].mid);
            if best.is_none_or(|b| d > b.0) {
                best = Some((d, i, j));
            }
        }
    }
    let (_, i, j) = best?;
    Some(((ends[i].p, ends[j].p), (ends[i].n, ends[j].n)))
}

fn track_length(item: &Item) -> f64 {
    match (&item.kind, &item.shape) {
        (Kind::Track, Shape::Capsule { a, b, .. }) => a.distance(*b),
        _ => 0.0,
    }
}

/// Whether a track runs beside one of `other`'s at the pair's pitch, as a coupled run does.
fn beside(board: &Board, id: ItemId, other: u32, pitch: Nm) -> bool {
    let Some(item) = board.item(id) else { return false };
    let Shape::Capsule { a, b, .. } = item.shape else { return false };
    let mid = Point::new((a.x + b.x) / 2, (a.y + b.y) / 2);
    let layer = item.layers.trailing_zeros() as usize;
    board.near(layer, item.shape.bbox().grow(pitch)).into_iter().any(|o| {
        board.item(o).is_some_and(|it| match (&it.kind, &it.shape) {
            (Kind::Track, Shape::Capsule { a, b, .. }) if it.net == other => (board::point_segment(mid, *a, *b) - pitch as f64).abs() <= 0.1 * pitch as f64,
            _ => false,
        })
    })
}

/// The shortest way along a net's copper from one item to another: its length, counting each
/// track whole, and the tracks on it.
pub fn copper_path(board: &Board, from: ItemId, to: ItemId) -> Option<(f64, Vec<ItemId>)> {
    let net = board.item(from)?.net;
    let mut best: HashMap<ItemId, (u64, Option<ItemId>)> = HashMap::from([(from, (0, None))]);
    let mut heap = BinaryHeap::from([Reverse((0u64, from))]);
    while let Some(Reverse((d, at))) = heap.pop() {
        if best.get(&at).is_some_and(|b| b.0 < d) {
            continue;
        }
        if at == to {
            let mut tracks = Vec::new();
            let mut k = Some(at);
            while let Some(id) = k {
                if board.item(id).is_some_and(|it| it.kind == Kind::Track) {
                    tracks.push(id);
                }
                k = best[&id].1;
            }
            return Some((d as f64, tracks));
        }
        let item = board.item(at)?;
        let bb = board.bbox(at).unwrap_or_else(|| item.shape.bbox());
        let mut next: Vec<ItemId> = (0..board.layers.len())
            .filter(|l| item.on(*l))
            .flat_map(|l| board.near(l, bb))
            .filter(|&o| o != at && board.item(o).is_some_and(|it| it.net == net && matches!(it.kind, Kind::Track | Kind::Via | Kind::Pad { .. })))
            .collect();
        next.sort_unstable();
        next.dedup();
        for o in next {
            if !joined(board, at, o) {
                continue;
            }
            let nd = d + track_length(board.item(o)?) as u64;
            if best.get(&o).is_none_or(|b| nd < b.0) {
                best.insert(o, (nd, Some(at)));
                heap.push(Reverse((nd, o)));
            }
        }
    }
    None
}

/// One net's length from end to end, and the tracks along it.
pub type NetRun = (f64, Vec<ItemId>);

/// Each net's length from one end of the pair to the other, and the tracks along it.
pub fn pair_lengths(board: &Board, pair: &Pair) -> Option<(NetRun, NetRun)> {
    let ((pa, pb), (na, nb)) = ends(board, pair)?;
    Some((copper_path(board, pa, pb)?, copper_path(board, na, nb)?))
}

/// Bump heights tried, tallest first, so few are needed; a bump is never taller than the length
/// still wanted calls for.
const HEIGHTS: [Nm; 8] = [1_300_000, 900_000, 600_000, 400_000, 250_000, 150_000, 100_000, 50_000];
/// Places along a track a bump is tried at.
const PLACES: usize = 16;

/// A bump `w` wide and `h` high put into the track from `a` to `b`, starting `at` along it, on
/// the side `sign` (+1 left of the direction of travel); it adds `2 h` of length.
fn bump(a: Point, b: Point, at: f64, w: f64, h: f64, sign: f64) -> [Point; 6] {
    let len = a.distance(b);
    let (ux, uy) = ((b.x - a.x) as f64 / len, (b.y - a.y) as f64 / len);
    let (nx, ny) = (-uy * sign, ux * sign);
    let p = |s: f64, up: f64| Point::new(a.x + (ux * s + nx * up).round() as Nm, a.y + (uy * s + ny * up).round() as Nm);
    [a, p(at, 0.0), p(at, h), p(at + w, h), p(at + w, 0.0), b]
}

/// Whether a bump's piece keeps the clearance from its own net's copper too, all but the pieces
/// of its own track (`chain`) and what the track ends on: a bump folded against its own track or
/// pad leaves a slot too narrow to etch cleanly, an acid trap.
fn own_room(board: &Board, piece: ItemId, chain: &[ItemId], ends: [Point; 2]) -> bool {
    let Some(item) = board.item(piece) else { return false };
    let gap = board.clearance(item.net, item.net) as f64;
    let layer = item.layers.trailing_zeros() as usize;
    board.near(layer, item.shape.bbox().grow(gap as Nm)).into_iter().all(|o| {
        o == piece
            || chain.contains(&o)
            || board.item(o).is_none_or(|it| it.net != item.net || !it.is_copper() || it.refilled() || !it.on(layer) || ends.iter().any(|&e| it.shape.contains(e)) || it.shape.gap_within(&item.shape, gap) >= gap)
    })
}

/// One bump put into one of `tracks`, the tallest that lays cleanly, up to `want / 2` high and
/// no lower than the track is wide. Returns the length added, the track replaced (its id and
/// itself), the new pieces, and of those the two straight leads either side of the bump, which
/// may take a bump of their own; its sides and top may not.
type Bumped = (f64, (ItemId, Item), Vec<ItemId>, Vec<ItemId>);

/// The pieces of `line` laid as copies of `item`, while each is clean: what the checker allows,
/// the regions' hard rules among it (`never` net in, `must` net out). Their price under the
/// regions' soft rules too, if all five lay: the sum of each side's and the top's length times
/// what it pays (the leads lie along the track they replace, and pay what it paid).
fn lay_bump(board: &mut Board, item: &Item, r: Nm, line: &[Point; 6]) -> (Vec<ItemId>, Option<f64>) {
    let mut laid = Vec::new();
    let mut price = 0.0;
    for (k, p) in line.windows(2).enumerate() {
        let piece = Item { shape: Shape::Capsule { a: p[0], b: p[1], r }, span: None, ..item.clone() };
        if (1..4).contains(&k) {
            price += region_price(board, &piece) * p[0].distance(p[1]);
        }
        let nid = board.insert(piece);
        laid.push(nid);
        if !check_item(board, nid).is_empty() {
            return (laid, None);
        }
    }
    (laid, Some(price))
}

fn add_bump(board: &mut Board, tracks: &[ItemId], want: f64) -> Option<Bumped> {
    // A bump the regions' soft rules price is kept only when no free one lays anywhere: the
    // cheapest per length added (its price, height, track, place and side).
    let mut priced: Option<(f64, f64, ItemId, f64, f64)> = None;
    for h in HEIGHTS {
        for &id in tracks {
            let Some(item) = board.item(id).cloned() else { continue };
            let Shape::Capsule { a, b, r } = item.shape else { continue };
            // No lower than the track is wide, so its sides do not fold back on themselves: one
            // that overshoots is kept only if it leaves the pair closer than it found it.
            let least = (2 * r) as f64;
            let h = (h as f64).min(want / 2.0);
            let h = if h >= least { h } else if want > least { least } else { continue };
            // A bump three tracks wide, so its sides stand apart, kept its width from the ends.
            let (len, w) = (a.distance(b), (6 * r) as f64);
            let lead = (LEAD as f64).max(w);
            let room = len - w - 2.0 * lead;
            if room < 0.0 {
                continue;
            }
            let removed = board.remove(id)?;
            for k in 0..PLACES {
                let at = lead + room * (k as f64 + 0.5) / PLACES as f64;
                for sign in [1.0, -1.0] {
                    let line = bump(a, b, at, w, h, sign);
                    let (laid, price) = lay_bump(board, &item, r, &line);
                    // The sides and top keep clear of the rest of their own net's copper.
                    let clean = price.is_some() && laid.len() == 5 && laid[1..4].iter().all(|&nid| own_room(board, nid, &laid, [a, b]));
                    match price {
                        Some(p) if clean && p == 0.0 => {
                            let leads = vec![laid[0], laid[4]];
                            return Some((2.0 * h, (id, removed), laid, leads));
                        }
                        Some(p) if clean && priced.is_none_or(|q| p / h < q.0 / q.1) => priced = Some((p, h, id, at, sign)),
                        _ => {}
                    }
                    for nid in laid.into_iter().rev() {
                        board.discard(nid);
                    }
                }
            }
            board.restore(id, removed);
        }
    }
    // None free: the cheapest priced one, laid again.
    let (_, h, id, at, sign) = priced?;
    let removed = board.remove(id)?;
    let Shape::Capsule { a, b, r } = removed.shape else { unreachable!("a bumped track is a capsule") };
    let (laid, _) = lay_bump(board, &removed, r, &bump(a, b, at, 6.0 * r as f64, h, sign));
    let leads = vec![laid[0], laid[4]];
    Some((2.0 * h, (id, removed), laid, leads))
}

/// One bump `match_lengths_watch` kept: the pair, the net bumped, the skew before and after it
/// in nanometres, the track it replaced and the pieces laid in its place.
#[derive(Debug, Clone)]
pub struct TuneBump<'a> {
    pub pair: &'a Pair,
    pub net: u32,
    pub before: f64,
    pub after: f64,
    pub replaced: &'a Item,
    pub laid: &'a [ItemId],
}

/// Matches every pair's lengths, bumping the shorter net's tracks that `soft` holds (the
/// router's own), one bump at a time, until `deadline`; `soft` is kept up to date.
pub fn match_lengths(board: &mut Board, pairs: &[Pair], soft: &mut Vec<ItemId>, deadline: &Deadline) -> TuneStats {
    match_lengths_watch(board, pairs, soft, deadline, &mut |_, _| {})
}

/// `match_lengths`, telling `watch` of every bump kept.
pub fn match_lengths_watch(board: &mut Board, pairs: &[Pair], soft: &mut Vec<ItemId>, deadline: &Deadline, watch: &mut dyn FnMut(&Board, &TuneBump)) -> TuneStats {
    let mut stats = TuneStats::default();
    for pair in pairs {
        let Some(((lp, tp), (ln, tn))) = pair_lengths(board, pair) else { continue };
        stats.pairs += 1;
        let before = (lp - ln).abs();
        let (mut want, tracks) = if lp < ln { (ln - lp, tp) } else { (lp - ln, tn) };
        // The shorter net is lengthened no further than a custom rule's maximum for it lets it,
        // as KiCad counts its length (vias' heights too).
        let short = if lp < ln { pair.p } else { pair.n };
        let within = |board: &Board| board.net_maximum("length", short).is_none_or(|(_, max)| board.net_length(short) <= max * 1e3 + 1e-6);
        if let Some((_, max)) = board.net_maximum("length", short) {
            want = want.min((max * 1e3 - board.net_length(short)).max(0.0) * 1e6);
        }
        let mut tracks: Vec<ItemId> = tracks.into_iter().filter(|id| soft.contains(id)).collect();
        let skew = |board: &Board| pair_lengths(board, pair).map(|((lp, _), (ln, _))| (lp - ln).abs());
        let mut now = before;
        while want > TOLERANCE && !deadline.expired() {
            // Tracks running beside the partner last, so the coupling is kept where it can be;
            // otherwise longest first, for the most room.
            let other = if lp < ln { pair.n } else { pair.p };
            tracks.sort_by_cached_key(|id| {
                let l = board.item(*id).map_or(0.0, track_length);
                (beside(board, *id, other, pair.pitch()), Reverse(l as Nm))
            });
            let Some((_, (old, item), laid, leads)) = add_bump(board, &tracks, want) else { break };
            // Measured again, as a bump off the measured way, or one that moves it, adds nothing.
            match skew(board) {
                Some(next) if next < now && within(board) => {
                    watch(board, &TuneBump { pair, net: if lp < ln { pair.p } else { pair.n }, before: now, after: next, replaced: &item, laid: &laid });
                    now = next;
                    want = next;
                }
                _ => {
                    for id in laid.into_iter().rev() {
                        board.discard(id);
                    }
                    board.restore(old, item);
                    tracks.retain(|t| *t != old);
                    continue;
                }
            }
            stats.bumps += 1;
            tracks.retain(|t| *t != old);
            soft.retain(|t| *t != old);
            tracks.extend(leads);
            soft.extend(laid);
        }
        let after = skew(board).unwrap_or(before);
        stats.skews.push((format!("{}/{}", board.net_name(pair.p), board.net_name(pair.n)), before * 1e-6, after * 1e-6));
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bump_adds_twice_its_height() {
        let (a, b) = (Point::new(0, 0), Point::new(5_000_000, 0));
        let line = bump(a, b, 1_000_000.0, 450_000.0, 300_000.0, 1.0);
        let len: f64 = line.windows(2).map(|w| w[0].distance(w[1])).sum();
        assert!((len - 5_000_000.0 - 2.0 * 300_000.0).abs() < 10.0, "{len}");
        assert_eq!((line[0], line[5]), (a, b));
        assert!(line[2].y > 0, "left of travel along +x is +y");
    }

    /// A pair on F.Cu from x 0 to 10 mm: P straight along y 0, N 0.4 mm above it with a 1.2 mm
    /// dogleg. The board, the router's tracks and the pair.
    fn dogleg() -> (Board, Vec<ItemId>, Pair) {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"D_P\") (net 2 \"D_N\"))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let pad = |net, x, y| Item { kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false }, net, layers: 1, shape: Shape::Capsule { a: Point::new(x, y), b: Point::new(x, y), r: 100_000 }, hole: None, locked: true, span: None };
        let track = |net, a: Point, b: Point| Item { kind: Kind::Track, net, layers: 1, shape: Shape::Capsule { a, b, r: 75_000 }, hole: None, locked: false, span: None };
        for (net, y) in [(1, 0), (2, 400_000)] {
            board.insert(pad(net, 0, y));
            board.insert(pad(net, 10_000_000, y));
        }
        // P runs straight; N takes a 1.2 mm dogleg.
        let mut soft = vec![board.insert(track(1, Point::new(0, 0), Point::new(10_000_000, 0)))];
        for (a, b) in [((0, 400_000), (4_000_000, 400_000)), ((4_000_000, 400_000), (4_000_000, 1_000_000)), ((4_000_000, 1_000_000), (6_000_000, 1_000_000)), ((6_000_000, 1_000_000), (6_000_000, 400_000)), ((6_000_000, 400_000), (10_000_000, 400_000))] {
            soft.push(board.insert(track(2, Point::new(a.0, a.1), Point::new(b.0, b.1))));
        }
        (board, soft, Pair { p: 1, n: 2, width: 150_000, gap: 250_000 })
    }

    /// Tunes the pair, checks it is matched, connected and clean, and gives the tracks.
    fn tuned(mut board: Board, mut soft: Vec<ItemId>, pair: Pair) -> (Board, Vec<ItemId>) {
        let t = match_lengths(&mut board, std::slice::from_ref(&pair), &mut soft, &Default::default());
        let (before, after) = (t.skews[0].1, t.skews[0].2);
        assert!((before - 1.2).abs() < 1e-6, "{t:?}");
        assert!(after <= TOLERANCE * 1e-6, "{t:?}");
        assert!(t.bumps > 0, "{t:?}");
        assert!(board::connectivity(&board).airlines.is_empty());
        for id in &soft {
            assert!(check_item(&board, *id).is_empty());
        }
        (board, soft)
    }

    #[test]
    fn bumps_bring_the_shorter_net_up_to_the_longer() {
        let (board, soft, pair) = dogleg();
        tuned(board, soft, pair);
    }

    fn rect(x0: Nm, y0: Nm, x1: Nm, y1: Nm) -> Shape {
        Shape::Polygon(vec![Point::new(x0, y0), Point::new(x1, y0), Point::new(x1, y1), Point::new(x0, y1)])
    }

    fn region(shape: Shape, kind: board::RegionKind) -> board::Region {
        board::Region { name: "r".into(), shape, layers: 1, rules: vec![board::RegionRule { nets: Some([1].into()), kind, vias: false }] }
    }

    /// P's bumps go below it, away from N: a region over the left half of that side, `never`
    /// for P, sends them to the right half; with `avoid` they go there too, as they can.
    #[test]
    fn bumps_keep_out_of_never_and_avoid_regions() {
        for kind in [board::RegionKind::Never, board::RegionKind::Avoid(1.0)] {
            let (mut board, soft, pair) = dogleg();
            board.regions = vec![region(rect(-1_000_000, -3_000_000, 5_000_000, -100_000), kind)];
            let (board, soft) = tuned(board, soft, pair);
            let r = &board.regions[0];
            for id in &soft {
                let it = board.item(*id).unwrap();
                assert!(it.net != 1 || !r.touches(&it.shape), "{kind:?}: {it:?}");
            }
        }
    }

    /// A `must` region round P's line, 0.5 mm either side, holds every bump within it: only the
    /// low ones fit.
    #[test]
    fn bumps_stay_within_must_regions() {
        let (mut board, soft, pair) = dogleg();
        board.regions = vec![region(rect(-1_000_000, -500_000, 11_000_000, 500_000), board::RegionKind::Must)];
        let (board, soft) = tuned(board, soft, pair);
        let r = &board.regions[0];
        for id in &soft {
            let it = board.item(*id).unwrap();
            assert!(it.net != 1 || !r.breaks(&r.rules[0], it), "{it:?}");
        }
    }
}
