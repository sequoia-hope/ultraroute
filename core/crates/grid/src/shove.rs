//! Shoving a trapped connection out: for an open connection with an end on a fine-pitch part,
//! the cheapest path to the rest of its net is found with other nets' routed copper passable at
//! a price, and that copper is shoved off the path, round after round, until the connection
//! routes.
//!
//! The region plough (`Plough::Region`) rips other nets' movable copper round where the path is
//! pinched and negotiates the trapped connection and the nets it ripped together; the rip plough
//! (`Plough::Rip`) lays the path outright, ripping exactly the copper its pieces would break a
//! rule against. Where neither lays it, copper crossing the path, and vias on it, are ripped, the
//! copper beside it is moved straight away from it, fading over the reach, pieces that meet at a
//! point moving together (`morph::shift_copper`), whatever that leaves breaking a rule is ripped,
//! and the trapped connection is routed first, through the room made for it. A round that still
//! leaves it trapped is followed by another along the new cheapest path, laid outright.
//! Connections the try knocked open can be ploughed out in turn (`cascade`), then everything
//! still open is routed again, and the whole try is kept only when it leaves fewer connections
//! open than it found.

use std::collections::{HashMap, HashSet};

use board::{connectivity, connectivity_without_pours, Airline, Board, Connectivity, Item, ItemId, Kind, Nm, Point};
use kicad_pcb::Span;

use crate::grid::{ObstacleMap, Profile, RouteGrid};
use crate::push::rip_broken;
use crate::report::{item_name, tell, Focus, Log};
use crate::route::{cluster_pads, ends_with_pads, lay, local_cluster, pieces, route_open, terminals, FieldSolver, Options, Terminal};
use crate::search::{GridPath, STRAIGHT};

#[derive(Debug, Clone)]
pub struct ShoveOptions {
    pub cell: Nm,
    pub via_cost: u32,
    /// Rounds of path and plough per connection before it is given up.
    pub rounds: usize,
    /// How the copper in the way moves.
    pub plough: Plough,
    /// How far from the path the plough still moves copper, in nanometres.
    pub reach: Nm,
    /// The reach for the connections of particular pads, and of any connection of particular
    /// nets, where it is not `reach`: the wider of a connection's pad's and its net's.
    pub reaches: HashMap<ItemId, Nm>,
    pub net_reaches: HashMap<u32, Nm>,
    /// Clearance kept beyond the rules, so the grid router's cell centres fit.
    pub margin: Nm,
    /// Cost of a path cell within other nets' movable copper's clearance, which the plough
    /// clears, and of one on the copper itself, which must be ripped for the path to cross.
    pub soft_cost: u32,
    pub cross_cost: u32,
    /// Connections a try knocked open that are ploughed out in turn before the try is judged.
    pub cascade: usize,
    /// Parts whose pads' open connections are tried; every open connection when `None`.
    pub parts: Option<HashSet<usize>>,
    /// Seconds for routing again after each try, and then for rip-up and reroute when that left
    /// more open than the try found, with the freed connection held still.
    pub reroute_seconds: f64,
    pub repair_seconds: f64,
    /// Shuffles the order the trapped connections are tried in; 0 keeps the board's order.
    pub seed: u64,
    /// Keep a try that frees its trapped connection and leaves no more open than it found, not
    /// only one that leaves fewer.
    pub accept_ties: bool,
    /// Passes after the first when the region plough renegotiates what it ripped, as
    /// [`crate::NegotiationOptions::repasses`].
    pub negotiate_repasses: usize,
    pub deadline: crate::clock::Deadline,
    pub pours_block: bool,
    pub log: Option<Log>,
}

impl Default for ShoveOptions {
    fn default() -> ShoveOptions {
        ShoveOptions {
            cell: 50_000,
            via_cost: 80,
            rounds: 4,
            plough: Plough::Region,
            reach: 2_000_000,
            reaches: HashMap::new(),
            net_reaches: HashMap::new(),
            margin: 50_000,
            soft_cost: 3 * STRAIGHT,
            cross_cost: 40 * STRAIGHT,
            cascade: 0,
            parts: None,
            reroute_seconds: 20.0,
            repair_seconds: 10.0,
            accept_ties: false,
            negotiate_repasses: 1,
            seed: 0,
            deadline: Default::default(),
            pours_block: false,
            log: None,
        }
    }
}

/// How the plough moves the copper in the way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plough {
    /// No plough: the path is laid outright from the first round, ripping exactly the copper its
    /// pieces would break a rule against.
    Rip,
    /// No plough: other nets' movable copper is ripped within the reach of where the path is
    /// pinched, and the trapped connection and every connection that opened negotiate their ways
    /// together, the trapped one favoured; when that leaves a ripped net worse off, it is tried
    /// again ripping round what still blocks it (see `renegotiate`). Later rounds lay the path
    /// outright, as `Rip`. On a board with planes, plane nets' connections are tried last and
    /// count after signal ones, and their copper is cheap to rip, joined to the planes again.
    Region,
}

/// One round of one try: the path ploughed along, and what it did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShoveRound {
    /// The connection's net: the trapped one's, or one a try knocked open and ploughed out after.
    pub net: u32,
    /// The path's cells as board layer and point; a layer change is a via.
    pub path: Vec<(usize, Point)>,
    /// Path cells inside other nets' movable copper.
    pub soft_cells: usize,
    /// Pieces ripped for crossing the path, moved by the plough, and ripped after for breaking
    /// a rule.
    pub crossed: usize,
    pub moved: usize,
    pub ripped: usize,
    /// Every piece ripped in the round, as it was.
    pub ripped_items: Vec<Item>,
    /// Whether the trapped connection routed at the end of the round.
    pub routed: bool,
}

/// One trapped connection's try.
#[derive(Debug, Clone, PartialEq)]
pub struct ShoveTry {
    pub net: u32,
    /// The pad on the part, and where it was trying to reach.
    pub pad: ItemId,
    pub to: Point,
    /// How far from the path the plough reached.
    pub reach: Nm,
    pub rounds: Vec<ShoveRound>,
    pub routed: bool,
    pub open_before: usize,
    pub open_after: usize,
    pub kept: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShoveStats {
    pub tries: Vec<ShoveTry>,
    /// Copper from the board file that moved or was ripped in a kept try, for removing from the
    /// file; what moved is in `soft` and is written back from there.
    pub removed_spans: Vec<Span>,
}

/// What `watch` is shown, with the board as it stands.
#[derive(Debug, Clone, PartialEq)]
pub enum ShoveEvent<'a> {
    /// A try begins, on the board as it was.
    Start(&'a ShoveTry),
    /// A round is done.
    Round(&'a ShoveTry, &'a ShoveRound),
    /// Everything opened was routed again; `kept` says whether the board stays like this.
    Done(&'a ShoveTry),
}

fn open_now(board: &Board, pours_block: bool) -> Vec<Airline> {
    if pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines
}

fn on_parts(board: &Board, id: ItemId, parts: &Option<HashSet<usize>>) -> bool {
    if parts.is_none() {
        return board.item(id).is_some_and(|it| matches!(it.kind, Kind::Pad { .. } | Kind::Track | Kind::Via));
    }
    let wanted = |f: usize| parts.as_ref().is_none_or(|p| p.contains(&f));
    match board.item(id).map(|it| &it.kind) {
        Some(Kind::Pad { footprint, .. }) => wanted(*footprint),
        Some(Kind::Track | Kind::Via) => {
            let cluster = local_cluster(board, id);
            let mut has_part = false;
            for m in cluster {
                if let Some(Kind::Pad { footprint, .. }) = board.item(m).map(|it| &it.kind) {
                    if wanted(*footprint) {
                        has_part = true;
                    } else {
                        return false;
                    }
                }
            }
            has_part
        }
        _ => false,
    }
}

/// Tries each open connection with an end on one of the parts, in turn, until none is left
/// untried. `soft` holds the router's copper; copper from the board file that moves joins it.
pub fn shove_escape(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &ShoveOptions,
    soft: &mut Vec<ItemId>,
    watch: &mut dyn FnMut(&Board, ShoveEvent),
) -> ShoveStats {
    let out_of_time = || options.deadline.expired();
    let region = options.plough == Plough::Region;
    let mut stats = ShoveStats::default();
    let mut cache = PassCache::of(board, options);
    if region {
        stats.removed_spans.extend(settle(board, soft, options.pours_block, &cache.plane_nets, None));
    }
    let mut tried: HashSet<ItemId> = HashSet::new();
    let (mut reaches, mut net_reaches) = (options.reaches.clone(), options.net_reaches.clone());
    // Ties the region plough kept since a try last freed anything, by net: past a few, nets only
    // swap a lane back and forth.
    let mut ties: HashMap<u32, usize> = HashMap::new();
    loop {
        if out_of_time() {
            break;
        }
        let mut open = open_now(board, options.pours_block);
        crate::jitter::shuffle(&mut open, options.seed.wrapping_add(stats.tries.len() as u64));
        // Plane nets last: `join_planes` makes most of their connections, not a plough.
        open.sort_by_key(|a| cache.plane_nets.contains(&a.net));
        let is_pad = |id: ItemId| board.item(id).is_some_and(|it| matches!(it.kind, Kind::Pad { .. }));
        let Some((pad, airline)) = open.iter().find_map(|a| {
            // A region try routes between its two ends alike, and starts from a pad where it can.
            let ends = if region && is_pad(a.to_item) && !is_pad(a.from_item) { [a.to_item, a.from_item] } else { [a.from_item, a.to_item] };
            ends.into_iter().find(|id| !tried.contains(id) && on_parts(board, *id, &options.parts)).map(|p| (p, a.clone()))
        }) else {
            break;
        };
        tried.insert(pad);
        let to = if pad == airline.from_item { airline.to } else { airline.from };
        let widened = reaches.get(&pad).into_iter().chain(net_reaches.get(&airline.net)).copied().max();
        let reach = widened.unwrap_or(options.reach);
        let options = &ShoveOptions { reach, ..options.clone() };
        let score_before = score(&open, &cache.plane_nets);
        let mut t = ShoveTry { net: airline.net, pad, to, reach, rounds: Vec::new(), routed: false, open_before: open.len(), open_after: 0, kept: false };
        tell(&options.log, "shoving", || format!("{}: {} is trapped; ploughing a way out", board.net_name(airline.net), item_name(board, pad)), Some(Focus::Connection { net: airline.net, from: airline.from, to: airline.to }));
        watch(board, ShoveEvent::Start(&t));
        let snapshot = (board.clone(), soft.clone());
        let mut spans = Vec::new();
        // Copper of the net that reaches no pad is no part of the rest of the net: a way out to
        // it would join nothing, so it goes first.
        spans.extend(rip_floating(board, soft, t.net, options.pours_block));
        let mut far = if pad == airline.from_item { airline.to_item } else { airline.from_item };
        let mut rounds = options.rounds.max(1);
        if region {
            match unzip(board, soft, &airline, pad, &open, options.pours_block, &mut spans) {
                Some(end) => (t.pad, far, t.to) = end,
                None => rounds = 0,
            }
        }
        for _ in 0..rounds {
            if out_of_time() {
                break;
            }
            let first = t.rounds.is_empty();
            let round = plough_round(board, solver, options, soft, &mut cache, t.pad, first, &out_of_time, &mut spans);
            let (done, walled) = (round.routed, round.path.is_empty());
            t.rounds.push(round);
            watch(board, ShoveEvent::Round(&t, t.rounds.last().unwrap()));
            if done || walled {
                break;
            }
        }
        t.routed = t.rounds.last().is_some_and(|r| r.routed);
        let changed = t.routed || !spans.is_empty() || soft.len() != snapshot.1.len() || t.rounds.iter().any(|r| !r.ripped_items.is_empty() || r.moved > 0);
        if t.routed {
            cascade(board, solver, options, soft, &mut cache, &mut t, &open, &snapshot.1, &mut spans, &out_of_time, watch);
        }
        if changed {
            let again = route_open(
                board,
                solver,
                &Options { cell: options.cell, via_cost: options.via_cost, deadline: options.deadline.seconds(options.reroute_seconds), nets: None, pours_block: options.pours_block, directional: false, seed: 0, log: None },
                &mut |_| {},
            );
            soft.extend(again.laid.iter().copied());
            if region {
                spans.extend(settle(board, soft, options.pours_block, &cache.plane_nets, None));
            }
            let worse = |board: &Board| score(&open_now(board, options.pours_block), &cache.plane_nets) >= score_before;
            if t.routed && options.repair_seconds > 0.0 && worse(board) {
                // Rip-up and reroute with the freed connection held still; for the region plough,
                // then again with it free, for two nets in one mouth that must swap lanes.
                let held = hold(board, soft, &snapshot.1, &|n| n == t.net);
                repair(board, solver, options, soft, if region { 0.55 } else { 1.0 });
                release(board, held);
                if region && worse(board) && !out_of_time() {
                    repair(board, solver, options, soft, 0.45);
                    t.routed = connection_of(board, t.pad, options.pours_block).is_none();
                }
                if region {
                    spans.extend(settle(board, soft, options.pours_block, &cache.plane_nets, None));
                }
            }
            let after = open_now(board, options.pours_block);
            let score_after = score(&after, &cache.plane_nets);
            t.open_after = after.len();
            let tie = options.accept_ties
                && t.routed
                && score_after == score_before
                && connection_of(board, t.pad, options.pours_block).is_none()
                && (!region || (ties.get(&t.net).copied().unwrap_or(0) < TIES_PER_NET && ties.values().sum::<usize>() < TIES));
            t.kept = score_after < score_before || tie;
            if region && score_after < score_before {
                // The board has changed: every connection may be tried again, at the narrowest reach.
                ties.clear();
                tried.clear();
                reaches.clear();
                net_reaches.clear();
            } else if region && tie {
                *ties.entry(t.net).or_default() += 1;
            }
        } else {
            t.open_after = t.open_before;
            t.kept = false;
        }
        tell(
            &options.log,
            "shoving",
            || format!("{}: {} rounds, {}; {} -> {} open, {}", board.net_name(t.net), t.rounds.len(), if t.routed { "routed" } else { "still trapped" }, t.open_before, t.open_after, if t.kept { "kept" } else { "undone" }),
            None,
        );
        watch(board, ShoveEvent::Done(&t));
        if t.kept {
            stats.removed_spans.extend(spans);
        } else {
            if changed {
                (*board, *soft) = snapshot;
            }
            if region {
                // The region plough routes between both ends alike: from the far end, on the
                // board as it was, it would do the same again.
                tried.extend([airline.from_item, airline.to_item, far]);
            }
        }
        stats.tries.push(t);
    }
    stats
}

/// Ties the region plough keeps in a row for one net, and for all, before it keeps none.
const TIES_PER_NET: usize = 2;
const TIES: usize = 6;

/// What a pass of tries keeps: the map of what never moves in a pass, per profile, the items
/// in it, and the plane nets, whose connections the region plough tries last and counts after
/// signal connections.
struct PassCache {
    fixed: HashSet<ItemId>,
    maps: Vec<ObstacleMap>,
    plane_nets: HashSet<u32>,
}

impl PassCache {
    fn of(board: &Board, options: &ShoveOptions) -> PassCache {
        let fixed = board.items().filter(|(_, it)| !matches!(it.kind, Kind::Track | Kind::Via) || it.locked).map(|(id, _)| id).collect();
        let plane_nets = if options.plough == Plough::Region && !options.pours_block && board.planes != 0 {
            board.items().filter(|(_, it)| board.is_plane(it) && it.net != 0).map(|(_, it)| it.net).collect()
        } else {
            HashSet::new()
        };
        PassCache { fixed, maps: Vec::new(), plane_nets }
    }

    /// What a path cannot cross: the fixed items, from the map kept for `profile`, and what has
    /// come to hold still since, which `movable` refuses (its own net's copper, copper held).
    fn walls(&mut self, board: &Board, profile: &Profile, options: &ShoveOptions, movable: &dyn Fn(&Item) -> bool) -> ObstacleMap {
        let fixed = &self.fixed;
        let idx = match self.maps.iter().position(|m| m.profile == *profile) {
            Some(i) => i,
            None => {
                self.maps.push(ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| !fixed.contains(&id)));
                self.maps.len() - 1
            }
        };
        let mut walls = self.maps[idx].clone();
        let still: Vec<ItemId> = board.items().filter(|(id, it)| !fixed.contains(id) && !movable(it)).map(|(id, _)| id).collect();
        for id in still {
            walls.stamp(board, id);
        }
        walls
    }
}

fn joined(board: &Board, pours_block: bool) -> Connectivity {
    if pours_block { connectivity(board) } else { connectivity_without_pours(board) }
}

/// Signal connections open, then all: what a try must leave fewer of.
fn score(open: &[Airline], plane_nets: &HashSet<u32>) -> (usize, usize) {
    (open.iter().filter(|a| !plane_nets.contains(&a.net)).count(), open.len())
}

/// Rip-up and reroute after a try, for `share` of the repair time.
fn repair(board: &mut Board, solver: &mut dyn FieldSolver, options: &ShoveOptions, soft: &mut Vec<ItemId>, share: f64) {
    crate::ripup::rip_up(
        board,
        solver,
        &crate::ripup::RipOptions {
            cell: options.cell,
            via_cost: options.via_cost,
            deadline: options.deadline.seconds(options.repair_seconds * share),
            pours_block: options.pours_block,
            region_repair: options.plough == Plough::Region,
            ..Default::default()
        },
        soft,
        &mut |_| {},
    );
}

/// Tidies up in the middle of a try, as the end of the run would: duplicate tracks go, and the
/// dangling stubs of the closed nets `only` names (all when `None`), while an open net keeps the
/// copper of a cut to join again; plane nets are joined to their planes again. The board file's
/// spans among what went.
fn settle(board: &mut Board, soft: &mut Vec<ItemId>, pours_block: bool, plane_nets: &HashSet<u32>, only: Option<&HashSet<u32>>) -> Vec<Span> {
    let (_, mut spans) = crate::cleanup::drop_duplicates(board, soft);
    let open: HashSet<u32> = open_now(board, pours_block).into_iter().map(|a| a.net).collect();
    let pick = |it: &Item| !it.locked && !open.contains(&it.net) && only.is_none_or(|o| o.contains(&it.net));
    spans.extend(crate::cleanup::prune_dangling_of(board, soft, &pick, &mut |_, _| {}).1);
    if !plane_nets.is_empty() {
        // A tidy-up finishes what it starts: the planes' joins are part of the board it leaves.
        soft.extend(crate::planes::join_planes(board, &None, &Default::default()).laid);
    }
    spans
}

/// Prunes the trapped net's stubs left dangling where an earlier try cut it: they would lead the
/// way out back across the net that cut it. The connection as it stands after, as `pad`, its far
/// end and that end's point: `pad`'s own if it is still open, else one the pruning opened, or one
/// with an end on either side, from a pad where it can; `None` when the net has none open.
fn unzip(board: &mut Board, soft: &mut Vec<ItemId>, airline: &Airline, pad: ItemId, open: &[Airline], pours_block: bool, spans: &mut Vec<Span>) -> Option<(ItemId, ItemId, Point)> {
    let ends = |a: &Airline, from: ItemId| if a.from_item == from { (a.from_item, a.to_item, a.to) } else { (a.to_item, a.from_item, a.from) };
    let (_, far, _) = ends(airline, pad);
    let net = airline.net;
    let pair = |a: &Airline| (a.from_item.min(a.to_item), a.from_item.max(a.to_item));
    let others: HashSet<(ItemId, ItemId)> = open.iter().filter(|a| a.net == net && pair(a) != pair(airline)).map(pair).collect();
    let sides: Vec<ItemId> = local_cluster(board, pad).into_iter().chain(local_cluster(board, far)).collect();
    let (pruned, gone) = crate::cleanup::prune_dangling_of(board, soft, &|it| it.net == net && !it.locked, &mut |_, _| {});
    spans.extend(gone);
    if pruned == 0 {
        return Some(ends(airline, pad));
    }
    let conn = joined(board, pours_block);
    if let Some(a) = conn.airlines.iter().find(|a| a.from_item == pad || a.to_item == pad) {
        return Some(ends(a, pad));
    }
    let near: HashSet<usize> = sides.iter().filter_map(|id| conn.cluster.get(id).copied()).collect();
    let mine = || conn.airlines.iter().filter(|a| a.net == net);
    let a = mine()
        .find(|a| !others.contains(&pair(a)))
        .or_else(|| mine().find(|a| [a.from_item, a.to_item].iter().any(|id| conn.cluster.get(id).is_some_and(|c| near.contains(c)))))?;
    let is_pad = |id: ItemId| board.item(id).is_some_and(|it| matches!(it.kind, Kind::Pad { .. }));
    Some(ends(a, if is_pad(a.to_item) && !is_pad(a.from_item) { a.to_item } else { a.from_item }))
}

/// Ploughs out, in turn, the connections the try knocked open, holding still the copper laid for
/// the connections already out, so the next plough cannot undo them.
#[allow(clippy::too_many_arguments)]
fn cascade(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &ShoveOptions,
    soft: &mut Vec<ItemId>,
    cache: &mut PassCache,
    t: &mut ShoveTry,
    open_before: &[Airline],
    soft_before: &[ItemId],
    spans: &mut Vec<Span>,
    out_of_time: &dyn Fn() -> bool,
    watch: &mut dyn FnMut(&Board, ShoveEvent),
) {
    let before: HashSet<(ItemId, ItemId)> = open_before.iter().map(|a| (a.from_item, a.to_item)).collect();
    let mut held = hold(board, soft, soft_before, &|n| n == t.net);
    let mut tried: HashSet<ItemId> = HashSet::new();
    for _ in 0..options.cascade {
        if out_of_time() {
            break;
        }
        let open = open_now(board, options.pours_block);
        let Some(next) = open.iter().find(|a| !before.contains(&(a.from_item, a.to_item)) && !tried.contains(&a.from_item)) else { break };
        let (end, net) = (next.from_item, next.net);
        tried.insert(end);
        for k in 0..options.rounds.max(1) {
            let round = plough_round(board, solver, options, soft, cache, end, k == 0, out_of_time, spans);
            let (done, walled) = (round.routed, round.path.is_empty());
            t.rounds.push(round);
            watch(board, ShoveEvent::Round(t, t.rounds.last().unwrap()));
            if done || walled || out_of_time() {
                break;
            }
        }
        held.extend(hold(board, soft, soft_before, &|n| n == net));
    }
    release(board, held);
}

/// Locks the copper of the nets `nets` picks laid since `soft_before`, so nothing after rips or
/// moves it: the pieces locked, for `release`.
fn hold(board: &mut Board, soft: &[ItemId], soft_before: &[ItemId], nets: &dyn Fn(u32) -> bool) -> Vec<ItemId> {
    let was: HashSet<ItemId> = soft_before.iter().copied().collect();
    let mut held = Vec::new();
    for &id in soft.iter().filter(|id| !was.contains(id)) {
        if board.item(id).is_some_and(|it| nets(it.net) && !it.locked) {
            if let Some(mut it) = board.remove(id) {
                it.locked = true;
                board.restore(id, it);
                held.push(id);
            }
        }
    }
    held
}

fn release(board: &mut Board, held: Vec<ItemId>) {
    for id in held {
        if let Some(mut it) = board.remove(id) {
            it.locked = false;
            board.restore(id, it);
        }
    }
}

/// Rips the unlocked tracks and vias of `net` in clusters that hold no pad and no kept pour:
/// the board file's spans among them.
fn rip_floating(board: &mut Board, soft: &mut Vec<ItemId>, net: u32, pours_block: bool) -> Vec<Span> {
    let c = if pours_block { connectivity(board) } else { connectivity_without_pours(board) };
    let mut anchored: HashSet<usize> = HashSet::new();
    let mut members: HashMap<usize, Vec<ItemId>> = HashMap::new();
    for (id, it) in board.items().filter(|(_, it)| it.net == net) {
        let Some(&k) = c.cluster.get(&id) else { continue };
        match it.kind {
            Kind::Pad { .. } | Kind::Pour { kept: true, .. } => {
                anchored.insert(k);
            }
            Kind::Track | Kind::Via => members.entry(k).or_default().push(id),
            _ => {}
        }
    }
    let mut spans = Vec::new();
    for (k, ids) in members {
        if anchored.contains(&k) || ids.iter().any(|id| board.item(*id).is_some_and(|it| it.locked)) {
            continue;
        }
        for id in ids {
            if let Some(it) = board.remove(id) {
                spans.extend(it.span);
            }
        }
    }
    soft.retain(|id| board.item(*id).is_some());
    spans
}

/// The open connection `pad` is an end of now, if any: its far end.
fn connection_of(board: &Board, pad: ItemId, pours_block: bool) -> Option<Airline> {
    open_now(board, pours_block).into_iter().find(|a| a.from_item == pad || a.to_item == pad)
}

/// One round: the path with other copper passable, routed for real if it is already clear,
/// otherwise the copper on it ripped or ploughed aside and the connection routed after.
/// After the first round, the path is laid outright, ripping the copper still in its way:
/// ploughing it again did not move it off.
#[allow(clippy::too_many_arguments)]
fn plough_round(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &ShoveOptions,
    soft: &mut Vec<ItemId>,
    cache: &mut PassCache,
    pad: ItemId,
    first: bool,
    out_of_time: &dyn Fn() -> bool,
    spans: &mut Vec<Span>,
) -> ShoveRound {
    let mut round = ShoveRound::default();
    let conn = joined(board, options.pours_block);
    let Some(airline) = conn.airlines.iter().find(|a| a.from_item == pad || a.to_item == pad).cloned() else {
        round.routed = true;
        return round;
    };
    let (far, net) = (if airline.from_item == pad { airline.to_item } else { airline.from_item }, airline.net);
    round.net = net;
    let movable = |it: &Item| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked && it.net != net;
    let profile = Profile::for_net(board, net);
    let Some(SoftPath { grid, soft: soft_grid, sources, targets, path, pinch }) = soft_path(board, solver, options, cache, &conn, net, pad, far) else {
        return round;
    };
    drop(conn);
    round.path = path.iter().map(|&(l, i, j)| (grid.layers[l], grid.centre(i, j))).collect();
    // A via needs more room than a trace: a via site counts when other copper is within its reach.
    let via_sites = path.windows(2).filter(|w| w[0].0 != w[1].0 && soft_grid.via_blocked[w[1].2 * grid.nx + w[1].1]).count();
    round.soft_cells = path.iter().filter(|&&(l, i, j)| soft_grid.blocked[grid.index(l, i, j)]).count() + via_sites;
    // Clear on the grid is not always clear: the region plough goes by the copper itself.
    if first && (round.soft_cells > 0 || !pinch.is_empty()) && options.plough == Plough::Region {
        renegotiate(board, solver, options, soft, cache, pad, far, net, &pinch, out_of_time, spans, &mut round);
        return round;
    }
    // Lay the path as it stands, ripping exactly the other copper it runs into.
    let laid = force_lay(board, &grid, &path, &sources, &targets, &movable, spans, &mut round.ripped_items);
    round.crossed = round.ripped_items.len();
    soft.retain(|id| board.item(*id).is_some());
    if let Some(items) = laid {
        soft.extend(items);
        round.routed = true;
    }
    if !round.ripped_items.is_empty() && !cache.plane_nets.is_empty() {
        soft.extend(crate::planes::join_planes(board, &None, &options.deadline).laid);
    }
    if round.routed {
        return round;
    }

    let corridor = Corridor::of(&round.path);
    // Copper crossing the path cannot be shoved off it sideways: it is ripped, to route again.
    let reach_of = |it: &Item| match it.shape {
        board::Shape::Capsule { r, .. } => r,
        _ => 0,
    };
    let crossing: Vec<ItemId> = board
        .items()
        .filter(|(_, it)| movable(it))
        .filter(|(_, it)| match (&it.kind, &it.shape) {
            // A via is shoved aside, unless it sits on the path itself.
            (Kind::Via, board::Shape::Capsule { a, .. }) => corridor.nearest(*a, None).is_none_or(|(d, _)| d < options.cell as f64),
            (Kind::Track, board::Shape::Capsule { a, b, .. }) => {
                let layer = it.layers.trailing_zeros() as usize;
                corridor.crosses(*a, *b, layer)
            }
            _ => false,
        })
        .map(|(id, _)| id)
        .collect();
    for id in crossing {
        if let Some(it) = board.remove(id) {
            spans.extend(it.span);
            round.ripped_items.push(it);
            round.crossed += 1;
        }
    }
    soft.retain(|id| board.item(*id).is_some());

    // How far each net's copper must clear the path: the path's half width (or its vias'), the
    // clearance between the nets, the net's widest piece, and the margin.
    let half = profile.half_width.max(profile.via_radius);
    let mut widest: HashMap<u32, Nm> = HashMap::new();
    for (_, it) in board.items().filter(|(_, it)| movable(it)) {
        let w = widest.entry(it.net).or_insert(0);
        *w = (*w).max(reach_of(it));
    }
    let clear: HashMap<u32, f64> = widest.iter().map(|(&n, &w)| (n, (half + board.clearance(net, n) + w + options.margin) as f64)).collect();
    let reach = options.reach as f64;
    let linear = |p: Point, n: u32, layer: Option<usize>| -> Option<(Point, bool)> {
        let r = *clear.get(&n)?;
        let w = reach.max(2.0 * r);
        let (d, (ux, uy)) = corridor.nearest(p, layer)?;
        if d >= w {
            return None;
        }
        let by = r * (1.0 - d / w);
        Some((Point::new((ux * by).round() as i64, (uy * by).round() as i64), false))
    };
    let (_, moved, moved_spans) = morph::shift_copper(board, soft, 150_000.0, &linear);
    round.moved = moved;
    spans.extend(moved_spans);
    let loose: Vec<(ItemId, Item)> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked).map(|(id, it)| (id, it.clone())).collect();
    let (ripped, ripped_spans) = rip_broken(board, soft, options.pours_block);
    round.ripped_items.extend(loose.into_iter().filter(|(id, _)| board.item(*id).is_none()).map(|(_, it)| it));
    round.ripped = ripped;
    spans.extend(ripped_spans);

    // The trapped connection first, before anything else takes the room made for it.
    round.routed = route_one(board, solver, options, soft, pad);
    round
}

/// The cheapest way from one end of a connection to the other with other nets' movable copper
/// passable at a price, and what it runs through.
struct SoftPath {
    grid: RouteGrid,
    /// The grid of other nets' movable copper alone (signal nets' only, on a board with planes).
    soft: RouteGrid,
    sources: Vec<Terminal>,
    targets: Vec<Terminal>,
    path: GridPath,
    /// For the region plough, where other nets' signal copper pinches the path: its cells and
    /// via sites in that copper's reach, and the ends and middle of each piece it would lay that
    /// breaks a rule against it, which the grid's cells can miss.
    pinch: Vec<Point>,
}

/// The way from `pad` to `far` with movable copper passable at a price; `None` when what cannot
/// move walls it in. For the region plough, an end crowded by copper about to be ripped is
/// found as if that copper were gone, and either end may also be a pad of its cluster near it.
#[allow(clippy::too_many_arguments)]
fn soft_path(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &ShoveOptions,
    cache: &mut PassCache,
    conn: &Connectivity,
    net: u32,
    pad: ItemId,
    far: ItemId,
) -> Option<SoftPath> {
    let region = options.plough == Plough::Region;
    let movable = |it: &Item| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked && it.net != net;
    let profile = Profile::for_net(board, net);
    let mut grid = cache.walls(board, &profile, options, &movable).view(net);
    grid.deadline = options.deadline.clone();
    let (sources, targets) = if region {
        let pads = cluster_pads(board, conn);
        let lift = |board: &mut Board, grid: &RouteGrid, id: ItemId| terminals_lifting(board, grid, id, &movable);
        let mut end = |id: ItemId| {
            let pads = conn.cluster.get(&id).and_then(|k| pads.get(k)).map_or(&[][..], Vec::as_slice);
            ends_with_pads(board, &grid, id, pads, &lift)
        };
        (end(pad), end(far))
    } else {
        (terminals(board, &grid, pad), terminals(board, &grid, far))
    };
    if sources.is_empty() || targets.is_empty() {
        return None;
    }
    let (src, tgt): (Vec<usize>, Vec<usize>) = (sources.iter().map(|t| t.0).collect(), targets.iter().map(|t| t.0).collect());
    solver.route(&grid, &src, &tgt, options.via_cost)?;
    let planes = &cache.plane_nets;
    let signal = |it: &Item| movable(it) && !planes.contains(&it.net);
    let soft = ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| !board.item(id).is_some_and(signal)).view(net);
    // A plane net's movable copper is mostly vias joining pads to the planes, which
    // `join_planes` puts back wherever there is room: cheap to clear out of the way.
    let plane = (!planes.is_empty()).then(|| {
        ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| !board.item(id).is_some_and(|it| movable(it) && planes.contains(&it.net))).view(net)
    });
    let cheap = (STRAIGHT / 2).max(1);
    let price = |soft: bool, plane: bool| if soft { options.soft_cost } else if plane { cheap } else { 0 };
    grid.penalty = (0..soft.blocked.len()).map(|n| price(soft.blocked[n], plane.as_ref().is_some_and(|g| g.blocked[n]))).collect();
    if region {
        // A via needs more room than a trace: it is priced where other copper is in its reach.
        grid.via_penalty.resize(soft.via_blocked.len(), 0);
        for (c, p) in grid.via_penalty.iter_mut().enumerate() {
            *p += price(soft.via_blocked[c], plane.as_ref().is_some_and(|g| g.via_blocked[c]));
        }
    }
    drop(plane);
    // On the copper itself, a path can only cross by ripping it.
    for (_, it) in board.items().filter(|(_, it)| movable(it)) {
        let board::Shape::Capsule { a, b, r } = it.shape else { continue };
        let cross = if planes.contains(&it.net) { STRAIGHT } else { options.cross_cost };
        let reach = (r + grid.half_width) as f64;
        let bb = board::BBox::of([a, b]).grow(r + grid.half_width);
        let top = Point::new(bb.max.x.min(grid.x0 + grid.h * (grid.nx as Nm - 1)), bb.max.y.min(grid.y0 + grid.h * (grid.ny as Nm - 1)));
        let (Some(lo), Some(hi)) = (grid.cell_at(Point::new(bb.min.x.max(grid.x0), bb.min.y.max(grid.y0))), grid.cell_at(top)) else { continue };
        for (k, &layer) in grid.layers.iter().enumerate() {
            if !it.on(layer) {
                continue;
            }
            for j in lo.1..=hi.1 {
                for i in lo.0..=hi.0 {
                    if board::point_segment(grid.centre(i, j), a, b) < reach {
                        let n = grid.index(k, i, j);
                        grid.penalty[n] = grid.penalty[n].max(cross);
                    }
                }
            }
        }
    }
    let path = solver.route(&grid, &src, &tgt, options.via_cost)?;
    let mut pinch = Vec::new();
    if region {
        pinch.extend(path.iter().filter(|&&(l, i, j)| soft.blocked[grid.index(l, i, j)]).map(|&(_, i, j)| grid.centre(i, j)));
        pinch.extend(path.windows(2).filter(|w| w[0].0 != w[1].0 && soft.via_blocked[w[1].2 * grid.nx + w[1].1]).map(|w| grid.centre(w[1].1, w[1].2)));
        let margin = profile.clearance.max(board::nm(board.rules.min_clearance)) + profile.via_radius;
        for piece in pieces(&grid, &path, &sources, &targets).unwrap_or_default() {
            let near = piece.shape.bbox().grow(margin);
            let breaks = |id: ItemId| {
                board.item(id).is_some_and(|it| signal(it) && piece.shape.gap(&it.shape) < profile.clearance.max(Profile::for_net(board, it.net).clearance) as f64)
            };
            if (0..board.layers.len()).filter(|l| piece.on(*l)).any(|l| board.near(l, near).into_iter().any(breaks)) {
                if let board::Shape::Capsule { a, b, .. } = piece.shape {
                    pinch.extend([a, b, Point::new((a.x + b.x) / 2, (a.y + b.y) / 2)]);
                }
            }
        }
    }
    Some(SoftPath { grid, soft, sources, targets, path, pinch })
}

/// The terminals of `end`; where it has none, those it has with the movable copper round it
/// lifted for the moment. A fine-pitch pad's mouth crowded by a neighbour's track the plough is
/// about to rip would otherwise refuse every leg out of it.
fn terminals_lifting(board: &mut Board, grid: &RouteGrid, end: ItemId, movable: &dyn Fn(&Item) -> bool) -> Vec<Terminal> {
    let found = terminals(board, grid, end);
    let Some(bb) = board.bbox(end).filter(|_| found.is_empty()) else { return found };
    let around = bb.grow((grid.h * 12).max(450_000));
    let near: HashSet<ItemId> = (0..board.layers.len()).flat_map(|l| board.near(l, around)).filter(|id| board.item(*id).is_some_and(movable)).collect();
    if near.is_empty() {
        return found;
    }
    let lifted: Vec<(ItemId, Item)> = near.into_iter().filter_map(|id| Some((id, board.remove(id)?))).collect();
    let found = terminals(board, grid, end);
    for (id, it) in lifted {
        board.restore(id, it);
    }
    found
}

/// Rips `ids`, and whatever of their nets is left reaching no pad: the nets ripped.
fn rip_near(board: &mut Board, ids: Vec<ItemId>, pours_block: bool, soft: &mut Vec<ItemId>, spans: &mut Vec<Span>, round: &mut ShoveRound) -> HashSet<u32> {
    let mut nets = HashSet::new();
    for id in ids {
        if let Some(it) = board.remove(id) {
            nets.insert(it.net);
            spans.extend(it.span);
            round.ripped_items.push(it);
        }
    }
    for &n in &nets {
        let had: Vec<(ItemId, Item)> = board.items().filter(|(_, it)| it.net == n).map(|(id, it)| (id, it.clone())).collect();
        spans.extend(rip_floating(board, soft, n, pours_block));
        round.ripped_items.extend(had.into_iter().filter(|(id, _)| board.item(*id).is_none()).map(|(_, it)| it));
    }
    soft.retain(|id| board.item(*id).is_some());
    round.crossed = round.ripped_items.len();
    nets
}

/// How far round the pinch points the region plough's first stage rips at most, however wide
/// the reach has grown, and how near a pinch the path's cells must come to be ripped round.
const STAGE_ONE_REACH: Nm = 2_200_000;
const PINCH_WINDOW: Nm = 450_000;
/// The second stage: at most so many nets negotiated, so many connections it opened traced
/// for their blockers, and so many nets repacked when nothing more is found to rip.
const STAGE_TWO_NETS: usize = 12;
const DISPLACED: usize = 3;
const REPACK_NETS: usize = 8;

/// The region plough. Other nets' movable copper within the reach (at most `STAGE_ONE_REACH`)
/// of the stretches of the path near where it is pinched is ripped, with whatever of those nets
/// it leaves reaching no pad: round a pinch, not along clear stretches of a long path, which
/// would scramble buses that were never in the way. The trapped net and every signal net ripped
/// are negotiated together, the trapped net's congestion priced at half; what is still open of
/// them is routed, closed nets' stubs are pruned and plane nets joined to the planes again.
///
/// When that leaves the nets ripped with more open than they had, or the trapped connection
/// open, a second stage starts again from the rip, and also rips round the cells where the
/// first stage's routes kept colliding and round whatever blocks the connections it opened (or,
/// finding none, just negotiates a few nets again as equals). It is kept when better.
#[allow(clippy::too_many_arguments)]
fn renegotiate(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &ShoveOptions,
    soft: &mut Vec<ItemId>,
    cache: &mut PassCache,
    pad: ItemId,
    far: ItemId,
    net: u32,
    pinch: &[Point],
    out_of_time: &dyn Fn() -> bool,
    spans: &mut Vec<Span>,
    round: &mut ShoveRound,
) {
    let plane_nets = cache.plane_nets.clone();
    let signal = |n: u32| !plane_nets.contains(&n);
    let pair = |a: &Airline| (a.from_item.min(a.to_item), a.from_item.max(a.to_item));
    let open_at_start = open_now(board, options.pours_block);
    let pairs_at_start: HashSet<(ItemId, ItemId)> = open_at_start.iter().map(pair).collect();

    // The path from anchor to anchor, cut into the stretches near a pinch.
    let mut path = Vec::with_capacity(round.path.len() + 2);
    if let (Some(&(l, p)), Some(it)) = (round.path.first(), board.item(pad)) {
        path.push((l, crate::route::anchor(it, p)));
    }
    path.extend_from_slice(&round.path);
    if let (Some(&(l, p)), Some(it)) = (round.path.last(), board.item(far)) {
        path.push((l, crate::route::anchor(it, p)));
    }
    let reach = options.reach.min(STAGE_ONE_REACH);
    let window = (reach + PINCH_WINDOW) as f64;
    let mut stretches: Vec<Corridor> = Vec::new();
    for near in path.chunk_by(|a, b| pinch.iter().any(|q| a.1.distance(*q) <= window) == pinch.iter().any(|q| b.1.distance(*q) <= window)) {
        if pinch.iter().any(|q| near[0].1.distance(*q) <= window) {
            stretches.push(Corridor::of(near));
        }
    }
    if stretches.is_empty() {
        stretches.push(Corridor::of(&path));
    }
    let movable = |it: &Item| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked && it.net != net;
    let reach = reach as f64;
    let near: Vec<ItemId> = board
        .items()
        .filter(|(_, it)| movable(it))
        .filter(|(_, it)| match (&it.kind, &it.shape) {
            (Kind::Via, board::Shape::Capsule { a, .. }) => {
                stretches.iter().any(|c| c.distance(*a, *a, None) < reach) || pinch.iter().any(|q| q.distance(*a) < reach)
            }
            (Kind::Track, board::Shape::Capsule { a, b, .. }) => {
                let layer = Some(it.layers.trailing_zeros() as usize);
                stretches.iter().any(|c| c.distance(*a, *b, layer) < reach) || pinch.iter().any(|q| board::point_segment(*q, *a, *b) < reach)
            }
            _ => false,
        })
        .map(|(id, _)| id)
        .collect();
    let ripped = rip_near(board, near, options.pours_block, soft, spans, round);
    let mut nets: HashSet<u32> = ripped.iter().copied().filter(|&n| signal(n)).collect();
    nets.insert(net);
    tell(&options.log, "shoving", || format!("{}: {} pieces of {} nets ripped round the way out; negotiating them again", board.net_name(net), round.crossed, ripped.len()), None);

    let ripped_state = (board.clone(), soft.clone(), spans.clone(), round.clone());
    // The cells shared by most routes, in the negotiation's least-colliding round.
    let mut hotspots: Vec<Point> = Vec::new();
    let mut fewest = usize::MAX;
    let mut watch_round = |view: crate::negotiate::RoundView| {
        if view.colliding == 0 || view.colliding > fewest {
            return;
        }
        fewest = view.colliding;
        hotspots.clear();
        let (nx, cells) = (view.grid.nx, view.grid.nx * view.grid.ny);
        for c in 0..cells {
            if (0..view.grid.layers.len()).any(|l| view.overlap[l * cells + c] >= 2) {
                hotspots.push(view.grid.centre(c % nx, c / nx));
            }
        }
    };
    negotiate(board, solver, options, soft, &nets, HashMap::from([(net, 0.5)]), options.seed, Some(&mut watch_round), &plane_nets, spans);
    let open = open_now(board, options.pours_block);
    let open_ripped = |open: &[Airline]| open.iter().filter(|a| nets.contains(&a.net) && signal(a.net)).count();
    let trapped = |board: &Board| connection_of(board, pad, options.pours_block).is_some();
    let first_score = (trapped(board), score(&open, &plane_nets));
    if !(open_ripped(&open) >= open_ripped(&open_at_start) || first_score.0) || nets.len() > STAGE_TWO_NETS || out_of_time() {
        round.routed = !first_score.0;
        return;
    }

    // The connections the first stage left open that it opened, or that are the trapped one's:
    // the copper still in their way, outside what was ripped.
    let ends_here = |a: &Airline| [a.from_item, a.to_item].iter().any(|id| *id == pad || *id == far);
    let mut displaced: Vec<Airline> =
        open.into_iter().filter(|a| nets.contains(&a.net) && signal(a.net) && (!pairs_at_start.contains(&pair(a)) || ends_here(a))).collect();
    displaced.sort_by_key(|a| !ends_here(a));
    displaced.truncate(DISPLACED);
    let mut blockers: Vec<Point> = Vec::new();
    if !displaced.is_empty() {
        // With the first stage's routes held still, then, where they fill the only way out, free.
        for hold_first in [true, false] {
            let held = if hold_first { hold(board, soft, &ripped_state.1, &|n| signal(n)) } else { Vec::new() };
            for a in &displaced {
                if out_of_time() {
                    break;
                }
                let conn = joined(board, options.pours_block);
                if let Some(found) = soft_path(board, solver, options, cache, &conn, a.net, a.from_item, a.to_item) {
                    blockers.extend(found.pinch);
                }
            }
            release(board, held);
            if !blockers.is_empty() {
                break;
            }
        }
    }
    let repack = !displaced.is_empty() && nets.len() <= REPACK_NETS;
    if hotspots.is_empty() && blockers.is_empty() && !repack {
        round.routed = !first_score.0;
        return;
    }
    let first_state = (board.clone(), soft.clone(), spans.clone(), round.clone());
    (*board, *soft, *spans, *round) = ripped_state;
    let (by_hotspot, by_blocker) = ((options.cell * 12).max(400_000) as f64, options.reach.clamp(450_000, 1_200_000) as f64);
    let more: Vec<ItemId> = board
        .items()
        .filter(|(_, it)| movable(it))
        .filter(|(_, it)| match (&it.kind, &it.shape) {
            (Kind::Via, board::Shape::Capsule { a, .. }) => {
                hotspots.iter().any(|p| p.distance(*a) < by_hotspot) || blockers.iter().any(|p| p.distance(*a) < by_blocker)
            }
            (Kind::Track, board::Shape::Capsule { a, b, .. }) => {
                hotspots.iter().any(|p| board::point_segment(*p, *a, *b) < by_hotspot) || blockers.iter().any(|p| board::point_segment(*p, *a, *b) < by_blocker)
            }
            _ => false,
        })
        .map(|(id, _)| id)
        .collect();
    let widened = !more.is_empty();
    if !widened && !repack {
        (*board, *soft, *spans, *round) = first_state;
        round.routed = !first_score.0;
        return;
    }
    nets.extend(rip_near(board, more, options.pours_block, soft, spans, round).into_iter().filter(|&n| signal(n)));
    // Ripped wider, the trapped net is still favoured, less; repacked, all are equals.
    let weights = if widened { HashMap::from([(net, 0.7)]) } else { HashMap::new() };
    negotiate(board, solver, options, soft, &nets, weights, options.seed.wrapping_add(31), None, &plane_nets, spans);
    let second_score = (trapped(board), score(&open_now(board, options.pours_block), &plane_nets));
    if second_score >= first_score {
        (*board, *soft, *spans, *round) = first_state;
    }
    round.routed = !trapped(board);
}

/// The region plough's negotiation of `nets`, then routing what is still open of them, pruning
/// closed nets' stubs and joining plane nets to the planes again.
#[allow(clippy::too_many_arguments)]
fn negotiate(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &ShoveOptions,
    soft: &mut Vec<ItemId>,
    nets: &HashSet<u32>,
    net_weight: HashMap<u32, f64>,
    seed: u64,
    on_round: Option<&mut dyn FnMut(crate::negotiate::RoundView)>,
    plane_nets: &HashSet<u32>,
    spans: &mut Vec<Span>,
) {
    let n = crate::negotiate::route_negotiated_rounds(
        board,
        solver,
        &crate::negotiate::NegotiationOptions {
            cell: options.cell,
            via_cost: options.via_cost,
            deadline: options.deadline.seconds(options.reroute_seconds),
            nets: Some(nets.clone()),
            net_weight,
            pours_block: options.pours_block,
            repasses: options.negotiate_repasses,
            rip_blocked: false,
            pad_ends: true,
            directional: false,
            seed,
            ..Default::default()
        },
        &mut |_| {},
        &mut |_, _| {},
        on_round,
    );
    soft.extend(n.laid);
    let again = route_open(
        board,
        solver,
        &Options {
            cell: options.cell,
            via_cost: options.via_cost,
            deadline: options.deadline.seconds(options.reroute_seconds.min(5.0)),
            nets: Some(nets.clone()),
            pours_block: options.pours_block,
            directional: false,
            seed,
            log: None,
        },
        &mut |_| {},
    );
    soft.extend(again.laid);
    spans.extend(settle(board, soft, options.pours_block, plane_nets, Some(nets)));
}

/// Lays `path` as tracks and vias after ripping the other nets' movable copper the pieces would
/// break a rule against; nothing is ripped, and nothing laid, when they would break one against
/// anything else. What was ripped goes into `ripped`.
#[allow(clippy::too_many_arguments)]
fn force_lay(
    board: &mut Board,
    grid: &crate::grid::RouteGrid,
    path: &crate::search::GridPath,
    sources: &[crate::route::Terminal],
    targets: &[crate::route::Terminal],
    movable: &dyn Fn(&Item) -> bool,
    spans: &mut Vec<Span>,
    ripped: &mut Vec<Item>,
) -> Option<Vec<ItemId>> {
    let mut blockers: Vec<ItemId> = Vec::new();
    for piece in pieces(grid, path, sources, targets)? {
        let id = board.insert(piece);
        let found = grid.violations(board, id);
        board.discard(id);
        for v in found {
            match v.b.filter(|b| board.item(*b).is_some_and(movable)) {
                Some(b) => blockers.push(b),
                None => return None,
            }
        }
    }
    for b in blockers {
        if let Some(it) = board.remove(b) {
            spans.extend(it.span);
            ripped.push(it);
        }
    }
    lay(board, grid, path, sources, targets)
}

/// Routes the open connection at `pad` on the ordinary grid; whether it was laid.
fn route_one(board: &mut Board, solver: &mut dyn FieldSolver, options: &ShoveOptions, soft: &mut Vec<ItemId>, pad: ItemId) -> bool {
    let Some(airline) = connection_of(board, pad, options.pours_block) else { return true };
    let far = if airline.from_item == pad { airline.to_item } else { airline.from_item };
    let profile = Profile::for_net(board, airline.net);
    let grid = RouteGrid { deadline: options.deadline.clone(), ..ObstacleMap::new(board, profile, options.cell, options.pours_block).view(airline.net) };
    let sources = terminals(board, &grid, pad);
    let targets = terminals(board, &grid, far);
    if sources.is_empty() || targets.is_empty() {
        return false;
    }
    let (src, tgt): (Vec<usize>, Vec<usize>) = (sources.iter().map(|t| t.0).collect(), targets.iter().map(|t| t.0).collect());
    let Some(path) = solver.route(&grid, &src, &tgt, options.via_cost) else { return false };
    match lay(board, &grid, &path, &sources, &targets) {
        Some(items) => {
            soft.extend(items);
            true
        }
        None => false,
    }
}

/// The path as segments on each layer and via sites, which are on every layer.
struct Corridor {
    segments: HashMap<usize, Vec<(Point, Point)>>,
    vias: Vec<Point>,
}

impl Corridor {
    fn of(path: &[(usize, Point)]) -> Corridor {
        let mut c = Corridor { segments: HashMap::new(), vias: Vec::new() };
        for w in path.windows(2) {
            let ((la, a), (lb, b)) = (w[0], w[1]);
            if la == lb {
                c.segments.entry(la).or_default().push((a, b));
            } else {
                c.vias.push(a);
            }
        }
        if let [(l, p)] = path {
            c.segments.entry(*l).or_default().push((*p, *p));
        }
        c
    }

    /// The distance from `p` to the path on `layer` (any layer for `None`), and the unit
    /// direction away from it; `None` where `p` lies on it and has no direction to go.
    fn nearest(&self, p: Point, layer: Option<usize>) -> Option<(f64, (f64, f64))> {
        let mut best: Option<(f64, (f64, f64))> = None;
        let mut consider = |q: (f64, f64)| {
            let (dx, dy) = (p.x as f64 - q.0, p.y as f64 - q.1);
            let d = dx.hypot(dy);
            if best.is_none_or(|b| d < b.0) {
                best = Some((d, if d > 0.0 { (dx / d, dy / d) } else { (0.0, 0.0) }));
            }
        };
        let segments: Box<dyn Iterator<Item = &(Point, Point)>> = match layer {
            Some(l) => Box::new(self.segments.get(&l).into_iter().flatten()),
            None => Box::new(self.segments.values().flatten()),
        };
        for &(a, b) in segments {
            consider(closest(p, a, b));
        }
        for &v in &self.vias {
            consider((v.x as f64, v.y as f64));
        }
        best.filter(|b| b.0 > 0.0)
    }

    /// How near the segment `a`–`b` on `layer` (any layer for `None`) comes to the path.
    fn distance(&self, a: Point, b: Point, layer: Option<usize>) -> f64 {
        let to = |p: Point, c: Point, d: Point| {
            let q = closest(p, c, d);
            (p.x as f64 - q.0).hypot(p.y as f64 - q.1)
        };
        let between = |c: Point, d: Point| if intersects(a, b, c, d) { 0.0 } else { to(a, c, d).min(to(b, c, d)).min(to(c, a, b)).min(to(d, a, b)) };
        let segments: Box<dyn Iterator<Item = &(Point, Point)>> = match layer {
            Some(l) => Box::new(self.segments.get(&l).into_iter().flatten()),
            None => Box::new(self.segments.values().flatten()),
        };
        segments.map(|&(c, d)| between(c, d)).chain(self.vias.iter().map(|&v| to(v, a, b))).fold(f64::INFINITY, f64::min)
    }

    /// Whether the segment `a`–`b` on `layer` crosses the path there, or runs through a via site.
    fn crosses(&self, a: Point, b: Point, layer: usize) -> bool {
        let through_via = self.vias.iter().any(|v| {
            let (x, y) = closest(*v, a, b);
            (v.x as f64 - x).hypot(v.y as f64 - y) < 1.0
        });
        through_via || self.segments.get(&layer).is_some_and(|s| s.iter().any(|&(c, d)| intersects(a, b, c, d)))
    }
}

fn closest(p: Point, a: Point, b: Point) -> (f64, f64) {
    let (dx, dy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 { 0.0 } else { (((p.x - a.x) as f64 * dx + (p.y - a.y) as f64 * dy) / len2).clamp(0.0, 1.0) };
    (a.x as f64 + t * dx, a.y as f64 + t * dy)
}

/// Whether two segments touch or cross.
fn intersects(a: Point, b: Point, c: Point, d: Point) -> bool {
    let orient = |p: Point, q: Point, r: Point| {
        let v = (q.x - p.x) as i128 * (r.y - p.y) as i128 - (q.y - p.y) as i128 * (r.x - p.x) as i128;
        v.signum()
    };
    let on = |p: Point, q: Point, r: Point| r.x >= p.x.min(q.x) && r.x <= p.x.max(q.x) && r.y >= p.y.min(q.y) && r.y <= p.y.max(q.y);
    let (o1, o2, o3, o4) = (orient(a, b, c), orient(a, b, d), orient(c, d, a), orient(c, d, b));
    (o1 != o2 && o3 != o4) || (o1 == 0 && on(a, b, c)) || (o2 == 0 && on(a, b, d)) || (o3 == 0 && on(c, d, a)) || (o4 == 0 && on(c, d, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossing_segments_intersect_and_parallel_ones_do_not() {
        let p = |x, y| Point::new(x, y);
        assert!(intersects(p(0, 0), p(10, 10), p(0, 10), p(10, 0)));
        assert!(!intersects(p(0, 0), p(10, 0), p(0, 1), p(10, 1)));
        assert!(intersects(p(0, 0), p(10, 0), p(5, 0), p(5, 5)), "touching counts");
    }

    #[test]
    fn the_corridor_points_away_from_the_path_on_its_layer() {
        let path = vec![(0, Point::new(0, 0)), (0, Point::new(1000, 0)), (1, Point::new(1000, 0)), (1, Point::new(1000, 1000))];
        let c = Corridor::of(&path);
        let (d, (ux, uy)) = c.nearest(Point::new(500, 300), Some(0)).unwrap();
        assert!((d - 300.0).abs() < 1e-9 && ux.abs() < 1e-9 && (uy - 1.0).abs() < 1e-9);
        // On layer 1 the path runs up x = 1000, and the via site at (1000, 0) counts everywhere.
        let (d, (ux, _)) = c.nearest(Point::new(700, 500), Some(1)).unwrap();
        assert!((d - 300.0).abs() < 1e-9 && (ux + 1.0).abs() < 1e-9);
        assert!(c.nearest(Point::new(500, 0), Some(0)).is_none(), "a point on the path has nowhere to go");
        assert!(c.crosses(Point::new(500, -100), Point::new(500, 100), 0));
        assert!(!c.crosses(Point::new(500, -100), Point::new(500, 100), 1));
        assert_eq!(c.distance(Point::new(500, -100), Point::new(500, 100), Some(0)), 0.0);
        assert!((c.distance(Point::new(0, 400), Point::new(600, 400), Some(0)) - 400.0).abs() < 1e-9);
    }

    #[test]
    fn the_region_plough_routes_the_wall_round_a_trapped_pad_on_one_layer() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
             (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1)))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let mm = |v: f64| (v * 1e6) as Nm;
        let pad = |net, x, y| Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net,
            layers: 1,
            shape: board::Shape::Capsule { a: Point::new(mm(x), mm(y)), b: Point::new(mm(x), mm(y)), r: mm(0.3) },
            hole: None,
            locked: true,
            span: None,
        };
        board.insert(pad(1, 5.0, 10.0));
        board.insert(pad(1, 15.0, 10.0));
        board.insert(pad(2, 2.0, 2.0));
        board.insert(pad(2, 2.0, 18.0));
        // B's track runs out round A's first pad, a wall between A's two pads on the only layer.
        let mut soft = Vec::new();
        for (a, b) in [((2.0, 2.0), (8.0, 2.0)), ((8.0, 2.0), (8.0, 18.0)), ((8.0, 18.0), (2.0, 18.0))] {
            soft.push(board.insert(Item {
                kind: Kind::Track,
                net: 2,
                layers: 1,
                shape: board::Shape::Capsule { a: Point::new(mm(a.0), mm(a.1)), b: Point::new(mm(b.0), mm(b.1)), r: mm(0.1) },
                hole: None,
                locked: false,
                span: None,
            }));
        }
        assert_eq!(connectivity_without_pours(&board).airlines.len(), 1, "A is open, B routed");
        let options = ShoveOptions { plough: Plough::Region, reach: mm(1.0), ..Default::default() };
        let stats = shove_escape(&mut board, &mut crate::route::CpuSolver, &options, &mut soft, &mut |_, _| {});
        assert!(stats.tries.iter().any(|t| t.kept && t.routed), "{stats:?}");
        assert!(connectivity_without_pours(&board).airlines.is_empty(), "A and B both routed");
        for &id in &soft {
            assert!(board::check_item(&board, id).is_empty(), "{:?}", board.item(id));
        }
    }

    #[test]
    fn net_reaches_override_default_reach_for_any_item_on_the_net() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
             (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1)))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let mm = |v: f64| (v * 1e6) as Nm;
        let pad = |net, x, y| Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net,
            layers: 1,
            shape: board::Shape::Capsule { a: Point::new(mm(x), mm(y)), b: Point::new(mm(x), mm(y)), r: mm(0.3) },
            hole: None,
            locked: true,
            span: None,
        };
        board.insert(pad(1, 5.0, 10.0));
        board.insert(pad(1, 15.0, 10.0));
        // A stub track attached to A's first pad becomes the airline endpoint with a fresh ItemId.
        let mut soft = vec![board.insert(Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: board::Shape::Capsule { a: Point::new(mm(5.0), mm(10.0)), b: Point::new(mm(5.5), mm(10.0)), r: mm(0.1) },
            hole: None,
            locked: false,
            span: None,
        })];
        board.insert(pad(2, 2.0, 2.0));
        board.insert(pad(2, 2.0, 18.0));
        for (a, b) in [((2.0, 2.0), (8.0, 2.0)), ((8.0, 2.0), (8.0, 18.0)), ((8.0, 18.0), (2.0, 18.0))] {
            soft.push(board.insert(Item {
                kind: Kind::Track,
                net: 2,
                layers: 1,
                shape: board::Shape::Capsule { a: Point::new(mm(a.0), mm(a.1)), b: Point::new(mm(b.0), mm(b.1)), r: mm(0.1) },
                hole: None,
                locked: false,
                span: None,
            }));
        }
        let options = ShoveOptions {
            plough: Plough::Region,
            reach: mm(0.5),
            net_reaches: HashMap::from([(1, mm(2.0))]),
            ..Default::default()
        };
        let stats = shove_escape(&mut board, &mut crate::route::CpuSolver, &options, &mut soft, &mut |_, _| {});
        assert_eq!(stats.tries[0].reach, mm(2.0), "net_reaches must apply even when the open end is a track");
        assert!(connectivity_without_pours(&board).airlines.is_empty());
    }

    #[test]
    fn immovable_wall_fast_path_leaves_board_untouched() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"A\") (net 2 \"WALL\")
             (gr_rect (start 0 0) (end 6 6) (layer \"Edge.Cuts\") (width 0.1)))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let mm = |v: f64| (v * 1e6) as Nm;
        let pad = |net, x, y| Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net,
            layers: 1,
            shape: board::Shape::Capsule { a: Point::new(mm(x), mm(y)), b: Point::new(mm(x), mm(y)), r: mm(0.25) },
            hole: None,
            locked: true,
            span: None,
        };
        board.insert(pad(1, 1.0, 3.0));
        board.insert(pad(1, 5.0, 3.0));
        // A locked wall from edge to edge leaves no path even with all movable copper ignored.
        board.insert(Item {
            kind: Kind::Track,
            net: 2,
            layers: 1,
            shape: board::Shape::Capsule { a: Point::new(mm(3.0), mm(0.0)), b: Point::new(mm(3.0), mm(6.0)), r: mm(0.2) },
            hole: None,
            locked: true,
            span: None,
        });
        let mut soft = Vec::new();
        let options = ShoveOptions {
            plough: Plough::Region,
            reach: mm(1.0),
            deadline: crate::clock::Deadline::max_seconds(Some(5.0)),
            ..Default::default()
        };
        let stats = shove_escape(&mut board, &mut crate::route::CpuSolver, &options, &mut soft, &mut |_, _| {});
        // One try: the far end, tried on the board put back, would find the same wall.
        assert_eq!(stats.tries.len(), 1);
        assert!(stats.tries.iter().all(|t| !t.routed && !t.kept && t.rounds[0].path.is_empty()));
        assert!(soft.is_empty());
    }

    #[test]
    fn the_region_plough_frees_a_pad_behind_two_nets() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"A\") (net 2 \"B\") (net 3 \"C\")
             (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1)))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let mm = |v: f64| (v * 1e6) as Nm;
        let pad = |net, x, y| Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net,
            layers: 1,
            shape: board::Shape::Capsule { a: Point::new(mm(x), mm(y)), b: Point::new(mm(x), mm(y)), r: mm(0.25) },
            hole: None,
            locked: true,
            span: None,
        };
        // A is walled in on one layer by B's track at x = 6.3 mm and C's at 7.05 mm, closer
        // together than the reach: B cannot step aside unless C does too.
        for (net, x, y) in [(1, 5.0, 10.0), (1, 15.0, 10.0), (2, 2.0, 3.0), (2, 2.0, 17.0), (3, 2.0, 1.5), (3, 2.0, 18.5)] {
            board.insert(pad(net, x, y));
        }
        let mut soft = Vec::new();
        for (net, x, y0, y1) in [(2, 6.3, 3.0, 17.0), (3, 7.05, 1.5, 18.5)] {
            for (a, b) in [((2.0, y0), (x, y0)), ((x, y0), (x, y1)), ((x, y1), (2.0, y1))] {
                soft.push(board.insert(Item {
                    kind: Kind::Track,
                    net,
                    layers: 1,
                    shape: board::Shape::Capsule { a: Point::new(mm(a.0), mm(a.1)), b: Point::new(mm(b.0), mm(b.1)), r: mm(0.1) },
                    hole: None,
                    locked: false,
                    span: None,
                }));
            }
        }
        let options = ShoveOptions { cell: 35_000, plough: Plough::Region, reach: mm(0.5), ..Default::default() };
        let stats = shove_escape(&mut board, &mut crate::route::CpuSolver, &options, &mut soft, &mut |_, _| {});
        assert!(stats.tries.iter().any(|t| t.kept && t.routed), "{stats:?}");
        assert!(connectivity_without_pours(&board).airlines.is_empty());
        for &id in &soft {
            assert!(board::check_item(&board, id).is_empty(), "{:?}", board.item(id));
        }
    }
}
