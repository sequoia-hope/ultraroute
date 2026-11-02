//! Pulling routed copper tight: each chain of the router's tracks between fixed points (pads,
//! vias, branches, copper it did not lay) is replaced, a run at a time, by the longest straight
//! track that keeps every rule. The ends stay where they are, so nothing is disconnected.

use std::collections::{HashMap, HashSet};

use board::{check_item, firmly_joined, joined, Board, Item, ItemId, Kind, Nm, Point, Shape};
use kicad_pcb::Span;

use crate::clock::Deadline;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CleanStats {
    pub chains: usize,
    pub segments_before: usize,
    pub segments_after: usize,
    /// Total length of the chains, in millimetres.
    pub length_before: f64,
    pub length_after: f64,
}

/// A chain's points and the track between each pair, all on one layer, one net, one width.
pub(crate) struct Chain {
    pub(crate) points: Vec<Point>,
    pub(crate) tracks: Vec<ItemId>,
    pub(crate) layer: u64,
    pub(crate) net: u32,
    pub(crate) half_width: board::Nm,
}

fn ends(item: &Item) -> Option<(Point, Point, board::Nm)> {
    match (&item.kind, &item.shape) {
        (Kind::Track, Shape::Capsule { a, b, r }) => Some((*a, *b, *r)),
        _ => None,
    }
}

/// The router's tracks grouped into chains between fixed points.
pub(crate) fn chains(board: &Board, soft: &HashSet<ItemId>) -> Vec<Chain> {
    // Per (layer, net, width, point): the soft tracks ending there.
    let mut at: HashMap<(u64, u32, board::Nm, Point), Vec<ItemId>> = HashMap::new();
    for &id in soft {
        let Some(item) = board.item(id) else { continue };
        let Some((a, b, r)) = ends(item) else { continue };
        for p in [a, b] {
            at.entry((item.layers, item.net, r, p)).or_default().push(id);
        }
    }
    // Per (layer, net, point): how many soft tracks end there, whatever their width.
    let mut ends_at: HashMap<(u64, u32, Point), usize> = HashMap::new();
    for ((layers, net, _, p), ids) in &at {
        *ends_at.entry((*layers, *net, *p)).or_default() += ids.len();
    }
    // A point is fixed unless exactly two of the chain's tracks meet there and nothing else does:
    // a track of another width ending there too is a junction, which pulling the chain tight
    // would leave beside the line (BeaconBuddy's VDD, a 0.2 mm track ending on a 0.2032 mm
    // chain's bend, left joined only where its round end overlapped the new line).
    let fixed = |board: &Board, key: &(u64, u32, board::Nm, Point)| -> bool {
        if at.get(key).map_or(0, Vec::len) != 2 || ends_at.get(&(key.0, key.1, key.3)).copied().unwrap_or(0) != 2 {
            return true;
        }
        let (layers, _, _, p) = *key;
        let probe = board::BBox::of([p]);
        (0..board.layers.len()).filter(|l| layers & (1 << l) != 0).any(|l| {
            board.near(l, probe).into_iter().any(|other| {
                !soft.contains(&other) && board.item(other).is_some_and(|o| o.is_copper() && o.shape.contains(p) && !o.refilled())
                    || board.item(other).is_some_and(|o| matches!(o.kind, Kind::Via) && o.shape.contains(p))
            })
        })
    };
    let mut done: HashSet<ItemId> = HashSet::new();
    let mut out = Vec::new();
    let mut starts: Vec<ItemId> = soft.iter().copied().filter(|id| board.item(*id).and_then(ends).is_some()).collect();
    starts.sort_unstable();
    for start in starts {
        if done.contains(&start) {
            continue;
        }
        let item = board.item(start).expect("listed above");
        let (a, _, r) = ends(item).expect("a track");
        let key = |p: Point| (item.layers, item.net, r, p);
        // Walk to one fixed end, then collect the chain from there.
        let mut from = a;
        let mut current = start;
        let mut guard = 0;
        while !fixed(board, &key(from)) && guard < 100_000 {
            guard += 1;
            let Some(&next) = at[&key(from)].iter().find(|&&t| t != current) else { break };
            let (na, nb, _) = ends(board.item(next).expect("soft")).expect("a track");
            from = if na == from { nb } else { na };
            current = next;
            if current == start {
                break;
            }
        }
        let mut points = vec![from];
        let mut tracks = Vec::new();
        let mut track = current;
        let mut p = from;
        loop {
            if done.contains(&track) {
                break;
            }
            done.insert(track);
            let (ta, tb, _) = ends(board.item(track).expect("soft")).expect("a track");
            let q = if ta == p { tb } else { ta };
            tracks.push(track);
            points.push(q);
            p = q;
            if fixed(board, &key(p)) {
                break;
            }
            let Some(&next) = at[&key(p)].iter().find(|&&t| t != track) else { break };
            track = next;
        }
        out.push(Chain { points, tracks, layer: item.layers, net: item.net, half_width: r });
    }
    out
}

/// Takes out the router's tracks lying exactly on another of the same net and layer, end to end,
/// which a reroute along copper its net already has, or a mend, can lay. Of two the narrower goes,
/// and of two as wide the router's own before the board file's. How many, and the file's spans among them.
pub fn drop_duplicates(board: &mut Board, soft: &mut Vec<ItemId>) -> (usize, Vec<Span>) {
    drop_duplicates_watch(board, soft, &mut |_, _| {})
}

/// As [`drop_duplicates`], showing `watch` each duplicate removed and the board after it goes.
pub fn drop_duplicates_watch(board: &mut Board, soft: &mut Vec<ItemId>, watch: &mut dyn FnMut(&Board, &Item)) -> (usize, Vec<Span>) {
    let soft_set: HashSet<ItemId> = soft.iter().copied().collect();
    // Whatever its width: a narrower track with the same ends lies wholly inside the wider and
    // joins what it joins (easy2350's USB_DM_C, a 0.09 mm neck under a 0.2 mm track).
    let key = |item: &Item| match &item.shape {
        Shape::Capsule { a, b, .. } if item.kind == Kind::Track => Some((item.net, item.layers, (*a).min(*b), (*a).max(*b))),
        _ => None,
    };
    let width = |id: &ItemId| match board.item(*id).map(|it| &it.shape) {
        Some(Shape::Capsule { r, .. }) => *r,
        _ => 0,
    };
    let mut seen: HashMap<(u32, u64, Point, Point), ItemId> = HashMap::new();
    // The widest first, and of those the file's, so a duplicate is the narrower or the router's.
    let mut ids: Vec<ItemId> = board.items().filter(|(_, it)| it.kind == Kind::Track).map(|(id, _)| id).collect();
    ids.sort_by_key(|id| (std::cmp::Reverse(width(id)), soft_set.contains(id), *id));
    let mut doubled = Vec::new();
    for id in ids {
        let Some(k) = board.item(id).and_then(key) else { continue };
        if seen.insert(k, id).is_some() && soft_set.contains(&id) {
            doubled.push(id);
        }
    }
    let mut spans = Vec::new();
    for &id in &doubled {
        if let Some(mut item) = board.remove(id) {
            spans.extend(item.span.take());
            watch(board, &item);
        }
    }
    soft.retain(|id| board.item(*id).is_some());
    (doubled.len(), spans)
}

/// Takes out the router's copper that leads nowhere, as KiCad would flag it (see [`dangles`]).
/// Taking one out can leave the next loose, so it goes round until none is left. How many went,
/// and the board file's spans among them, for removing from the file.
pub fn prune_dangling(board: &mut Board, soft: &mut Vec<ItemId>) -> (usize, Vec<Span>) {
    prune_dangling_watch(board, soft, &mut |_, _| {})
}

/// As [`prune_dangling`], showing `watch` each dangling track or via removed.
pub fn prune_dangling_watch(board: &mut Board, soft: &mut Vec<ItemId>, watch: &mut dyn FnMut(&Board, &Item)) -> (usize, Vec<Span>) {
    prune_dangling_of(board, soft, &|_| true, watch)
}

/// As [`prune_dangling`], for the copper `which` picks only: the shove prunes the nets it is
/// done with mid-run, and leaves an open net's cut copper for the net to join again.
pub(crate) fn prune_dangling_of(board: &mut Board, soft: &mut Vec<ItemId>, which: &dyn Fn(&Item) -> bool, watch: &mut dyn FnMut(&Board, &Item)) -> (usize, Vec<Span>) {
    let (mut pruned, mut spans) = (0, Vec::new());
    loop {
        let loose: Vec<ItemId> =
            soft.iter().copied().filter(|&id| board.item(id).is_some_and(which) && (dangles(board, id) || redundant(board, id))).collect();
        let before = pruned;
        // One at a time, judged again: two stubs covering each other must not both go. What
        // carries a connection stays however it looks, so a round may take nothing out.
        for id in loose {
            if !dangles(board, id) {
                if redundant(board, id) && bridges_nothing(board, id) {
                    if let Some(mut item) = board.remove(id) {
                        spans.extend(item.span.take());
                        pruned += 1;
                        watch(board, &item);
                        board::probe::mark("prune/redundant", board);
                    }
                }
                continue;
            }
            if !bridges_nothing(board, id) {
                if let Some((gone, new_ids)) = collapse_via(board, id) {
                    spans.extend(gone);
                    soft.extend(new_ids);
                    pruned += 1;
                    board::probe::mark("prune/collapse-via", board);
                    continue;
                }
                if let Some((span, new_ids)) = trim_overhang(board, id) {
                    spans.extend(span);
                    soft.extend(new_ids);
                    pruned += 1;
                    board::probe::mark("prune/trim-overhang", board);
                }
                continue;
            }
            if let Some(mut item) = board.remove(id) {
                spans.extend(item.span.take());
                pruned += 1;
                watch(board, &item);
                board::probe::mark("prune/dangling", board);
            }
        }
        soft.retain(|id| board.item(*id).is_some());
        if pruned == before {
            break;
        }
    }
    (pruned, spans)
}

/// Splits each of the router's tracks where another track of its net ends on its middle, so the
/// two meet end to end. KiCad joins such a T by the track's body only sometimes: a short piece
/// from it at an angle is found with an unconnected end (`track_dangling`), and split, never.
/// The tracks split are taken out of `soft` and their halves put in; the spans of what went.
pub fn split_at_joins(board: &mut Board, soft: &mut Vec<ItemId>) -> (usize, Vec<Span>) {
    let (mut split, mut spans) = (0, Vec::new());
    let mut queue: Vec<ItemId> = soft.clone();
    while let Some(id) = queue.pop() {
        let Some(item) = board.item(id).cloned() else { continue };
        let (Kind::Track, Shape::Capsule { a, b, r }) = (&item.kind, &item.shape) else { continue };
        let (a, b, r) = (*a, *b, *r);
        if a == b || item.net == 0 || item.locked {
            continue;
        }
        let layer = item.layers.trailing_zeros() as usize;
        // An end of another track of the net on the centre line, away from this track's ends:
        // within a quarter of the width, so the halves bend by no more than that. Away is
        // measured along the line: an end beside a short track, near its middle but off to the
        // side, would split it into two halves each of which the other's far end then splits,
        // making the track again, for ever.
        let lean = (r / 2).max(1);
        let at = board.near(layer, item.shape.bbox()).into_iter().filter(|&o| o != id).find_map(|o| {
            let other = board.item(o)?;
            let (Kind::Track, Shape::Capsule { a: p, b: q, .. }) = (&other.kind, &other.shape) else { return None };
            if other.net != item.net || !other.on(layer) {
                return None;
            }
            [*p, *q].into_iter().find(|&e| {
                let (_, foot) = closest_on_segment(a, b, e);
                foot.distance(e) <= lean as f64 && foot.distance(a) > lean as f64 && foot.distance(b) > lean as f64
            })
        });
        let Some(at) = at else { continue };
        let was_dangling = dangles(board, id);
        let old = board.remove(id).expect("present");
        let halves = [board.insert(Item { shape: Shape::Capsule { a, b: at, r }, span: None, ..old.clone() }), board.insert(Item { shape: Shape::Capsule { a: at, b, r }, span: None, ..old.clone() })];
        if halves.iter().any(|&h| !check_item(board, h).is_empty() || (!was_dangling && dangles(board, h))) {
            for h in halves.into_iter().rev() {
                board.discard(h);
            }
            board.restore(id, old);
            continue;
        }
        spans.extend(old.span);
        soft.retain(|s| *s != id);
        soft.extend(halves);
        queue.extend(halves);
        split += 1;
    }
    (split, spans)
}

/// A track end joined to the rest of its net only where round copper overlaps, narrower than
/// the tracks: KiCad counts it joined, the fab may etch through it.
#[derive(Debug, Clone, PartialEq)]
pub struct NarrowJoint {
    pub net: u32,
    pub layer: usize,
    pub at: Point,
    /// The copper's width through the joint, and the narrower track's, in nm.
    pub width: f64,
    pub full: f64,
    /// The track whose end it is, and the one its round end overlaps.
    pub track: ItemId,
    pub other: ItemId,
}

/// The ends of the router's tracks joined only through narrow overlaps: not at another
/// track's end, nor on its body, nor in a pad or via of the net, yet overlapping another track
/// of the net through less than [`board::FULL_JOINT`] of the narrower width. An end that meets
/// nothing is a dangling end, not a joint.
pub fn narrow_joints(board: &Board, laid: &[ItemId]) -> Vec<NarrowJoint> {
    let mut out = Vec::new();
    for &id in laid {
        let Some(item) = board.item(id) else { continue };
        let (Kind::Track, Shape::Capsule { a, b, r }) = (&item.kind, &item.shape) else { continue };
        if item.net == 0 {
            continue;
        }
        let layer = item.layers.trailing_zeros() as usize;
        for p in [*a, *b] {
            let near = board.near(layer, board::BBox::of([p]).grow(2 * *r));
            let ours = || near.iter().copied().filter(|&o| o != id).filter_map(|o| board.item(o).map(|c| (o, c))).filter(|(_, c)| c.net == item.net && c.on(layer));
            // In a pad, a via or a pour of the net: that copper joins it.
            if ours().any(|(_, c)| c.kind != Kind::Track && c.is_copper() && c.shape.contains(p)) {
                continue;
            }
            let mut best: Option<(f64, f64, ItemId)> = None;
            for (o, c) in ours() {
                let Shape::Capsule { a: q0, b: q1, r: r2 } = c.shape else { continue };
                // A neighbour sharing this track's other end is joined there already: its round
                // end against this end is a tip, not a joint.
                let far = if p == *a { *b } else { *a };
                if c.kind != Kind::Track || (far != p && (q0 == far || q1 == far)) {
                    continue;
                }
                let Some(w) = board::end_neck(p, *r as f64, q0, q1, r2 as f64) else { continue };
                let full = 2.0 * (*r).min(r2) as f64;
                if best.is_none_or(|(bw, bf, _)| w / full > bw / bf) {
                    best = Some((w, full, o));
                }
            }
            if let Some((width, full, other)) = best {
                if width < board::FULL_JOINT * full && !firmly_linked(board, id, other) {
                    out.push(NarrowJoint { net: item.net, layer, at: p, width, full, track: id, other });
                }
            }
        }
    }
    out
}

/// Whether `a` reaches `b` through its net's copper joined at full width: then a narrow overlap
/// between them is a tip grazing copper already joined, not the joint that carries the net.
fn firmly_linked(board: &Board, a: ItemId, b: ItemId) -> bool {
    let mut seen: HashSet<ItemId> = HashSet::from([a]);
    let mut stack = vec![a];
    while let Some(at) = stack.pop() {
        for next in firm_neighbours(board, at) {
            if next == b {
                return true;
            }
            if seen.insert(next) {
                stack.push(next);
            }
        }
    }
    false
}

/// Lays a short piece from each narrow joint's end onto the other track's centre line, at the
/// end's width or, where that breaks a rule, the narrowest the net's rules allow; the pieces join
/// `laid`. How many joints it mended.
pub fn mend_joints(board: &mut Board, laid: &mut Vec<ItemId>) -> usize {
    let mut mended = 0;
    for joint in narrow_joints(board, laid) {
        // A piece laid for the joint's other end may have mended this one.
        if firmly_linked(board, joint.track, joint.other) {
            continue;
        }
        let (Some(track), Some(other)) = (board.item(joint.track).cloned(), board.item(joint.other)) else { continue };
        let (Shape::Capsule { r, .. }, Shape::Capsule { a: q0, b: q1, .. }) = (&track.shape, &other.shape) else { continue };
        let (_, foot) = closest_on_segment(*q0, *q1, joint.at);
        if foot == joint.at {
            continue;
        }
        let least = (board.min_joint_of(track.net) + 1) / 2;
        for r in [*r, least] {
            if (2 * r) as f64 <= joint.width {
                break;
            }
            let piece = board.insert(Item { shape: Shape::Capsule { a: joint.at, b: foot, r }, span: None, locked: false, ..track.clone() });
            if check_item(board, piece).is_empty() {
                laid.push(piece);
                mended += 1;
                break;
            }
            board.discard(piece);
        }
    }
    mended
}

/// Where one of the router's tracks grazes other copper of its net on its layer without joining
/// it end to end: their copper overlaps, but by less than the narrower is wide, so KiCad measures
/// the sliver between them as a neck (Own-Mailbox's GND, two 0.2 mm tracks 0.197 mm apart centre
/// to centre, 0.03 mm across). `from` on the track's centre line and `to` in the other copper,
/// nearest each other, and the overlap, in nm.
#[derive(Debug, Clone, PartialEq)]
pub struct Graze {
    pub track: ItemId,
    pub other: ItemId,
    pub from: Point,
    pub to: Point,
    pub overlap: f64,
}

/// The closest points of segments `ab` and `cd`, which do not cross.
fn closest_points(a: Point, b: Point, c: Point, d: Point) -> (Point, Point) {
    let candidates = [(a, closest_on_segment(c, d, a).1), (b, closest_on_segment(c, d, b).1), (closest_on_segment(a, b, c).1, c), (closest_on_segment(a, b, d).1, d)];
    candidates.into_iter().min_by(|x, y| x.0.distance(x.1).total_cmp(&y.0.distance(y.1))).expect("four")
}

/// The router's tracks that graze copper of their net ([`Graze`]): a track or via whose centre
/// line stays clear of the track's, or a pad its centre line stays outside of, overlapping it by
/// less than the narrower's width, and neither meeting the other squarely at an end.
pub fn grazes(board: &Board, laid: &[ItemId]) -> Vec<Graze> {
    let mine: HashSet<ItemId> = laid.iter().copied().collect();
    let mut out = Vec::new();
    for &id in laid {
        let Some(item) = board.item(id) else { continue };
        let (Kind::Track, Shape::Capsule { a, b, r }) = (&item.kind, &item.shape) else { continue };
        if item.net == 0 || a == b {
            continue;
        }
        let layer = item.layers.trailing_zeros() as usize;
        for other in board.near(layer, item.shape.bbox().grow(*r)) {
            // Each pair of the router's tracks once.
            if other == id || (mine.contains(&other) && other < id) {
                continue;
            }
            let Some(o) = board.item(other) else { continue };
            if o.net != item.net || !o.on(layer) || !o.is_copper() || o.refilled() || o.shape.gap_within(&item.shape, 0.0) > 0.0 {
                continue;
            }
            let squarely = |p: Point, r: Nm, x: &Item| end_square(p, r, x);
            if squarely(*a, *r, o) || squarely(*b, *r, o) {
                continue;
            }
            let (from, to, overlap, full) = match (&o.kind, &o.shape) {
                (Kind::Track | Kind::Via, Shape::Capsule { a: c, b: d, r: r2 }) => {
                    if o.kind == Kind::Track && (squarely(*c, *r2, item) || squarely(*d, *r2, item)) {
                        continue;
                    }
                    let gap = board::geom::segment_segment(*a, *b, *c, *d);
                    if gap == 0.0 {
                        continue;
                    }
                    let (p, q) = closest_points(*a, *b, *c, *d);
                    (p, q, (*r + *r2) as f64 - gap, 2.0 * (*r).min(*r2) as f64)
                }
                (Kind::Pad { .. }, shape) => {
                    let line = Shape::Capsule { a: *a, b: *b, r: 0 };
                    let gap = shape.gap(&line);
                    if gap == 0.0 {
                        continue;
                    }
                    // The centre line's point nearest the pad, and the pad's nearest it.
                    let p = (0..=32).map(|k| Point::new(a.x + (b.x - a.x) * k / 32, a.y + (b.y - a.y) * k / 32)).min_by(|x, y| point_gap(shape, *x).total_cmp(&point_gap(shape, *y))).expect("points");
                    let centre = shape.bbox();
                    let centre = Point::new((centre.min.x + centre.max.x) / 2, (centre.min.y + centre.max.y) / 2);
                    let into = (1..=20).map(|k| Point::new(p.x + (centre.x - p.x) * k / 20, p.y + (centre.y - p.y) * k / 20)).find(|&q| shape.depth(q) >= *r as f64).unwrap_or(centre);
                    (p, into, *r as f64 - gap, 2.0 * *r as f64)
                }
                _ => continue,
            };
            // Inside a via's or pad's copper the two are joined by it at full width already:
            // no neck, and a bridge there only piles copper on copper (easy2350's USB_DM_C, a
            // staircase inside its via's land bridged step to step).
            let covered = || {
                board.near(layer, board::BBox::of([from, to])).into_iter().filter_map(|c| board.item(c)).any(|c| {
                    matches!(c.kind, Kind::Via | Kind::Pad { .. }) && c.net == item.net && c.on(layer) && c.shape.contains(from) && c.shape.contains(to)
                })
            };
            if overlap > 0.0 && overlap < full && !bridged(board, layer, id, item, other, o) && !covered() {
                out.push(Graze { track: id, other, from, to, overlap });
            }
        }
    }
    out
}

/// Whether a track of the net already bridges `x` and `y`: one end squarely on each.
fn bridged(board: &Board, layer: usize, xi: ItemId, x: &Item, yi: ItemId, y: &Item) -> bool {
    board.near(layer, x.shape.bbox()).into_iter().filter(|&t| t != xi && t != yi).filter_map(|t| board.item(t)).any(|t| {
        let (Kind::Track, Shape::Capsule { a, b, r }) = (&t.kind, &t.shape) else { return false };
        t.net == x.net && t.on(layer) && ((end_square(*a, *r, x) && end_square(*b, *r, y)) || (end_square(*b, *r, x) && end_square(*a, *r, y)))
    })
}

/// Bridges each graze with a short piece from the track's centre line into the other copper, so
/// they meet at the narrower's full width: at the track's width or, where that breaks a rule, the
/// narrowest the net's rules allow; the pieces join `laid`. How many it mended.
pub fn mend_grazes(board: &mut Board, laid: &mut Vec<ItemId>) -> usize {
    let mut mended = 0;
    for graze in grazes(board, laid) {
        let Some(track) = board.item(graze.track).cloned() else { continue };
        let Shape::Capsule { r, .. } = track.shape else { continue };
        if graze.from == graze.to {
            continue;
        }
        // A track of the net already lying from one end to the other is the bridge.
        let layer = track.layers.trailing_zeros() as usize;
        let span = board::BBox::of([graze.from, graze.to]);
        if board.near(layer, span).into_iter().filter_map(|t| board.item(t)).any(|t| {
            t.kind == Kind::Track && t.net == track.net && matches!(t.shape, Shape::Capsule { a, b, .. } if (a == graze.from && b == graze.to) || (a == graze.to && b == graze.from))
        }) {
            continue;
        }
        let least = (board.min_joint_of(track.net) + 1) / 2;
        for r in [r, least] {
            let piece = board.insert(Item { shape: Shape::Capsule { a: graze.from, b: graze.to, r }, span: None, locked: false, ..track.clone() });
            if check_item(board, piece).is_empty() {
                laid.push(piece);
                mended += 1;
                break;
            }
            board.discard(piece);
        }
    }
    mended
}

/// A track end joined to a pad of its net only where its round end overlaps the pad's edge:
/// KiCad counts them joined, but the copper across the edge is the chord the end cuts there,
/// narrower than the track.
#[derive(Debug, Clone, PartialEq)]
pub struct PadJoint {
    pub net: u32,
    pub layer: usize,
    pub at: Point,
    /// The chord across the pad's edge, and the track's width, in nm.
    pub width: f64,
    pub full: f64,
    pub track: ItemId,
    pub pad: ItemId,
}

/// The ends of the router's tracks that meet nothing of their net squarely ([`end_square`]) yet
/// overlap a pad of it, the end's centre outside the pad; and those whose round end, inside a
/// pad, sticks out past one of its corners ([`straddled_corner`]): the copper round the corner is
/// narrower than the track (59pct_keyboard's 0.18 mm tracks ending 1 um inside a square pad's
/// corner, 0.13 mm across, as KiCad's connection width check measures it).
pub fn pad_joints(board: &Board, laid: &[ItemId]) -> Vec<PadJoint> {
    let mut out = Vec::new();
    for &id in laid {
        let Some(item) = board.item(id) else { continue };
        let (Kind::Track, Shape::Capsule { a, b, r }) = (&item.kind, &item.shape) else { continue };
        if item.net == 0 || a == b {
            continue;
        }
        let layer = item.layers.trailing_zeros() as usize;
        for p in [*a, *b] {
            let near = board.near(layer, board::BBox::of([p]).grow(2 * *r));
            let ours: Vec<(ItemId, &Item)> = near
                .iter()
                .copied()
                .filter(|&o| o != id)
                .filter_map(|o| board.item(o).map(|c| (o, c)))
                .filter(|(_, c)| c.net == item.net && c.on(layer) && c.is_copper() && (!c.refilled() || board.is_plane(c)))
                .collect();
            if let Some(&(pad, c)) = ours.iter().find(|(_, c)| matches!(c.kind, Kind::Pad { .. }) && end_square(p, *r, c)) {
                if let Some(width) = straddled_corner(&c.shape, p, if p == *a { *b } else { *a }, *r) {
                    if !ours.iter().any(|(o, c)| *o != pad && !matches!(c.kind, Kind::Pad { .. }) && end_square(p, *r, c)) {
                        out.push(PadJoint { net: item.net, layer, at: p, width, full: 2.0 * *r as f64, track: id, pad });
                    }
                    continue;
                }
            }
            if ours.iter().any(|(_, c)| end_square(p, *r, c)) {
                continue;
            }
            let pad = ours
                .iter()
                .filter(|(_, c)| matches!(c.kind, Kind::Pad { .. }))
                .map(|&(o, c)| (o, point_gap(&c.shape, p)))
                .filter(|&(_, d)| d < *r as f64)
                .min_by(|x, y| x.1.total_cmp(&y.1));
            if let Some((pad, d)) = pad {
                let r = *r as f64;
                out.push(PadJoint { net: item.net, layer, at: p, width: 2.0 * (r * r - d * d).max(0.0).sqrt(), full: 2.0 * r, track: id, pad });
            }
        }
    }
    out
}

/// Lays a short piece from each pad joint's end into the pad, so the track ends inside it: to
/// where the pad is as deep as the piece's half width, else to its centre, at the end's width or,
/// where that breaks a rule, the narrowest the net's rules allow; the pieces join `laid`. How
/// many it mended.
pub fn mend_pad_joints(board: &mut Board, laid: &mut Vec<ItemId>) -> usize {
    let mut mended = 0;
    for joint in pad_joints(board, laid) {
        let (Some(track), Some(pad)) = (board.item(joint.track).cloned(), board.item(joint.pad).cloned()) else { continue };
        let Shape::Capsule { r, .. } = track.shape else { continue };
        let centre = match &pad.shape {
            Shape::Capsule { a, b, .. } => closest_on_segment(*a, *b, joint.at).1,
            Shape::Polygon(p) => {
                let n = p.len().max(1) as Nm;
                Point::new(p.iter().map(|q| q.x).sum::<Nm>() / n, p.iter().map(|q| q.y).sum::<Nm>() / n)
            }
        };
        if !pad.shape.contains(centre) || centre == joint.at {
            continue;
        }
        let least = (board.min_joint_of(track.net) + 1) / 2;
        let along = |t: f64| Point::new(joint.at.x + ((centre.x - joint.at.x) as f64 * t).round() as Nm, joint.at.y + ((centre.y - joint.at.y) as f64 * t).round() as Nm);
        'widths: for r in [r, least] {
            if (2 * r) as f64 <= joint.width {
                break;
            }
            let deep = (1..=20).map(|k| along(k as f64 / 20.0)).find(|&q| pad.shape.depth(q) >= r as f64).unwrap_or(centre);
            for to in [deep, centre] {
                let piece = board.insert(Item { shape: Shape::Capsule { a: joint.at, b: to, r }, span: None, locked: false, ..track.clone() });
                if check_item(board, piece).is_empty() {
                    laid.push(piece);
                    mended += 1;
                    break 'widths;
                }
                board.discard(piece);
            }
        }
    }
    mended
}

/// Where a track's round end at `p`, inside a polygon pad, is not wholly inside it and a corner
/// of the pad (a turn of more than 30 degrees) lies under the track near its end, `q` the track's
/// other end: the copper's width round that corner, from the corner across the track to its far
/// edge.
fn straddled_corner(pad: &Shape, p: Point, q: Point, r: Nm) -> Option<f64> {
    let Shape::Polygon(poly) = pad else { return None };
    let r = r as f64;
    if pad.depth(p) >= r - 500.0 || poly.len() < 3 {
        return None;
    }
    let (dx, dy) = ((p.x - q.x) as f64, (p.y - q.y) as f64);
    let len = dx.hypot(dy);
    if len == 0.0 {
        return None;
    }
    let (ux, uy) = (dx / len, dy / len);
    let n = poly.len();
    let mut narrowest: Option<f64> = None;
    for i in 0..n {
        let (prev, v, next) = (poly[(i + n - 1) % n], poly[i], poly[(i + 1) % n]);
        let (ax, ay) = ((v.x - prev.x) as f64, (v.y - prev.y) as f64);
        let (bx, by) = ((next.x - v.x) as f64, (next.y - v.y) as f64);
        let turn = (ax * by - ay * bx).atan2(ax * bx + ay * by).abs();
        if turn < 30f64.to_radians() {
            continue;
        }
        // The corner's place along the track from its end, and across it.
        let (cx, cy) = ((v.x - p.x) as f64, (v.y - p.y) as f64);
        let (along, across) = (cx * ux + cy * uy, -cx * uy + cy * ux);
        if along > r || along < -2.0 * r || across.abs() >= r {
            continue;
        }
        let width = r + across.abs();
        narrowest = Some(narrowest.map_or(width, |w: f64| w.min(width)));
    }
    narrowest.filter(|&w| w < 2.0 * r)
}

fn closest_on_segment(a: Point, b: Point, p: Point) -> (f64, Point) {
    let (dx, dy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
    let len_sq = dx * dx + dy * dy;
    if len_sq <= 0.0 {
        return (0.0, a);
    }
    let t = (((p.x - a.x) as f64 * dx + (p.y - a.y) as f64 * dy) / len_sq).clamp(0.0, 1.0);
    let proj = Point::new(a.x + (t * dx).round() as board::Nm, a.y + (t * dy).round() as board::Nm);
    (t, proj)
}

fn point_gap(shape: &Shape, p: Point) -> f64 {
    shape.gap(&Shape::Capsule { a: p, b: p, r: 0 })
}

/// A via that meets copper on one layer only, yet joins pieces there (a track ending in a via
/// that stands on a pad): KiCad finds it dangling all the same. The tracks ending in it are drawn
/// on to its centre and it goes, when that leaves everything it joined joined and breaks no rule.
/// The spans of what went, and the tracks drawn on.
fn collapse_via(board: &mut Board, id: ItemId) -> Option<(Vec<Span>, Vec<ItemId>)> {
    let via = board.item(id)?.clone();
    let (Kind::Via, Shape::Capsule { a: centre, .. }) = (&via.kind, &via.shape) else { return None };
    let centre = *centre;
    let around = neighbours(board, id);
    let layers: HashSet<usize> = around.iter().filter_map(|&o| board.item(o)).flat_map(|o| (0..board.layers.len()).filter(|l| o.on(*l) && via.on(*l)).collect::<Vec<_>>()).collect();
    if layers.len() != 1 || around.len() < 2 {
        return None;
    }
    let mut moved: Vec<(ItemId, Item)> = Vec::new();
    let mut drawn: Vec<ItemId> = Vec::new();
    for &o in &around {
        let Some(it) = board.item(o).cloned() else { continue };
        let (Kind::Track, Shape::Capsule { a, b, r }) = (&it.kind, &it.shape) else { continue };
        let inside = |p: Point| via.shape.gap_within(&Shape::Capsule { a: p, b: p, r: 0 }, 0.0) == 0.0;
        let (a2, b2) = (if inside(*a) { centre } else { *a }, if inside(*b) { centre } else { *b });
        if (a2, b2) == (*a, *b) || a2 == b2 {
            continue;
        }
        board.remove(o);
        moved.push((o, it.clone()));
        drawn.push(board.insert(Item { shape: Shape::Capsule { a: a2, b: b2, r: *r }, span: None, locked: false, ..it }));
    }
    board.remove(id);
    let kept: Vec<ItemId> = around.iter().copied().filter(|o| board.item(*o).is_some()).chain(drawn.iter().copied()).collect();
    let legal = drawn.iter().all(|&d| check_item(board, d).is_empty());
    if legal && all_joined(board, &kept) {
        let mut gone: Vec<Span> = via.span.into_iter().collect();
        gone.extend(moved.into_iter().filter_map(|(_, it)| it.span));
        return Some((gone, drawn));
    }
    for d in drawn {
        board.discard(d);
    }
    for (o, it) in moved {
        board.restore(o, it);
    }
    board.restore(id, via);
    None
}

/// Whether the pieces are all joined to one another, by a short search through their net's copper.
fn all_joined(board: &Board, ids: &[ItemId]) -> bool {
    let Some((&first, rest)) = ids.split_first() else { return true };
    let mut wanted: HashSet<ItemId> = rest.iter().copied().collect();
    let mut seen: HashSet<ItemId> = HashSet::from([first]);
    let mut stack = vec![first];
    while let Some(at) = stack.pop() {
        if wanted.is_empty() || seen.len() > DETOUR_LIMIT {
            break;
        }
        for next in neighbours(board, at) {
            if seen.insert(next) {
                wanted.remove(&next);
                stack.push(next);
            }
        }
    }
    wanted.is_empty()
}

/// Whether the round cap of radius `r` at `track_id`'s end `p` (other end `q`) connects to
/// `other` as [`joined`] and KiCad judge it: strict copper overlap rather than two caps merely
/// kissing at `2 * r` apart (except pours, which join wherever they touch).
fn end_overlaps(board: &Board, track_id: ItemId, p: Point, q: Point, r: Nm, other_id: ItemId, other: &Item) -> bool {
    if !joined(board, track_id, other_id) {
        return false;
    }
    let touches = if matches!(other.kind, Kind::Pour { .. }) {
        other.shape.gap_within(&Shape::Capsule { a: p, b: p, r }, 0.0) == 0.0
    } else {
        let reach = r as f64;
        let gap = other.shape.gap_within(&Shape::Capsule { a: p, b: p, r: 0 }, reach);
        gap == 0.0 || gap < reach - 0.5
    };
    touches && (other.shape.contains(p) || point_gap(&other.shape, p) <= point_gap(&other.shape, q))
}

/// Whether a track's round end at `p` (radius `r`) meets `other` squarely: on a track's end or
/// centre line, at a via's centre or wholly inside it ([`board::meets_squarely`]), inside a pad
/// (to within a micrometre of its edge), or anywhere in a pour.
fn end_square(p: Point, r: Nm, other: &Item) -> bool {
    match (&other.kind, &other.shape) {
        (Kind::Track | Kind::Via, Shape::Capsule { a, b, r: r2 }) => board::meets_squarely(p, r as f64, *a, *b, *r2 as f64),
        (Kind::Pad { .. }, shape) => shape.contains(p) || point_gap(shape, p) <= 1_000.0,
        _ => true,
    }
}

/// When a track has one loose end because another track joined it mid-segment (such as a global
/// route meeting a pre-fanout inner-layer stub before its tip), trims the overhanging tail back to
/// the furthest neighbour joint so KiCad does not flag the unused tail as `track_dangling`.
fn trim_overhang(board: &mut Board, id: ItemId) -> Option<(Option<Span>, Vec<ItemId>)> {
    let item = board.item(id)?.clone();
    let (Kind::Track, Shape::Capsule { a, b, r }) = (&item.kind, &item.shape) else { return None };
    let (a, b, r) = (*a, *b, *r);
    if a == b || item.net == 0 {
        return None;
    }
    let layer = item.layers.trailing_zeros() as usize;
    let near = board.near(layer, item.shape.bbox());
    let end_met = |p: Point, q: Point| {
        near.iter().any(|&o| {
            o != id
                && board.item(o).is_some_and(|it| {
                    it.net == item.net
                        && it.is_copper()
                        && (!it.refilled() || board.is_plane(it))
                        && it.on(layer)
                        && end_overlaps(board, id, p, q, r, o, it)
                })
        })
    };
    let (keep, loose) = match (end_met(a, b), end_met(b, a)) {
        (true, false) => (a, b),
        (false, true) => (b, a),
        (false, false) => return trim_both(board, id, &item, a, b, r),
        _ => return None,
    };
    let around = neighbours(board, id);
    if around.is_empty() {
        return None;
    }
    let seg = Shape::Capsule { a: keep, b: loose, r: 0 };
    let mut best: Option<(f64, Point)> = None;
    for &o in &around {
        let Some(other) = board.item(o) else { continue };
        let pt = match &other.shape {
            Shape::Capsule { a: oa, b: ob, .. } => {
                if point_gap(&seg, *oa) <= point_gap(&seg, *ob) {
                    *oa
                } else {
                    *ob
                }
            }
            Shape::Polygon(pts) => {
                let n = pts.len().max(1) as board::Nm;
                Point::new(pts.iter().map(|q| q.x).sum::<board::Nm>() / n, pts.iter().map(|q| q.y).sum::<board::Nm>() / n)
            }
        };
        let (t, _) = closest_on_segment(keep, loose, pt);
        if t > 0.0 && best.is_none_or(|(bt, _)| t > bt) {
            best = Some((t, pt));
        }
    }
    let (_, target) = best?;
    let (_, proj) = closest_on_segment(keep, loose, target);
    let mut candidates: Vec<Vec<(Point, Point)>> = Vec::new();
    if target != keep && target != loose {
        candidates.push(vec![(keep, target)]);
    }
    if proj != keep && proj != loose {
        if proj == target {
            candidates.push(vec![(keep, proj)]);
        } else {
            candidates.push(vec![(keep, proj), (proj, target)]);
            // Cut back level with the joint, with no piece across: enough when the joint's copper
            // reaches the cut track there, as a via or another track's end beside it may.
            candidates.push(vec![(keep, proj)]);
        }
    }
    if candidates.is_empty() {
        return None;
    }
    let removed = board.remove(id)?;
    for segs in candidates {
        let mut new_ids = Vec::with_capacity(segs.len());
        for &(p, q) in &segs {
            new_ids.push(board.insert(Item {
                kind: Kind::Track,
                net: item.net,
                layers: item.layers,
                shape: Shape::Capsule { a: p, b: q, r },
                hole: None,
                locked: item.locked,
                span: None,
            }));
        }
        let legal = new_ids.iter().all(|&nid| check_item(board, nid).is_empty());
        let connected = around.iter().all(|&o| new_ids.iter().any(|&nid| joined(board, nid, o)));
        let clean = new_ids.iter().all(|&nid| !dangles(board, nid));
        if legal && connected && clean {
            return Some((removed.span, new_ids));
        }
        for nid in new_ids.into_iter().rev() {
            board.discard(nid);
        }
    }
    board.restore(id, removed);
    None
}

/// A track loose at both ends that still joins what lies along it, such as a coupled pair run
/// crossing the vias its legs came down: cut back to the stretch between its outermost joints,
/// a part of itself and so as legal as it was.
fn trim_both(board: &mut Board, id: ItemId, item: &Item, a: Point, b: Point, r: Nm) -> Option<(Option<Span>, Vec<ItemId>)> {
    let around = neighbours(board, id);
    let along = |p: Point| closest_on_segment(a, b, p);
    let mut ts: Vec<(f64, Point)> = Vec::new();
    for &o in &around {
        let Some(other) = board.item(o) else { continue };
        let at = match &other.shape {
            Shape::Capsule { a: oa, b: ob, .. } if oa == ob => *oa,
            Shape::Capsule { a: oa, b: ob, .. } => {
                if point_gap(&item.shape, *oa) <= point_gap(&item.shape, *ob) {
                    *oa
                } else {
                    *ob
                }
            }
            Shape::Polygon(_) => return None,
        };
        // Only a joint the track's own line runs through: its new end must lie in the joint.
        let (t, p) = along(at);
        if other.shape.contains(p) {
            ts.push((t, p));
        } else {
            return None;
        }
    }
    let first = ts.iter().copied().min_by(|x, y| x.0.total_cmp(&y.0))?;
    let last = ts.iter().copied().max_by(|x, y| x.0.total_cmp(&y.0))?;
    if first.1 == last.1 || (first.1 == a && last.1 == b) {
        return None;
    }
    let removed = board.remove(id)?;
    let nid = board.insert(Item { shape: Shape::Capsule { a: first.1, b: last.1, r }, span: None, ..item.clone() });
    let connected = around.iter().all(|&o| joined(board, nid, o));
    if connected && check_item(board, nid).is_empty() && !dangles(board, nid) {
        return Some((removed.span, vec![nid]));
    }
    board.discard(nid);
    board.restore(id, removed);
    None
}

/// Pieces a search for another way round may visit before giving up and keeping the piece.
const DETOUR_LIMIT: usize = 400;

/// Its net's copper joined to an item, leaving out pours a refill may take back.
fn neighbours(board: &Board, id: ItemId) -> Vec<ItemId> {
    neighbours_by(board, id, joined)
}

/// [`neighbours`] joined at full width, not by round ends that only overlap.
fn firm_neighbours(board: &Board, id: ItemId) -> Vec<ItemId> {
    neighbours_by(board, id, firmly_joined)
}

fn neighbours_by(board: &Board, id: ItemId, joins: fn(&Board, ItemId, ItemId) -> bool) -> Vec<ItemId> {
    let Some(item) = board.item(id) else { return Vec::new() };
    let bb = board.bbox(id).unwrap_or_else(|| item.shape.bbox());
    let mut out: Vec<ItemId> = (0..board.layers.len())
        .filter(|l| item.on(*l))
        .flat_map(|l| board.near(l, bb).into_iter().filter(move |&o| o != id))
        .filter(|&o| board.item(o).is_some_and(|it| it.net == item.net && it.is_copper() && (!it.refilled() || board.is_plane(it))) && joins(board, id, o))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Whether taking the item out leaves everything it joins still joined: true when it joins at
/// most one piece, or when a short search finds a way between all its neighbours without it.
/// Whatever KiCad calls a dangling end, copper that carries a connection stays, and so does a
/// short link between two tracks whose round ends would be left joined only by overlapping.
fn bridges_nothing(board: &Board, id: ItemId) -> bool {
    let around = neighbours(board, id);
    let Some((&first, rest)) = around.split_first() else { return true };
    if rest.is_empty() {
        return true;
    }
    let mut wanted: HashSet<ItemId> = rest.iter().copied().collect();
    let mut seen: HashSet<ItemId> = HashSet::from([id, first]);
    let mut stack = vec![first];
    while let Some(at) = stack.pop() {
        if seen.len() > DETOUR_LIMIT {
            return false;
        }
        for next in firm_neighbours(board, at) {
            if seen.insert(next) {
                wanted.remove(&next);
                if wanted.is_empty() {
                    return true;
                }
                stack.push(next);
            }
        }
    }
    false
}

/// Whether a track has both ends inside other tracks of its net, as a short piece from
/// one track into the side of another does: KiCad finds such a piece's end loose. Whether the
/// rest still joins without it is for the caller to judge.
fn redundant(board: &Board, id: ItemId) -> bool {
    let Some(item) = board.item(id) else { return false };
    let (Kind::Track, Shape::Capsule { a, b, .. }) = (&item.kind, &item.shape) else { return false };
    if item.net == 0 {
        return false;
    }
    let layer = item.layers.trailing_zeros() as usize;
    let near = board.near(layer, item.shape.bbox());
    let ours = |o: ItemId| board.item(o).filter(|c| o != id && c.net == item.net && c.on(layer));
    // A piece into a via is how the via meets this layer, as KiCad sees it.
    if near.iter().any(|&o| ours(o).is_some_and(|c| c.kind == Kind::Via && c.shape.gap_within(&item.shape, 0.0) == 0.0)) {
        return false;
    }
    // A link whose ends both meet other tracks' ends is a piece of a chain, which KiCad finds
    // whole: taking it out would leave its neighbours joined only where their round ends overlap.
    let meets_end = |p: Point| near.iter().any(|&o| ours(o).is_some_and(|c| matches!(c.shape, Shape::Capsule { a: q0, b: q1, .. } if c.kind == Kind::Track && (q0 == p || q1 == p))));
    if meets_end(*a) && meets_end(*b) {
        return false;
    }
    // Nor into a pad: what else meets the pad may do so only by overlapping it, which KiCad
    // does not count as an end met.
    [*a, *b].iter().all(|&p| near.iter().any(|&o| ours(o).is_some_and(|c| c.kind == Kind::Track && c.shape.contains(p))))
}

/// Whether KiCad would call it dangling: a track with an end that meets nothing of its net, or
/// lying wholly inside another piece of its net's copper that already joins everything it joins
/// (a stub that adds nothing); a via that meets its net's copper on fewer than two layers. Ends
/// meet as [`joined`] has them, by the track's round end. Pours do not count as covering a
/// track, as a refill may draw them back from it.
pub(crate) fn dangles(board: &Board, id: ItemId) -> bool {
    let Some(item) = board.item(id) else { return false };
    if item.net == 0 {
        return false;
    }
    let ours = |layer: usize, other: ItemId| {
        (other != id)
            .then(|| board.item(other))
            .flatten()
            .filter(|o| o.net == item.net && o.is_copper() && (!o.refilled() || board.is_plane(o)) && o.on(layer))
    };
    match (&item.kind, &item.shape) {
        (Kind::Track, Shape::Capsule { a, b, r }) => {
            let layer = item.layers.trailing_zeros() as usize;
            let near = board.near(layer, item.shape.bbox());
            // An end that only grazes copper, off its centre line or outside a pad, leads nowhere
            // a drawn joint would: the overlap left is narrower than the track.
            let loose_end = [(*a, *b), (*b, *a)].iter().any(|&(p, q)| {
                !near.iter().any(|&o| ours(layer, o).is_some_and(|other| end_overlaps(board, id, p, q, *r, o, other) && end_square(p, *r, other)))
            });
            if loose_end {
                return true;
            }
            let neighbours: Vec<ItemId> = near.iter().copied().filter(|&o| ours(layer, o).is_some() && joined(board, id, o)).collect();
            near.iter().any(|&cover| {
                ours(layer, cover).is_some_and(|c| !matches!(c.kind, Kind::Pour { .. } | Kind::Via) && c.shape.contains(*a) && c.shape.contains(*b))
                    && neighbours.iter().all(|&o| o == cover || firmly_joined(board, cover, o))
            })
        }
        (Kind::Via, _) => {
            let met = (0..board.layers.len())
                .filter(|l| item.on(*l))
                .filter(|&l| board.near(l, item.shape.bbox()).into_iter().any(|o| ours(l, o).is_some_and(|o| o.shape.gap_within(&item.shape, 0.0) == 0.0)))
                .count();
            met < 2
        }
        _ => false,
    }
}

fn length(points: &[Point]) -> f64 {
    points.windows(2).map(|w| w[0].distance(w[1])).sum::<f64>() * 1e-6
}

/// Pulls every chain of the router's tracks tight. `soft` holds the copper the router laid and
/// is updated with the tracks that replace it. Chains not reached by `deadline` stay as they are.
pub fn pull_tight(board: &mut Board, soft: &mut Vec<ItemId>, deadline: &Deadline) -> CleanStats {
    pull_tight_watch(board, soft, deadline, &mut |_, _, _, _| {})
}

/// The straight track from `chain`'s point `i` to its point `j` in place of the tracks between,
/// if whatever joined those tracks still joins it or the chain's tracks either side, and it
/// breaks no rule: its id and the tracks it replaced. The board is left as it was otherwise.
fn straighten(board: &mut Board, chain: &Chain, i: usize, j: usize) -> Option<(ItemId, Vec<Item>)> {
    let span: HashSet<ItemId> = chain.tracks[i..j].iter().copied().collect();
    let mut partners: Vec<ItemId> = Vec::new();
    for &t in &chain.tracks[i..j] {
        let Some(track) = board.item(t) else { continue };
        for l in (0..board.layers.len()).filter(|l| track.on(*l)) {
            for other in board.near(l, track.shape.bbox()) {
                if !span.contains(&other) && board.item(other).is_some_and(|o| o.net == chain.net) && joined(board, t, other) {
                    partners.push(other);
                }
            }
        }
    }
    partners.sort_unstable();
    partners.dedup();
    let sides: Vec<ItemId> = [i.checked_sub(1).map(|k| chain.tracks[k]), chain.tracks.get(j).copied()].into_iter().flatten().collect();
    let removed: Vec<(ItemId, Item)> = chain.tracks[i..j].iter().filter_map(|t| Some((*t, board.remove(*t)?))).collect();
    let straight = Item {
        kind: Kind::Track,
        net: chain.net,
        layers: chain.layer,
        shape: Shape::Capsule { a: chain.points[i], b: chain.points[j], r: chain.half_width },
        hole: None,
        locked: false,
        span: None,
    };
    let id = board.insert(straight);
    let still_joined = partners.iter().all(|&p| joined(board, p, id) || sides.iter().any(|&s| s == p || joined(board, p, s)));
    if still_joined && check_item(board, id).is_empty() {
        return Some((id, removed.into_iter().map(|(_, it)| it).collect()));
    }
    board.discard(id);
    // Put back what was taken, under the ids the chain knows them by.
    for (old, item) in removed {
        board.restore(old, item);
    }
    None
}

/// How far a bend may lie off the straight line between its neighbours and still be taken out
/// as a step of a staircase: a grid path's 10 um steps, which pulling tight never reaches on a
/// differential pair's copper (or anywhere, once time runs out), and which the graze bridging
/// then joined to each other pairwise (easy2350's USB_DP_C, 162 pieces where a dozen do).
const STEP_TOLERANCE: f64 = 15_000.0;

/// Takes out the steps of the router's staircases: each bend lying within [`STEP_TOLERANCE`] of
/// the straight line between its neighbours, replaced by that line where it joins all the bend
/// did and breaks no rule, round after round until none is left. Too slight to pull a coupled
/// pair apart. How many it took out.
pub fn smooth_staircases(board: &mut Board, soft: &mut Vec<ItemId>) -> usize {
    let mut taken = 0;
    loop {
        let soft_set: HashSet<ItemId> = soft.iter().copied().filter(|id| board.item(*id).is_some()).collect();
        let mut replaced: HashSet<ItemId> = HashSet::new();
        let mut added: Vec<ItemId> = Vec::new();
        for chain in chains(board, &soft_set) {
            let mut k = 1;
            while k + 1 < chain.points.len() {
                let (a, p, b) = (chain.points[k - 1], chain.points[k], chain.points[k + 1]);
                if board::point_segment(p, a, b) <= STEP_TOLERANCE && chain.tracks[k - 1..=k].iter().all(|t| !replaced.contains(t)) {
                    if let Some((id, _)) = straighten(board, &chain, k - 1, k + 1) {
                        replaced.extend(&chain.tracks[k - 1..=k]);
                        added.push(id);
                        k += 2;
                        continue;
                    }
                }
                k += 1;
            }
        }
        if added.is_empty() {
            return taken;
        }
        taken += added.len();
        soft.retain(|id| !replaced.contains(id) && board.item(*id).is_some());
        soft.extend(added);
    }
}

/// Takes out the router's turn-backs: where a chain turns by more than 135 degrees (it runs past
/// a point and comes back, as a pair's escape overshooting its via did on the earring's USB_C_DN),
/// the straight track across the turn in its place, where it joins all the turn joined and breaks
/// no rule. Pulling tight does this for the single-ended copper; this reaches a differential
/// pair's too, which pulling tight leaves alone, without pulling its coupled runs apart (they
/// never turn back). How many it took out.
pub fn straighten_turn_backs(board: &mut Board, soft: &mut Vec<ItemId>) -> usize {
    let soft_set: HashSet<ItemId> = soft.iter().copied().filter(|id| board.item(*id).is_some()).collect();
    let mut replaced: HashSet<ItemId> = HashSet::new();
    let mut added: Vec<ItemId> = Vec::new();
    let mut taken = 0;
    for chain in chains(board, &soft_set) {
        let mut k = 1;
        while k + 1 < chain.points.len() {
            let (a, p, b) = (chain.points[k - 1], chain.points[k], chain.points[k + 1]);
            let (u, v) = (((a.x - p.x) as f64, (a.y - p.y) as f64), ((b.x - p.x) as f64, (b.y - p.y) as f64));
            let inside = (u.0 * v.1 - u.1 * v.0).atan2(u.0 * v.0 + u.1 * v.1).abs();
            if inside < 45f64.to_radians() && chain.tracks[k - 1..=k].iter().all(|t| !replaced.contains(t)) {
                if let Some((id, _)) = straighten(board, &chain, k - 1, k + 1) {
                    replaced.extend(&chain.tracks[k - 1..=k]);
                    added.push(id);
                    taken += 1;
                    k += 2;
                    continue;
                }
            }
            k += 1;
        }
    }
    soft.retain(|id| !replaced.contains(id) && board.item(*id).is_some());
    soft.extend(added);
    taken
}

/// Sees a chain pulled tight: the board after, the net, the tracks replaced and the new ones.
pub type TightWatch<'a> = dyn FnMut(&Board, u32, &[Item], &[ItemId]) + 'a;

/// As [`pull_tight`], calling `watch` with the net, replaced tracks and new track ids whenever a
/// chain is shortened.
pub fn pull_tight_watch(board: &mut Board, soft: &mut Vec<ItemId>, deadline: &Deadline, watch: &mut TightWatch) -> CleanStats {
    let soft_set: HashSet<ItemId> = soft.iter().copied().filter(|id| board.item(*id).is_some()).collect();
    let mut stats = CleanStats::default();
    let mut replaced: HashSet<ItemId> = HashSet::new();
    let mut added: Vec<ItemId> = Vec::new();
    for chain in chains(board, &soft_set) {
        if deadline.expired() {
            break;
        }
        stats.chains += 1;
        stats.segments_before += chain.tracks.len();
        stats.length_before += length(&chain.points);
        let mut kept_points = vec![chain.points[0]];
        let mut chain_old: Vec<Item> = Vec::new();
        let mut chain_new: Vec<ItemId> = Vec::new();
        let mut i = 0;
        let n = chain.points.len() - 1;
        while i < n {
            let mut j = n;
            let mut laid = None;
            let mut taken_here: Vec<Item> = Vec::new();
            while j > i + 1 {
                if let Some((id, taken)) = straighten(board, &chain, i, j) {
                    taken_here = taken;
                    laid = Some(id);
                    break;
                }
                j -= 1;
            }
            match laid {
                Some(id) => {
                    replaced.extend(&chain.tracks[i..j]);
                    added.push(id);
                    chain_old.extend(taken_here);
                    chain_new.push(id);
                }
                None => j = i + 1,
            }
            kept_points.push(chain.points[j]);
            i = j;
        }
        stats.segments_after += kept_points.len() - 1;
        stats.length_after += length(&kept_points);
        if !chain_new.is_empty() {
            watch(board, chain.net, &chain_old, &chain_new);
        }
    }
    soft.retain(|id| !replaced.contains(id) && board.item(*id).is_some());
    soft.extend(added);
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_board() -> Board {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"N1\"))",
        )
        .unwrap();
        Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap())
    }

    fn seg(a: Point, b: Point) -> Item {
        Item { kind: Kind::Track, net: 1, layers: 1, shape: Shape::Capsule { a, b, r: 50_000 }, hole: None, locked: false, span: None }
    }

    #[test]
    fn a_short_piece_from_one_track_into_the_side_of_another_is_pruned() {
        let mut board = empty_board();
        let mut put = |kind: Kind, a: Point, b: Point, r| {
            board.insert(Item { kind, net: 1, layers: 1, shape: Shape::Capsule { a, b, r }, hole: None, locked: false, span: None })
        };
        let pad = || Kind::Pad { footprint: 0, number: "1".into(), plated: false };
        // bluebuzz's XOUT on In1.Cu, less (101.9, 116.6) mm: a fan-out stub from one pad, a
        // route from the other that crosses the stub's tip, and the route's last short piece
        // ending in the stub's side.
        put(pad(), Point::new(90_000, 20_000), Point::new(90_000, 20_000), 100_000);
        put(pad(), Point::new(275_000, 1_230_000), Point::new(275_000, 1_230_000), 100_000);
        let stub = put(Kind::Track, Point::new(90_000, 20_000), Point::new(90_000, 660_000), 63_500);
        let route = put(Kind::Track, Point::new(275_000, 1_230_000), Point::new(25_000, 530_000), 75_000);
        let piece = put(Kind::Track, Point::new(25_000, 530_000), Point::new(75_000, 430_000), 75_000);
        let mut soft = vec![stub, route, piece];
        prune_dangling(&mut board, &mut soft);
        assert!(board.item(piece).is_none(), "the short piece into the stub's side adds nothing");
        assert!(board.item(stub).is_some() && board.item(route).is_some());
        assert!(board::connectivity(&board).airlines.is_empty());
    }

    fn pad_at(board: &mut Board, x: Nm, y: Nm) -> ItemId {
        board.insert(Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(x, y), b: Point::new(x, y), r: 150_000 },
            hole: None,
            locked: true,
            span: None,
        })
    }

    #[test]
    fn short_links_of_a_chain_stay_so_no_joint_is_left_to_overlapping_round_ends() {
        // MySRaspiGW's MOSI on B.Cu: a grid path's diagonal steps, each a link shorter than the
        // track is wide. Taken out, they left the runs either side joined only where their round
        // ends overlapped, 0.06 mm wide on a 0.254 mm track.
        let mut board = empty_board();
        let r = 127_000;
        let pts = [(0, 0), (2_000_000, 0), (2_200_000, 100_000), (2_250_000, 150_000), (2_450_000, 300_000), (3_375_000, 565_000)];
        pad_at(&mut board, 0, 0);
        pad_at(&mut board, 3_375_000, 565_000);
        let mut soft: Vec<ItemId> = pts
            .windows(2)
            .map(|w| board.insert(Item { shape: Shape::Capsule { a: Point::new(w[0].0, w[0].1), b: Point::new(w[1].0, w[1].1), r }, ..seg(Point::new(0, 0), Point::new(0, 0)) }))
            .collect();
        prune_dangling(&mut board, &mut soft);
        assert_eq!(soft.len(), 5, "every link stays");
        assert!(narrow_joints(&board, &soft).is_empty());
        assert!(board::connectivity(&board).airlines.is_empty());
    }

    #[test]
    fn a_link_inside_its_neighbour_stays_when_the_neighbours_would_join_only_by_their_ends() {
        // The earring's VBATT on In2.Cu: a run, a link one diagonal grid step long lying wholly
        // inside the next run's round end, then that run.
        let mut board = empty_board();
        let r = 75_000;
        pad_at(&mut board, 0, 0);
        pad_at(&mut board, 2_100_000, 1_100_000);
        let put = |board: &mut Board, a: (Nm, Nm), b: (Nm, Nm)| board.insert(Item { shape: Shape::Capsule { a: Point::new(a.0, a.1), b: Point::new(b.0, b.1), r }, ..seg(Point::new(0, 0), Point::new(0, 0)) });
        let first = put(&mut board, (0, 0), (1_000_000, 0));
        let link = put(&mut board, (1_000_000, 0), (1_100_000, 100_000));
        let second = put(&mut board, (1_100_000, 100_000), (1_100_000, 1_100_000));
        let last = put(&mut board, (1_100_000, 1_100_000), (2_100_000, 1_100_000));
        let mut soft = vec![first, link, second, last];
        prune_dangling(&mut board, &mut soft);
        assert!(board.item(link).is_some(), "the link carries the joint at full width");
        assert!(narrow_joints(&board, &soft).is_empty());
    }

    #[test]
    fn a_link_between_two_runs_one_step_apart_stays_though_each_run_ends_in_the_others_round_end() {
        // sms-cart's /a5 on F.Cu: two diagonal runs whose ends lie a grid step apart, each inside
        // the other's round end, and the step between them. The shove's tidy-up took the step out
        // as dangling when an end inside a round end counted as a full-width joint.
        let mut board = empty_board();
        let r = 125_000;
        pad_at(&mut board, 2_900_000, 2_950_000);
        pad_at(&mut board, 0, 0);
        let put = |board: &mut Board, a: (Nm, Nm), b: (Nm, Nm)| board.insert(Item { shape: Shape::Capsule { a: Point::new(a.0, a.1), b: Point::new(b.0, b.1), r }, ..seg(Point::new(0, 0), Point::new(0, 0)) });
        let first = put(&mut board, (2_900_000, 2_950_000), (300_000, 350_000));
        let link = put(&mut board, (300_000, 350_000), (300_000, 300_000));
        let second = put(&mut board, (300_000, 300_000), (0, 0));
        let mut soft = vec![first, link, second];
        prune_dangling(&mut board, &mut soft);
        assert!(board.item(link).is_some(), "the runs meet squarely only through the link");
    }

    #[test]
    fn a_track_ending_short_of_its_pad_is_found_and_carried_on_into_it() {
        // easy2350's U1-6 on +1V1: a 0.25 mm track's end 0.1 mm short of a QFN pad's end, joined
        // only by the chord its round end cuts across the pad's edge.
        let mut board = empty_board();
        let pad = board.insert(Item {
            kind: Kind::Pad { footprint: 0, number: "6".into(), plated: false },
            net: 1,
            layers: 1,
            shape: Shape::Polygon(vec![Point::new(-100_000, -437_500), Point::new(100_000, -437_500), Point::new(100_000, 437_500), Point::new(-100_000, 437_500)]),
            hole: None,
            locked: true,
            span: None,
        });
        let track = board.insert(Item { shape: Shape::Capsule { a: Point::new(0, 537_500), b: Point::new(0, 2_000_000), r: 125_000 }, ..seg(Point::new(0, 0), Point::new(0, 0)) });
        let mut laid = vec![track];
        let found = pad_joints(&board, &laid);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].track, found[0].pad), (track, pad));
        assert!(found[0].width < found[0].full);
        assert_eq!(mend_pad_joints(&mut board, &mut laid), 1);
        assert!(pad_joints(&board, &laid).is_empty());
    }

    #[test]
    fn a_track_ending_in_a_pads_corner_is_carried_on_into_it_and_one_along_an_edge_is_left() {
        // 59pct_keyboard's D01:9: a 0.18 mm track ending 1 um inside a 1 mm square pad's corner,
        // half its round end outside, 0.13 mm of copper round the corner as KiCad measures it.
        let mut board = empty_board();
        let pad = board.insert(Item {
            kind: Kind::Pad { footprint: 0, number: "C".into(), plated: false },
            net: 1,
            layers: 1,
            shape: Shape::Polygon(vec![Point::new(-500_000, -500_000), Point::new(500_000, -500_000), Point::new(500_000, 500_000), Point::new(-500_000, 500_000)]),
            hole: None,
            locked: true,
            span: None,
        });
        let track = |board: &mut Board, end: (Nm, Nm), from: (Nm, Nm)| board.insert(Item { shape: Shape::Capsule { a: Point::new(end.0, end.1), b: Point::new(from.0, from.1), r: 90_000 }, ..seg(Point::new(0, 0), Point::new(0, 0)) });
        let corner = track(&mut board, (499_000, 497_000), (5_149_000, 13_197_000));
        let mut laid = vec![corner];
        let found = pad_joints(&board, &laid);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].track, found[0].pad), (corner, pad));
        assert_eq!(mend_pad_joints(&mut board, &mut laid), 1);
        assert!(pad_joints(&board, &laid).is_empty(), "{:?}", pad_joints(&board, &laid));
        // dc25_spqr_badge's C25: the end a micrometre outside the corner, as good as inside.
        let mut board = empty_board();
        board.insert(Item { kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false }, net: 1, layers: 1, shape: Shape::Polygon(vec![Point::new(-500_000, -500_000), Point::new(500_000, -500_000), Point::new(500_000, 500_000), Point::new(-500_000, 500_000)]), hole: None, locked: true, span: None });
        let outside = track(&mut board, (500_400, 500_500), (5_149_000, 13_197_000));
        assert_eq!(pad_joints(&board, &[outside]).len(), 1);
        // An end as shallow in the middle of an edge crosses it at the track's full width.
        let mut board = empty_board();
        board.insert(Item { kind: Kind::Pad { footprint: 0, number: "C".into(), plated: false }, net: 1, layers: 1, shape: Shape::Polygon(vec![Point::new(-500_000, -500_000), Point::new(500_000, -500_000), Point::new(500_000, 500_000), Point::new(-500_000, 500_000)]), hole: None, locked: true, span: None });
        let edge = track(&mut board, (0, 497_000), (0, 5_000_000));
        assert!(pad_joints(&board, &[edge]).is_empty());
    }

    #[test]
    fn a_short_track_is_not_split_where_an_end_lies_beside_it() {
        // PCIE-to-MXM's write stage: a 64 um piece of a 0.25 mm track, another track's end 60 um
        // to the side of its middle. Split there, each half was split again at the other's far
        // end, making the piece again, for ever.
        let mut board = empty_board();
        let r = 125_000;
        let put = |board: &mut Board, a: (Nm, Nm), b: (Nm, Nm)| board.insert(Item { shape: Shape::Capsule { a: Point::new(a.0, a.1), b: Point::new(b.0, b.1), r }, ..seg(Point::new(0, 0), Point::new(0, 0)) });
        let piece = put(&mut board, (85_950_000, 126_630_000), (85_910_000, 126_580_000));
        let beside = put(&mut board, (85_976_850, 126_567_500), (86_976_850, 126_567_500));
        let mut soft = vec![piece, beside];
        let (split, _) = split_at_joins(&mut board, &mut soft);
        assert_eq!(split, 0);
        assert!(board.item(piece).is_some());
    }

    #[test]
    fn a_joint_of_overlapping_round_ends_is_found_and_mended_onto_the_other_track() {
        let mut board = empty_board();
        pad_at(&mut board, 0, 0);
        pad_at(&mut board, 1_100_000, 1_100_000);
        let wide = |a, b| Item { shape: Shape::Capsule { a, b, r: 75_000 }, ..seg(a, b) };
        let first = board.insert(wide(Point::new(0, 0), Point::new(1_000_000, 0)));
        // Its end one diagonal 0.1 mm grid step from the first's: 0.141 mm apart, 0.05 mm wide.
        let second = board.insert(wide(Point::new(1_100_000, 100_000), Point::new(1_100_000, 1_100_000)));
        let mut laid = vec![first, second];
        let found = narrow_joints(&board, &laid);
        assert_eq!(found.len(), 2, "each end overlaps the other's: {found:?}");
        assert!(found.iter().all(|j| j.width < 0.75 * j.full), "{found:?}");
        assert!(board::joined(&board, first, second), "KiCad counts it joined all the same");
        assert_eq!(mend_joints(&mut board, &mut laid), 1, "one piece mends both ends");
        assert!(narrow_joints(&board, &laid).is_empty());
        assert!(board::connectivity(&board).airlines.is_empty());
    }

    #[test]
    fn a_run_crossing_the_vias_it_joins_loses_both_tails() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal)) (net 0 \"\") (net 1 \"N1\"))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let mut put = |kind: Kind, layers: u64, a: Point, b: Point, r, hole| {
            board.insert(Item { kind, net: 1, layers, shape: Shape::Capsule { a, b, r }, hole, locked: false, span: None })
        };
        let pad = || Kind::Pad { footprint: 0, number: "1".into(), plated: false };
        // Two pads on the back, each under a via; a front track runs over both vias and 1 mm
        // past each, carrying the only connection between them.
        for x in [1_000_000, 3_000_000] {
            put(pad(), 2, Point::new(x, 0), Point::new(x, 0), 150_000, None);
            put(Kind::Via, 3, Point::new(x, 50_000), Point::new(x, 50_000), 200_000, Some((Point::new(x, 50_000), 100_000)));
        }
        let run = put(Kind::Track, 1, Point::new(0, 0), Point::new(4_000_000, 0), 75_000, None);
        let mut soft = vec![run];
        prune_dangling(&mut board, &mut soft);
        assert!(board.item(run).is_none());
        let [cut] = soft[..] else { panic!("one track in place of the run: {soft:?}") };
        assert_eq!(board.item(cut).unwrap().shape, Shape::Capsule { a: Point::new(1_000_000, 0), b: Point::new(3_000_000, 0), r: 75_000 });
        assert!(board::connectivity(&board).airlines.is_empty());
    }

    #[test]
    fn a_staircase_of_small_steps_is_smoothed_into_one_track() {
        // easy2350's USB_DP_C escape: 10 um steps, the track zigzagging 10 um off its line.
        let mut board = empty_board();
        pad_at(&mut board, 0, 0);
        pad_at(&mut board, 1_000_000, 0);
        let t = |board: &mut Board, a: (Nm, Nm), b: (Nm, Nm)| board.insert(Item { shape: Shape::Capsule { a: Point::new(a.0, a.1), b: Point::new(b.0, b.1), r: 100_000 }, ..seg(Point::new(0, 0), Point::new(0, 0)) });
        let pts: Vec<(Nm, Nm)> = (0..=10).map(|k| (k * 100_000, if k % 2 == 1 && k < 10 { 10_000 } else { 0 })).collect();
        let mut soft: Vec<ItemId> = pts.windows(2).map(|w| t(&mut board, w[0], w[1])).collect();
        assert!(smooth_staircases(&mut board, &mut soft) > 0);
        // One track runs between the pads; a bend inside a pad stays, as pulling tight leaves it.
        assert!(soft.len() <= 3, "{} pieces left", soft.len());
        let longest = soft.iter().filter_map(|id| match board.item(*id).unwrap().shape {
            Shape::Capsule { a, b, .. } => Some((a, b)),
            _ => None,
        }).max_by(|x, y| x.0.distance(x.1).total_cmp(&y.0.distance(y.1))).unwrap();
        assert!(longest.0.distance(longest.1) >= 800_000.0, "the run between the pads is one track: {longest:?}");
        assert!(board::connectivity(&board).airlines.is_empty());
    }

    #[test]
    fn a_track_that_runs_past_its_via_and_comes_back_is_straightened() {
        // The earring's USB_C_DN escape: from its via 0.24 mm on past it, then back up past the via
        // the way it came, a turn of 177 degrees.
        let mut board = empty_board();
        let via = board.insert(Item { kind: Kind::Via, net: 1, layers: 1, shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(0, 0), r: 200_000 }, hole: Some((Point::new(0, 0), 100_000)), locked: false, span: None });
        pad_at(&mut board, 600_000, -2_100_000);
        let t = |board: &mut Board, a: (Nm, Nm), b: (Nm, Nm)| board.insert(Item { shape: Shape::Capsule { a: Point::new(a.0, a.1), b: Point::new(b.0, b.1), r: 75_000 }, ..seg(Point::new(0, 0), Point::new(0, 0)) });
        let mut soft = vec![t(&mut board, (0, 0), (-78_000, 230_000)), t(&mut board, (-78_000, 230_000), (600_000, -2_100_000))];
        assert_eq!(straighten_turn_backs(&mut board, &mut soft), 1);
        let [one] = soft[..] else { panic!("{soft:?}") };
        assert_eq!(board.item(one).unwrap().shape, Shape::Capsule { a: Point::new(0, 0), b: Point::new(600_000, -2_100_000), r: 75_000 });
        assert!(board.item(via).is_some() && board::connectivity(&board).airlines.is_empty());
    }

    #[test]
    fn tracks_that_only_graze_are_bridged_and_joined_or_crossing_ones_left() {
        // Own-Mailbox's GND: two 0.2 mm tracks 0.197 mm apart, centre to centre, overlapping by
        // 3 um, which KiCad measures as a 0.03 mm neck.
        let mut board = empty_board();
        let t = |board: &mut Board, a: (Nm, Nm), b: (Nm, Nm)| board.insert(Item { shape: Shape::Capsule { a: Point::new(a.0, a.1), b: Point::new(b.0, b.1), r: 100_000 }, ..seg(Point::new(0, 0), Point::new(0, 0)) });
        let one = t(&mut board, (0, 0), (2_000_000, 0));
        let two = t(&mut board, (0, 197_000), (2_000_000, 197_000));
        let mut laid = vec![one, two];
        let found = grazes(&board, &laid);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!((found[0].overlap - 3_000.0).abs() < 1.0);
        assert_eq!(mend_grazes(&mut board, &mut laid), 1);
        assert!(grazes(&board, &laid).is_empty(), "{:?}", grazes(&board, &laid));
        // End to end, and crossing outright: joints, not grazes.
        let mut board = empty_board();
        let laid = [t(&mut board, (0, 0), (1_000_000, 0)), t(&mut board, (1_000_000, 0), (2_000_000, 500_000)), t(&mut board, (500_000, -500_000), (500_000, 500_000))];
        assert!(grazes(&board, &laid).is_empty(), "{:?}", grazes(&board, &laid));
    }

    #[test]
    fn tightening_keeps_a_bend_where_a_track_of_another_width_ends() {
        // BeaconBuddy's VDD: a 0.2 mm track ending on the bend of a 0.2032 mm chain. Pulled
        // tight past the bend, the chain left the narrower track's end beside its new line.
        let mut board = empty_board();
        let pad = |x| Item { kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false }, net: 1, layers: 1, shape: Shape::Capsule { a: Point::new(x, 0), b: Point::new(x, 0), r: 200_000 }, hole: None, locked: true, span: None };
        board.insert(pad(0));
        board.insert(pad(2_000_000));
        board.insert(Item { shape: Shape::Capsule { a: Point::new(1_000_000, 3_000_000), b: Point::new(1_000_000, 3_000_000), r: 200_000 }, ..pad(0) });
        let wide = |a: (Nm, Nm), b: (Nm, Nm)| Item { shape: Shape::Capsule { a: Point::new(a.0, a.1), b: Point::new(b.0, b.1), r: 101_600 }, ..seg(Point::new(0, 0), Point::new(0, 0)) };
        let mut soft = vec![
            board.insert(wide((0, 0), (1_000_000, 100_000))),
            board.insert(wide((1_000_000, 100_000), (2_000_000, 0))),
            board.insert(Item { shape: Shape::Capsule { a: Point::new(1_000_000, 100_000), b: Point::new(1_000_000, 3_000_000), r: 100_000 }, ..seg(Point::new(0, 0), Point::new(0, 0)) }),
        ];
        pull_tight(&mut board, &mut soft, &Default::default());
        // The bend 0.1 mm off the straight line: pulled tight, the narrower track's round end
        // would still overlap it, joined only there.
        let bend = Point::new(1_000_000, 100_000);
        let wide_at_bend = soft.iter().filter_map(|&id| board.item(id)).filter(|it| matches!(it.shape, Shape::Capsule { a, b, r: 101_600 } if a == bend || b == bend)).count();
        assert_eq!(wide_at_bend, 2, "the junction stays where the narrower track ends");
    }

    #[test]
    fn watch_hooks_report_tightened_chains_duplicates_and_dangling_stubs() {
        let mut board = empty_board();
        for x in [0, 1_000_000] {
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
        let s1 = board.insert(seg(Point::new(0, 0), Point::new(500_000, 300_000)));
        let s2 = board.insert(seg(Point::new(500_000, 300_000), Point::new(1_000_000, 0)));
        let mut soft = vec![s1, s2];
        let mut tightened = 0;
        let clean = pull_tight_watch(&mut board, &mut soft, &Default::default(), &mut |_, net, old, new| {
            assert_eq!(net, 1);
            assert_eq!(old.len(), 2);
            assert_eq!(new.len(), 1);
            tightened += 1;
        });
        assert_eq!((clean.segments_before, clean.segments_after, tightened), (2, 1, 1));

        // A duplicate of the pulled-tight track and a dangling branch off one end.
        let dup = board.insert(seg(Point::new(0, 0), Point::new(1_000_000, 0)));
        let stub = board.insert(seg(Point::new(0, 0), Point::new(0, 600_000)));
        soft.extend([dup, stub]);
        let mut dups = 0;
        assert_eq!(drop_duplicates_watch(&mut board, &mut soft, &mut |_, _| dups += 1).0, 1);
        assert_eq!(dups, 1);
        let mut pruned = 0;
        assert_eq!(prune_dangling_watch(&mut board, &mut soft, &mut |_, _| pruned += 1).0, 1);
        assert_eq!(pruned, 1);
    }

    #[test]
    fn prune_dangling_removes_single_layer_via_and_trims_overhanging_stub() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal)) (net 0 \"\") (net 1 \"N1\"))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let p1 = board.insert(Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(0, 0), r: 100_000 },
            hole: None,
            locked: true,
            span: None,
        });
        let p2 = board.insert(Item {
            kind: Kind::Pad { footprint: 0, number: "2".into(), plated: false },
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(2_000_000, 300_000), b: Point::new(2_000_000, 300_000), r: 100_000 },
            hole: None,
            locked: true,
            span: None,
        });
        // Short track from p1 to via center (covered by the via's 200 um radius), an unused via at
        // (120_000, 0), a long vertical stub from (120_000, 0) to (120_000, 1_000_000), and a
        // horizontal track from p2 joining that stub mid-segment at (120_000, 300_000).
        let t_pad = board.insert(seg(Point::new(0, 0), Point::new(120_000, 0)));
        let unused_via = board.insert(Item {
            kind: Kind::Via,
            net: 1,
            layers: 3,
            shape: Shape::Capsule { a: Point::new(120_000, 0), b: Point::new(120_000, 0), r: 200_000 },
            hole: Some((Point::new(120_000, 0), 100_000)),
            locked: false,
            span: None,
        });
        let stub = board.insert(seg(Point::new(120_000, 0), Point::new(120_000, 1_000_000)));
        let branch = board.insert(seg(Point::new(120_000, 300_000), Point::new(2_000_000, 300_000)));
        let mut soft = vec![t_pad, unused_via, stub, branch];
        let (pruned, _) = prune_dangling(&mut board, &mut soft);
        assert!(pruned >= 2, "expected unused via and overhanging stub to be cleaned: pruned={pruned}");
        assert!(board.item(unused_via).is_none(), "single-layer via should be pruned");
        assert!(board.item(t_pad).is_some(), "pad-to-via-center track on F.Cu must stay");
        assert!(board::connectivity(&board).airlines.is_empty(), "pads p1={p1} and p2={p2} must stay connected");
        for &id in &soft {
            assert!(!dangles(&board, id), "remaining soft item {id} still dangles");
        }
    }

    #[test]
    fn a_track_another_ends_on_is_split_there() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"N1\"))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let track = |a: (i64, i64), b: (i64, i64)| Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(a.0, a.1), b: Point::new(b.0, b.1), r: 75_000 },
            hole: None,
            locked: false,
            span: None,
        };
        // A short piece at 45 degrees from the long track's middle, 6 um off its centre line
        // (KiCad calls it dangling), and one from its end (already end to end).
        let long = board.insert(track((0, 0), (10_000_000, 0)));
        let t = board.insert(track((5_000_000, 6_000), (5_100_000, 106_000)));
        let e = board.insert(track((10_000_000, 0), (10_000_000, 1_000_000)));
        let mut soft = vec![long, t, e];
        assert_eq!(split_at_joins(&mut board, &mut soft).0, 1);
        assert!(board.item(long).is_none());
        let mut ends: Vec<(Point, Point)> = soft
            .iter()
            .filter(|&&id| id != t && id != e)
            .filter_map(|&id| match board.item(id)?.shape {
                Shape::Capsule { a, b, .. } => Some((a, b)),
                _ => None,
            })
            .collect();
        ends.sort_by_key(|(a, _)| a.x);
        assert_eq!(ends, vec![(Point::new(0, 0), Point::new(5_000_000, 6_000)), (Point::new(5_000_000, 6_000), Point::new(10_000_000, 0))]);
    }

    #[test]
    fn kissing_track_ends_count_as_dangling_and_are_pruned() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"N1\"))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let pad = |x, y| Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(x, y), b: Point::new(x, y), r: 150_000 },
            hole: None,
            locked: true,
            span: None,
        };
        board.insert(pad(0, 0));
        board.insert(pad(1_000_000, 0));
        // Main run joins the two pads; a redundant 3-piece detour has a 0.15 mm middle bridge
        // (`r = 75_000`, so removing the middle leaves two ends 0.15 mm = `2 * r` apart that
        // merely kiss without overlapping). Both stubs must be recognised as dangling and pruned.
        let main_run = board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(1_000_000, 0), r: 75_000 },
            hole: None,
            locked: false,
            span: None,
        });
        let left_stub = board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(100_000, 0), b: Point::new(425_000, 300_000), r: 75_000 },
            hole: None,
            locked: false,
            span: None,
        });
        let bridge = board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(425_000, 300_000), b: Point::new(575_000, 300_000), r: 75_000 },
            hole: None,
            locked: false,
            span: None,
        });
        let right_stub = board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(575_000, 300_000), b: Point::new(900_000, 0), r: 75_000 },
            hole: None,
            locked: false,
            span: None,
        });
        let mut soft = vec![main_run, left_stub, bridge, right_stub];
        let (pruned, _) = prune_dangling(&mut board, &mut soft);
        assert_eq!(pruned, 3, "bridge and both kissing-end stubs must be pruned");
        assert_eq!(soft, vec![main_run]);
        assert!(board::connectivity(&board).airlines.is_empty());
    }

    #[test]
    fn prune_dangling_of_takes_only_the_copper_picked() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"N1\") (net 2 \"N2\"))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let mut soft = Vec::new();
        for (net, y) in [(1, 0), (2, 1_000_000)] {
            board.insert(Item {
                kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
                net,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(0, y), b: Point::new(0, y), r: 100_000 },
                hole: None,
                locked: true,
                span: None,
            });
            soft.push(board.insert(Item {
                kind: Kind::Track,
                net,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(0, y), b: Point::new(800_000, y), r: 50_000 },
                hole: None,
                locked: false,
                span: None,
            }));
        }
        let (s1, s2) = (soft[0], soft[1]);
        let (pruned, _) = prune_dangling_of(&mut board, &mut soft, &|it| it.net != 2, &mut |_, _| {});
        assert_eq!(pruned, 1);
        assert!(board.item(s1).is_none(), "net 1's stub goes");
        assert!(board.item(s2).is_some(), "net 2's stub was not picked");
        assert_eq!(soft, vec![s2]);
    }
}
