//! Pulling routed copper tight: each chain of the router's tracks between fixed points (pads,
//! vias, branches, copper it did not lay) is replaced, a run at a time, by the longest straight
//! track that keeps every rule. The ends stay where they are, so nothing is disconnected.

use std::collections::{HashMap, HashSet};

use board::{check_item, joined, Board, Item, ItemId, Kind, Point, Shape};

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
                !soft.contains(&other) && board.item(other).is_some_and(|o| o.is_copper() && o.shape.contains(p) && !matches!(o.kind, Kind::Pour { .. }))
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

fn length(points: &[Point]) -> f64 {
    points.windows(2).map(|w| w[0].distance(w[1])).sum::<f64>() * 1e-6
}

/// Pulls every chain of the router's tracks tight. `soft` holds the copper the router laid and
/// is updated with the tracks that replace it.
pub fn pull_tight(board: &mut Board, soft: &mut Vec<ItemId>) -> CleanStats {
    let soft_set: HashSet<ItemId> = soft.iter().copied().filter(|id| board.item(*id).is_some()).collect();
    let mut stats = CleanStats::default();
    let mut replaced: HashSet<ItemId> = HashSet::new();
    let mut added: Vec<ItemId> = Vec::new();
    for chain in chains(board, &soft_set) {
        stats.chains += 1;
        stats.segments_before += chain.tracks.len();
        stats.length_before += length(&chain.points);
        let mut kept_points = vec![chain.points[0]];
        let mut i = 0;
        let n = chain.points.len() - 1;
        while i < n {
            let mut j = n;
            let mut laid = None;
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
                    laid = Some(id);
                    break;
                }
                board.remove(id);
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
                }
                None => j = i + 1,
            }
            kept_points.push(chain.points[j]);
            i = j;
        }
        stats.segments_after += kept_points.len() - 1;
        stats.length_after += length(&kept_points);
    }
    soft.retain(|id| !replaced.contains(id) && board.item(*id).is_some());
    soft.extend(added);
    stats
}
