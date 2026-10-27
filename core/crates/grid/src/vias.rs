//! Eliminating unnecessary vias after routing.
//!
//! Grid negotiation and single-net finishes often take a pair of vias across an obstacle, or
//! switch layers into a through-hole pad, where a short detour on one layer fits once surrounding
//! copper has settled and been pulled tight. Pre-fanout can also lay a dogbone via for a fine-pitch
//! pad whose net ended up connecting to a neighbour on the same outer layer through a second via.
//!
//! `reduce_vias` lifts one soft via at a time, then pairs of soft vias joined by an unbranched
//! track run, and finally small multi-via soft tangles, trying to reconnect the net inside a
//! bounded local window with zero vias (or strictly fewer vias, within a wirelength trade cap).
//! Every replacement is checked against the board's exact rules, net connectivity, and non-soft
//! dangling copper before it is kept; anything else puts the lifted items back under their ids.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use board::{joined, Airline, BBox, Board, Item, ItemId, Kind, Nm, Point, Shape};
use kicad_pcb::Span;

use crate::cleanup::dangles;
use crate::grid::{ObstacleMap, Profile, RouteGrid};
use crate::report::{tell, Log};
use crate::route::{item_terminals, lay, terminals, Terminal};
use crate::search::{route_astar, via_cost_mm};

#[derive(Debug, Clone)]
pub struct ViaOptions {
    pub cell: Nm,
    /// Padding around lifted copper and its opened airlines for the local reroute window.
    pub window_margin: Nm,
    /// Most extra track length, in millimetres, accepted per eliminated via.
    pub max_extra_mm_per_via: f64,
    /// Largest bounding-box span of a connected soft cluster tried all at once.
    pub max_cluster_span: Nm,
    /// Most soft vias in a connected cluster tried all at once.
    pub max_cluster_vias: usize,
    /// Passes over the board before stopping; stops early as soon as a pass drops nothing.
    pub max_passes: usize,
    pub max_seconds: Option<f64>,
    pub pours_block: bool,
    pub cancel: Option<Arc<AtomicBool>>,
    pub log: Option<Log>,
}

impl Default for ViaOptions {
    fn default() -> Self {
        ViaOptions {
            cell: 50_000,
            window_margin: 6_000_000,
            max_extra_mm_per_via: 12.0,
            max_cluster_span: 45_000_000,
            max_cluster_vias: 8,
            max_passes: 3,
            max_seconds: Some(8.0),
            pours_block: false,
            cancel: None,
            log: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ViaStats {
    pub vias_before: usize,
    pub vias_after: usize,
    pub dropped: usize,
    pub tried: usize,
    pub kept: usize,
    pub removed_spans: Vec<Span>,
}

struct Sets(Vec<usize>);

impl Sets {
    fn find(&mut self, mut i: usize) -> usize {
        while self.0[i] != i {
            self.0[i] = self.0[self.0[i]];
            i = self.0[i];
        }
        i
    }

    fn join(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.0[a] = b;
        }
    }
}

fn item_anchors(item: &Item) -> ([Point; 2], usize) {
    let z = Point::new(0, 0);
    match (&item.kind, &item.shape) {
        (_, Shape::Capsule { a, b, .. }) if a == b => ([*a, z], 1),
        (Kind::Track, Shape::Capsule { a, b, .. }) => ([*a, *b], 2),
        (Kind::Pad { .. }, Shape::Polygon(p)) => {
            let n = p.len().max(1) as Nm;
            ([Point::new(p.iter().map(|q| q.x).sum::<Nm>() / n, p.iter().map(|q| q.y).sum::<Nm>() / n), z], 1)
        }
        _ => ([z, z], 0),
    }
}

fn cluster_member(board: &Board, item: &Item, pours_block: bool) -> bool {
    pours_block || !item.refilled() || board.is_plane(item)
}

/// Clusters and Prim airlines for a single `net`, matching `board::connectivity` /
/// `board::connectivity_without_pours` without scanning every other net on the board.
fn net_connectivity(board: &Board, net: u32, pours_block: bool) -> (HashMap<ItemId, usize>, Vec<Airline>) {
    let ids: Vec<ItemId> = board
        .items()
        .filter(|(_, it)| it.net == net && it.is_copper() && cluster_member(board, it, pours_block))
        .map(|(id, _)| id)
        .collect();
    let pos: HashMap<ItemId, usize> = ids.iter().copied().enumerate().map(|(k, id)| (id, k)).collect();
    let mut sets = Sets((0..ids.len()).collect());
    for (k, &id) in ids.iter().enumerate() {
        let Some(item) = board.item(id) else { continue };
        let bb = board.bbox(id).unwrap_or_else(|| item.shape.bbox());
        for layer in (0..board.layers.len()).filter(|l| item.on(*l)) {
            for other in board.near(layer, bb) {
                if other <= id {
                    continue;
                }
                let Some(&ok) = pos.get(&other) else { continue };
                let Some(o) = board.item(other) else { continue };
                if o.on(layer) && sets.find(k) != sets.find(ok) && joined(board, id, other) {
                    sets.join(k, ok);
                }
            }
        }
    }
    let mut cluster: HashMap<ItemId, usize> = HashMap::with_capacity(ids.len());
    let mut groups: BTreeMap<usize, Vec<(Point, ItemId)>> = BTreeMap::new();
    for (k, &id) in ids.iter().enumerate() {
        let root = sets.find(k);
        cluster.insert(id, root);
        let Some(item) = board.item(id) else { continue };
        if matches!(item.kind, Kind::Pour { .. }) {
            continue;
        }
        let (pts, len) = item_anchors(item);
        let entry = groups.entry(root).or_default();
        for &p in &pts[..len] {
            entry.push((p, id));
        }
    }
    let clusters: Vec<&Vec<(Point, ItemId)>> = groups.values().filter(|g| !g.is_empty()).collect();
    let mut airlines = Vec::new();
    if clusters.len() < 2 {
        return (cluster, airlines);
    }
    type Anchor = (Point, ItemId);
    let closest = |a: &Vec<Anchor>, b: &Vec<Anchor>| {
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
    let mut best_to: Vec<Option<(f64, Anchor, Anchor)>> = (0..clusters.len()).map(|j| (j > 0).then(|| closest(clusters[0], clusters[j]))).collect();
    for _ in 1..clusters.len() {
        let mut pick: Option<(f64, Anchor, Anchor, usize)> = None;
        for (j, slot) in best_to.iter().enumerate() {
            if let Some((d, pa, pb)) = slot {
                if pick.as_ref().is_none_or(|bst| *d < bst.0) {
                    pick = Some((*d, *pa, *pb, j));
                }
            }
        }
        let Some((_, (from, from_item), (to, to_item), chosen)) = pick else { break };
        airlines.push(Airline { net, from, to, from_item, to_item });
        best_to[chosen] = None;
        for (j, slot) in best_to.iter_mut().enumerate() {
            if let Some(cur) = slot {
                let cand = closest(clusters[chosen], clusters[j]);
                if cand.0 < cur.0 {
                    *cur = cand;
                }
            }
        }
    }
    (cluster, airlines)
}

fn net_neighbours(board: &Board, id: ItemId, pours_block: bool) -> Vec<ItemId> {
    let Some(item) = board.item(id) else { return Vec::new() };
    let bb = board.bbox(id).unwrap_or_else(|| item.shape.bbox());
    let mut out: Vec<ItemId> = (0..board.layers.len())
        .filter(|l| item.on(*l))
        .flat_map(|l| board.near(l, bb).into_iter().filter(move |&o| o != id))
        .filter(|&o| board.item(o).is_some_and(|it| it.net == item.net && it.is_copper() && cluster_member(board, it, pours_block)) && joined(board, id, o))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Walks an unbranched chain of soft tracks out of `start_via` starting along `first_track`,
/// stopping just before any pad, locked copper, branch junction, or second via. Returns the tracks
/// in the chain and, when the chain ended directly at another soft via, that via's id.
fn walk_chain(board: &Board, start_via: ItemId, first_track: ItemId, soft_set: &HashSet<ItemId>, pours_block: bool) -> (Vec<ItemId>, Option<ItemId>) {
    let mut chain = Vec::new();
    let mut prev = start_via;
    let mut cur = first_track;
    let mut seen = HashSet::from([start_via]);
    while seen.insert(cur) {
        chain.push(cur);
        let others: Vec<ItemId> = net_neighbours(board, cur, pours_block).into_iter().filter(|&x| x != prev).collect();
        if others.len() != 1 {
            return (chain, None);
        }
        let next = others[0];
        if !soft_set.contains(&next) {
            return (chain, None);
        }
        match board.item(next).map(|it| &it.kind) {
            Some(Kind::Track) => {
                prev = cur;
                cur = next;
            }
            Some(Kind::Via) => return (chain, Some(next)),
            _ => return (chain, None),
        }
    }
    (chain, None)
}

fn track_mm(item: &Item) -> f64 {
    match (&item.kind, &item.shape) {
        (Kind::Track, Shape::Capsule { a, b, .. }) => a.distance(*b) * 1e-6,
        _ => 0.0,
    }
}

/// Free cells on `root`'s cluster inside `win_bounds`, starting with `terminals` on `root` itself
/// (so fine-pitch pads keep necked-down escape legs) and adding nearby tracks and pads of the same
/// cluster so a single-layer detour can meet existing copper of its net anywhere inside the window.
fn cluster_terminals(board: &mut Board, grid: &RouteGrid, clusters: &HashMap<ItemId, usize>, root: ItemId, win_bounds: BBox) -> Vec<Terminal> {
    let mut by_cell: BTreeMap<usize, Option<(Point, Nm)>> = BTreeMap::new();
    for (cell, leg) in terminals(board, grid, root) {
        match by_cell.get(&cell) {
            Some(None) => {}
            Some(Some(_)) if leg.is_none() => {
                by_cell.insert(cell, None);
            }
            None => {
                by_cell.insert(cell, leg);
            }
            _ => {}
        }
    }
    let Some(&cid) = clusters.get(&root) else {
        return by_cell.into_iter().collect();
    };
    let origin = board.item(root).map_or(Point::new(0, 0), |it| {
        let b = it.shape.bbox();
        Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2)
    });
    let mut extras: Vec<(f64, ItemId)> = clusters
        .iter()
        .filter(|&(&id, &c)| c == cid && id != root)
        .filter_map(|(&id, _)| {
            let it = board.item(id)?;
            if !matches!(it.kind, Kind::Pad { .. } | Kind::Track | Kind::Via) {
                return None;
            }
            let bb = it.shape.bbox();
            if !bb.overlaps(win_bounds) {
                return None;
            }
            let mid = Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2);
            Some((mid.distance(origin), id))
        })
        .collect();
    extras.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (_, id) in extras.into_iter().take(16) {
        for (cell, leg) in item_terminals(board, grid, id, grid.half_width, 3, 12) {
            match by_cell.get(&cell) {
                Some(None) => {}
                Some(Some(_)) if leg.is_none() => {
                    by_cell.insert(cell, None);
                }
                None => {
                    by_cell.insert(cell, leg);
                }
                _ => {}
            }
        }
    }
    by_cell.into_iter().collect()
}

/// Attempts to replace `candidate` items on `net` with strictly fewer vias. Commits and updates
/// `soft_set` on success, or restores `board` to its exact prior item ids on failure.
#[allow(clippy::too_many_arguments)]
fn try_replace(
    board: &mut Board,
    net: u32,
    candidate: &[ItemId],
    fixed_map: &ObstacleMap,
    options: &ViaOptions,
    soft_set: &mut HashSet<ItemId>,
    spans: &mut Vec<Span>,
    watch: &mut dyn FnMut(&Board),
) -> Option<(usize, BBox)> {
    let mut lifted_ids: Vec<ItemId> = candidate.iter().copied().filter(|id| soft_set.contains(id) && board.item(*id).is_some()).collect();
    lifted_ids.sort_unstable();
    lifted_ids.dedup();
    let old_vias = lifted_ids.iter().filter(|&&id| board.item(id).is_some_and(|it| it.kind == Kind::Via)).count();
    if old_vias == 0 {
        return None;
    }
    let lifted_lookup: HashSet<ItemId> = lifted_ids.iter().copied().collect();
    let mut old_len = 0.0;
    let mut lifted_bbox: Option<BBox> = None;
    let mut touching: Vec<ItemId> = Vec::new();
    for &id in &lifted_ids {
        let Some(it) = board.item(id) else { continue };
        old_len += track_mm(it);
        let bb = it.shape.bbox();
        lifted_bbox = Some(lifted_bbox.map_or(bb, |b| b.union(bb)));
        for o in net_neighbours(board, id, options.pours_block) {
            if !lifted_lookup.contains(&o) && !dangles(board, o) {
                touching.push(o);
            }
        }
    }
    touching.sort_unstable();
    touching.dedup();
    let touched_before = lifted_bbox.unwrap_or_else(|| BBox::of([Point::new(0, 0)]));

    let open_before = net_connectivity(board, net, options.pours_block).1.len();
    let removed: Vec<(ItemId, Item)> = lifted_ids.iter().filter_map(|&id| Some((id, board.remove(id)?))).collect();
    let (init_clusters, init_airlines) = net_connectivity(board, net, options.pours_block);

    // If removing the via chain leaves no new open airlines and leaves neighbouring copper clean,
    // the lifted copper was a redundant loop and needs no replacement at all.
    if init_airlines.len() <= open_before && touching.iter().all(|&o| !dangles(board, o)) {
        for (old_id, mut old_it) in removed {
            soft_set.remove(&old_id);
            spans.extend(old_it.span.take());
        }
        watch(board);
        return Some((old_vias, touched_before));
    }

    let mut raw_box = touched_before;
    for a in &init_airlines {
        raw_box = raw_box.union(BBox::of([a.from, a.to]));
    }
    let base_map = fixed_map.subwindow(board, raw_box.grow(options.window_margin));
    let win_bounds = BBox {
        min: Point::new(base_map.x0, base_map.y0),
        max: Point::new(base_map.x0 + base_map.nx as Nm * base_map.h, base_map.y0 + base_map.ny as Nm * base_map.h),
    };
    let via_step_cost = via_cost_mm(options.cell, 5.0);

    let modes: &[(bool, usize)] = if old_vias >= 2 { &[(false, 0), (true, old_vias - 1)] } else { &[(false, 0)] };
    for (attempt_idx, &(allow_vias, max_new_vias)) in modes.iter().enumerate() {
        let mut map = if attempt_idx + 1 < modes.len() { base_map.clone() } else { base_map.clone() };
        let mut grid = map.view(net);
        if !allow_vias {
            grid.via = None;
        }
        let mut clusters = init_clusters.clone();
        let mut airlines = init_airlines.clone();
        let mut laid_now: Vec<ItemId> = Vec::new();
        let mut new_vias = 0usize;
        let mut new_len = 0.0f64;
        let max_steps = airlines.len() + 4;
        let mut steps = 0;
        let mut failed = false;

        while airlines.len() > open_before {
            steps += 1;
            if steps > max_steps {
                failed = true;
                break;
            }
            airlines.sort_by(|a, b| a.from.distance(a.to).total_cmp(&b.from.distance(b.to)));
            let airline = airlines[0].clone();
            let sources = cluster_terminals(board, &grid, &clusters, airline.from_item, win_bounds);
            let targets = cluster_terminals(board, &grid, &clusters, airline.to_item, win_bounds);
            if sources.is_empty() || targets.is_empty() {
                failed = true;
                break;
            }
            let src_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
            let dst_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
            let Some(path) = route_astar(&grid, &src_cells, &dst_cells, via_step_cost) else {
                failed = true;
                break;
            };
            let path_vias = path.windows(2).filter(|w| w[0].0 != w[1].0).count();
            if new_vias + path_vias > max_new_vias {
                failed = true;
                break;
            }
            let Some(items) = lay(board, &grid, &path, &sources, &targets) else {
                failed = true;
                break;
            };
            for &id in &items {
                if let Some(it) = board.item(id) {
                    if it.kind == Kind::Via {
                        new_vias += 1;
                    } else {
                        new_len += track_mm(it);
                    }
                }
            }
            laid_now.extend(items.iter().copied());
            if new_vias > max_new_vias || new_len > old_len + options.max_extra_mm_per_via * (old_vias as f64) {
                failed = true;
                break;
            }
            (clusters, airlines) = net_connectivity(board, net, options.pours_block);
            if airlines.len() > open_before {
                for &id in &items {
                    map.stamp(board, id);
                }
                map.view_into(net, &mut grid);
                if !allow_vias {
                    grid.via = None;
                }
            }
        }

        let saved = old_vias.saturating_sub(new_vias);
        let ok = !failed
            && airlines.len() <= open_before
            && saved >= 1
            && new_len <= old_len + options.max_extra_mm_per_via * (saved as f64)
            && touching.iter().all(|&o| !dangles(board, o));
        if ok {
            let mut dirty = touched_before;
            for &id in &laid_now {
                if let Some(it) = board.item(id) {
                    dirty = dirty.union(it.shape.bbox());
                }
            }
            for (old_id, mut old_it) in removed {
                soft_set.remove(&old_id);
                spans.extend(old_it.span.take());
            }
            soft_set.extend(laid_now);
            watch(board);
            return Some((saved, dirty));
        }
        for id in laid_now.into_iter().rev() {
            board.discard(id);
        }
    }

    for (old_id, old_it) in removed {
        board.restore(old_id, old_it);
    }
    None
}

/// Connected components of `net`'s soft items (tracks and vias), stopping at pads and locked copper.
fn soft_clusters(board: &Board, net_soft: &[ItemId], soft_set: &HashSet<ItemId>, pours_block: bool) -> Vec<Vec<ItemId>> {
    let allowed: HashSet<ItemId> = net_soft.iter().copied().filter(|id| soft_set.contains(id) && board.item(*id).is_some()).collect();
    let mut seen: HashSet<ItemId> = HashSet::new();
    let mut out = Vec::new();
    for &start in net_soft {
        if !allowed.contains(&start) || !seen.insert(start) {
            continue;
        }
        let mut comp = Vec::new();
        let mut queue = VecDeque::from([start]);
        while let Some(cur) = queue.pop_front() {
            comp.push(cur);
            for next in net_neighbours(board, cur, pours_block) {
                if allowed.contains(&next) && seen.insert(next) {
                    queue.push_back(next);
                }
            }
        }
        out.push(comp);
    }
    out
}

fn candidate_bbox(board: &Board, cand: &[ItemId]) -> Option<BBox> {
    cand.iter().filter_map(|&id| board.item(id).map(|it| it.shape.bbox())).reduce(BBox::union)
}

/// Drops redundant vias from `soft` by locally rerouting single vias, connected via pairs, and
/// small multi-via soft clusters onto single layers wherever every board rule holds.
pub fn reduce_vias(board: &mut Board, options: &ViaOptions, soft: &mut Vec<ItemId>, watch: &mut dyn FnMut(&Board)) -> ViaStats {
    reduce_vias_all(board, options, soft, &mut Vec::new(), watch)
}

/// Like [`reduce_vias`], also considering pre-fanout escapes in `fanout` whose nets are completely
/// routed (`0` open airlines) so outer-to-outer dogbone pairs can collapse onto the pad's surface
/// while needed escapes stay in `fanout` untouched by later tightening or length-matching passes.
pub fn reduce_vias_all(
    board: &mut Board,
    options: &ViaOptions,
    soft: &mut Vec<ItemId>,
    fanout: &mut Vec<ItemId>,
    watch: &mut dyn FnMut(&Board),
) -> ViaStats {
    let started = crate::clock::Instant::now();
    let out_of_time = || {
        options.max_seconds.is_some_and(|limit| started.elapsed().as_secs_f64() > limit)
            || options.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))
    };

    // Plane nets on a board with dedicated inner planes keep their direct stitching vias beside
    // decoupling and power pads instead of daisy-chaining return currents along outer tracks.
    let plane_nets: HashSet<u32> = if board.planes != 0 {
        board.items().filter(|(_, it)| board.is_plane(it) && it.net != 0).map(|(_, it)| it.net).collect()
    } else {
        HashSet::new()
    };

    let mut soft_set: HashSet<ItemId> = soft
        .iter()
        .copied()
        .filter(|&id| board.item(id).is_some_and(|it| !it.locked && it.net != 0 && matches!(it.kind, Kind::Track | Kind::Via)))
        .collect();
    let orig_fanout: HashSet<ItemId> = fanout
        .iter()
        .copied()
        .filter(|&id| !soft_set.contains(&id) && board.item(id).is_some_and(|it| !it.locked && it.net != 0 && matches!(it.kind, Kind::Track | Kind::Via)))
        .collect();
    if !orig_fanout.is_empty() {
        let mut fanout_nets: BTreeSet<u32> = BTreeSet::new();
        for &id in &orig_fanout {
            if let Some(it) = board.item(id) {
                if !plane_nets.contains(&it.net) {
                    fanout_nets.insert(it.net);
                }
            }
        }
        let closed_nets: HashSet<u32> = fanout_nets
            .into_iter()
            .filter(|&net| net_connectivity(board, net, options.pours_block).1.is_empty())
            .collect();
        for &id in &orig_fanout {
            if board.item(id).is_some_and(|it| closed_nets.contains(&it.net)) {
                soft_set.insert(id);
            }
        }
    }

    let count_vias = |board: &Board, set: &HashSet<ItemId>| {
        set.iter()
            .filter(|&&id| board.item(id).is_some_and(|it| it.kind == Kind::Via && !plane_nets.contains(&it.net)))
            .count()
    };

    let mut stats = ViaStats { vias_before: count_vias(board, &soft_set), ..ViaStats::default() };
    if stats.vias_before == 0 {
        return stats;
    }

    let mut fixed_maps: HashMap<String, ObstacleMap> = HashMap::new();
    let mut prev_dirty: Option<Vec<BBox>> = None;
    let mut prev_dirty_nets: HashSet<u32> = HashSet::new();

    for pass in 0..options.max_passes.max(1) {
        if out_of_time() {
            break;
        }
        let dropped_before_pass = stats.dropped;
        let mut next_dirty: Vec<BBox> = Vec::new();
        let mut next_dirty_nets: HashSet<u32> = HashSet::new();
        let mut by_net: BTreeMap<u32, Vec<ItemId>> = BTreeMap::new();
        for &id in &soft_set {
            if let Some(it) = board.item(id) {
                if !plane_nets.contains(&it.net) {
                    by_net.entry(it.net).or_default().push(id);
                }
            }
        }
        // Nets with fewer vias first: clearing simple one- and two-via detours opens wide via
        // halos on both layers before denser multi-via nets try their own local windows.
        let mut nets: Vec<(usize, u32)> = by_net
            .iter()
            .filter_map(|(&net, ids)| {
                let v = ids.iter().filter(|&&id| board.item(id).is_some_and(|it| it.kind == Kind::Via)).count();
                (v > 0).then_some((v, net))
            })
            .collect();
        nets.sort_unstable();

        let worth_retrying = |board: &Board, net: u32, cand: &[ItemId], prev: &Option<Vec<BBox>>, prev_nets: &HashSet<u32>| -> bool {
            let Some(boxes) = prev else { return true };
            if prev_nets.contains(&net) {
                return true;
            }
            let Some(bb) = candidate_bbox(board, cand) else { return false };
            let reach = bb.grow(options.window_margin + 1_000_000);
            boxes.iter().any(|d| d.overlaps(reach))
        };

        for (_, net) in nets {
            if out_of_time() {
                break;
            }
            let profile = Profile::for_net(board, net);
            let class = profile.class.clone();
            if !fixed_maps.contains_key(&class) {
                fixed_maps.insert(class.clone(), ObstacleMap::fixed_board(board, profile, options.cell, options.pours_block));
            }
            let fixed_map = &fixed_maps[&class];

            // Stage 1: connected soft via pairs `(u, v)` joined by an unbranched track chain
            // (single-layer hops and outer-to-outer dogbone pairs).
            let mut vias_now: Vec<ItemId> = soft_set
                .iter()
                .copied()
                .filter(|&id| board.item(id).is_some_and(|it| it.net == net && it.kind == Kind::Via))
                .collect();
            vias_now.sort_unstable();
            let mut tried_pairs: BTreeSet<(ItemId, ItemId)> = BTreeSet::new();
            for &v in &vias_now {
                if out_of_time() || !soft_set.contains(&v) || board.item(v).is_none() {
                    continue;
                }
                let nbrs = net_neighbours(board, v, options.pours_block);
                let mut chains_v: Vec<(Vec<ItemId>, Option<ItemId>)> = Vec::new();
                for &n in &nbrs {
                    if soft_set.contains(&n) && board.item(n).is_some_and(|it| it.kind == Kind::Track) {
                        chains_v.push(walk_chain(board, v, n, &soft_set, options.pours_block));
                    } else if soft_set.contains(&n) && board.item(n).is_some_and(|it| it.kind == Kind::Via) {
                        chains_v.push((Vec::new(), Some(n)));
                    }
                }
                let partners: Vec<ItemId> = chains_v.iter().filter_map(|(_, end)| *end).filter(|&u| u != v).collect();
                for u in partners {
                    let pair_key = (v.min(u), v.max(u));
                    if !tried_pairs.insert(pair_key) || !soft_set.contains(&v) || !soft_set.contains(&u) {
                        continue;
                    }
                    let mut cand = vec![v, u];
                    for (ch, _) in &chains_v {
                        cand.extend(ch.iter().copied());
                    }
                    for n in net_neighbours(board, u, options.pours_block) {
                        if soft_set.contains(&n) && board.item(n).is_some_and(|it| it.kind == Kind::Track) {
                            let (ch, _) = walk_chain(board, u, n, &soft_set, options.pours_block);
                            cand.extend(ch);
                        }
                    }
                    if !worth_retrying(board, net, &cand, &prev_dirty, &prev_dirty_nets) {
                        continue;
                    }
                    stats.tried += 1;
                    if let Some((saved, dirty)) = try_replace(board, net, &cand, fixed_map, options, &mut soft_set, &mut stats.removed_spans, watch) {
                        stats.kept += 1;
                        stats.dropped += saved;
                        next_dirty.push(dirty);
                        next_dirty_nets.insert(net);
                        tell(&options.log, "vias", || format!("pass {}: dropped {saved} via(s) on {}", pass + 1, board.net_name(net)), None);
                        break;
                    }
                }
            }

            // Stage 2: individual soft vias (layer switches into through-hole pads or branches).
            let mut single_vias: Vec<ItemId> = soft_set
                .iter()
                .copied()
                .filter(|&id| board.item(id).is_some_and(|it| it.net == net && it.kind == Kind::Via))
                .collect();
            single_vias.sort_unstable();
            for v in single_vias {
                if out_of_time() || !soft_set.contains(&v) || board.item(v).is_none() {
                    continue;
                }
                let mut cand = vec![v];
                for n in net_neighbours(board, v, options.pours_block) {
                    if soft_set.contains(&n) && board.item(n).is_some_and(|it| it.kind == Kind::Track) {
                        let (ch, _) = walk_chain(board, v, n, &soft_set, options.pours_block);
                        cand.extend(ch);
                    }
                }
                if !worth_retrying(board, net, &cand, &prev_dirty, &prev_dirty_nets) {
                    continue;
                }
                stats.tried += 1;
                if let Some((saved, dirty)) = try_replace(board, net, &cand, fixed_map, options, &mut soft_set, &mut stats.removed_spans, watch) {
                    stats.kept += 1;
                    stats.dropped += saved;
                    next_dirty.push(dirty);
                    next_dirty_nets.insert(net);
                    tell(&options.log, "vias", || format!("pass {}: dropped {saved} via on {}", pass + 1, board.net_name(net)), None);
                }
            }

            // Stage 3: whole connected soft components of bounded span with remaining vias (for
            // 3+ via zig-zags or branched local tangles that Stages 1 and 2 could not peel one
            // piece at a time).
            let mut net_items: Vec<ItemId> = soft_set
                .iter()
                .copied()
                .filter(|&id| board.item(id).is_some_and(|it| it.net == net))
                .collect();
            net_items.sort_unstable();
            for comp in soft_clusters(board, &net_items, &soft_set, options.pours_block) {
                if out_of_time() {
                    break;
                }
                let vcount = comp.iter().filter(|&&id| board.item(id).is_some_and(|it| it.kind == Kind::Via)).count();
                if vcount == 0 || vcount > options.max_cluster_vias {
                    continue;
                }
                let Some(bb) = candidate_bbox(board, &comp) else {
                    continue;
                };
                if (bb.max.x - bb.min.x).max(bb.max.y - bb.min.y) > options.max_cluster_span {
                    continue;
                }
                if !worth_retrying(board, net, &comp, &prev_dirty, &prev_dirty_nets) {
                    continue;
                }
                stats.tried += 1;
                if let Some((saved, dirty)) = try_replace(board, net, &comp, fixed_map, options, &mut soft_set, &mut stats.removed_spans, watch) {
                    stats.kept += 1;
                    stats.dropped += saved;
                    next_dirty.push(dirty);
                    next_dirty_nets.insert(net);
                    tell(&options.log, "vias", || format!("pass {}: dropped {saved} via(s) on {} cluster", pass + 1, board.net_name(net)), None);
                }
            }
        }

        if stats.dropped == dropped_before_pass {
            break;
        }
        prev_dirty = Some(next_dirty);
        prev_dirty_nets = next_dirty_nets;
    }

    // Surviving fanout escapes stay in `fanout`; new replacement tracks join `soft` (`laid`).
    fanout.retain(|id| board.item(*id).is_some());
    soft.retain(|id| board.item(*id).is_some_and(|it| it.locked || it.net == 0 || !matches!(it.kind, Kind::Track | Kind::Via)));
    let mut kept_soft: Vec<ItemId> = soft_set.into_iter().filter(|&id| !orig_fanout.contains(&id) && board.item(id).is_some()).collect();
    kept_soft.sort_unstable();
    soft.extend(kept_soft);
    stats.vias_after = stats.vias_before.saturating_sub(stats.dropped);
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn two_layer_board() -> Board {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal)) (net 0 \"\") (net 1 \"A\") (net 2 \"BLOCK\"))",
        )
        .unwrap();
        Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap())
    }

    #[test]
    fn net_connectivity_agrees_with_board_connectivity_on_earring() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pcb");
        let board = Board::load(&path).unwrap().0;
        for pours_block in [false, true] {
            let full = if pours_block { board::connectivity(&board) } else { board::connectivity_without_pours(&board) };
            for net in 1..board.nets.len() as u32 {
                let (_, mine) = net_connectivity(&board, net, pours_block);
                let expected = full.airlines.iter().filter(|a| a.net == net).count();
                assert_eq!(mine.len(), expected, "net {net} pours_block={pours_block}");
            }
        }
    }

    #[test]
    fn collapses_two_via_hop_around_an_obstacle_onto_one_layer() {
        let mut board = two_layer_board();
        for x in [0, 4_000_000] {
            board.insert(Item {
                kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
                net: 1,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(x, 0), b: Point::new(x, 0), r: 200_000 },
                hole: None,
                locked: true,
                span: None,
            });
        }
        // Short F.Cu obstacle blocking the straight y = 0 line, with room around y = +/- 1 mm.
        board.insert(Item {
            kind: Kind::Track,
            net: 2,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(2_000_000, -400_000), b: Point::new(2_000_000, 400_000), r: 100_000 },
            hole: None,
            locked: true,
            span: None,
        });
        let t1 = board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(1_000_000, 0), r: 100_000 },
            hole: None,
            locked: false,
            span: None,
        });
        let v1 = board.insert(Item {
            kind: Kind::Via,
            net: 1,
            layers: 3,
            shape: Shape::Capsule { a: Point::new(1_000_000, 0), b: Point::new(1_000_000, 0), r: 300_000 },
            hole: Some((Point::new(1_000_000, 0), 150_000)),
            locked: false,
            span: None,
        });
        let tb = board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 2,
            shape: Shape::Capsule { a: Point::new(1_000_000, 0), b: Point::new(3_000_000, 0), r: 100_000 },
            hole: None,
            locked: false,
            span: None,
        });
        let v2 = board.insert(Item {
            kind: Kind::Via,
            net: 1,
            layers: 3,
            shape: Shape::Capsule { a: Point::new(3_000_000, 0), b: Point::new(3_000_000, 0), r: 300_000 },
            hole: Some((Point::new(3_000_000, 0), 150_000)),
            locked: false,
            span: None,
        });
        let t2 = board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(3_000_000, 0), b: Point::new(4_000_000, 0), r: 100_000 },
            hole: None,
            locked: false,
            span: None,
        });
        let mut soft = vec![t1, v1, tb, v2, t2];
        let stats = reduce_vias(&mut board, &ViaOptions::default(), &mut soft, &mut |_| {});
        assert_eq!((stats.vias_before, stats.vias_after, stats.dropped), (2, 0, 2));
        assert!(board::connectivity_without_pours(&board).airlines.is_empty());
        assert_eq!(board.items().filter(|(_, it)| it.kind == Kind::Via).count(), 0);
        for &id in &soft {
            assert!(board::check_item(&board, id).is_empty());
        }
    }

    #[test]
    fn eliminates_single_via_between_through_hole_pads() {
        let mut board = two_layer_board();
        for x in [0, 3_000_000] {
            board.insert(Item {
                kind: Kind::Pad { footprint: 0, number: "1".into(), plated: true },
                net: 1,
                layers: 3,
                shape: Shape::Capsule { a: Point::new(x, 0), b: Point::new(x, 0), r: 400_000 },
                hole: Some((Point::new(x, 0), 200_000)),
                locked: true,
                span: None,
            });
        }
        let tf = board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(1_500_000, 0), r: 100_000 },
            hole: None,
            locked: false,
            span: None,
        });
        let v = board.insert(Item {
            kind: Kind::Via,
            net: 1,
            layers: 3,
            shape: Shape::Capsule { a: Point::new(1_500_000, 0), b: Point::new(1_500_000, 0), r: 300_000 },
            hole: Some((Point::new(1_500_000, 0), 150_000)),
            locked: false,
            span: None,
        });
        let tb = board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 2,
            shape: Shape::Capsule { a: Point::new(1_500_000, 0), b: Point::new(3_000_000, 0), r: 100_000 },
            hole: None,
            locked: false,
            span: None,
        });
        let mut soft = vec![tf, v, tb];
        let stats = reduce_vias(&mut board, &ViaOptions::default(), &mut soft, &mut |_| {});
        assert_eq!((stats.vias_before, stats.vias_after, stats.dropped), (1, 0, 1));
        assert!(board::connectivity_without_pours(&board).airlines.is_empty());
        assert_eq!(board.items().filter(|(_, it)| it.kind == Kind::Via).count(), 0);
    }

    #[test]
    fn never_leaves_a_locked_stub_dangling() {
        let mut board = two_layer_board();
        // Pad 1 on F.Cu has a locked B.Cu stub joined by a locked via? Simpler: Pad 1 is PTH at
        // (0, 0) with a locked B.Cu stub out to (0, 1.5 mm), and the router continued from that
        // stub's tip through a soft via to an F.Cu SMD pad at (2 mm, 0). Bypassing the stub on
        // F.Cu from (0, 0) to (2 mm, 0) would strand the locked B.Cu stub's tip at (0, 1.5 mm).
        board.insert(Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: true },
            net: 1,
            layers: 3,
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(0, 0), r: 300_000 },
            hole: Some((Point::new(0, 0), 150_000)),
            locked: true,
            span: None,
        });
        let locked_stub = board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 2,
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(0, 1_500_000), r: 100_000 },
            hole: None,
            locked: true,
            span: None,
        });
        board.insert(Item {
            kind: Kind::Pad { footprint: 0, number: "2".into(), plated: false },
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(2_000_000, 0), b: Point::new(2_000_000, 0), r: 200_000 },
            hole: None,
            locked: true,
            span: None,
        });
        let v = board.insert(Item {
            kind: Kind::Via,
            net: 1,
            layers: 3,
            shape: Shape::Capsule { a: Point::new(0, 1_500_000), b: Point::new(0, 1_500_000), r: 300_000 },
            hole: Some((Point::new(0, 1_500_000), 150_000)),
            locked: false,
            span: None,
        });
        let tf = board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(0, 1_500_000), b: Point::new(2_000_000, 0), r: 100_000 },
            hole: None,
            locked: false,
            span: None,
        });
        let mut soft = vec![v, tf];
        let stats = reduce_vias(&mut board, &ViaOptions::default(), &mut soft, &mut |_| {});
        assert_eq!(stats.dropped, 0, "must not bypass the locked B.Cu stub and leave its tip dangling");
        assert!(!dangles(&board, locked_stub));
    }

    #[test]
    fn collapses_closed_fanout_dogbone_pair_while_preserving_open_fanout_escape() {
        let mut board = two_layer_board();
        // Net 1 (closed): pad at (0, 0) escaped via a fanout dogbone to (1 mm, 0), routed on B.Cu
        // to (3 mm, 0) and back up a router via to a second F.Cu pad at (4 mm, 0).
        for (num, x) in [("1", 0), ("2", 4_000_000)] {
            board.insert(Item {
                kind: Kind::Pad { footprint: 0, number: num.into(), plated: false },
                net: 1,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(x, 0), b: Point::new(x, 0), r: 200_000 },
                hole: None,
                locked: true,
                span: None,
            });
        }
        let f_stub1 = board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(0, 0), b: Point::new(1_000_000, 0), r: 100_000 },
            hole: None,
            locked: false,
            span: None,
        });
        let f_via1 = board.insert(Item {
            kind: Kind::Via,
            net: 1,
            layers: 3,
            shape: Shape::Capsule { a: Point::new(1_000_000, 0), b: Point::new(1_000_000, 0), r: 300_000 },
            hole: Some((Point::new(1_000_000, 0), 150_000)),
            locked: false,
            span: None,
        });
        let r_inner = board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 2,
            shape: Shape::Capsule { a: Point::new(1_000_000, 0), b: Point::new(3_000_000, 0), r: 100_000 },
            hole: None,
            locked: false,
            span: None,
        });
        let r_via = board.insert(Item {
            kind: Kind::Via,
            net: 1,
            layers: 3,
            shape: Shape::Capsule { a: Point::new(3_000_000, 0), b: Point::new(3_000_000, 0), r: 300_000 },
            hole: Some((Point::new(3_000_000, 0), 150_000)),
            locked: false,
            span: None,
        });
        let r_outer = board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(3_000_000, 0), b: Point::new(4_000_000, 0), r: 100_000 },
            hole: None,
            locked: false,
            span: None,
        });

        // Net 2 (still open): pad at (0, 3 mm) with a fanout dogbone, destination at (4 mm, 3 mm) unrouted.
        for (num, x) in [("3", 0), ("4", 4_000_000)] {
            board.insert(Item {
                kind: Kind::Pad { footprint: 0, number: num.into(), plated: false },
                net: 2,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(x, 3_000_000), b: Point::new(x, 3_000_000), r: 200_000 },
                hole: None,
                locked: true,
                span: None,
            });
        }
        let f_stub2 = board.insert(Item {
            kind: Kind::Track,
            net: 2,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(0, 3_000_000), b: Point::new(1_000_000, 3_000_000), r: 100_000 },
            hole: None,
            locked: false,
            span: None,
        });
        let f_via2 = board.insert(Item {
            kind: Kind::Via,
            net: 2,
            layers: 3,
            shape: Shape::Capsule { a: Point::new(1_000_000, 3_000_000), b: Point::new(1_000_000, 3_000_000), r: 300_000 },
            hole: Some((Point::new(1_000_000, 3_000_000), 150_000)),
            locked: false,
            span: None,
        });

        let mut soft = vec![r_inner, r_via, r_outer];
        let mut fanout = vec![f_stub1, f_via1, f_stub2, f_via2];
        let stats = reduce_vias_all(&mut board, &ViaOptions::default(), &mut soft, &mut fanout, &mut |_| {});
        assert_eq!((stats.vias_before, stats.vias_after, stats.dropped), (2, 0, 2));
        assert_eq!(fanout, vec![f_stub2, f_via2], "open net's fanout escape stays intact in fanout");
        assert!(!soft.is_empty(), "closed net's zero-via surface track joins soft");
        assert_eq!(board.items().filter(|(_, it)| it.kind == Kind::Via).count(), 1);
    }
}
