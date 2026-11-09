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
    pub kept_ns: u64,
    pub unchanged_ns: u64,
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
        let (layers, net, _, p) = *key;
        let probe = board::BBox::of([p]);
        // A kept pour of the chain's own net under the point fixes nothing: the chain lies in it
        // whichever way it bends, and straightening keeps what it joined (easy2350's +3V3 into
        // U1, a staircase inside a protected strip, every step held as a junction).
        let own_pour = |o: &Item| matches!(o.kind, Kind::Pour { .. }) && o.net == net;
        (0..board.layers.len()).filter(|l| layers & (1 << l) != 0).any(|l| {
            board.near(l, probe).into_iter().any(|other| {
                !soft.contains(&other) && board.item(other).is_some_and(|o| o.is_copper() && o.shape.contains(p) && !o.refilled() && !own_pour(o))
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
    let _b = board::work::enter(board::work::Bucket::Geometry);
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
    let _b = board::work::enter(board::work::Bucket::Geometry);
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
    let _b = board::work::enter(board::work::Bucket::Geometry);
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
    let _b = board::work::enter(board::work::Bucket::Geometry);
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
    /// Into a pad, the piece tried first: from the centre line's point nearest the pad's centre
    /// to that centre.
    pub centre: Option<(Point, Point)>,
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
            let (from, to, overlap, full, centre) = match (&o.kind, &o.shape) {
                (Kind::Track | Kind::Via, Shape::Capsule { a: c, b: d, r: r2 }) => {
                    if o.kind == Kind::Track && (squarely(*c, *r2, item) || squarely(*d, *r2, item)) {
                        continue;
                    }
                    let gap = board::geom::segment_segment(*a, *b, *c, *d);
                    if gap == 0.0 {
                        continue;
                    }
                    let (p, q) = closest_points(*a, *b, *c, *d);
                    (p, q, (*r + *r2) as f64 - gap, 2.0 * (*r).min(*r2) as f64, None)
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
                    (p, into, *r as f64 - gap, 2.0 * *r as f64, shape.contains(centre).then(|| (closest_on_segment(*a, *b, centre).1, centre)))
                }
                _ => continue,
            };
            // Inside a via's, pad's or kept pour's copper the two are joined by it at full width
            // already: no neck, and a bridge there only piles copper on copper (easy2350's
            // USB_DM_C, a staircase inside its via's land bridged step to step; its +3V3 into U1,
            // inside a protected strip, bridged at 0.09 mm). A pour the refill redraws does not count.
            let covered = || {
                board.near(layer, board::BBox::of([from, to])).into_iter().filter_map(|c| board.item(c)).any(|c| {
                    matches!(c.kind, Kind::Via | Kind::Pad { .. } | Kind::Pour { kept: true, .. })
                        && c.net == item.net
                        && c.on(layer)
                        && c.shape.contains(from)
                        && c.shape.contains(to)
                })
            };
            if overlap > 0.0 && overlap < full && !bridged(board, layer, id, item, other, o) && !covered() {
                out.push(Graze { track: id, other, from, to, overlap, centre });
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

/// Bridges each graze with a piece from the track's centre line into the other copper, so they
/// meet at the narrower's full width: into a pad, to its centre where that breaks no rule, a
/// joint as a person draws it (sunflower's VSYS across C32-1's corner), else just far enough; at
/// the track's width or, where that breaks a rule, the narrowest the net's rules allow; the pieces
/// join `laid`. How many it mended.
pub fn mend_grazes(board: &mut Board, laid: &mut Vec<ItemId>) -> usize {
    let _b = board::work::enter(board::work::Bucket::Geometry);
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
        'widths: for r in [r, least] {
            for (from, to) in graze.centre.into_iter().chain([(graze.from, graze.to)]) {
                let piece = board.insert(Item { shape: Shape::Capsule { a: from, b: to, r }, span: None, locked: false, ..track.clone() });
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

/// Two vias of a net whose lands overlap by less than the net's narrowest joint, or half the
/// smaller land:
/// KiCad measures the sliver between them as a neck on every layer both have lands on (the
/// earring's VLED_C vias 0.403 mm apart, 0.4 mm lands, 0.029 mm across). Each pair, the router's
/// via first, once.
/// How near two lands' rims must come to touch, in nm.
const TOUCH: f64 = 1000.0;

fn via_grazes(board: &Board, laid: &[ItemId]) -> Vec<(ItemId, ItemId)> {
    let mine: HashSet<ItemId> = laid.iter().copied().collect();
    let mut out = Vec::new();
    for &id in laid {
        let Some(v) = board.item(id) else { continue };
        let (Kind::Via, Shape::Capsule { a, r, .. }) = (&v.kind, &v.shape) else { continue };
        if v.net == 0 {
            continue;
        }
        let layer = v.layers.trailing_zeros() as usize;
        for other in board.near(layer, v.shape.bbox().grow(*r)) {
            if other == id || (mine.contains(&other) && other < id) {
                continue;
            }
            let Some(o) = board.item(other) else { continue };
            let (Kind::Via, Shape::Capsule { a: c, r: r2, .. }) = (&o.kind, &o.shape) else { continue };
            if o.net != v.net || o.layers & v.layers == 0 {
                continue;
            }
            // The chord where the two lands cross, as wide as the copper joining them is.
            let d = a.distance(*c);
            let (r1, r2) = (*r as f64, *r2 as f64);
            // Lands that only touch count too: KiCad joins them through a point (esp32-iot-uno's
            // IO15/CMD, two 1.1 mm vias 1.1 mm apart).
            if d > r1 + r2 + TOUCH || d <= (r1 - r2).abs() {
                continue;
            }
            let x = (d * d + r1 * r1 - r2 * r2) / (2.0 * d);
            let chord = 2.0 * (r1 * r1 - x * x).max(0.0).sqrt();
            // As wide as the net's narrowest joint, and at least half the smaller land.
            let least = (board.min_joint_of(v.net) as f64).max(r1.min(r2));
            if chord < least {
                out.push((id, other));
            }
        }
    }
    out
}

/// Mends each pair of [`via_grazes`]: one of the router's two goes where its net stays as joined
/// without it, as one of the two needs not be there; else a track of the net's narrowest joint
/// width joins their centres on every layer both have lands on, where that breaks no rule. The
/// spans of what went, and how many pairs were mended.
pub fn mend_via_grazes(board: &mut Board, laid: &mut Vec<ItemId>) -> (usize, Vec<Span>) {
    let _b = board::work::enter(board::work::Bucket::Geometry);
    let (mut mended, mut spans) = (0, Vec::new());
    'pairs: for (id, other) in via_grazes(board, laid) {
        let (Some(v), Some(o)) = (board.item(id).cloned(), board.item(other).cloned()) else { continue };
        let before = board::net_connectivity(board, v.net, false).airlines.len();
        for gone in [id, other].into_iter().filter(|g| laid.contains(g)) {
            let removed = board.remove(gone).expect("listed above");
            if board::net_connectivity(board, v.net, false).airlines.len() <= before {
                spans.extend(removed.span);
                laid.retain(|t| *t != gone);
                mended += 1;
                continue 'pairs;
            }
            board.restore(gone, removed);
        }
        let (Shape::Capsule { a, .. }, Shape::Capsule { a: c, .. }) = (&v.shape, &o.shape) else { continue };
        let r = (board.min_joint_of(v.net) + 1) / 2;
        let mut pieces = Vec::new();
        for layer in (0..board.layers.len()).filter(|&l| v.on(l) && o.on(l)) {
            let piece = board.insert(Item { kind: Kind::Track, net: v.net, layers: 1 << layer, shape: Shape::Capsule { a: *a, b: *c, r }, hole: None, locked: false, span: None });
            pieces.push(piece);
            if !check_item(board, piece).is_empty() {
                break;
            }
        }
        if pieces.iter().all(|&p| check_item(board, p).is_empty()) {
            laid.extend(pieces);
            mended += 1;
        } else {
            for p in pieces {
                board.discard(p);
            }
        }
    }
    (mended, spans)
}

/// The router's vias whose land overlaps a pad of their net only past the pad's edge, the via's
/// centre outside the pad, through a chord narrower than the net's narrowest joint or half the
/// land: KiCad measures that chord as a neck (LoRaWAN_GPS's C15 pad 1, clipped at its corner by
/// a 0.6 mm via, 0.056 mm across). Each via and pad once.
fn via_pad_grazes(board: &Board, laid: &[ItemId]) -> Vec<(ItemId, ItemId)> {
    let mut out = Vec::new();
    for &id in laid {
        let Some(v) = board.item(id) else { continue };
        let (Kind::Via, Shape::Capsule { a: c, r, .. }) = (&v.kind, &v.shape) else { continue };
        if v.net == 0 {
            continue;
        }
        let mut seen = HashSet::new();
        for l in (0..board.layers.len()).filter(|&l| v.on(l)) {
            for other in board.near(l, v.shape.bbox().grow(*r)) {
                let Some(o) = board.item(other) else { continue };
                if !matches!(o.kind, Kind::Pad { .. }) || o.net != v.net || o.layers & v.layers == 0 || o.shape.contains(*c) || !seen.insert(other) {
                    continue;
                }
                if bridged(board, l, id, v, other, o) {
                    continue;
                }
                let Some((chord, _)) = land_chord(&o.shape, *c, *r as f64) else { continue };
                let least = (board.min_joint_of(v.net) as f64).max(*r as f64);
                if chord < least {
                    out.push((id, other));
                }
            }
        }
    }
    out
}

/// The narrowest chord a disc's rim (centre `c`, radius `r`) cuts across `shape`'s edge, in nm,
/// and its middle: between the rim's two crossings of each stretch of rim inside it, or nothing
/// at the point where a rim that never enters it touches it (sigfoxuino's +3.3V, a 0.889 mm via
/// on U2 pad 8's corner, 0.0001 mm across). `None` when the rim stays clear of it.
fn land_chord(shape: &Shape, c: Point, r: f64) -> Option<(f64, Point)> {
    const N: usize = 360;
    let rim: Vec<Point> = (0..N)
        .map(|k| {
            let t = k as f64 * std::f64::consts::TAU / N as f64;
            Point::new(c.x + (r * t.cos()).round() as Nm, c.y + (r * t.sin()).round() as Nm)
        })
        .collect();
    let inside: Vec<bool> = rim.iter().map(|&p| shape.contains(p)).collect();
    let start = (0..N).find(|&k| !inside[k])?;
    if !inside.contains(&true) {
        let nearest = rim.iter().copied().min_by(|x, y| point_gap(shape, *x).total_cmp(&point_gap(shape, *y))).expect("rim");
        return (point_gap(shape, nearest) <= TOUCH).then_some((0.0, nearest));
    }
    let mut best: Option<(f64, Point)> = None;
    let mut run: Option<usize> = None;
    for step in 1..=N {
        let k = (start + step) % N;
        match (inside[k], run) {
            (true, None) => run = Some(k),
            (false, Some(first)) => {
                let last = (k + N - 1) % N;
                let chord = rim[first].distance(rim[last]);
                let middle = Point::new((rim[first].x + rim[last].x) / 2, (rim[first].y + rim[last].y) / 2);
                if best.is_none_or(|(b, _)| chord < b) {
                    best = Some((chord, middle));
                }
                run = None;
            }
            _ => {}
        }
    }
    best
}

/// Mends each of [`via_pad_grazes`]: the via goes where its net stays as joined without it; else
/// a track from its centre on into the pad, as wide as the land or, where that breaks a rule, the
/// net's narrowest joint, on every layer both are on, where that breaks no rule. The spans of
/// what went, and how many were mended.
pub fn mend_via_pad_grazes(board: &mut Board, laid: &mut Vec<ItemId>) -> (usize, Vec<Span>) {
    let _b = board::work::enter(board::work::Bucket::Geometry);
    let (mut mended, mut spans) = (0, Vec::new());
    for (id, pad) in via_pad_grazes(board, laid) {
        let (Some(v), Some(p)) = (board.item(id).cloned(), board.item(pad).cloned()) else { continue };
        // An earlier mend took the via, or bridged it already.
        if !via_pad_grazes(board, &[id]).iter().any(|&(_, q)| q == pad) {
            continue;
        }
        let before = board::net_connectivity(board, v.net, false).airlines.len();
        let removed = board.remove(id).expect("listed above");
        if board::net_connectivity(board, v.net, false).airlines.len() <= before {
            spans.extend(removed.span);
            laid.retain(|t| *t != id);
            mended += 1;
            continue;
        }
        board.restore(id, removed);
        let Shape::Capsule { a: c, r, .. } = v.shape else { continue };
        let Some((_, middle)) = land_chord(&p.shape, c, r as f64) else { continue };
        let centre = p.shape.bbox();
        let centre = Point::new((centre.min.x + centre.max.x) / 2, (centre.min.y + centre.max.y) / 2);
        let least = (board.min_joint_of(v.net) + 1) / 2;
        'widths: for w in [r, (r + least) / 2, least] {
            // On through the sliver's middle, so even a piece narrower than the land covers it,
            // until as deep in the pad as it is wide; else at the pad's centre.
            let (dx, dy) = ((middle.x - c.x) as f64, (middle.y - c.y) as f64);
            let along = |t: f64| Point::new(c.x + (dx * t).round() as Nm, c.y + (dy * t).round() as Nm);
            let into = (1..=80).map(|k| along(k as f64 / 20.0)).find(|&q| p.shape.depth(q) >= w as f64).unwrap_or(centre);
            let mut pieces = Vec::new();
            for layer in (0..board.layers.len()).filter(|&l| v.on(l) && p.on(l)) {
                let piece = board.insert(Item { kind: Kind::Track, net: v.net, layers: 1 << layer, shape: Shape::Capsule { a: c, b: into, r: w }, hole: None, locked: false, span: None });
                pieces.push(piece);
                if !check_item(board, piece).is_empty() {
                    for p in pieces {
                        board.discard(p);
                    }
                    continue 'widths;
                }
            }
            laid.extend(pieces);
            mended += 1;
            break;
        }
    }
    (mended, spans)
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
    let _b = board::work::enter(board::work::Bucket::Geometry);
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
    let _b = board::work::enter(board::work::Bucket::Geometry);
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
    let _b = board::work::enter(board::work::Bucket::Geometry);
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
    let _b = board::work::enter(board::work::Bucket::Geometry);
    let soft_set: HashSet<ItemId> = soft.iter().copied().filter(|id| board.item(*id).is_some()).collect();
    let mut stats = CleanStats::default();
    let mut replaced: HashSet<ItemId> = HashSet::new();
    let mut added: Vec<ItemId> = Vec::new();
    for chain in chains(board, &soft_set) {
        if deadline.expired() {
            break;
        }
        let ct0 = board::work::timed_run_ns();
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
            stats.kept_ns = stats.kept_ns.saturating_add(board::work::timed_run_ns().saturating_sub(ct0));
        } else {
            stats.unchanged_ns = stats.unchanged_ns.saturating_add(board::work::timed_run_ns().saturating_sub(ct0));
        }
    }
    soft.retain(|id| !replaced.contains(id) && board.item(*id).is_some());
    soft.extend(added);
    stats
}

/// The least a cut must shorten a chain by to be kept, in nm: below it a bend wrapping round
/// copper would be cut ever finer for nothing anyone could see.
const TAUT_GAIN: f64 = 50_000.0;

/// How many rounds of cuts a chain gets: each round cuts every bend once, the bends a cut makes
/// included, so a chain that wraps round an obstacle nears it in a few.
const TAUT_ROUNDS: usize = 4;

/// How much wider than they are a cut's pieces are tried, in nm: a cut is pulled as deep as it
/// fits, which is against what it wraps, where the check's slack would let KiCad find it short
/// (23 boards of the gate tier, each 0.5 um inside a clearance).
const TAUT_MARGIN: Nm = 2_000;

/// Pulls the router's chains taut, as a rubber band: where [`pull_tight`] can only join two of a
/// chain's points, this cuts each bend from a point on one of its tracks to a point on the other,
/// as deep as no rule is broken and nothing the chain goes round lies in the corner cut off. The
/// chain bends anew where the cut meets copper, round after round, so it comes to lie along what
/// it wraps (Dense Square's SW2_9 on In4.Cu ran 2.4 mm past a via before turning back down to
/// pass under it). Chains not reached by `deadline` stay as they are.
pub fn pull_taut_watch(board: &mut Board, soft: &mut Vec<ItemId>, deadline: &Deadline, watch: &mut TightWatch) -> CleanStats {
    let _b = board::work::enter(board::work::Bucket::Geometry);
    let soft_set: HashSet<ItemId> = soft.iter().copied().filter(|id| board.item(*id).is_some()).collect();
    let mut stats = CleanStats::default();
    let mut replaced: HashSet<ItemId> = HashSet::new();
    // In the order laid, so the board written is the same run to run.
    let mut added: Vec<ItemId> = Vec::new();
    for chain in chains(board, &soft_set) {
        if deadline.expired() {
            break;
        }
        let ct0 = board::work::timed_run_ns();
        stats.chains += 1;
        stats.segments_before += chain.tracks.len();
        stats.length_before += length(&chain.points);
        let (mut points, mut tracks) = (chain.points.clone(), chain.tracks.clone());
        let (mut old, mut new): (Vec<Item>, Vec<ItemId>) = (Vec::new(), Vec::new());
        for _ in 0..TAUT_ROUNDS {
            let before = length(&points);
            let mut k = 1;
            while k + 1 < points.len() && !deadline.expired() {
                match cut_bend(board, &chain, &points, &tracks, k) {
                    Some((cut, pieces, taken)) => {
                        for t in &tracks[k - 1..=k] {
                            match added.iter().position(|a| a == t) {
                                Some(i) => {
                                    added.remove(i);
                                }
                                None => {
                                    replaced.insert(*t);
                                }
                            }
                        }
                        new.retain(|n| !tracks[k - 1..=k].contains(n));
                        old.extend(taken);
                        added.extend(&pieces);
                        new.extend(&pieces);
                        let step = cut.len();
                        points.splice(k..=k, cut);
                        tracks.splice(k - 1..=k, pieces);
                        k += step;
                    }
                    None => k += 1,
                }
            }
            if before - length(&points) < TAUT_GAIN * 1e-6 {
                break;
            }
        }
        stats.segments_after += tracks.len();
        stats.length_after += length(&points);
        if !new.is_empty() {
            watch(board, chain.net, &old, &new);
            stats.kept_ns = stats.kept_ns.saturating_add(board::work::timed_run_ns().saturating_sub(ct0));
        } else {
            stats.unchanged_ns = stats.unchanged_ns.saturating_add(board::work::timed_run_ns().saturating_sub(ct0));
        }
    }
    soft.retain(|id| !replaced.contains(id) && board.item(*id).is_some());
    soft.extend(added);
    stats
}

/// The deepest cut of the bend at `points[k]`: from a point on the track before it to a point on
/// the track after, searched symmetrically and then each side on its own. The bend's new points,
/// the pieces now between `points[k - 1]` and `points[k + 1]`, and the tracks they replaced; the
/// board is left as it was if no cut shortens the chain by [`TAUT_GAIN`].
fn cut_bend(board: &mut Board, chain: &Chain, points: &[Point], tracks: &[ItemId], k: usize) -> Option<(Vec<Point>, Vec<ItemId>, Vec<Item>)> {
    let (u, p, w) = (points[k - 1], points[k], points[k + 1]);
    if board::point_segment(p, u, w) < 1_000.0 {
        return None;
    }
    let at = |from: Point, to: Point, t: f64| Point::new(from.x + ((to.x - from.x) as f64 * t).round() as Nm, from.y + ((to.y - from.y) as f64 * t).round() as Nm);
    let cut = |ta: f64, tb: f64| (at(p, u, ta), at(p, w, tb));
    let fits = |board: &mut Board, ta: f64, tb: f64| {
        let (a, b) = cut(ta, tb);
        try_cut(board, chain, tracks, k, u, a, p, b, w, false).is_some()
    };
    // The largest of `lo..=hi` that fits, `lo` fitting, as `fits_at` grows.
    let deepest = |board: &mut Board, fits_at: &mut dyn FnMut(&mut Board, f64) -> bool, lo: f64, hi: f64| {
        if fits_at(board, hi) {
            return hi;
        }
        let (mut lo, mut hi) = (lo, hi);
        for _ in 0..7 {
            let mid = (lo + hi) / 2.0;
            if fits_at(board, mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    };
    let t = deepest(board, &mut |b, t| fits(b, t, t), 0.0, 1.0);
    let ta = deepest(board, &mut |b, x| fits(b, x, t), t, 1.0);
    let tb = deepest(board, &mut |b, x| fits(b, ta, x), t, 1.0);
    let (a, b) = cut(ta, tb);
    let gain = u.distance(p) + p.distance(w) - (u.distance(a) + a.distance(b) + b.distance(w));
    if gain < TAUT_GAIN {
        return None;
    }
    let (pieces, taken) = try_cut(board, chain, tracks, k, u, a, p, b, w, true)?;
    let mut bends: Vec<Point> = [a, b].into_iter().filter(|&q| q != u && q != w).collect();
    bends.dedup();
    Some((bends, pieces, taken))
}

/// Cuts the bend `p` between `u` and `w` from `a` to `b`, where the triangle cut off holds
/// nothing of another net, no hole and no edge, whatever joined the two tracks still joins the
/// pieces or the tracks either side, and no piece breaks a rule or grazes its net's copper. The
/// pieces and the tracks taken out if `keep`; else, or if it does not fit, the board is left as
/// it was. Tried without keeping, the pieces are [`TAUT_MARGIN`] wider.
#[allow(clippy::too_many_arguments)]
fn try_cut(board: &mut Board, chain: &Chain, tracks: &[ItemId], k: usize, u: Point, a: Point, p: Point, b: Point, w: Point, keep: bool) -> Option<(Vec<ItemId>, Vec<Item>)> {
    let layer = chain.layer.trailing_zeros() as usize;
    // A piece shorter than the track is wide leaves the pieces either side overlapping without
    // meeting end to end, a graze (a 1 mm power net's bend cut 0.37 mm long, bridged at 0.15 mm).
    if [(u, a), (a, b), (b, w)].iter().any(|&(x, y)| x != y && x.distance(y) < (2 * chain.half_width) as f64) {
        return None;
    }
    if !sweep_clear(board, chain.net, layer, a, p, b) {
        return None;
    }
    let span = &tracks[k - 1..=k];
    let mut partners: Vec<ItemId> = Vec::new();
    for &t in span {
        let track = board.item(t)?;
        for other in board.near(layer, track.shape.bbox()) {
            if !span.contains(&other) && board.item(other).is_some_and(|o| o.net == chain.net) && joined(board, t, other) {
                partners.push(other);
            }
        }
    }
    partners.sort_unstable();
    partners.dedup();
    let sides: Vec<ItemId> = [k.checked_sub(2).map(|i| tracks[i]), tracks.get(k + 1).copied()].into_iter().flatten().collect();
    let removed: Vec<(ItemId, Item)> = span.iter().filter_map(|t| Some((*t, board.remove(*t)?))).collect();
    let mut pieces = Vec::new();
    let mut ok = removed.len() == 2;
    for (x, y) in [(u, a), (a, b), (b, w)] {
        if x == y || !ok {
            continue;
        }
        let r = if keep { chain.half_width } else { chain.half_width + TAUT_MARGIN };
        let id = board.insert(Item { kind: Kind::Track, net: chain.net, layers: chain.layer, shape: Shape::Capsule { a: x, b: y, r }, hole: None, locked: false, span: None });
        pieces.push(id);
        ok = check_item(board, id).is_empty();
    }
    ok = ok && partners.iter().all(|&o| pieces.iter().chain(&sides).any(|&s| s == o || joined(board, o, s)));
    // Nor may a piece be pulled against its own net's copper so they only overlap by a sliver,
    // which KiCad measures as a neck (ChirpHardware's +5V, 7 um across C5-1's edge).
    ok = ok && grazes(board, &pieces).is_empty();
    if ok && keep {
        return Some((pieces, removed.into_iter().map(|(_, it)| it).collect()));
    }
    for id in pieces {
        board.discard(id);
    }
    for (id, item) in removed {
        board.restore(id, item);
    }
    ok.then(|| (Vec::new(), Vec::new()))
}

/// The ways from `s` to `e` along 45-degree multiples, as the points between: a straight piece
/// and a diagonal one meeting at a knee (straight first, then diagonal first), then Zs, the
/// diagonal partway along the straight (half, a quarter, three quarters) or the straight partway
/// along the diagonal (half), each lying closer to the line than a knee. None when `s` to `e`
/// already runs at a multiple of 45 degrees.
fn octilinear_ways(s: Point, e: Point) -> Option<Vec<Vec<Point>>> {
    let (dx, dy) = (e.x - s.x, e.y - s.y);
    let (ax, ay) = (dx.abs(), dy.abs());
    if dx == 0 || dy == 0 || ax == ay {
        return None;
    }
    let (sx, sy) = (dx.signum(), dy.signum());
    // The straight run and the diagonal one, as (x, y) steps.
    let (straight, diagonal) = if ax > ay { ((sx * (ax - ay), 0), (sx * ay, sy * ay)) } else { ((0, sy * (ay - ax)), (sx * ax, sy * ax)) };
    let step = |p: Point, v: (Nm, Nm), f: f64| Point::new(p.x + (v.0 as f64 * f).round() as Nm, p.y + (v.1 as f64 * f).round() as Nm);
    let mut ways = vec![vec![step(s, straight, 1.0)], vec![step(s, diagonal, 1.0)]];
    for f in [0.5, 0.25, 0.75] {
        let q = step(s, straight, f);
        ways.push(vec![q, step(q, diagonal, 1.0)]);
        if f == 0.5 {
            // A diagonal's steps are as long on both axes: halve one and keep them so.
            let h = (diagonal.0.abs() / 2) as Nm;
            let q = Point::new(s.x + diagonal.0.signum() * h, s.y + diagonal.1.signum() * h);
            ways.push(vec![q, step(q, straight, 1.0)]);
        }
    }
    Some(ways)
}

/// Further Z ways from `s` to `e` whose middle piece is longer than [`DOGLEG`], split by length
/// from each end as well as finer fractions: on a 16 mm run a quarter is 4 mm out, missing a
/// clear gap just past the pad (PICA's VCC), and along the diagonal [`octilinear_ways`] tries only
/// the midpoint even when the straight middle exceeds [`DOGLEG`] so every split is dogleg-free.
fn extra_z_ways(s: Point, e: Point, half_width: Nm) -> Vec<Vec<Point>> {
    let (dx, dy) = (e.x - s.x, e.y - s.y);
    let (ax, ay) = (dx.abs(), dy.abs());
    if dx == 0 || dy == 0 || ax == ay {
        return Vec::new();
    }
    let (sx, sy) = (dx.signum(), dy.signum());
    let (st_len, dg_step, st_dir) = if ax > ay { (ax - ay, ay, (sx, 0)) } else { (ay - ax, ax, (0, sy)) };
    let straight = (st_dir.0 * st_len, st_dir.1 * st_len);
    let diagonal = (sx * dg_step, sy * dg_step);
    let min_end = 2 * half_width + 2_000;
    let over_dogleg = DOGLEG as Nm + 10_000;
    let mut ways = Vec::new();
    if (dg_step as f64) * std::f64::consts::SQRT_2 > DOGLEG && st_len >= 2 * min_end {
        let mut ks: Vec<Nm> = [min_end, st_len - min_end, over_dogleg, st_len - over_dogleg, 2_000_000, st_len - 2_000_000, st_len / 8, st_len - st_len / 8]
            .into_iter()
            .filter(|&k| k >= min_end && k <= st_len - min_end)
            .collect();
        ks.sort_by_key(|&k| k.min(st_len - k));
        ks.dedup();
        for k in ks {
            let q = Point::new(s.x + st_dir.0 * k, s.y + st_dir.1 * k);
            ways.push(vec![q, Point::new(q.x + diagonal.0, q.y + diagonal.1)]);
        }
    }
    let min_d = ((min_end as f64) / std::f64::consts::SQRT_2).ceil() as Nm;
    let dog_d = (DOGLEG / std::f64::consts::SQRT_2).ceil() as Nm + 10_000;
    if (st_len as f64) > DOGLEG && dg_step >= 2 * min_d {
        let mut hs: Vec<Nm> = [dg_step / 4, dg_step - dg_step / 4, dg_step / 8, dg_step - dg_step / 8, min_d, dg_step - min_d, dog_d, dg_step - dog_d, 1_500_000, dg_step - 1_500_000]
            .into_iter()
            .filter(|&h| h >= min_d && h <= dg_step - min_d)
            .collect();
        hs.sort_by_key(|&h| (h - dg_step / 2).abs());
        hs.dedup();
        for h in hs {
            let q = Point::new(s.x + sx * h, s.y + sy * h);
            ways.push(vec![q, Point::new(q.x + straight.0, q.y + straight.1)]);
        }
    }
    ways
}

/// Whether `a` to `b` heads as `c` to `d` does.
fn same_heading(a: Point, b: Point, c: Point, d: Point) -> bool {
    let (u, v) = (((b.x - a.x) as f64, (b.y - a.y) as f64), ((d.x - c.x) as f64, (d.y - c.y) as f64));
    let (lu, lv) = (u.0.hypot(u.1), v.0.hypot(v.1));
    lu > 0.0 && lv > 0.0 && (u.0 * v.1 - u.1 * v.0).abs() < 1e-6 * lu * lv && u.0 * v.0 + u.1 * v.1 > 0.0
}

/// How many rounds [`octilinear_watch`] takes: a track bent can make room for its neighbour,
/// which lay too close beside it to bend first (two tracks running side by side at an angle).
const OCTILINEAR_ROUNDS: usize = 3;

/// Puts the router's tracks on 45-degree multiples, as a designer draws them: each track at
/// another angle becomes straight and diagonal pieces ([`octilinear_ways`], the way carrying on
/// as a neighbouring track runs tried first), where whatever joined the track still joins the
/// pieces and no piece breaks a rule, grazes its net's copper or crosses the board's edge (a way
/// passing a via on its other side breaks nothing, so unlike [`try_cut`] it may). A track no way fits stays as it is, so nothing is lost to the
/// look. About three quarters of the length pulled taut ran at other angles (the gate's boards,
/// tools/tidy.py); a designer's runs at none. Chains not reached by `deadline` stay as they are.
/// How many tracks it bent.
pub fn octilinear_watch(board: &mut Board, soft: &mut Vec<ItemId>, deadline: &Deadline, watch: &mut TightWatch) -> usize {
    let _b = board::work::enter(board::work::Bucket::Geometry);
    let mut bent = 0;
    for _ in 0..OCTILINEAR_ROUNDS {
        let round = octilinear_round(board, soft, deadline, watch);
        bent += round;
        if round == 0 || deadline.expired() {
            break;
        }
    }
    bent
}

/// The most of a chain's tracks [`octilinear_round`] lays one way in place of: a run pulled taut
/// lies in pieces each a little off the last, and bent one by one they step aside by slivers,
/// a ladder of doglegs (stm32_nucleo's GND, four steps of 40 um down 12 mm).
const OCTILINEAR_SPAN: usize = 8;

fn octilinear_round(board: &mut Board, soft: &mut Vec<ItemId>, deadline: &Deadline, watch: &mut TightWatch) -> usize {
    let soft_set: HashSet<ItemId> = soft.iter().copied().filter(|id| board.item(*id).is_some()).collect();
    let mut replaced: HashSet<ItemId> = HashSet::new();
    let mut added: Vec<ItemId> = Vec::new();
    let mut bent = 0;
    for chain in chains(board, &soft_set) {
        if deadline.expired() {
            break;
        }
        let (mut points, mut tracks) = (chain.points.clone(), chain.tracks.clone());
        let (mut old, mut new): (Vec<Item>, Vec<ItemId>) = (Vec::new(), Vec::new());
        let mut i = 0;
        while i < tracks.len() {
            let mut laid = None;
            // The longest run from track `i` holding a track off 45 degrees first, as pulling tight
            // takes the longest straight.
            for j in (i + 1..=tracks.len().min(i + OCTILINEAR_SPAN)).rev() {
                if !(i..j).any(|k| octilinear_ways(points[k], points[k + 1]).is_some()) {
                    continue;
                }
                let (s, e) = (points[i], points[j]);
                let s_pad = if i == 0 { pad_centre(board, &chain, tracks[i], s) } else { None };
                let e_pad = if j == tracks.len() { pad_centre(board, &chain, tracks[j - 1], e) } else { None };
                // First an end in a pad slid within it onto the line from the other end: no bend,
                // and no sliver of a step beside the pad leaving a slot between (stm32_nucleo's
                // /PA2). When both ends lie in pads, both may slide to meet on one line too. Then
                // the run straight, if it lines up; else the knees and Zs.
                let mut paths: Vec<(Vec<Point>, [Option<ItemId>; 2])> = Vec::new();
                if i == 0 {
                    paths.extend(slides(board, &chain, tracks[i], e, s).into_iter().map(|(q, pad)| (vec![q, e], [Some(pad), None])));
                }
                if j == tracks.len() {
                    paths.extend(slides(board, &chain, tracks[j - 1], s, e).into_iter().map(|(q, pad)| (vec![s, q], [None, Some(pad)])));
                }
                if i == 0 && j == tracks.len() {
                    paths.extend(two_slides(board, &chain, tracks[i], s, tracks[j - 1], e).into_iter().map(|([a, b], pads)| (vec![a, b], pads)));
                    if let Some((ce, pe)) = e_pad {
                        paths.extend(slides(board, &chain, tracks[i], ce, s).into_iter().filter(|&(_, ps)| ps != pe).map(|(q, ps)| (vec![q, ce], [Some(ps), Some(pe)])));
                    }
                    if let Some((cs, ps)) = s_pad {
                        paths.extend(slides(board, &chain, tracks[j - 1], cs, e).into_iter().filter(|&(_, pe)| ps != pe).map(|(q, pe)| (vec![cs, q], [Some(ps), Some(pe)])));
                    }
                }
                let moved = |path: &[Point]| path[0].distance(s) + path[path.len() - 1].distance(e);
                paths.sort_by(|a, b| moved(&a.0).total_cmp(&moved(&b.0)));
                // Then from the ends where they are, and last to the centre of a pad an end lies
                // in (one end, or both when both lay near their rims): the router ends tracks
                // anywhere in a pad, and a 45-degree piece through an end far off a round pad's
                // centre meets its rim at a slant, a slot; into the centre it meets the rim
                // square on, as a designer draws it.
                let mut ends = vec![(s, e, [None, None])];
                if let Some((c, pad)) = s_pad {
                    ends.push((c, e, [Some(pad), None]));
                }
                if let Some((c, pad)) = e_pad {
                    ends.push((s, c, [None, Some(pad)]));
                }
                if let (Some((cs, ps)), Some((ce, pe))) = (s_pad, e_pad) {
                    if ps != pe && cs != ce {
                        ends.push((cs, ce, [Some(ps), Some(pe)]));
                    }
                }
                for &(s, e, pads) in &ends {
                    match octilinear_ways(s, e) {
                        None => paths.push((vec![s, e], pads)),
                        Some(mut ways) => {
                            // A knee whose first piece carries on the track before, or whose last
                            // carries on into the track after, first: one bend fewer.
                            let carries = |q: Point| (i > 0 && same_heading(points[i - 1], s, s, q)) || (j + 1 < points.len() && same_heading(q, e, e, points[j + 1]));
                            if carries(ways[1][0]) && !carries(ways[0][0]) {
                                ways.swap(0, 1);
                            }
                            paths.extend(ways.into_iter().map(|way| (std::iter::once(s).chain(way).chain(std::iter::once(e)).collect(), pads)));
                        }
                    }
                }
                // No way stepping aside by more doglegs than the run did, with the tracks either
                // side (two points each way, as a dogleg spans three pieces): a Z or knee a sliver
                // long between two runs the same way, or turning a neighbour piece into one, is a
                // wiggle, not tidier than the slant it squares (LimeSDR's GND, 8 doglegs to 89).
                let around = |path: &[Point]| -> Vec<Point> {
                    points[i.saturating_sub(2)..i]
                        .iter()
                        .copied()
                        .chain(path.iter().copied())
                        .chain(points[(j + 1).min(points.len())..(j + 3).min(points.len())].iter().copied())
                        .collect()
                };
                let was = doglegs(&around(&points[i..=j]), chain.half_width);
                // The net's own count of what is open, before and after: the joins asked of a way
                // are a track's, and the connectivity's are stricter (a track within a pad, its
                // end moved off another track's). When a way with margin fails at its true width
                // or opens a join, the rest of `paths` are still tried; only when none fits are
                // length-picked Z splits and three-piece square pad exits built.
                let open = |board: &Board| board::net_connectivity(board, chain.net, true).airlines.len();
                let mut before = None;
                for wave in 0..3 {
                    let mut batch = match wave {
                        0 => std::mem::take(&mut paths),
                        1 => ends
                            .iter()
                            .flat_map(|&(s, e, pads)| {
                                extra_z_ways(s, e, chain.half_width)
                                    .into_iter()
                                    .map(move |way| (std::iter::once(s).chain(way).chain(std::iter::once(e)).collect(), pads))
                            })
                            .collect(),
                        _ => pad_exit_ways(board, &chain, &points, &tracks, i, j),
                    };
                    batch.retain(|(path, _)| doglegs(&around(path), chain.half_width) <= was);
                    for (path, pads) in batch {
                        if try_way(board, &chain, &points, &tracks, i, j, &path, pads, false).is_none() {
                            continue;
                        }
                        let before = *before.get_or_insert_with(|| open(board));
                        let Some((pieces, taken)) = try_way(board, &chain, &points, &tracks, i, j, &path, pads, true) else {
                            continue;
                        };
                        if open(board) > before {
                            for id in pieces {
                                board.discard(id);
                            }
                            for (id, item) in taken {
                                board.restore(id, item);
                            }
                            continue;
                        }
                        laid = Some(((pieces, taken.into_iter().map(|(_, it)| it).collect::<Vec<Item>>()), path, j));
                        break;
                    }
                    if laid.is_some() {
                        break;
                    }
                }
                if laid.is_some() {
                    break;
                }
            }
            let Some(((pieces, taken), path, j)) = laid else {
                i += 1;
                continue;
            };
            for t in &tracks[i..j] {
                match added.iter().position(|a| a == t) {
                    Some(at) => {
                        added.remove(at);
                    }
                    None => {
                        replaced.insert(*t);
                    }
                }
            }
            new.retain(|n| !tracks[i..j].contains(n));
            old.extend(taken);
            added.extend(&pieces);
            new.extend(&pieces);
            let step = pieces.len();
            points.splice(i..=j, path);
            tracks.splice(i..j, pieces);
            bent += 1;
            i += step;
        }
        if !new.is_empty() {
            watch(board, chain.net, &old, &new);
        }
    }
    soft.retain(|id| !replaced.contains(id) && board.item(*id).is_some());
    soft.extend(added);
    bent
}

/// Under this a straight piece between two runs heading the same way is a dogleg (tools/tidy.py).
const DOGLEG: f64 = 1_000_000.0;

/// The doglegs along the line through `points`, as tools/tidy.py counts them: a piece shorter
/// than [`DOGLEG`], bent off at both ends, between pieces heading the same way, stepping the
/// track over by half its width or more.
fn doglegs(points: &[Point], half_width: Nm) -> usize {
    let heading = |p: Point, q: Point| ((q.y - p.y) as f64).atan2((q.x - p.x) as f64);
    let turn = |a: f64, b: f64| {
        let d = (a - b).abs() % std::f64::consts::TAU;
        d.min(std::f64::consts::TAU - d)
    };
    let straight = 5f64.to_radians();
    points
        .windows(4)
        .filter(|w| {
            let (before, mid, after) = (heading(w[0], w[1]), heading(w[1], w[2]), heading(w[2], w[3]));
            let (dx, dy) = ((w[2].x - w[1].x) as f64, (w[2].y - w[1].y) as f64);
            let step = (after.cos() * dy - after.sin() * dx).abs();
            w[1].distance(w[2]) <= DOGLEG && turn(before, mid) >= straight && turn(mid, after) >= straight && turn(before, after) < straight && step >= half_width as f64
        })
        .count()
}

/// Whether the pieces leave a slot, an acid trap (tools/dfm.py): a piece beside its net's other
/// copper on its layer, nearer than the clearance without touching it, or running into a pad of
/// its net so near the pad's rim that its edge and the rim part at a sliver. Squaring the
/// gate's boards without asking added two thirds to their slots (Atreus 65 -> 157, the same
/// binary with and without), most of them these two.
fn leaves_slot(board: &Board, chain: &Chain, pieces: &[ItemId], same_ends: &[Option<Point>; 2]) -> bool {
    let layer = chain.layer.trailing_zeros() as usize;
    let gap = board.clearance(chain.net, chain.net) as f64;
    pieces.iter().any(|&id| {
        let Some(piece) = board.item(id) else { return false };
        let Shape::Capsule { a, b, r: r1 } = piece.shape else { return false };
        let reach = piece.shape.bbox().grow(gap as Nm);
        board.near(layer, reach).into_iter().any(|o| {
            if pieces.contains(&o) {
                return false;
            }
            let Some(other) = board.item(o) else { return false };
            if other.net != chain.net || !other.on(layer) || matches!(other.kind, Kind::Pour { .. } | Kind::Keepout { .. } | Kind::Spoke { .. }) {
                return false;
            }
            let apart = piece.shape.gap(&other.shape);
            if apart > 0.0 {
                return apart < gap;
            }
            if let (Kind::Track, Shape::Capsule { a: c, b: d, r: r2 }) = (&other.kind, &other.shape) {
                return track_leaves_slot(board, layer, chain.net, gap, (a, b, r1), (*c, *d, *r2), same_ends);
            }
            // Into a pad or a via's round land: its centre line's nearest point to the centre at
            // least half as deep in as the centre, so the edge crosses the rim steeply, not along
            // it (Microdox's COL4 into a switch's round pad, 0.83 mm off the centre of 1.14;
            // FireMC's +12V bending right on a 0.7 mm via's rim). On a round or capsule shape, an
            // end inside it (deep enough or unchanged from before squaring, as Atreus's diodes have
            // copper at the centre) heading out square to the rim (`!slants_into`) already crosses
            // steeply without passing near the centre.
            if !matches!(other.kind, Kind::Pad { .. } | Kind::Via) {
                return false;
            }
            if slants_into(a, b, &other.shape) {
                return true;
            }
            if matches!(other.shape, Shape::Capsule { .. }) {
                let end_ok = |p: Point| other.shape.depth(p) >= chain.half_width as f64 || same_ends.iter().flatten().any(|&e| e == p);
                match (other.shape.contains(a), other.shape.contains(b)) {
                    (true, false) if end_ok(a) => return false,
                    (false, true) if end_ok(b) => return false,
                    (true, true) if end_ok(a) && end_ok(b) => return false,
                    _ => {}
                }
            }
            let bb = other.shape.bbox();
            let centre = Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2);
            let (_, m) = closest_on_segment(a, b, centre);
            other.shape.depth(m) < other.shape.depth(centre) / 2.0
        })
    })
}

/// Whether two touching tracks `a..b` (radius `r1`) and `c..d` (radius `r2`) of `net` on `layer`
/// part at a sharp wedge or kiss at their sides: a piece peeling off another track of its net at
/// 45 degrees leaves a 3165 um2 acid trap in the fork (PICA's /HE1_D, BUF-DiffToSE's GNDA), unless
/// a pad of the net covers the fork out to where the edges clear by `gap`.
fn track_leaves_slot(board: &Board, layer: usize, net: u32, gap: f64, (a, b, r1): (Point, Point, Nm), (c, d, r2): (Point, Point, Nm), same_ends: &[Option<Point>; 2]) -> bool {
    if a == b || c == d {
        return false;
    }
    let sep = board::geom::segment_segment(a, b, c, d);
    if sep <= 2_000.0 && (same_heading(a, b, c, d) || same_heading(a, b, d, c)) {
        return false;
    }
    let (ux, uy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
    let (vx, vy) = ((d.x - c.x) as f64, (d.y - c.y) as f64);
    let cross = ux * vy - uy * vx;
    let (p, q) = if board::geom::segments_cross(a, b, c, d) && cross.abs() > 1e-6 {
        let t = (((c.x - a.x) as f64 * vy - (c.y - a.y) as f64 * vx) / cross).clamp(0.0, 1.0);
        let at = Point::new(a.x + (t * ux).round() as Nm, a.y + (t * uy).round() as Nm);
        (at, at)
    } else {
        [(a, closest_on_segment(c, d, a).1), (b, closest_on_segment(c, d, b).1), (closest_on_segment(a, b, c).1, c), (closest_on_segment(a, b, d).1, d)]
            .into_iter()
            .min_by(|(p1, q1), (p2, q2)| p1.distance(*q1).total_cmp(&p2.distance(*q2)))
            .expect("four pairs")
    };
    let arm_min = r1.max(r2) as f64;
    let kept_end = same_ends.iter().flatten().any(|&e| e.distance(p) <= arm_min + 2_000.0);
    if p.distance(q) > r1.min(r2) as f64 && !kept_end {
        return true;
    }
    let arms = |from: Point, ends: [Point; 2]| -> Vec<((f64, f64), f64)> {
        ends.into_iter()
            .filter_map(|to| {
                let len = from.distance(to);
                (len > arm_min).then(|| (((to.x - from.x) as f64 / len, (to.y - from.y) as f64 / len), len))
            })
            .collect()
    };
    let sharp = 85f64.to_radians().cos();
    let u_arms = arms(p, [a, b]);
    let v_arms = arms(q, [c, d]);
    for &(u, lu) in &u_arms {
        for &(v, lv) in &v_arms {
            let cos = (u.0 * v.0 + u.1 * v.1).clamp(-1.0, 1.0);
            if cos <= sharp {
                continue;
            }
            let half_sin = ((1.0 - cos) / 2.0).sqrt().max(0.05);
            let crotch = (r1 as f64 + r2 as f64 + gap) / (2.0 * half_sin);
            let (du, dv) = (crotch.min(lu), crotch.min(lv));
            let pu = Point::new(p.x + (u.0 * du).round() as Nm, p.y + (u.1 * du).round() as Nm);
            let qv = Point::new(q.x + (v.0 * dv).round() as Nm, q.y + (v.1 * dv).round() as Nm);
            let covered = board.near(layer, board::BBox::of([pu, qv])).into_iter().any(|id| {
                board.item(id).is_some_and(|it| it.net == net && it.on(layer) && matches!(it.kind, Kind::Pad { .. }) && it.shape.depth(pu) >= r1 as f64 && it.shape.depth(qv) >= r2 as f64)
            });
            if !covered {
                return true;
            }
        }
    }
    false
}

/// `ULTRAROUTE_SQUARE_TRACE=X,Y` (mm): each way squaring tries within 1 mm of there, printed
/// with what refused it (a rule, a join lost, a graze, a slot) or that it fits.
fn square_trace() -> Option<Point> {
    static AT: std::sync::OnceLock<Option<Point>> = std::sync::OnceLock::new();
    *AT.get_or_init(|| {
        let v: Vec<f64> = std::env::var("ULTRAROUTE_SQUARE_TRACE").ok()?.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        (v.len() == 2).then(|| Point::new((v[0] * 1e6).round() as Nm, (v[1] * 1e6).round() as Nm))
    })
}

/// The least angle at which a track may cross a pad's rim, from along it: square on, but for
/// the rounding of the rim's corners.
const SQUARE_ON: f64 = 67.5;

/// Whether the centre line `a` to `b` crosses the rim of `pad` at a slant, so the track's edge
/// and the rim part at a sharp wedge on one side: a 45-degree piece into the long side of a
/// rectangular or oval pad, run to its centre (PICA's EXP1 and Q4, a wedge of 3165 um2 at
/// each, a third of the slots squaring added on the gate's boards). A designer runs the last
/// piece in square to the side.
fn slants_into(a: Point, b: Point, pad: &Shape) -> bool {
    let u = ((b.x - a.x) as f64, (b.y - a.y) as f64);
    let lu = u.0.hypot(u.1);
    if lu == 0.0 {
        return false;
    }
    // The sine of the angle between the piece and the rim's direction there.
    let steep = SQUARE_ON.to_radians().sin();
    match pad {
        Shape::Polygon(p) => (0..p.len()).any(|k| {
            let (c, d) = (p[k], p[(k + 1) % p.len()]);
            let v = ((d.x - c.x) as f64, (d.y - c.y) as f64);
            let lv = v.0.hypot(v.1);
            lv > 0.0 && board::geom::segments_cross(a, b, c, d) && (u.0 * v.1 - u.1 * v.0).abs() < steep * lu * lv
        }),
        Shape::Capsule { a: pa, b: pb, r } => {
            // Where the centre line leaves the pad, from the end inside it: the rim's normal
            // runs from the pad's spine to there.
            let (inside, outside) = match (pad.contains(a), pad.contains(b)) {
                (true, false) => (a, b),
                (false, true) => (b, a),
                _ => return false,
            };
            let (mut lo, mut hi) = (0.0, 1.0);
            for _ in 0..40 {
                let t = (lo + hi) / 2.0;
                let q = Point::new(inside.x + ((outside.x - inside.x) as f64 * t).round() as Nm, inside.y + ((outside.y - inside.y) as f64 * t).round() as Nm);
                if pad.contains(q) {
                    lo = t;
                } else {
                    hi = t;
                }
            }
            let q = (inside.x as f64 + (outside.x - inside.x) as f64 * lo, inside.y as f64 + (outside.y - inside.y) as f64 * lo);
            let (_, m) = closest_on_segment(*pa, *pb, Point::new(q.0.round() as Nm, q.1.round() as Nm));
            let n = (q.0 - m.x as f64, q.1 - m.y as f64);
            let ln = n.0.hypot(n.1);
            // Along the normal is square on: the cosine to the normal is the sine to the rim.
            *r > 0 && ln > 0.0 && (u.0 * n.0 + u.1 * n.1).abs() < steep * lu * ln
        }
    }
}

/// The pad of the chain's net its end `p` lies in, the track `track` ending there, if nothing
/// else lies there: an end only the pad holds can move within it, but one where other copper
/// meets the track too would leave that copper behind (wifi_pants' GND, a track within a pad
/// to where another joins it, slid away from the joint and three connections opened).
fn pad_alone_at(board: &Board, chain: &Chain, track: ItemId, p: Point) -> Option<ItemId> {
    let layer = chain.layer.trailing_zeros() as usize;
    let at: Vec<ItemId> = board.near(layer, board::BBox::of([p])).into_iter().filter(|&o| o != track && board.item(o).is_some_and(|it| it.is_copper() && it.on(layer) && it.shape.contains(p))).collect();
    match at[..] {
        [pad] if board.item(pad).is_some_and(|it| matches!(it.kind, Kind::Pad { .. }) && it.net == chain.net) => Some(pad),
        _ => None,
    }
}

/// The centre of the pad of the chain's net its end `p` lies in alone ([`pad_alone_at`]), when
/// that centre is not already `p` and lies in the pad, and the pad.
fn pad_centre(board: &Board, chain: &Chain, track: ItemId, p: Point) -> Option<(Point, ItemId)> {
    let pad = pad_alone_at(board, chain, track, p)?;
    let shape = &board.item(pad)?.shape;
    let bb = shape.bbox();
    let c = Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2);
    (c != p && shape.contains(c)).then_some((c, pad))
}

/// The pad centre `(c, moved_pad, pad)` from which [`pad_exit_ways`] may leave or enter at `p`:
/// either `p` itself when `p` is already the pad's centre (`moved_pad` is `None`, so [`try_way`]
/// keeps every joint at `p` even when another track of the net meets there too), or [`pad_centre`]
/// when `p` lies alone off-centre in the pad.
fn pad_for_exit(board: &Board, chain: &Chain, track: ItemId, p: Point) -> Option<(Point, Option<ItemId>, ItemId)> {
    let layer = chain.layer.trailing_zeros() as usize;
    for pad in board.near(layer, board::BBox::of([p])) {
        let Some(it) = board.item(pad) else { continue };
        if !matches!(it.kind, Kind::Pad { .. }) || it.net != chain.net || !it.on(layer) || !it.shape.contains(p) {
            continue;
        }
        let bb = it.shape.bbox();
        if Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2) == p {
            return Some((p, None, pad));
        }
    }
    pad_centre(board, chain, track, p).map(|(c, pad)| (c, Some(pad), pad))
}

/// Points just outside `pad` reached from its centre `c` along 45-degree multiples that cross its
/// rim square on ([`slants_into`]), far enough out that the next piece turning away clears `pad`
/// by the net's clearance (no slot at the rim, 1Bitsy's /PA4) and leaves neither knee piece to
/// `toward` shorter than the track is wide.
fn pad_exits(board: &Board, chain: &Chain, pad: ItemId, c: Point, toward: Point) -> Vec<Point> {
    let Some(item) = board.item(pad) else { return Vec::new() };
    let shape = &item.shape;
    let need = (chain.half_width + TAUT_MARGIN + board.clearance(chain.net, chain.net) + 2_000) as f64;
    let least = (2 * chain.half_width + 2_000) as f64;
    let max_k: Nm = 5_000_000;
    let mut out = Vec::new();
    for (sx, sy) in [(-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (-1, 1), (1, -1), (1, 1)] {
        if (toward.x - c.x) as f64 * (sx as f64) + (toward.y - c.y) as f64 * (sy as f64) < -1_000.0 {
            continue;
        }
        let far = Point::new(c.x + sx * 10_000_000, c.y + sy * 10_000_000);
        if slants_into(c, far, shape) {
            continue;
        }
        let clear = |k: Nm| {
            let q = Point::new(c.x + sx * k, c.y + sy * k);
            point_gap(shape, q) >= need && c.distance(q) >= least
        };
        if !clear(max_k) {
            continue;
        }
        let (mut lo, mut hi) = (1, max_k);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if clear(mid) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        let mut k = lo;
        let q = Point::new(c.x + sx * k, c.y + sy * k);
        let (dx, dy) = ((toward.x - q.x).abs(), (toward.y - q.y).abs());
        let diff = (dx - dy).abs();
        if diff > 0 && (diff as f64) < least {
            let bump = (least as Nm - diff) + 1_000;
            let q2 = Point::new(c.x + sx * (k + bump), c.y + sy * (k + bump));
            if ((toward.x - q2.x).abs() - (toward.y - q2.y).abs()).abs() as f64 >= least {
                k += bump;
            }
        } else if dx.min(dy) > 0 && (dx.min(dy) as f64 * std::f64::consts::SQRT_2) < least {
            let need_d = (least / std::f64::consts::SQRT_2).ceil() as Nm;
            let bump = (need_d - dx.min(dy)).max(0) + 1_000;
            let q2 = Point::new(c.x + sx * (k + bump), c.y + sy * (k + bump));
            if (toward.x - q2.x).abs().min((toward.y - q2.y).abs()) as f64 * std::f64::consts::SQRT_2 >= least {
                k += bump;
            }
        }
        out.push(Point::new(c.x + sx * k, c.y + sy * k));
    }
    out.sort_by(|a, b| a.distance(toward).total_cmp(&b.distance(toward)));
    out.truncate(3);
    out
}

/// Three- and four-piece ways that leave or enter an end's pad along [`pad_exits`], square to its
/// side and past its rim before turning onto a knee: a track from a long SMD pad to a point off
/// its side has no two-piece way when the diagonal slants across the long side and the axial run
/// hits the neighbouring pin in the row. Gentle 135-degree bends come before right-angle ones, and
/// acute bends are left out.
fn pad_exit_ways(board: &Board, chain: &Chain, points: &[Point], tracks: &[ItemId], i: usize, j: usize) -> Vec<(Vec<Point>, [Option<ItemId>; 2])> {
    let (s, e) = (points[i], points[j]);
    let s_pad = (i == 0).then(|| pad_for_exit(board, chain, tracks[i], s)).flatten();
    let e_pad = (j == tracks.len()).then(|| pad_for_exit(board, chain, tracks[j - 1], e)).flatten();
    if s_pad.is_none() && e_pad.is_none() {
        return Vec::new();
    }
    let knees = |a: Point, b: Point| -> Vec<Vec<Point>> {
        match octilinear_ways(a, b) {
            None => vec![vec![a, b]],
            Some(mut ws) => {
                ws.truncate(2);
                ws.into_iter().map(|w| std::iter::once(a).chain(w).chain(std::iter::once(b)).collect()).collect()
            }
        }
    };
    let mut raw: Vec<(Vec<Point>, [Option<ItemId>; 2])> = Vec::new();
    if let Some((cs, ms, ps)) = s_pad {
        let mut ends = vec![(e, None)];
        if let Some((ce, me, pe)) = e_pad {
            if pe != ps && ce != e {
                ends.push((ce, me));
            }
        }
        for (end, end_pad) in ends {
            for p in pad_exits(board, chain, ps, cs, end) {
                for tail in knees(p, end) {
                    let mut path = vec![cs];
                    path.extend(tail);
                    raw.push((path, [ms, end_pad]));
                }
            }
        }
    }
    if let Some((ce, me, pe)) = e_pad {
        let mut starts = vec![(s, None)];
        if let Some((cs, ms, ps)) = s_pad {
            if ps != pe && cs != s {
                starts.push((cs, ms));
            }
        }
        for (start, start_pad) in starts {
            for q in pad_exits(board, chain, pe, ce, start) {
                for mut head in knees(start, q) {
                    head.push(ce);
                    raw.push((head, [start_pad, me]));
                }
            }
        }
    }
    if let (Some((cs, ms, ps)), Some((ce, me, pe))) = (s_pad, e_pad) {
        if ps != pe {
            let ps_out = pad_exits(board, chain, ps, cs, ce);
            let pe_out = pad_exits(board, chain, pe, ce, cs);
            for &p in ps_out.iter().take(2) {
                for &q in pe_out.iter().take(2) {
                    for mid in knees(p, q) {
                        let mut path = vec![cs];
                        path.extend(mid);
                        path.push(ce);
                        raw.push((path, [ms, me]));
                    }
                }
            }
        }
    }
    let non_acute = |a: Point, b: Point, c: Point| -> Option<usize> {
        let (u, v) = (((b.x - a.x) as f64, (b.y - a.y) as f64), ((c.x - b.x) as f64, (c.y - b.y) as f64));
        let (lu, lv) = (u.0.hypot(u.1), v.0.hypot(v.1));
        if lu == 0.0 || lv == 0.0 {
            return None;
        }
        let dot = u.0 * v.0 + u.1 * v.1;
        (dot >= -1e-6 * lu * lv).then_some(usize::from(dot <= 1e-6 * lu * lv))
    };
    let mut scored = Vec::new();
    for (path, pads) in raw {
        if path.len() <= 2 || path.windows(3).any(|w| same_heading(w[0], w[1], w[1], w[2])) {
            continue;
        }
        let Some(rights) = path.windows(3).try_fold(0usize, |acc, w| Some(acc + non_acute(w[0], w[1], w[2])?)) else {
            continue;
        };
        if i > 0 && non_acute(points[i - 1], path[0], path[1]).is_none() {
            continue;
        }
        if j < tracks.len() && non_acute(path[path.len() - 2], path[path.len() - 1], points[j + 1]).is_none() {
            continue;
        }
        let len_nm: i64 = path.windows(2).map(|w| w[0].distance(w[1]) as i64).sum();
        scored.push(((rights, path.len(), len_nm), path, pads));
    }
    scored.sort_by_key(|x| x.0);
    let mut seen: HashSet<Vec<Point>> = HashSet::new();
    scored.into_iter().filter(|(_, p, _)| seen.insert(p.clone())).map(|(_, p, pads)| (p, pads)).collect()
}

/// Where the end `e` of the track `track` from `s` may slide within the pad of its net it lies in
/// alone ([`pad_alone_at`]), onto
/// a line from `s` at a multiple of 45 degrees (along either axis or the nearer diagonal), and the
/// pad: the points deep enough inside it.
fn slides(board: &Board, chain: &Chain, track: ItemId, s: Point, e: Point) -> Vec<(Point, ItemId)> {
    let Some(pad) = pad_alone_at(board, chain, track, e) else {
        return Vec::new();
    };
    let shape = &board.item(pad).expect("found").shape;
    // Deep enough that the track's round end stays in the pad (or as deep as it was, in a pad
    // too small for that), and half as deep as it was: an end slid towards the pad's rim leaves
    // a slot between the track and the pad's curve (ErgoDone's ROW lines into its switches'
    // round pads, slid 0.9 mm off their centres).
    let was = shape.depth(e);
    let deep = (chain.half_width as f64).min(was).max(was / 2.0);
    let (dx, dy) = (e.x - s.x, e.y - s.y);
    let (sx, sy) = (dx.signum(), dy.signum());
    let t = (dx * sx + dy * sy) / 2;
    [Point::new(e.x, s.y), Point::new(s.x, e.y), Point::new(s.x + sx * t, s.y + sy * t)]
        .into_iter()
        .filter(|&q| q != e && q != s && shape.depth(q) >= deep)
        .map(|q| (q, pad))
        .collect()
}

/// Where both ends `s` and `e` of a chain lie alone in distinct pads of its net
/// ([`pad_alone_at`]), the pairs of points inside those pads that meet on one straight or
/// 45-degree line halfway between `s` and `e`, each deep enough inside its pad.
fn two_slides(board: &Board, chain: &Chain, first: ItemId, s: Point, last: ItemId, e: Point) -> Vec<([Point; 2], [Option<ItemId>; 2])> {
    let (Some(ps), Some(pe)) = (pad_alone_at(board, chain, first, s), pad_alone_at(board, chain, last, e)) else {
        return Vec::new();
    };
    if ps == pe {
        return Vec::new();
    }
    let (ss, se) = (&board.item(ps).expect("found").shape, &board.item(pe).expect("found").shape);
    let (ws, we) = (ss.depth(s), se.depth(e));
    let deep_s = (chain.half_width as f64).min(ws).max(ws / 2.0);
    let deep_e = (chain.half_width as f64).min(we).max(we / 2.0);
    let (xm, ym) = ((s.x + e.x) / 2, (s.y + e.y) / 2);
    let (dx, dy) = (e.x - s.x, e.y - s.y);
    let (sx, sy) = (dx.signum(), dy.signum());
    let d = (dx * sx - dy * sy) / 4;
    let qs = Point::new(s.x + sx * d, s.y - sy * d);
    let t = ((e.x - qs.x) * sx + (e.y - qs.y) * sy) / 2;
    let qe = Point::new(qs.x + sx * t, qs.y + sy * t);
    [(Point::new(s.x, ym), Point::new(e.x, ym)), (Point::new(xm, s.y), Point::new(xm, e.y)), (qs, qe)]
        .into_iter()
        .filter(|&(a, b)| a != b && (a != s || b != e) && ss.depth(a) >= deep_s && se.depth(b) >= deep_e)
        .map(|(a, b)| ([a, b], [Some(ps), Some(pe)]))
        .collect()
}

/// Whether the overlap of `g` lies wholly inside other connected copper of `net` on `layer`
/// (a pad, via, kept pour, or an adjacent track of the way or chain bridged to `g.other`), so no
/// narrow sliver is left exposed on the copper outline (Biscay's C18-2, a piece from a pad's
/// centre overlapping a via whose land stands outside the pad's centre but deep inside its rim).
fn graze_covered(board: &Board, layer: usize, net: u32, r1: Nm, pieces: &[ItemId], sides: &[ItemId], g: &Graze) -> bool {
    let Some(other) = board.item(g.other) else { return false };
    let (Kind::Track | Kind::Via, Shape::Capsule { r: r2, .. }) = (&other.kind, &other.shape) else {
        return false;
    };
    let d = g.from.distance(g.to);
    if d <= 0.0 {
        return false;
    }
    let along = |dist: f64| {
        Point::new(
            g.from.x + ((g.to.x - g.from.x) as f64 * (dist / d)).round() as Nm,
            g.from.y + ((g.to.y - g.from.y) as f64 * (dist / d)).round() as Nm,
        )
    };
    let (p_rim, q_rim) = (along((r1 as f64).min(d)), along((d - *r2 as f64).max(0.0)));
    board.near(layer, board::BBox::of([p_rim, q_rim])).into_iter().any(|c_id| {
        if c_id == g.track || c_id == g.other {
            return false;
        }
        let Some(c) = board.item(c_id) else { return false };
        if c.net != net || !c.on(layer) || !c.shape.contains(p_rim) || !c.shape.contains(q_rim) {
            return false;
        }
        match c.kind {
            Kind::Pad { .. } | Kind::Via | Kind::Pour { kept: true, .. } => true,
            Kind::Track => {
                (pieces.contains(&c_id) || sides.contains(&c_id))
                    && firmly_joined(board, c_id, g.track)
                    && (firmly_joined(board, c_id, g.other) || bridged(board, layer, c_id, c, g.other, other))
            }
            _ => false,
        }
    })
}

type WayOut = (Vec<ItemId>, Vec<(ItemId, Item)>);

/// Lays the tracks `tracks[i..j]` along `path` (their ends, or ends slid within `pads`, and
/// the points between), as [`try_cut`] cuts a bend: no piece across the board's edge, whatever
/// joined the tracks joined to the pieces or the tracks either side, no piece of a Z shorter than
/// the track is wide, and none breaking a rule or pulling into a new graze with its net's copper.
/// The pieces and the tracks taken out (with their ids, to put back) if `keep`; else, or if it
/// does not fit, the board is left as it was. Tried without keeping, the pieces are [`TAUT_MARGIN`]
/// wider.
#[allow(clippy::too_many_arguments)]
fn try_way(board: &mut Board, chain: &Chain, points: &[Point], tracks: &[ItemId], i: usize, j: usize, path: &[Point], pads: [Option<ItemId>; 2], keep: bool) -> Option<WayOut> {
    let layer = chain.layer.trailing_zeros() as usize;
    let way = &path[1..path.len() - 1];
    // A Z's piece shorter than the track is wide leaves the pieces either side overlapping
    // without meeting end to end, a graze; a knee's two pieces always meet end to end, so a
    // knee may step aside by a sliver (a track off straight by 40 um over 4 mm, its ends'
    // pads not quite in line).
    if way.len() > 1 && path.windows(2).any(|w| w[0].distance(w[1]) < (2 * chain.half_width) as f64) {
        return None;
    }
    // Off the board's edge: the rules would find a piece too near it, but not one wholly past a
    // notch. What lies between the track and the way can stay: the way passes it on the other
    // side, which the rules allow as they allow any route.
    let off_edge = path.windows(2).any(|w| {
        let piece = Shape::Capsule { a: w[0], b: w[1], r: 0 };
        let bbox = piece.bbox();
        board.outline_edges.iter().any(|(x, y, bb, _)| bb.overlaps(bbox) && piece.gap(&Shape::Capsule { a: *x, b: *y, r: 0 }) == 0.0)
    });
    if off_edge {
        return None;
    }
    let span = &tracks[i..j];
    let mut partners: Vec<ItemId> = Vec::new();
    for &t in span {
        let track = board.item(t)?;
        partners.extend(board.near(layer, track.shape.bbox()).into_iter().filter(|&o| !span.contains(&o) && board.item(o).is_some_and(|it| it.net == chain.net) && joined(board, t, o)));
    }
    partners.sort_unstable();
    partners.dedup();
    // An end slid within its pad: what met the track there meets the pad.
    let sides: Vec<ItemId> = [i.checked_sub(1).map(|k| tracks[k]), tracks.get(j).copied(), pads[0], pads[1]].into_iter().flatten().collect();
    let removed: Vec<(ItemId, Item)> = span.iter().filter_map(|t| Some((*t, board.remove(*t)?))).collect();
    let r = if keep { chain.half_width } else { chain.half_width + TAUT_MARGIN };
    let mut pieces = Vec::new();
    let mut ok = true;
    for w in path.windows(2) {
        if !ok {
            break;
        }
        let id = board.insert(Item { kind: Kind::Track, net: chain.net, layers: chain.layer, shape: Shape::Capsule { a: w[0], b: w[1], r }, hole: None, locked: false, span: None });
        pieces.push(id);
        ok = check_item(board, id).is_empty();
    }
    let rule = ok;
    ok = ok && partners.iter().all(|&o| pieces.iter().chain(&sides).any(|&p| p == o || joined(board, o, p)));
    let join = ok;
    let same_ends: [Option<Point>; 2] = [(path[0] == points[i]).then_some(path[0]), (path[path.len() - 1] == points[j]).then_some(path[path.len() - 1])];
    if ok {
        // An unchanged end that met another track only by overlapping round ends before squaring
        // (`mend_joints` mends it in write, 1Bitsy's /PA4) is no worse after; a new graze pulled
        // against copper mid-run is refused.
        let gs = grazes(board, &pieces);
        if !gs.is_empty() {
            let mut old_gs: Option<Vec<Graze>> = None;
            ok = gs.iter().all(|g| {
                if graze_covered(board, layer, chain.net, r, &pieces, &sides, g) {
                    return true;
                }
                let Some(end) = same_ends.iter().flatten().copied().find(|&p| g.from.distance(p) <= r as f64) else {
                    return false;
                };
                let old = old_gs.get_or_insert_with(|| {
                    let held: Vec<(ItemId, Item)> = pieces.iter().filter_map(|&id| Some((id, board.remove(id)?))).collect();
                    for (id, it) in &removed {
                        board.restore(*id, it.clone());
                    }
                    let found = grazes(board, span);
                    for &(id, _) in &removed {
                        board.remove(id);
                    }
                    for (id, it) in held {
                        board.restore(id, it);
                    }
                    found
                });
                old.iter().any(|og| og.other == g.other && og.from.distance(end) <= r as f64 && g.overlap + 1_000.0 >= og.overlap)
            });
        }
    }
    let graze = ok;
    ok = ok && !leaves_slot(board, chain, &pieces, &same_ends);
    ok = ok && removed.len() == span.len();
    if let Some(at) = square_trace() {
        if path.windows(2).any(|w| board::point_segment(at, w[0], w[1]) < 1e6) || removed.iter().any(|(_, it)| it.shape.bbox().grow(500_000).contains(at)) {
            let mm = |p: &Point| format!("({:.3},{:.3})", p.x as f64 / 1e6, p.y as f64 / 1e6);
            let why = match (rule, join, graze, ok) {
                (false, ..) => format!("rule {:?}", pieces.last().and_then(|&id| check_item(board, id).into_iter().next())),
                (_, false, ..) => "join".into(),
                (_, _, false, _) => "graze".into(),
                (.., false) => "slot".into(),
                _ => "fits".into(),
            };
            eprintln!("squaring {}{} -> {why}", path.iter().map(mm).collect::<Vec<_>>().join(" "), if keep { " (laid)" } else { "" });
        }
    }
    if ok && keep {
        return Some((pieces, removed));
    }
    for id in pieces {
        board.discard(id);
    }
    for (id, item) in removed {
        board.restore(id, item);
    }
    ok.then(|| (Vec::new(), Vec::new()))
}

/// Whether the triangle `a`, `p`, `b` on `layer` holds nothing a chain of `net` must go round:
/// another net's copper but a pour that is refilled, a keepout, a hole or the board's edge. A
/// cut across it then slides the chain over empty board, keeping it on the same side of all else.
fn sweep_clear(board: &Board, net: u32, layer: usize, a: Point, p: Point, b: Point) -> bool {
    let tri = Shape::Polygon(vec![a, p, b]);
    let bbox = board::BBox::of([a, p, b]);
    let blocks = |o: &Item| match o.kind {
        Kind::Spoke { .. } => false,
        Kind::Pour { .. } if o.refilled() => false,
        Kind::Keepout { .. } => true,
        _ => o.net != net || net == 0,
    };
    let edge = board.outline_edges.iter().any(|(x, y, bb, _)| bb.overlaps(bbox) && tri.gap(&Shape::Capsule { a: *x, b: *y, r: 0 }) == 0.0);
    let copper = board.near(layer, bbox).into_iter().filter_map(|o| board.item(o)).any(|o| o.on(layer) && blocks(o) && o.shape.gap_within(&tri, 0.0) == 0.0);
    let holes = board.layers.len();
    let hole = board.near(holes, bbox).into_iter().any(|o| board.item(o).and_then(|it| board.hole_shape(o, it)).is_some_and(|h| h.gap_within(&tri, 0.0) == 0.0));
    !edge && !copper && !hole
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
        // A crystal net's XOUT on In1.Cu, less (101.9, 116.6) mm: a fan-out stub from one pad, a
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
    fn of_two_vias_whose_lands_only_touch_the_one_not_needed_goes() {
        // The earring's VLED_C: two vias 0.403 mm apart with 0.4 mm lands, a 0.029 mm sliver.
        let mut board = empty_board();
        let via = |board: &mut Board, x: Nm| board.insert(Item { kind: Kind::Via, net: 1, layers: 1, shape: Shape::Capsule { a: Point::new(x, 0), b: Point::new(x, 0), r: 200_000 }, hole: Some((Point::new(x, 0), 100_000)), locked: false, span: None });
        let pad = pad_at(&mut board, -2_000_000, 0);
        let keep = via(&mut board, 0);
        let spare = via(&mut board, 395_000);
        let track = board.insert(seg(Point::new(-2_000_000, 0), Point::new(0, 0)));
        let mut laid = vec![keep, spare, track];
        assert_eq!(via_grazes(&board, &laid).len(), 1);
        let (mended, _) = mend_via_grazes(&mut board, &mut laid);
        assert_eq!(mended, 1);
        assert!(board.item(spare).is_none() && board.item(keep).is_some() && board.item(pad).is_some());
        assert!(via_grazes(&board, &laid).is_empty());
    }

    #[test]
    fn a_via_whose_land_only_clips_its_pads_corner_is_bridged_into_the_pad() {
        // LoRaWAN_GPS's C15 pad 1: a 0.6 mm via clipping the corner of a 0.8 x 0.95 mm pad, a
        // 0.056 mm sliver, the via needed for the track leaving it on the other side.
        let mut board = empty_board();
        let pad = board.insert(Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net: 1,
            layers: 1,
            shape: Shape::Polygon(vec![Point::new(-400_000, -475_000), Point::new(400_000, -475_000), Point::new(400_000, 475_000), Point::new(-400_000, 475_000)]),
            hole: None,
            locked: true,
            span: None,
        });
        let at = Point::new(-575_000, 585_000);
        let via = board.insert(Item { kind: Kind::Via, net: 1, layers: 1, shape: Shape::Capsule { a: at, b: at, r: 300_000 }, hole: Some((at, 150_000)), locked: false, span: None });
        pad_at(&mut board, -3_000_000, 3_000_000);
        let track = board.insert(seg(at, Point::new(-3_000_000, 3_000_000)));
        let far = pad_at(&mut board, 3_000_000, 0);
        let back = board.insert(seg(Point::new(400_000, 0), Point::new(3_000_000, 0)));
        let mut laid = vec![via, track, back];
        assert_eq!(via_pad_grazes(&board, &laid), vec![(via, pad)]);
        let (mended, _) = mend_via_pad_grazes(&mut board, &mut laid);
        assert_eq!(mended, 1);
        assert!(board.item(via).is_some(), "the via joins the track to the pad");
        assert!(via_pad_grazes(&board, &laid).is_empty(), "bridged, it is no longer a graze");
        assert_eq!(mend_via_pad_grazes(&mut board, &mut laid).0, 0);
        let piece = *laid.last().unwrap();
        let Shape::Capsule { a, b, r } = board.item(piece).unwrap().shape.clone() else { panic!() };
        assert_eq!((a, r), (at, 300_000));
        assert!(board.item(pad).unwrap().shape.depth(b) >= 300_000.0, "{b:?}");
        assert!(board.item(far).is_some());
    }

    #[test]
    fn a_via_whose_land_only_touches_its_pads_corner_is_a_graze() {
        let mut board = empty_board();
        let pad = board.insert(Item {
            kind: Kind::Pad { footprint: 0, number: "8".into(), plated: false },
            net: 1,
            layers: 1,
            shape: Shape::Polygon(vec![Point::new(-400_000, -900_000), Point::new(400_000, -900_000), Point::new(400_000, 900_000), Point::new(-400_000, 900_000)]),
            hole: None,
            locked: true,
            span: None,
        });
        // 0.4445 mm land, its centre on the corner's diagonal, the rim 0.1 um from the corner.
        let off = ((444_500.0 + 0.1) / std::f64::consts::SQRT_2).round() as Nm;
        let at = Point::new(400_000 + off, 900_000 + off);
        let via = board.insert(Item { kind: Kind::Via, net: 1, layers: 1, shape: Shape::Capsule { a: at, b: at, r: 444_500 }, hole: Some((at, 317_500)), locked: false, span: None });
        assert_eq!(via_pad_grazes(&board, &[via]), vec![(via, pad)]);
    }

    #[test]
    fn a_via_centred_in_its_pad_or_clear_of_it_is_no_graze() {
        let mut board = empty_board();
        let pad = pad_at(&mut board, 0, 0);
        let via = |board: &mut Board, x: Nm| board.insert(Item { kind: Kind::Via, net: 1, layers: 1, shape: Shape::Capsule { a: Point::new(x, 0), b: Point::new(x, 0), r: 200_000 }, hole: Some((Point::new(x, 0), 100_000)), locked: false, span: None });
        let inside = via(&mut board, 100_000);
        let clear = via(&mut board, 400_000);
        let deep = via(&mut board, -250_000);
        assert!(via_pad_grazes(&board, &[inside, clear]).is_empty());
        // 0.25 mm from the centre of a 0.3 mm round pad, with a 0.4 mm land: a 0.31 mm chord.
        assert!(via_pad_grazes(&board, &[deep]).is_empty(), "a deep overlap is a joint");
        assert!(board.item(pad).is_some());
    }

    #[test]
    fn two_vias_whose_lands_only_touch_at_a_point_are_a_graze() {
        let mut board = empty_board();
        let via = |board: &mut Board, x: Nm| board.insert(Item { kind: Kind::Via, net: 1, layers: 1, shape: Shape::Capsule { a: Point::new(x, 0), b: Point::new(x, 0), r: 550_000 }, hole: Some((Point::new(x, 0), 300_000)), locked: false, span: None });
        let one = via(&mut board, 0);
        let two = via(&mut board, 1_100_000);
        let three = via(&mut board, 3_000_000);
        assert_eq!(via_grazes(&board, &[one, two, three]), vec![(one, two)]);
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
        // Inside a kept pour of their net (easy2350's protected +3V3 strip), the pour joins them:
        // no graze. A pour the refill redraws still leaves one.
        for (kept, want) in [(true, 0), (false, 1)] {
            let mut board = empty_board();
            let laid = [t(&mut board, (0, 0), (2_000_000, 0)), t(&mut board, (0, 197_000), (2_000_000, 197_000))];
            let square = vec![Point::new(-500_000, -500_000), Point::new(2_500_000, -500_000), Point::new(2_500_000, 700_000), Point::new(-500_000, 700_000)];
            board.insert(Item { kind: Kind::Pour { zone: 0, kept }, net: 1, layers: 1, shape: Shape::Polygon(square), hole: None, locked: true, span: None });
            assert_eq!(grazes(&board, &laid).len(), want, "kept {kept}");
        }
    }

    #[test]
    fn a_track_across_a_pads_corner_is_joined_by_a_piece_to_the_pads_centre() {
        // sunflower's VSYS on B.Cu: a 0.2 mm track across C32-1's corner, 0.56 x 0.62 mm, the
        // overlap 0.02 mm. The piece that bridged it reached just past the pad's edge; a person
        // draws one from where the track passes to the pad's centre.
        let mut board = empty_board();
        let rect = |w: Nm, h: Nm| vec![Point::new(-w, -h), Point::new(w, -h), Point::new(w, h), Point::new(-w, h)];
        board.insert(Item { kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false }, net: 1, layers: 1, shape: Shape::Polygon(rect(310_000, 280_000)), hole: None, locked: true, span: None });
        let track = board.insert(Item { shape: Shape::Capsule { a: Point::new(-1_000_000, -360_000), b: Point::new(1_000_000, -360_000), r: 100_000 }, ..seg(Point::new(0, 0), Point::new(0, 0)) });
        let mut laid = vec![track];
        assert_eq!(grazes(&board, &laid).len(), 1);
        assert_eq!(mend_grazes(&mut board, &mut laid), 1);
        let piece = board.item(laid[1]).expect("the piece").clone();
        let Shape::Capsule { a, b, r } = piece.shape else { panic!("a track") };
        assert_eq!((a, b, r), (Point::new(0, -360_000), Point::new(0, 0), 100_000), "from the track to the centre, at its width");
    }

    #[test]
    fn a_chain_that_overshoots_a_via_is_pulled_taut_round_it() {
        // Dense Square's SW2_9 on In4.Cu: up past a via of another net and back down to pass
        // under it, 2.4 mm out of its way. Joining two of its points crosses the via, so pulling
        // tight keeps it; pulled taut it bends round the via, the via on the side it was.
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"N1\") (net 2 \"N2\"))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let (p0, p1, p2) = (Point::new(0, 0), Point::new(0, -4_000_000), Point::new(2_700_000, -2_650_000));
        for at in [p0, p2] {
            board.insert(Item { kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false }, net: 1, layers: 1, shape: Shape::Capsule { a: at, b: at, r: 150_000 }, hole: None, locked: true, span: None });
        }
        let o = Point::new(1_350_000, -1_325_000);
        board.insert(Item { kind: Kind::Via, net: 2, layers: 1, shape: Shape::Capsule { a: o, b: o, r: 300_000 }, hole: Some((o, 150_000)), locked: true, span: None });
        let mut soft = vec![board.insert(seg(p0, p1)), board.insert(seg(p1, p2))];
        let before = pull_tight(&mut board, &mut soft, &Default::default());
        assert_eq!(before.segments_after, 2, "no two of its points can be joined");
        let taut = pull_taut_watch(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {});
        assert!(taut.length_after < 4.5, "{taut:?}");
        assert!(board::check(&board).is_empty(), "{:?}", board::check(&board));
        assert!(board::connectivity(&board).airlines.is_empty());
        // Every piece passes the via on its left, walking from p0, as the chain did.
        for id in &soft {
            let Shape::Capsule { a, b, .. } = board.item(*id).expect("laid").shape else { panic!("a track") };
            let (a, b) = if a.distance(p0) < b.distance(p0) { (a, b) } else { (b, a) };
            let cross = (b.x - a.x) as f64 * (o.y - a.y) as f64 - (b.y - a.y) as f64 * (o.x - a.x) as f64;
            assert!(cross > 0.0, "{a:?} -> {b:?} passes the via on its other side");
        }
    }

    #[test]
    fn a_track_at_an_odd_angle_is_bent_onto_45_degrees_on_the_side_that_fits() {
        // Pad to pad 4 mm across and 1.5 mm down: straight first, the knee at (2.5, 0), passes a
        // via of another net too close, so the knee goes diagonal first, at (1.5, 1.5).
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"N1\") (net 2 \"N2\"))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let (s, e) = (Point::new(0, 0), Point::new(4_000_000, 1_500_000));
        for at in [s, e] {
            board.insert(Item { kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false }, net: 1, layers: 1, shape: Shape::Capsule { a: at, b: at, r: 150_000 }, hole: None, locked: true, span: None });
        }
        let o = Point::new(2_300_000, 300_000);
        board.insert(Item { kind: Kind::Via, net: 2, layers: 1, shape: Shape::Capsule { a: o, b: o, r: 100_000 }, hole: Some((o, 50_000)), locked: true, span: None });
        let mut soft = vec![board.insert(seg(s, e))];
        assert!(board::check(&board).is_empty());
        let bent = octilinear_watch(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {});
        assert_eq!(bent, 1);
        assert!(board::check(&board).is_empty(), "{:?}", board::check(&board));
        assert!(board::connectivity(&board).airlines.is_empty());
        let mut ends: Vec<(Point, Point)> = soft.iter().map(|id| match board.item(*id).expect("laid").shape { Shape::Capsule { a, b, .. } => (a, b), _ => panic!("a track") }).collect();
        ends.sort_by_key(|(a, b)| (a.x.min(b.x), a.y.min(b.y)));
        assert_eq!(ends, vec![(s, Point::new(1_500_000, 1_500_000)), (Point::new(1_500_000, 1_500_000), e)]);
        // A track already on 45 degrees is left alone.
        assert_eq!(octilinear_watch(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {}), 0);
    }

    #[test]
    fn a_piece_slants_into_a_pad_crossing_its_long_side_at_45_degrees() {
        let mm = |x: f64, y: f64| Point::new((x * 1e6) as Nm, (y * 1e6) as Nm);
        let rect = Shape::Polygon(vec![mm(-1.0, -0.5), mm(1.0, -0.5), mm(1.0, 0.5), mm(-1.0, 0.5)]);
        let oval = Shape::Capsule { a: mm(-1.0, 0.0), b: mm(1.0, 0.0), r: 500_000 };
        let round = Shape::Capsule { a: mm(0.0, 0.0), b: mm(0.0, 0.0), r: 500_000 };
        for pad in [&rect, &oval] {
            assert!(slants_into(mm(-2.0, -2.0), mm(0.0, 0.0), pad));
            assert!(!slants_into(mm(0.0, -2.0), mm(0.0, 0.0), pad));
            assert!(!slants_into(mm(-3.0, 0.0), mm(0.0, 0.0), pad));
        }
        // Into a round pad's centre at any angle is square on.
        assert!(!slants_into(mm(-2.0, -2.0), mm(0.0, 0.0), &round));
        assert!(slants_into(mm(-2.0, -2.0), mm(0.3, 0.0), &round));
    }

    #[test]
    fn doglegs_are_short_steps_between_runs_heading_the_same_way() {
        let mm = |x: f64, y: f64| Point::new((x * 1e6) as Nm, (y * 1e6) as Nm);
        let hw = 125_000;
        assert_eq!(doglegs(&[mm(0.0, 0.0), mm(5.0, 0.0), mm(5.5, 0.5), mm(10.0, 0.5)], hw), 1);
        // A long step, a knee and a step too small to show are none.
        assert_eq!(doglegs(&[mm(0.0, 0.0), mm(5.0, 0.0), mm(7.0, 2.0), mm(10.0, 2.0)], hw), 0);
        assert_eq!(doglegs(&[mm(0.0, 0.0), mm(5.0, 0.0), mm(5.5, 0.5), mm(5.5, 3.0)], hw), 0);
        assert_eq!(doglegs(&[mm(0.0, 0.0), mm(5.0, 0.0), mm(5.05, 0.05), mm(10.0, 0.05)], hw), 0);
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

    #[test]
    fn a_track_with_both_ends_off_centre_in_its_pads_moves_both_ends_to_square() {
        let mut board = empty_board();
        let pad = |x, y, r| Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(x, y), b: Point::new(x, y), r },
            hole: None,
            locked: true,
            span: None,
        };
        // Both ends lie near opposite rims of 0.8 mm round pads: moving one end alone leaves the
        // other shallow enough to form a slot, so both move to their pads' centres.
        board.insert(pad(0, 0, 400_000));
        board.insert(pad(4_000_000, 1_500_000, 400_000));
        let mut soft = vec![board.insert(seg(Point::new(0, -320_000), Point::new(4_000_000, 1_820_000)))];
        let bent = octilinear_watch(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {});
        assert_eq!(bent, 1);
        assert_eq!(soft.len(), 2);
        assert!(board::check(&board).is_empty());
        assert!(board::connectivity(&board).airlines.is_empty());
        let mut ends: Vec<(Point, Point)> = soft.iter().map(|&id| match board.item(id).unwrap().shape { Shape::Capsule { a, b, .. } => (a, b), _ => panic!() }).collect();
        ends.sort_by_key(|(a, b)| (a.x.min(b.x), a.y.min(b.y)));
        assert_eq!(ends, vec![(Point::new(0, 0), Point::new(2_500_000, 0)), (Point::new(2_500_000, 0), Point::new(4_000_000, 1_500_000))]);

        // Two pads offset by 0.16 mm in y: neither pad's centre line stays deep enough in the
        // other, while sliding both ends to y = 0 lays one straight track with no bend.
        let mut board = empty_board();
        board.insert(pad(0, 80_000, 250_000));
        board.insert(pad(4_000_000, -80_000, 250_000));
        let mut soft = vec![board.insert(Item { shape: Shape::Capsule { a: Point::new(0, 200_000), b: Point::new(4_000_000, -200_000), r: 75_000 }, ..seg(Point::new(0, 0), Point::new(0, 0)) })];
        assert_eq!(octilinear_watch(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {}), 1);
        assert_eq!(soft.len(), 1, "one straight track without a knee: {soft:?}");
        assert_eq!(board.item(soft[0]).unwrap().shape, Shape::Capsule { a: Point::new(0, 0), b: Point::new(4_000_000, 0), r: 75_000 });
        assert!(board::connectivity(&board).airlines.is_empty());
    }

    #[test]
    fn a_graze_already_at_an_unchanged_end_allows_squaring_while_a_new_graze_refuses_it() {
        // 1Bitsy's /PA4: a slanted track ending 0.07 mm from a locked escape's start, joined by
        // overlapping round ends that `mend_joints` mends on write.
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"N1\") (net 2 \"N2\"))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        pad_at(&mut board, 0, 0);
        pad_at(&mut board, 9_000_000, 6_225_000);
        board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(7_085_000, 6_225_000), b: Point::new(9_000_000, 6_225_000), r: 75_000 },
            hole: None,
            locked: true,
            span: None,
        });
        let slanted = board.insert(Item {
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(7_015_000, 6_225_000), r: 75_000 },
            ..seg(Point::new(0, 0), Point::new(0, 0))
        });
        assert_eq!(grazes(&board, &[slanted]).len(), 1, "the round ends already graze");
        let mut soft = vec![slanted];
        assert_eq!(octilinear_watch(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {}), 1);
        assert_eq!(soft.len(), 2);
        assert!(board::connectivity(&board).airlines.is_empty());

        // A way that pulls a track into a new mid-run graze against a pad's edge is still refused.
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        pad_at(&mut board, 0, 0);
        pad_at(&mut board, 4_000_000, 1_500_000);
        board.insert(Item {
            kind: Kind::Pad { footprint: 0, number: "2".into(), plated: false },
            net: 1,
            layers: 1,
            shape: Shape::Polygon(vec![Point::new(1_200_000, -265_000), Point::new(1_400_000, -265_000), Point::new(1_400_000, -65_000), Point::new(1_200_000, -65_000)]),
            hole: None,
            locked: true,
            span: None,
        });
        for (x, y) in [(1_000_000, 1_000_000), (2_000_000, 750_000), (1_500_000, 300_000), (2_600_000, 750_000)] {
            let at = Point::new(x, y);
            board.insert(Item { kind: Kind::Via, net: 2, layers: 1, shape: Shape::Capsule { a: at, b: at, r: 250_000 }, hole: Some((at, 125_000)), locked: true, span: None });
        }
        let slanted = board.insert(Item { shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(4_000_000, 1_500_000), r: 75_000 }, ..seg(Point::new(0, 0), Point::new(0, 0)) });
        assert!(grazes(&board, &[slanted]).is_empty());
        let mut soft = vec![slanted];
        assert_eq!(octilinear_watch(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {}), 0);
    }

    #[test]
    fn a_slanted_track_into_a_long_pad_exits_square_to_its_side() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"N1\") (net 2 \"N2\"))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let rect_pad = |net, x0, x1, y0, y1| Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net,
            layers: 1,
            shape: Shape::Polygon(vec![Point::new(x0, y0), Point::new(x1, y0), Point::new(x1, y1), Point::new(x0, y1)]),
            hole: None,
            locked: true,
            span: None,
        };
        // A vertical SMD pad in a row: going East hits the neighbouring pin, and going NE at 45
        // degrees crosses the vertical long side at a slant; leaving North out the pad's end, then
        // NE, then East squares the track in three pieces with no slot or dogleg.
        board.insert(rect_pad(1, -250_000, 250_000, -750_000, 750_000));
        board.insert(rect_pad(2, 550_000, 1_050_000, -750_000, 750_000));
        pad_at(&mut board, 4_500_000, 2_000_000);
        let mut soft = vec![board.insert(Item {
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(4_500_000, 2_000_000), r: 75_000 },
            ..seg(Point::new(0, 0), Point::new(0, 0))
        })];
        assert_eq!(octilinear_watch(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {}), 1);
        assert_eq!(soft.len(), 3, "north exit, diagonal, east straight: {soft:?}");
        for &id in &soft {
            let Shape::Capsule { a, b, .. } = board.item(id).unwrap().shape else { panic!() };
            assert!(octilinear_ways(a, b).is_none(), "off-45 piece left: {a:?} -> {b:?}");
        }
        assert!(board::check(&board).is_empty());
        assert!(board::connectivity(&board).airlines.is_empty());

        // A round pad with foreign copper at its centre (Atreus's diodes): an unchanged end near
        // the East rim exiting straight East crosses the rim square on without passing the centre.
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        board.insert(Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(0, 0), r: 800_000 },
            hole: None,
            locked: true,
            span: None,
        });
        board.insert(Item {
            kind: Kind::Track,
            net: 2,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(0, -1_500_000), r: 250_000 },
            hole: None,
            locked: true,
            span: None,
        });
        board.insert(Item {
            kind: Kind::Via,
            net: 2,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(2_500_000, 0), b: Point::new(2_500_000, 0), r: 250_000 },
            hole: Some((Point::new(2_500_000, 0), 125_000)),
            locked: true,
            span: None,
        });
        pad_at(&mut board, 5_000_000, 2_000_000);
        let mut soft = vec![board.insert(Item {
            shape: Shape::Capsule { a: Point::new(780_000, 0), b: Point::new(5_000_000, 2_000_000), r: 75_000 },
            ..seg(Point::new(0, 0), Point::new(0, 0))
        })];
        assert_eq!(octilinear_watch(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {}), 1);
        for &id in &soft {
            let Shape::Capsule { a, b, .. } = board.item(id).unwrap().shape else { panic!() };
            assert!(octilinear_ways(a, b).is_none(), "off-45 piece left: {a:?} -> {b:?}");
            assert!(check_item(&board, id).is_empty());
        }
        assert!(board::net_connectivity(&board, 1, true).airlines.is_empty());
    }

    #[test]
    fn a_45_degree_track_fork_is_refused_while_a_shared_pad_centre_still_exits_square() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"N1\") (net 2 \"N2\"))",
        )
        .unwrap();
        let pad_at = |board: &mut Board, x: Nm, y: Nm| {
            board.insert(Item {
                kind: Kind::Pad { footprint: 0, number: "p".into(), plated: false },
                net: 1,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(x, y), b: Point::new(x, y), r: 150_000 },
                hole: None,
                locked: true,
                span: None,
            })
        };

        // A branch off the middle of an East track to (2.5, 3.0): when the 90-degree South leg is
        // blocked, every remaining 45-degree knee or Z peels off the East track at 45 degrees.
        let fork_board = |block_south: bool| {
            let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
            pad_at(&mut board, 0, 0);
            pad_at(&mut board, 4_000_000, 0);
            pad_at(&mut board, 2_500_000, 3_000_000);
            board.insert(Item {
                kind: Kind::Track,
                net: 1,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(4_000_000, 0), r: 75_000 },
                hole: None,
                locked: true,
                span: None,
            });
            if block_south {
                board.insert(Item {
                    kind: Kind::Via,
                    net: 2,
                    layers: 1,
                    shape: Shape::Capsule { a: Point::new(1_000_000, 700_000), b: Point::new(1_000_000, 700_000), r: 200_000 },
                    hole: Some((Point::new(1_000_000, 700_000), 100_000)),
                    locked: true,
                    span: None,
                });
            }
            let soft = vec![board.insert(Item {
                shape: Shape::Capsule { a: Point::new(1_000_000, 0), b: Point::new(2_500_000, 3_000_000), r: 75_000 },
                ..seg(Point::new(0, 0), Point::new(0, 0))
            })];
            (board, soft)
        };
        let (mut board, mut soft) = fork_board(true);
        assert_eq!(octilinear_watch(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {}), 0);
        let (mut board, mut soft) = fork_board(false);
        assert_eq!(octilinear_watch(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {}), 1);

        // Two daisy-chain tracks meeting at a long pad's centre (1Bitsy's X3-3): one goes South,
        // and the slanted one still leaves East square to the long side without losing the joint.
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        board.insert(Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net: 1,
            layers: 1,
            shape: Shape::Polygon(vec![Point::new(-370_000, -1_400_000), Point::new(370_000, -1_400_000), Point::new(370_000, 1_400_000), Point::new(-370_000, 1_400_000)]),
            hole: None,
            locked: true,
            span: None,
        });
        pad_at(&mut board, 0, 3_000_000);
        pad_at(&mut board, 3_500_000, -1_500_000);
        board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(0, 3_000_000), r: 75_000 },
            hole: None,
            locked: true,
            span: None,
        });
        let mut soft = vec![board.insert(Item {
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(3_500_000, -1_500_000), r: 75_000 },
            ..seg(Point::new(0, 0), Point::new(0, 0))
        })];
        assert_eq!(octilinear_watch(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {}), 1);
        assert!(board::check(&board).is_empty());
        assert!(board::connectivity(&board).airlines.is_empty());
    }

    #[test]
    fn length_picked_z_splits_thread_a_corridor_without_doglegs() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"N1\") (net 2 \"N2\"))",
        )
        .unwrap();
        let pad_at = |board: &mut Board, x: Nm, y: Nm| {
            board.insert(Item {
                kind: Kind::Pad { footprint: 0, number: "p".into(), plated: false },
                net: 1,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(x, y), b: Point::new(x, y), r: 150_000 },
                hole: None,
                locked: true,
                span: None,
            })
        };
        let block = |board: &mut Board, x: Nm, y: Nm, r: Nm| {
            board.insert(Item {
                kind: Kind::Via,
                net: 2,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(x, y), b: Point::new(x, y), r },
                hole: Some((Point::new(x, y), r / 2)),
                locked: true,
                span: None,
            })
        };

        // Diagonal offset 0.6 mm (< DOGLEG, so every straight-split Z is a dogleg) and straight
        // run 2.4 mm (> DOGLEG): blocking both knees and the 0.5 diagonal split leaves only an
        // off-midpoint diagonal split (y = 0.15 mm) clear.
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        pad_at(&mut board, 0, 0);
        pad_at(&mut board, 3_000_000, 600_000);
        block(&mut board, 1_500_000, 650_000, 150_000);
        block(&mut board, 2_200_000, -300_000, 150_000);
        let mut soft = vec![board.insert(Item {
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(3_000_000, 600_000), r: 75_000 },
            ..seg(Point::new(0, 0), Point::new(0, 0))
        })];
        assert_eq!(octilinear_watch(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {}), 1);
        assert_eq!(soft.len(), 3);
        let mut pts = vec![Point::new(0, 0)];
        for &id in &soft {
            let Shape::Capsule { a, b, .. } = board.item(id).unwrap().shape else { panic!() };
            assert!(octilinear_ways(a, b).is_none(), "off-45 piece left: {a:?} -> {b:?}");
            pts.push(b);
        }
        assert_eq!(doglegs(&pts, 75_000), 0);
        assert!(board::check(&board).is_empty());
        assert!(board::net_connectivity(&board, 1, true).airlines.is_empty());

        // Long 16 mm straight run with 1.0 mm vertical offset (diagonal 1.414 mm > DOGLEG):
        // blocking both knees and the 0.25/0.5/0.75 splits leaves the 1.01 mm length-picked gap.
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        pad_at(&mut board, 0, 0);
        pad_at(&mut board, 16_000_000, 1_000_000);
        for &(x, y) in &[(1_000_000, 1_000_000), (14_000_000, -250_000), (4_250_000, 500_000), (8_000_000, 500_000), (11_750_000, 500_000)] {
            block(&mut board, x, y, 150_000);
        }
        let mut soft = vec![board.insert(Item {
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(16_000_000, 1_000_000), r: 75_000 },
            ..seg(Point::new(0, 0), Point::new(0, 0))
        })];
        assert_eq!(octilinear_watch(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {}), 1);
        assert_eq!(soft.len(), 3);
        let mut pts = vec![Point::new(0, 0)];
        for &id in &soft {
            let Shape::Capsule { a, b, .. } = board.item(id).unwrap().shape else { panic!() };
            assert!(octilinear_ways(a, b).is_none(), "off-45 piece left: {a:?} -> {b:?}");
            pts.push(b);
        }
        assert_eq!(doglegs(&pts, 75_000), 0);
        assert!(board::check(&board).is_empty());
        assert!(board::net_connectivity(&board, 1, true).airlines.is_empty());
    }
}
