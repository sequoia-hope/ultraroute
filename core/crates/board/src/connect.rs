//! Which copper is joined: same-net items that touch on a shared layer are one cluster. A net
//! with several clusters is open; its airlines join the clusters most cheaply.

use std::collections::{BTreeMap, HashMap};

use crate::model::{Board, ItemId, Kind};
use crate::geom::{Point, Shape};

#[derive(Debug, Clone, PartialEq)]
pub struct Airline {
    pub net: u32,
    pub from: Point,
    pub to: Point,
    pub from_item: ItemId,
    pub to_item: ItemId,
}

#[derive(Debug, Clone)]
pub struct Connectivity {
    /// The cluster of every copper item.
    pub cluster: HashMap<ItemId, usize>,
    pub airlines: Vec<Airline>,
}

type Anchor = (Point, ItemId);

struct Sets(Vec<usize>);

impl Sets {
    fn find(&mut self, mut i: usize) -> usize {
        while self.0[i] != i {
            self.0[i] = self.0[self.0[i]];
            i = self.0[i];
        }
        i
    }

    fn join(&mut self, a: usize, b: usize) -> bool {
        let (a, b) = (self.find(a), self.find(b));
        if a == b {
            return false;
        }
        self.0[a] = b;
        true
    }
}

/// Points of an item a connection may start from: a pad or via centre, a track's ends; airlines
/// run between them.
pub fn anchors(board: &Board, id: ItemId) -> ([Point; 2], usize) {
    let z = Point::new(0, 0);
    let Some(item) = board.item(id) else { return ([z, z], 0) };
    match (&item.kind, &item.shape) {
        (_, crate::geom::Shape::Capsule { a, b, .. }) if a == b => ([*a, z], 1),
        (Kind::Track, crate::geom::Shape::Capsule { a, b, .. }) => ([*a, *b], 2),
        (Kind::Pad { .. }, crate::geom::Shape::Polygon(p)) => {
            let n = p.len().max(1) as i64;
            ([Point::new(p.iter().map(|q| q.x).sum::<i64>() / n, p.iter().map(|q| q.y).sum::<i64>() / n), z], 1)
        }
        _ => ([z, z], 0),
    }
}

/// Whether copper joins its net's clusters: all of it with pours counted; without, all but pours
/// refilled round new copper, though a plane's pour still joins.
fn cluster_member(board: &Board, item: &crate::model::Item, with_pours: bool) -> bool {
    with_pours || !item.refilled() || board.is_plane(item)
}

/// As KiCad joins them: at anchors. A track anchors at its round ends, a via or a round pad at
/// its whole disc, any other pad at its centre; two items join where an anchor of one overlaps
/// the other's copper, so copper that merely overlaps a pad's corner or crosses a track does not
/// connect. Overlap means more than touching: KiCad leaves two collinear pieces of track whose
/// round ends only touch (0.15 mm wide, their ends 0.15 mm apart) unconnected. Polygon pads, and
/// pours, join wherever they touch.
pub fn joined(board: &Board, a: ItemId, b: ItemId) -> bool {
    let (Some(x), Some(y)) = (board.item(a), board.item(b)) else { return false };
    let pour = |it: &crate::model::Item| matches!(it.kind, Kind::Pour { .. });
    let poly_pad = |it: &crate::model::Item| matches!(it.kind, Kind::Pad { .. }) && matches!(it.shape, Shape::Polygon(_));
    let pad = |it: &crate::model::Item| matches!(it.kind, Kind::Pad { .. });
    if (pour(x) && pad(y) && !board.pour_joins_pad(x, b)) || (pour(y) && pad(x) && !board.pour_joins_pad(y, a)) {
        return false;
    }
    if pour(x) || pour(y) || (poly_pad(x) && poly_pad(y)) {
        return x.shape.gap_within(&y.shape, 0.0) == 0.0;
    }
    let reaches = |from: &crate::model::Item, from_id: ItemId, into: &crate::model::Item| {
        let reach = match &from.shape {
            Shape::Capsule { r, .. } => *r as f64,
            Shape::Polygon(_) => 0.0,
        };
        let (pts, len) = anchors(board, from_id);
        pts[..len].iter().any(|p| {
            let dot = Shape::Capsule { a: *p, b: *p, r: 0 };
            let gap = into.shape.gap_within(&dot, reach);
            gap == 0.0 || gap < reach - 0.5
        })
    };
    reaches(x, a, y) || reaches(y, b, x)
}

pub fn connectivity(board: &Board) -> Connectivity {
    joined_copper(board, true, None)
}

/// Connectivity as it will stand whatever the pours are refilled to: poured copper joins nothing,
/// except a block's kept pours, which are not refilled, and planes, whose layer nothing else
/// may take.
pub fn connectivity_without_pours(board: &Board) -> Connectivity {
    joined_copper(board, false, None)
}

/// One net's part of [`connectivity`] (`with_pours`) or [`connectivity_without_pours`], found
/// without looking at the other nets' copper.
pub fn net_connectivity(board: &Board, net: u32, with_pours: bool) -> Connectivity {
    joined_copper(board, with_pours, Some(net))
}

fn joined_copper(board: &Board, with_pours: bool, only: Option<u32>) -> Connectivity {
    crate::work::count(&crate::work::CONNECTIVITY);
    let ids: Vec<ItemId> = board
        .items()
        .filter(|(_, it)| it.is_copper() && it.net != 0 && only.is_none_or(|n| it.net == n) && cluster_member(board, it, with_pours))
        .map(|(id, _)| id)
        .collect();
    let max = ids.iter().copied().max().map_or(0, |m| m + 1);
    let mut sets = Sets((0..max).collect());
    for &id in &ids {
        let item = board.item(id).expect("listed above");
        let bb = board.bbox(id).expect("listed above");
        for layer in (0..board.layers.len()).filter(|l| item.on(*l)) {
            for other in board.near(layer, bb) {
                if other <= id {
                    continue;
                }
                let Some(o) = board.item(other) else { continue };
                if o.net == item.net && o.is_copper() && o.on(layer) && cluster_member(board, o, with_pours) && sets.find(id) != sets.find(other) && joined(board, id, other) {
                    sets.join(id, other);
                }
            }
        }
    }
    let mut cluster: HashMap<ItemId, usize> = HashMap::with_capacity(ids.len());
    let mut net_first_cluster: HashMap<u32, usize> = HashMap::new();
    let mut multi_cluster_nets: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for &id in &ids {
        let c = sets.find(id);
        cluster.insert(id, c);
        let item = board.item(id).expect("listed above");
        if matches!(item.kind, Kind::Pour { .. }) {
            continue;
        }
        match net_first_cluster.entry(item.net) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(c);
            }
            std::collections::hash_map::Entry::Occupied(e) => {
                if *e.get() != c {
                    multi_cluster_nets.insert(item.net);
                }
            }
        }
    }

    // Per net, the clusters that must be joined and each cluster's anchor points. As in KiCad, a
    // loose piece of track or a via still needs connecting; a pour island alone does not.
    let mut by_net: BTreeMap<u32, BTreeMap<usize, Vec<(Point, ItemId)>>> = BTreeMap::new();
    if !multi_cluster_nets.is_empty() {
        for &id in &ids {
            let item = board.item(id).expect("listed above");
            if matches!(item.kind, Kind::Pour { .. }) || !multi_cluster_nets.contains(&item.net) {
                continue;
            }
            let (pts, len) = anchors(board, id);
            let entry = by_net.entry(item.net).or_default().entry(cluster[&id]).or_default();
            for &p in &pts[..len] {
                entry.push((p, id));
            }
        }
    }

    let mut airlines = Vec::new();
    let mut nets: Vec<u32> = by_net.keys().copied().collect();
    nets.sort_unstable();
    for net in nets {
        let clusters: Vec<&Vec<(Point, ItemId)>> = by_net[&net].values().filter(|p| !p.is_empty()).collect();
        if clusters.len() < 2 {
            continue;
        }
        // Prim's tree over the clusters, each edge the closest pair of anchors between two.
        let closest = |a: &Vec<(Point, ItemId)>, b: &Vec<(Point, ItemId)>| {
            let mut best = (f64::INFINITY, a[0], b[0]);
            for pa in a {
                for pb in b {
                    let d = pa.0.distance(pb.0);
                    if d < best.0 {
                        best = (d, *pa, *pb);
                    }
                }
            }
            best
        };
        let mut best_to: Vec<Option<(f64, Anchor, Anchor)>> = (0..clusters.len())
            .map(|j| (j > 0).then(|| closest(clusters[0], clusters[j])))
            .collect();
        for _ in 1..clusters.len() {
            let mut pick: Option<(f64, Anchor, Anchor, usize)> = None;
            for (j, slot) in best_to.iter().enumerate() {
                if let Some((d, pa, pb)) = slot {
                    if pick.as_ref().is_none_or(|bst| *d < bst.0) {
                        pick = Some((*d, *pa, *pb, j));
                    }
                }
            }
            let Some((_, pa, pb, j)) = pick else { break };
            best_to[j] = None;
            airlines.push(Airline { net, from: pa.0, to: pb.0, from_item: pa.1, to_item: pb.1 });
            for (k, slot) in best_to.iter_mut().enumerate() {
                if let Some(cur) = slot {
                    let cand = closest(clusters[j], clusters[k]);
                    if cand.0 < cur.0 {
                        *cur = cand;
                    }
                }
            }
        }
    }
    Connectivity { cluster, airlines }
}
