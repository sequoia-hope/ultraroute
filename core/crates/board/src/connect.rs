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
        // An island that must join its net: the point of it deepest in from its edge, on a
        // strip its middle, where a via or a track's end joins it.
        (Kind::Pour { .. }, crate::geom::Shape::Polygon(_)) if board.must_join(id) => ([deepest(&item.shape), z], 1),
        _ => ([z, z], 0),
    }
}

/// The point of a shape furthest in from its edge, to a twentieth of its box; the box's centre
/// when no point sampled lies inside.
pub fn deepest(shape: &crate::geom::Shape) -> Point {
    let b = shape.bbox();
    let (w, h) = ((b.max.x - b.min.x).max(1), (b.max.y - b.min.y).max(1));
    let mut best = (f64::NEG_INFINITY, Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2));
    const N: i64 = 20;
    for j in 0..=N {
        for i in 0..=N {
            let p = Point::new(b.min.x + w * i / N, b.min.y + h * j / N);
            if shape.contains(p) {
                let d = shape.depth(p);
                if d > best.0 {
                    best = (d, p);
                }
            }
        }
    }
    best.1
}

/// Whether copper joins its net's clusters: all of it with pours counted; without, all but pours
/// refilled round new copper, though a plane's pour still joins.
fn cluster_member(board: &Board, item: &crate::model::Item, with_pours: bool) -> bool {
    with_pours || !item.refilled() || board.is_plane(item)
}

/// How deep a round end must reach into other copper to join it, in nanometres: KiCad 9 joins
/// shapes that collide (two round pads 1 um into each other, `kicad_agreement`), and the router's
/// pad outlines are a hair larger than KiCad's (an oval's end polygonised outward, about 0.1 um on
/// a 0.25 mm pin), so an end that only grazes a pad by less than this is no joint to KiCad
/// (WeatherSpot's GND: a track end 0.251461 mm from an oval pad's end circle, radii summing to
/// 0.25146 mm, judged unconnected).
const MIN_OVERLAP: f64 = 500.0;

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
            gap == 0.0 || gap < reach - MIN_OVERLAP
        })
    };
    reaches(x, a, y) || reaches(y, b, x)
}

/// A joint narrower than this share of the narrower track's width is a neck, not a joint.
pub const FULL_JOINT: f64 = 0.98;

/// How wide the copper is where the round end at `p` (radius `r1`) of one track reaches the
/// track `q0`–`q1` (radius `r2`), in nm: full width when `p` lies on it or inside its straight
/// body, else the lens two round ends make or the chord a side's edge cuts, however KiCad counts
/// them joined. `None` when they do not overlap.
pub fn end_neck(p: Point, r1: f64, q0: Point, q1: Point, r2: f64) -> Option<f64> {
    let full = 2.0 * r1.min(r2);
    let d = crate::geom::point_segment(p, q0, q1);
    let at_end = p.distance(q0).min(p.distance(q1)) <= d + 0.5;
    if meets_squarely(p, r1, q0, q1, r2) || d + r1.min(r2) <= r1.max(r2) {
        return Some(full);
    }
    if d >= r1 + r2 {
        return None;
    }
    let half = if at_end {
        let along = (d * d + r1 * r1 - r2 * r2) / (2.0 * d);
        (r1 * r1 - along * along).max(0.0).sqrt()
    } else {
        (r1 * r1 - (d - r2) * (d - r2)).max(0.0).sqrt()
    };
    Some((2.0 * half).min(full))
}

/// The widest joint between two tracks of a net through either's round ends ([`end_neck`]).
/// `None` for anything not two tracks on a shared layer, or tracks that do not meet.
pub fn joint_neck(board: &Board, a: ItemId, b: ItemId) -> Option<f64> {
    let (Some(x), Some(y)) = (board.item(a), board.item(b)) else { return None };
    let (Kind::Track, Kind::Track) = (&x.kind, &y.kind) else { return None };
    let (Shape::Capsule { a: xa, b: xb, r: xr }, Shape::Capsule { a: ya, b: yb, r: yr }) = (&x.shape, &y.shape) else { return None };
    if x.layers & y.layers == 0 {
        return None;
    }
    let (xr, yr) = (*xr as f64, *yr as f64);
    [end_neck(*xa, xr, *ya, *yb, yr), end_neck(*xb, xr, *ya, *yb, yr), end_neck(*ya, yr, *xa, *xb, xr), end_neck(*yb, yr, *xa, *xb, xr)].into_iter().flatten().reduce(f64::max)
}

/// How near an end must come to another track's end or centre line to meet it there, in
/// nanometres: points the router computes, not grid points, are rounded to the nanometre.
const SQUARE: f64 = 1000.0;

/// Whether the round end at `p` (radius `r1`) meets the track `q0`–`q1` (radius `r2`)
/// squarely: on its end or its centre line, as a drawn joint does, inside its straight body,
/// which its own body then crosses whole, or wholly inside its round end. An end near the other's
/// end but not on it is joined, if at all, by whatever their round ends' overlap leaves, which
/// narrows as they turn. A via is a track with its two ends at one point.
pub fn meets_squarely(p: Point, r1: f64, q0: Point, q1: Point, r2: f64) -> bool {
    let d = crate::geom::point_segment(p, q0, q1);
    if d <= SQUARE || d + r1 <= r2 {
        return true;
    }
    let to_end = p.distance(q0).min(p.distance(q1));
    to_end > d + 0.5 && d <= r2
}

/// [`joined`], and as a drawn joint is: a track meets another track or a via squarely
/// ([`meets_squarely`]) at an end of one, and a pad with an end inside it; other copper as
/// KiCad joins it.
pub fn firmly_joined(board: &Board, a: ItemId, b: ItemId) -> bool {
    if !joined(board, a, b) {
        return false;
    }
    let (Some(x), Some(y)) = (board.item(a), board.item(b)) else { return false };
    let ends = |it: &crate::model::Item| match (&it.kind, &it.shape) {
        (Kind::Track, Shape::Capsule { a, b, r }) => Some(([*a, *b], *r as f64)),
        _ => None,
    };
    let meets = |track: &crate::model::Item, other: &crate::model::Item| {
        let Some((pts, r1)) = ends(track) else { return false };
        match (&other.kind, &other.shape) {
            (Kind::Track | Kind::Via, Shape::Capsule { a: q0, b: q1, r: r2 }) => pts.iter().any(|&p| meets_squarely(p, r1, *q0, *q1, *r2 as f64)),
            (Kind::Pad { .. }, shape) => pts.iter().any(|&p| shape.contains(p)),
            _ => true,
        }
    };
    match (ends(x).is_some(), ends(y).is_some()) {
        (true, true) => meets(x, y) || meets(y, x),
        (true, false) => meets(x, y),
        (false, true) => meets(y, x),
        (false, false) => true,
    }
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
        if matches!(item.kind, Kind::Pour { .. }) && !board.must_join(id) {
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
    // loose piece of track or a via still needs connecting; a pour island alone does not, unless
    // its zone keeps its islands.
    let mut by_net: BTreeMap<u32, BTreeMap<usize, Vec<(Point, ItemId)>>> = BTreeMap::new();
    if !multi_cluster_nets.is_empty() {
        for &id in &ids {
            let item = board.item(id).expect("listed above");
            if (matches!(item.kind, Kind::Pour { .. }) && !board.must_join(id)) || !multi_cluster_nets.contains(&item.net) {
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
