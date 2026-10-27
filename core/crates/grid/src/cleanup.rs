//! Pulling routed copper tight: each chain of the router's tracks between fixed points (pads,
//! vias, branches, copper it did not lay) is replaced, a run at a time, by the longest straight
//! track that keeps every rule. The ends stay where they are, so nothing is disconnected.

use std::collections::{HashMap, HashSet};

use board::{check_item, joined, Board, Item, ItemId, Kind, Nm, Point, Shape};
use kicad_pcb::Span;

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
struct Chain {
    points: Vec<Point>,
    tracks: Vec<ItemId>,
    layer: u64,
    net: u32,
    half_width: board::Nm,
}

fn ends(item: &Item) -> Option<(Point, Point, board::Nm)> {
    match (&item.kind, &item.shape) {
        (Kind::Track, Shape::Capsule { a, b, r }) => Some((*a, *b, *r)),
        _ => None,
    }
}

/// The router's tracks grouped into chains between fixed points.
fn chains(board: &Board, soft: &HashSet<ItemId>) -> Vec<Chain> {
    // Per (layer, net, width, point): the soft tracks ending there.
    let mut at: HashMap<(u64, u32, board::Nm, Point), Vec<ItemId>> = HashMap::new();
    for &id in soft {
        let Some(item) = board.item(id) else { continue };
        let Some((a, b, r)) = ends(item) else { continue };
        for p in [a, b] {
            at.entry((item.layers, item.net, r, p)).or_default().push(id);
        }
    }
    // A point is fixed unless exactly two of the chain's tracks meet there and nothing else does.
    let fixed = |board: &Board, key: &(u64, u32, board::Nm, Point)| -> bool {
        if at.get(key).map_or(0, Vec::len) != 2 {
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

/// Takes out the router's tracks lying exactly on another of the same net, layer and width, which
/// a reroute along copper its net already has can lay. Which one goes does not matter; the
/// router's own goes before the board file's. How many, and the file's spans among them.
pub fn drop_duplicates(board: &mut Board, soft: &mut Vec<ItemId>) -> (usize, Vec<Span>) {
    drop_duplicates_watch(board, soft, &mut |_, _| {})
}

/// As [`drop_duplicates`], showing `watch` each duplicate removed and the board after it goes.
pub fn drop_duplicates_watch(board: &mut Board, soft: &mut Vec<ItemId>, watch: &mut dyn FnMut(&Board, &Item)) -> (usize, Vec<Span>) {
    let soft_set: HashSet<ItemId> = soft.iter().copied().collect();
    let key = |item: &Item| match &item.shape {
        Shape::Capsule { a, b, r } if item.kind == Kind::Track => Some((item.net, item.layers, *r, (*a).min(*b), (*a).max(*b))),
        _ => None,
    };
    let mut seen: HashMap<(u32, u64, board::Nm, Point, Point), ItemId> = HashMap::new();
    // The file's tracks first, so a duplicate of one is the router's and goes.
    let mut ids: Vec<ItemId> = board.items().filter(|(_, it)| it.kind == Kind::Track).map(|(id, _)| id).collect();
    ids.sort_by_key(|id| (soft_set.contains(id), *id));
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
    let (mut pruned, mut spans) = (0, Vec::new());
    loop {
        let loose: Vec<ItemId> = soft.iter().copied().filter(|&id| dangles(board, id) || redundant(board, id)).collect();
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
                    }
                }
                continue;
            }
            if !bridges_nothing(board, id) {
                if let Some((span, new_ids)) = trim_overhang(board, id) {
                    spans.extend(span);
                    soft.extend(new_ids);
                    pruned += 1;
                }
                continue;
            }
            if let Some(mut item) = board.remove(id) {
                spans.extend(item.span.take());
                pruned += 1;
                watch(board, &item);
            }
        }
        soft.retain(|id| board.item(*id).is_some());
        if pruned == before {
            break;
        }
    }
    (pruned, spans)
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
        let end = Shape::Capsule { a: p, b: p, r };
        near.iter().any(|&o| {
            o != id
                && board.item(o).is_some_and(|it| {
                    it.net == item.net
                        && it.is_copper()
                        && (!it.refilled() || board.is_plane(it))
                        && it.on(layer)
                        && it.shape.gap_within(&end, 0.0) == 0.0
                        && (it.shape.contains(p) || point_gap(&it.shape, p) <= point_gap(&it.shape, q))
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
            // The joint already inside the track: cut back to it, with no piece across.
            if proj.distance(target) <= r as f64 {
                candidates.push(vec![(keep, proj)]);
            }
        }
    }
    if candidates.is_empty() {
        return None;
    }
    let removed = board.remove(id)?;
    for segs in candidates {
        let mut new_ids = Vec::with_capacity(segs.len());
        for (p, q) in segs {
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
pub(crate) fn neighbours(board: &Board, id: ItemId) -> Vec<ItemId> {
    let Some(item) = board.item(id) else { return Vec::new() };
    let bb = board.bbox(id).unwrap_or_else(|| item.shape.bbox());
    let mut out: Vec<ItemId> = (0..board.layers.len())
        .filter(|l| item.on(*l))
        .flat_map(|l| board.near(l, bb).into_iter().filter(move |&o| o != id))
        .filter(|&o| board.item(o).is_some_and(|it| it.net == item.net && it.is_copper() && (!it.refilled() || board.is_plane(it))) && joined(board, id, o))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Whether taking the item out leaves everything it joins still joined: true when it joins at
/// most one piece, or when a short search finds a way between all its neighbours without it.
/// Whatever KiCad calls a dangling end, copper that carries a connection stays.
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
        for next in neighbours(board, at) {
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
            let loose_end = [(*a, *b), (*b, *a)].iter().any(|&(p, q)| {
                let end = Shape::Capsule { a: p, b: p, r: *r };
                !near.iter().any(|&o| {
                    ours(layer, o).is_some_and(|o| {
                        o.shape.gap_within(&end, 0.0) == 0.0
                            && (o.shape.contains(p) || point_gap(&o.shape, p) <= point_gap(&o.shape, q))
                    })
                })
            });
            if loose_end {
                return true;
            }
            let neighbours: Vec<ItemId> = near.iter().copied().filter(|&o| ours(layer, o).is_some() && joined(board, id, o)).collect();
            near.iter().any(|&cover| {
                ours(layer, cover).is_some_and(|c| !matches!(c.kind, Kind::Pour { .. } | Kind::Via) && c.shape.contains(*a) && c.shape.contains(*b))
                    && neighbours.iter().all(|&o| o == cover || joined(board, cover, o))
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
/// is updated with the tracks that replace it.
pub fn pull_tight(board: &mut Board, soft: &mut Vec<ItemId>) -> CleanStats {
    pull_tight_watch(board, soft, &mut |_, _, _, _| {})
}

/// Sees a chain pulled tight: the board after, the net, the tracks replaced and the new ones.
pub type TightWatch<'a> = dyn FnMut(&Board, u32, &[Item], &[ItemId]) + 'a;

/// As [`pull_tight`], calling `watch` with the net, replaced tracks and new track ids whenever a
/// chain is shortened.
pub fn pull_tight_watch(board: &mut Board, soft: &mut Vec<ItemId>, watch: &mut TightWatch) -> CleanStats {
    let soft_set: HashSet<ItemId> = soft.iter().copied().filter(|id| board.item(*id).is_some()).collect();
    let mut stats = CleanStats::default();
    let mut replaced: HashSet<ItemId> = HashSet::new();
    let mut added: Vec<ItemId> = Vec::new();
    for chain in chains(board, &soft_set) {
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
                // Try the straight track from point i to point j in place of the tracks between.
                // Whatever joined those tracks must still join the straight one, or the chain's
                // tracks on either side of it.
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
                    taken_here = removed.into_iter().map(|(_, it)| it).collect();
                    laid = Some(id);
                    break;
                }
                board.discard(id);
                // Put back what was taken, under the ids the chain knows them by.
                for (old, item) in removed {
                    board.restore(old, item);
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
        let clean = pull_tight_watch(&mut board, &mut soft, &mut |_, net, old, new| {
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
}

