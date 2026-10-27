//! Shoving a trapped connection out: for an open connection with an end on a fine-pitch part,
//! the cheapest path to the rest of its net is found with other nets' routed copper passable at
//! a price, and that copper is shoved off the path, round after round, until the connection
//! routes.
//!
//! The shove is a plough (`Plough::Warp`): on each layer the path runs on, a strip `R` wide on
//! either side of it moves `R` away, where `R` clears the path for the widest copper; pads, vias,
//! locked copper and kept pours hold still; and in between the move is a harmonic warp field,
//! falling to nothing at the reach. So the copper in the way is carried aside in a wave with its
//! neighbours, and copper squeezed against a pad moves only as far as there is room. Pieces of
//! copper that meet at a point move together (`morph::shift_copper`) and points on pads stay put.
//! Copper crossing the path, and vias on it, are ripped, since no sideways move clears them.
//! Whatever the plough leaves breaking a rule is ripped too, and the trapped connection is routed
//! first, through the room made for it. A round that still leaves it trapped is followed by
//! another along the new cheapest path, which is laid outright, ripping exactly the copper its
//! pieces would break a rule against.
//! Connections the try knocked open can be ploughed out in turn (`cascade`), then everything
//! still open is routed again, and the whole try is kept only when it leaves fewer connections
//! open than it found.

use std::collections::{HashMap, HashSet};

use board::{connectivity, connectivity_without_pours, Airline, Board, Item, ItemId, Kind, Nm, Point};
use kicad_pcb::Span;

use crate::grid::{ObstacleMap, Profile, RouteGrid};
use crate::push::rip_broken;
use crate::report::{item_name, tell, Focus, Log};
use crate::route::{lay, local_cluster, pieces, route_open, terminals, FieldSolver, Options, Terminal};
use crate::search::STRAIGHT;

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
    /// The reach for the connections of particular pads, where it is not `reach`.
    pub reaches: HashMap<ItemId, Nm>,
    /// The reach for the connections of particular nets when an endpoint item has been replaced.
    pub net_reaches: HashMap<u32, Nm>,
    /// In the region plough, pack negotiated zones to exact grid clearance, expand locally round
    /// collision pinch points, and rejoin plane pads by vias.
    pub region_whole: bool,
    /// Steps of a gentle plough or dipole before it gives way to ripping.
    pub steps: usize,
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
    pub max_seconds: Option<f64>,
    pub pours_block: bool,
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub log: Option<Log>,
}

impl Default for ShoveOptions {
    fn default() -> ShoveOptions {
        ShoveOptions {
            cell: 50_000,
            via_cost: 80,
            rounds: 4,
            plough: Plough::Warp,
            steps: 8,
            reach: 2_000_000,
            reaches: HashMap::new(),
            net_reaches: HashMap::new(),
            region_whole: true,
            margin: 50_000,
            soft_cost: 3 * STRAIGHT,
            cross_cost: 40 * STRAIGHT,
            cascade: 0,
            parts: None,
            reroute_seconds: 20.0,
            repair_seconds: 10.0,
            accept_ties: false,
            seed: 0,
            max_seconds: None,
            pours_block: false,
            cancel: None,
            log: None,
        }
    }
}

/// How the plough moves the copper in the way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plough {
    /// Straight away from the path, by `R (1 - d / W)` at a distance `d`: the full clearance on
    /// the path, fading to nothing at the reach. Blind to what stays put, so it can press copper
    /// into pads.
    Linear,
    /// By a warp field on each layer of the path: strips either side of the path move a full
    /// clearance away from it, pads, locked copper and kept pours hold still, and in between the
    /// field is harmonic, falling to nothing at the reach. Copper squeezed between the path and a
    /// pad moves only as far as there is room.
    Warp,
    /// No plough: the path is laid outright from the first round, ripping exactly the copper its
    /// pieces would break a rule against.
    Rip,
    /// The warp plough a step at a time, `steps` of them, trying the route after each; a net
    /// whose copper a step leaves breaking a rule goes back to where it was before that step.
    /// Nothing is ripped until the steps are spent; then the next round lays the path outright.
    Gentle,
    /// As `Gentle`, but the push is a dipole's: copper is repelled from the trapped pad, straight
    /// away from it, fading over the reach.
    Dipole,
    /// No plough: every other net's movable copper within the reach of the path is ripped, and
    /// the trapped connection and every connection that opened negotiate their ways together,
    /// the trapped one favoured. Later rounds lay the path outright, as `Rip`.
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
    shove_escape_steps(board, solver, options, soft, watch, &mut |_, _, _, _| {})
}

/// As [`shove_escape`], also reporting each step of a gentle or dipole plough via `on_step`.
pub fn shove_escape_steps(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &ShoveOptions,
    soft: &mut Vec<ItemId>,
    watch: &mut dyn FnMut(&Board, ShoveEvent),
    on_step: &mut dyn FnMut(&Board, &ShoveTry, &ShoveRound, usize),
) -> ShoveStats {
    let started = crate::clock::Instant::now();
    let out_of_time = || {
        options.max_seconds.is_some_and(|limit| started.elapsed().as_secs_f64() > limit)
            || options.cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    };
    let whole = options.region_whole && options.plough == Plough::Region;
    let plane_nets = plane_nets_of(board);
    let mut stats = ShoveStats::default();
    if whole {
        stats.removed_spans.extend(clean_closed_stubs(board, soft, options.pours_block, &plane_nets, None));
    }
    // Fixed items (pads, keepouts, locked vendor copper, kept pours, outline) never move during a
    // shove pass, so cache their obstacle map per net class once rather than restamping them on
    // every try.
    let fixed_ids: HashSet<ItemId> = board
        .items()
        .filter(|(_, it)| !(matches!(it.kind, Kind::Track | Kind::Via) && !it.locked))
        .map(|(id, _)| id)
        .collect();
    let mut fixed = FixedWalls { ids: fixed_ids, maps: HashMap::new() };
    let mut tried: HashSet<ItemId> = HashSet::new();
    let mut tie_counts: HashMap<u32, usize> = HashMap::new();
    let mut total_ties = 0usize;
    let mut cur_reaches = options.reaches.clone();
    let mut cur_net_reaches = options.net_reaches.clone();
    loop {
        if out_of_time() {
            break;
        }
        let mut open = open_now(board, options.pours_block);
        crate::jitter::shuffle(&mut open, options.seed.wrapping_add(stats.tries.len() as u64));
        let Some((pad, airline)) = open.iter().find_map(|a| {
            if whole && !options.pours_block && plane_nets.contains(&a.net) {
                return None;
            }
            let mut ends = [a.from_item, a.to_item];
            if whole && board.item(ends[1]).is_some_and(|it| matches!(it.kind, Kind::Pad { .. })) && !board.item(ends[0]).is_some_and(|it| matches!(it.kind, Kind::Pad { .. })) {
                ends.swap(0, 1);
            }
            ends.into_iter()
                .find(|id| !tried.contains(id) && on_parts(board, *id, &options.parts))
                .map(|p| (p, a.clone()))
        }) else {
            break;
        };
        let mut pad = pad;
        let mut far_item = if pad == airline.from_item { airline.to_item } else { airline.from_item };
        let orig_far = far_item;
        tried.insert(pad);
        let to = if pad == airline.from_item { airline.to } else { airline.from };
        let reach = cur_reaches
            .get(&pad)
            .or_else(|| whole.then(|| cur_net_reaches.get(&airline.net)).flatten())
            .copied()
            .unwrap_or(options.reach);
        let options = &ShoveOptions { reach, ..options.clone() };
        let score_before = score_open(&open, options.pours_block, &plane_nets);
        let mut t = ShoveTry { net: airline.net, pad, to, reach, rounds: Vec::new(), routed: false, open_before: open.len(), open_after: 0, kept: false };
        tell(&options.log, "shoving", || format!("{}: {} is trapped; ploughing a way out", board.net_name(airline.net), item_name(board, pad)), Some(Focus::Connection { net: airline.net, from: airline.from, to: airline.to }));
        watch(board, ShoveEvent::Start(&t));
        let snapshot = (board.clone(), soft.clone());
        let mut spans = Vec::new();
        // Copper of the net that reaches no pad is no part of the rest of the net: a way out to
        // it would join nothing, so it goes first.
        spans.extend(rip_floating(board, soft, t.net, options.pours_block));
        // When an earlier tie cut `t.net` mid-corridor, its two dead-end soft tails still sit on
        // either side of the cut and lure `find_soft_path` into stepping straight back through the
        // net that just displaced it. Unzip `t.net`'s dangling soft stubs back to its pads or live
        // junctions before searching for a way out.
        if whole {
            let chosen_pair = (airline.from_item.min(airline.to_item), airline.from_item.max(airline.to_item));
            let other_pairs: HashSet<(ItemId, ItemId)> = open
                .iter()
                .filter(|a| a.net == t.net)
                .map(|a| (a.from_item.min(a.to_item), a.from_item.max(a.to_item)))
                .filter(|&p| p != chosen_pair)
                .collect();
            let side_a: HashSet<ItemId> = local_cluster(board, pad).into_iter().collect();
            let side_b: HashSet<ItemId> = local_cluster(board, far_item).into_iter().collect();
            let only_this = HashSet::from([t.net]);
            let (_, p_spans) = crate::cleanup::prune_dangling_filtered(board, soft, Some(&only_this), &HashSet::new(), &mut |_, _| {});
            spans.extend(p_spans);
            if let Some(cur) = connection_of(board, pad, options.pours_block) {
                far_item = if cur.from_item == pad { cur.to_item } else { cur.from_item };
                t.to = if cur.from_item == pad { cur.to } else { cur.from };
            } else {
                let conn_after = if options.pours_block { connectivity(board) } else { connectivity_without_pours(board) };
                let ca = side_a.iter().find_map(|id| conn_after.cluster.get(id).copied());
                let cb = side_b.iter().find_map(|id| conn_after.cluster.get(id).copied());
                let replacement = conn_after
                    .airlines
                    .iter()
                    .filter(|a| a.net == t.net)
                    .find(|a| !other_pairs.contains(&(a.from_item.min(a.to_item), a.from_item.max(a.to_item))))
                    .or_else(|| {
                        conn_after.airlines.iter().filter(|a| a.net == t.net).find(|a| {
                            let (fa, ta) = (conn_after.cluster.get(&a.from_item).copied(), conn_after.cluster.get(&a.to_item).copied());
                            (ca.is_some() && (fa == ca || ta == ca)) || (cb.is_some() && (fa == cb || ta == cb))
                        })
                    });
                if let Some(a) = replacement {
                    let mut ends = [(a.from_item, a.to_item, a.to), (a.to_item, a.from_item, a.from)];
                    if board.item(ends[1].0).is_some_and(|it| matches!(it.kind, Kind::Pad { .. }))
                        && !board.item(ends[0].0).is_some_and(|it| matches!(it.kind, Kind::Pad { .. }))
                    {
                        ends.swap(0, 1);
                    }
                    (pad, far_item, t.to) = ends[0];
                    t.pad = pad;
                }
            }
        }
        for _ in 0..options.rounds.max(1) {
            if out_of_time() {
                break;
            }
            let first = t.rounds.is_empty();
            let round = plough_round(board, solver, options, soft, &mut fixed, pad, first, &out_of_time, &mut spans, &mut |b, r, step| on_step(b, &t, r, step));
            let (done, walled) = (round.routed, round.path.is_empty());
            t.rounds.push(round);
            watch(board, ShoveEvent::Round(&t, t.rounds.last().unwrap()));
            if done || walled {
                break;
            }
        }
        t.routed = t.rounds.last().is_some_and(|r| r.routed);
        let board_changed = !spans.is_empty()
            || t.rounds.iter().any(|r| r.routed || r.moved > 0 || !r.ripped_items.is_empty());
        if whole && !board_changed {
            t.open_after = t.open_before;
            t.kept = false;
        } else {
            if t.routed {
                cascade(board, solver, options, soft, &mut fixed, &mut t, &open, &snapshot.1, &mut spans, &out_of_time, watch);
            }
            let again = route_open(
                board,
                solver,
                &Options { cell: options.cell, via_cost: options.via_cost, max_seconds: Some(options.reroute_seconds), nets: None, pours_block: options.pours_block, seed: 0, cancel: options.cancel.clone(), log: None },
                &mut |_| {},
            );
            soft.extend(again.laid.iter().copied());
            if whole {
                spans.extend(clean_closed_stubs(board, soft, options.pours_block, &plane_nets, None));
            }
            if whole && board.planes != 0 && !options.pours_block {
                let jp = crate::planes::join_planes(board, &None);
                soft.extend(jp.laid);
            }
            let needs_repair = if whole {
                score_open(&open_now(board, options.pours_block), options.pours_block, &plane_nets) >= score_before
            } else {
                open_now(board, options.pours_block).len() >= t.open_before
            };
            if t.routed && options.repair_seconds > 0.0 && needs_repair && !out_of_time() {
                let first_budget = if whole { options.repair_seconds * 0.55 } else { options.repair_seconds };
                let held = hold(board, soft, &snapshot.1, t.net);
                crate::ripup::rip_up(
                    board,
                    solver,
                    &crate::ripup::RipOptions {
                        cell: options.cell,
                        via_cost: options.via_cost,
                        max_seconds: Some(first_budget),
                        pours_block: options.pours_block,
                        region_repair: whole,
                        cancel: options.cancel.clone(),
                        log: None,
                        ..Default::default()
                    },
                    soft,
                    &mut |_| {},
                );
                release(board, held);
                if whole
                    && score_open(&open_now(board, options.pours_block), options.pours_block, &plane_nets) >= score_before
                    && !out_of_time()
                {
                    // Holding the newly routed net still prevents rip-up from swapping lanes when
                    // two nets share a mouth and the favoured net took the outer lane. Try a
                    // second repair pass with `t.net` unlocked; `rip_up` only commits strict
                    // reductions in open signal connections, negotiating ripped nets at equal weight.
                    crate::ripup::rip_up(
                        board,
                        solver,
                        &crate::ripup::RipOptions {
                            cell: options.cell,
                            via_cost: options.via_cost,
                            max_seconds: Some(options.repair_seconds * 0.45),
                            pours_block: options.pours_block,
                            region_repair: true,
                            cancel: options.cancel.clone(),
                            log: None,
                            ..Default::default()
                        },
                        soft,
                        &mut |_| {},
                    );
                    t.routed = connection_of(board, t.pad, options.pours_block).is_none();
                }
                if whole {
                    spans.extend(clean_closed_stubs(board, soft, options.pours_block, &plane_nets, None));
                }
                if whole && board.planes != 0 && !options.pours_block {
                    let jp = crate::planes::join_planes(board, &None);
                    soft.extend(jp.laid);
                }
            }
            let after_open = open_now(board, options.pours_block);
            t.open_after = after_open.len();
            if whole {
                let score_after = score_open(&after_open, options.pours_block, &plane_nets);
                let improved = score_after < score_before;
                let tied = options.accept_ties
                    && !out_of_time()
                    && t.routed
                    && score_after == score_before
                    && tie_counts.get(&t.net).copied().unwrap_or(0) < 2
                    && total_ties < 6
                    && connection_of(board, t.pad, options.pours_block).is_none();
                t.kept = improved || tied;
                if improved {
                    tie_counts.clear();
                    total_ties = 0;
                    cur_reaches.clear();
                    cur_net_reaches.clear();
                    tried.clear();
                } else if tied {
                    *tie_counts.entry(t.net).or_insert(0) += 1;
                    total_ties += 1;
                }
            } else {
                t.kept = t.open_after < t.open_before
                    || (options.accept_ties && t.routed && t.open_after == t.open_before && connection_of(board, t.pad, options.pours_block).is_none());
            }
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
            (*board, *soft) = snapshot;
            // Region renegotiation routes between both ends of the airline symmetrically; when an
            // undone try leaves the board unchanged, trying the far end at the same reach repeats
            // the same search for nothing.
            if whole {
                tried.insert(orig_far);
                tried.insert(far_item);
            }
        }
        stats.tries.push(t);
    }
    stats
}

struct FixedWalls {
    ids: HashSet<ItemId>,
    maps: HashMap<String, ObstacleMap>,
}

fn plane_nets_of(board: &Board) -> HashSet<u32> {
    if board.planes == 0 {
        return HashSet::new();
    }
    board.items().filter(|(_, it)| board.is_plane(it) && it.net != 0).map(|(_, it)| it.net).collect()
}

fn score_open(open: &[Airline], pours_block: bool, plane_nets: &HashSet<u32>) -> usize {
    if pours_block || plane_nets.is_empty() {
        open.len()
    } else {
        open.iter().filter(|a| !plane_nets.contains(&a.net)).count()
    }
}

/// Drops exact duplicate soft tracks and prunes dead-end or redundant soft stubs on closed nets,
/// leaving open signal nets untouched so locally cut cross-board trunks stay available to bridge.
fn clean_closed_stubs(
    board: &mut Board,
    soft: &mut Vec<ItemId>,
    pours_block: bool,
    plane_nets: &HashSet<u32>,
    only_nets: Option<&HashSet<u32>>,
) -> Vec<Span> {
    let mut spans = Vec::new();
    let (_, dup_spans) = crate::cleanup::drop_duplicates(board, soft);
    spans.extend(dup_spans);
    let open_sig: HashSet<u32> = open_now(board, pours_block)
        .into_iter()
        .filter(|a| pours_block || !plane_nets.contains(&a.net))
        .map(|a| a.net)
        .collect();
    let (_, prune_spans) = crate::cleanup::prune_dangling_filtered(board, soft, only_nets, &open_sig, &mut |_, _| {});
    spans.extend(prune_spans);
    spans
}

fn hold_since(board: &mut Board, soft: &[ItemId], soft_before: &[ItemId], skip_nets: &HashSet<u32>) -> Vec<ItemId> {
    let was: HashSet<ItemId> = soft_before.iter().copied().collect();
    let mut held = Vec::new();
    for &id in soft.iter().filter(|id| !was.contains(id)) {
        if board.item(id).is_some_and(|it| !it.locked && !skip_nets.contains(&it.net)) {
            if let Some(mut it) = board.remove(id) {
                it.locked = true;
                board.restore(id, it);
                held.push(id);
            }
        }
    }
    held
}

/// Ploughs out, in turn, the connections the try knocked open, holding still the copper laid for
/// the connections already out, so the next plough cannot undo them.
#[allow(clippy::too_many_arguments)]
fn cascade(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &ShoveOptions,
    soft: &mut Vec<ItemId>,
    fixed: &mut FixedWalls,
    t: &mut ShoveTry,
    open_before: &[Airline],
    soft_before: &[ItemId],
    spans: &mut Vec<Span>,
    out_of_time: &dyn Fn() -> bool,
    watch: &mut dyn FnMut(&Board, ShoveEvent),
) {
    let before: HashSet<(ItemId, ItemId)> = open_before.iter().map(|a| (a.from_item, a.to_item)).collect();
    let mut held = hold(board, soft, soft_before, t.net);
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
            let round = plough_round(board, solver, options, soft, fixed, end, k == 0, out_of_time, spans, &mut |_, _, _| {});
            let (done, walled) = (round.routed, round.path.is_empty());
            t.rounds.push(round);
            watch(board, ShoveEvent::Round(t, t.rounds.last().unwrap()));
            if done || walled || out_of_time() {
                break;
            }
        }
        held.extend(hold(board, soft, soft_before, net));
    }
    release(board, held);
}

/// Locks the copper of `net` laid since `soft_before`, so nothing after rips or moves it: the
/// pieces locked, for `release`.
fn hold(board: &mut Board, soft: &[ItemId], soft_before: &[ItemId], net: u32) -> Vec<ItemId> {
    let was: HashSet<ItemId> = soft_before.iter().copied().collect();
    let mut held = Vec::new();
    for &id in soft.iter().filter(|id| !was.contains(id)) {
        if board.item(id).is_some_and(|it| it.net == net && !it.locked) {
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
    fixed: &mut FixedWalls,
    pad: ItemId,
    first: bool,
    out_of_time: &dyn Fn() -> bool,
    spans: &mut Vec<Span>,
    on_step: &mut dyn FnMut(&Board, &ShoveRound, usize),
) -> ShoveRound {
    let mut round = ShoveRound::default();
    let Some(airline) = connection_of(board, pad, options.pours_block) else {
        round.routed = true;
        return round;
    };
    let (far, net) = (if airline.from_item == pad { airline.to_item } else { airline.from_item }, airline.net);
    round.net = net;
    if options.region_whole && options.plough == Plough::Region {
        let Some((grid, soft_grid, sources, targets, path, pinch)) = find_soft_path(board, solver, options, fixed, net, pad, far) else {
            return round;
        };
        round.path = path.iter().map(|&(l, i, j)| (grid.layers[l], grid.centre(i, j))).collect();
        let via_sites = path.windows(2).filter(|w| w[0].0 != w[1].0 && soft_grid.via_blocked[w[1].2 * grid.nx + w[1].1]).count();
        round.soft_cells = path.iter().filter(|&&(l, i, j)| soft_grid.blocked[grid.index(l, i, j)]).count() + via_sites;
        if first && (round.soft_cells > 0 || !pinch.is_empty()) {
            renegotiate_region(board, solver, options, soft, fixed, pad, far, net, &pinch, out_of_time, spans, &mut round);
            return round;
        }
        let movable = |it: &Item| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked && it.net != net;
        let laid = force_lay(board, &grid, &path, &sources, &targets, &movable, spans, &mut round.ripped_items);
        round.crossed = round.ripped_items.len();
        soft.retain(|id| board.item(*id).is_some());
        if let Some(items) = laid {
            soft.extend(items);
            round.routed = true;
        }
        if board.planes != 0 && !options.pours_block && !round.ripped_items.is_empty() {
            let jp = crate::planes::join_planes(board, &None);
            soft.extend(jp.laid);
        }
        return round;
    }
    let movable = |it: &Item| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked && it.net != net;
    let profile = Profile::for_net(board, net);
    let walls = ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| board.item(id).is_none_or(|it| movable(it)));
    let others = ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| !board.item(id).is_some_and(|it| movable(it)));
    let mut grid = walls.view(net);
    let soft_grid = others.view(net);
    grid.penalty = soft_grid.blocked.iter().map(|b| if *b { options.soft_cost } else { 0 }).collect();
    // On the copper itself, a path can only cross by ripping it.
    for (_, it) in board.items().filter(|(_, it)| movable(it)) {
        let board::Shape::Capsule { a, b, r } = it.shape else { continue };
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
                        grid.penalty[n] = grid.penalty[n].max(options.cross_cost);
                    }
                }
            }
        }
    }
    let sources = terminals(board, &grid, pad);
    let targets = terminals(board, &grid, far);
    let (src, tgt): (Vec<usize>, Vec<usize>) = (sources.iter().map(|t| t.0).collect(), targets.iter().map(|t| t.0).collect());
    let Some(path) = (!src.is_empty() && !tgt.is_empty()).then(|| solver.route(&grid, &src, &tgt, options.via_cost)).flatten() else {
        return round;
    };
    round.path = path.iter().map(|&(l, i, j)| (grid.layers[l], grid.centre(i, j))).collect();
    // A via needs more room than a trace: a via site counts when other copper is within its reach.
    let via_sites = path.windows(2).filter(|w| w[0].0 != w[1].0 && soft_grid.via_blocked[w[1].2 * grid.nx + w[1].1]).count();
    round.soft_cells = path.iter().filter(|&&(l, i, j)| soft_grid.blocked[grid.index(l, i, j)]).count() + via_sites;
    if first && round.soft_cells > 0 && matches!(options.plough, Plough::Gentle | Plough::Dipole) {
        gentle(board, solver, options, soft, pad, net, &movable, spans, &mut round, on_step);
        return round;
    }
    if first && round.soft_cells > 0 && options.plough == Plough::Region {
        renegotiate(board, solver, options, soft, pad, net, &movable, spans, &mut round);
        return round;
    }
    if round.soft_cells == 0 || !first || matches!(options.plough, Plough::Rip | Plough::Region) {
        // Lay the path as it stands, ripping exactly the other copper it runs into: clear
        // already, or a later round, where ploughing again did not move that copper off it.
        let laid = force_lay(board, &grid, &path, &sources, &targets, &movable, spans, &mut round.ripped_items);
        round.crossed = round.ripped_items.len();
        if let Some(items) = laid {
            soft.retain(|id| board.item(*id).is_some());
            soft.extend(items);
            round.routed = true;
            return round;
        }
        soft.retain(|id| board.item(*id).is_some());
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
            (Kind::Via, board::Shape::Capsule { a, r, .. }) => {
                // The warp plough holds vias still, so one on the path must go; the linear one
                // shoves them, unless one sits on the path itself.
                let room = match options.plough {
                    Plough::Warp | Plough::Gentle | Plough::Dipole => (profile.half_width + board.clearance(net, it.net) + r) as f64,
                    Plough::Linear | Plough::Rip | Plough::Region => options.cell as f64,
                };
                corridor.nearest(*a, None).is_none_or(|(d, _)| d < room)
            }
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
    let fields = match options.plough {
        Plough::Linear | Plough::Rip | Plough::Gentle | Plough::Dipole | Plough::Region => HashMap::new(),
        Plough::Warp => plough_fields(board, &round.path, clear.values().copied().fold(0.0, f64::max), reach, net),
    };
    let warped = |p: Point, n: u32, layer: Option<usize>| -> Option<(Point, bool)> {
        if n == net {
            return None;
        }
        // Vias hold still, and copper ending in one with them.
        let (dx, dy) = fields.get(&layer?)?.displacement(p);
        (dx.hypot(dy) >= 1_000.0).then(|| (Point::new(dx.round() as i64, dy.round() as i64), false))
    };
    let rule: &dyn Fn(Point, u32, Option<usize>) -> Option<(Point, bool)> = match options.plough {
        Plough::Linear | Plough::Rip | Plough::Gentle | Plough::Dipole | Plough::Region => &linear,
        Plough::Warp => &warped,
    };
    let (_, moved, moved_spans) = morph::shift_copper(board, soft, 150_000.0, rule);
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

/// The region plough: other nets' movable copper within the reach of the round's path ripped,
/// with whatever of those nets it leaves reaching no pad, and the connections of the trapped net
/// and of every net ripped negotiated together, the trapped net's congestion priced at half.
#[allow(clippy::too_many_arguments)]
fn renegotiate(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &ShoveOptions,
    soft: &mut Vec<ItemId>,
    pad: ItemId,
    net: u32,
    movable: &dyn Fn(&Item) -> bool,
    spans: &mut Vec<Span>,
    round: &mut ShoveRound,
) {
    let corridor = Corridor::of(&round.path);
    let reach = options.reach as f64;
    let near: Vec<ItemId> = board
        .items()
        .filter(|(_, it)| movable(it))
        .filter(|(_, it)| match (&it.kind, &it.shape) {
            (Kind::Via, board::Shape::Capsule { a, .. }) => corridor.distance(*a, *a, None) < reach,
            (Kind::Track, board::Shape::Capsule { a, b, .. }) => corridor.distance(*a, *b, Some(it.layers.trailing_zeros() as usize)) < reach,
            _ => false,
        })
        .map(|(id, _)| id)
        .collect();
    let mut nets: HashSet<u32> = HashSet::from([net]);
    for id in near {
        if let Some(it) = board.remove(id) {
            nets.insert(it.net);
            spans.extend(it.span.clone());
            round.ripped_items.push(it);
        }
    }
    round.crossed = round.ripped_items.len();
    for &n in &nets {
        let had: Vec<(ItemId, Item)> = board.items().filter(|(_, it)| it.net == n).map(|(id, it)| (id, it.clone())).collect();
        spans.extend(rip_floating(board, soft, n, options.pours_block));
        round.ripped_items.extend(had.into_iter().filter(|(id, _)| board.item(*id).is_none()).map(|(_, it)| it));
    }
    soft.retain(|id| board.item(*id).is_some());
    tell(&options.log, "shoving", || format!("{}: {} pieces of {} nets ripped round the way out; negotiating them again", board.net_name(net), round.crossed, nets.len() - 1), None);
    let n = crate::negotiate::route_negotiated(
        board,
        solver,
        &crate::negotiate::NegotiationOptions {
            cell: options.cell,
            via_cost: options.via_cost,
            max_seconds: Some(options.reroute_seconds),
            nets: Some(nets),
            net_weight: HashMap::from([(net, 0.5)]),
            pours_block: options.pours_block,
            seed: options.seed,
            cancel: options.cancel.clone(),
            ..Default::default()
        },
        &mut |_| {},
        &mut |_, _| {},
    );
    soft.extend(n.laid);
    round.routed = connection_of(board, pad, options.pours_block).is_none();
}

/// Terminals of `end` when movable copper near its mouth is temporarily lifted, so a necked-down
/// leg from an unescaped fine-pitch pad is not refused against a neighbour track that the plough
/// is about to rip.
fn terminals_through(
    board: &mut Board,
    grid: &RouteGrid,
    end: ItemId,
    movable: &dyn Fn(ItemId, &Item) -> bool,
) -> Vec<Terminal> {
    let found = terminals(board, grid, end);
    if !found.is_empty() {
        return found;
    }
    let Some(bb) = board.bbox(end).or_else(|| board.item(end).map(|it| it.shape.bbox())) else {
        return found;
    };
    let search = bb.grow((grid.h * 12).max(450_000));
    let mut near_ids = HashSet::new();
    for l in 0..board.layers.len() {
        for id in board.near(l, search) {
            if board.item(id).is_some_and(|it| movable(id, it)) {
                near_ids.insert(id);
            }
        }
    }
    if near_ids.is_empty() {
        return found;
    }
    let mut parked = Vec::with_capacity(near_ids.len());
    for id in near_ids {
        if let Some(it) = board.remove(id) {
            parked.push((id, it));
        }
    }
    let found = terminals(board, grid, end);
    for (id, it) in parked {
        board.restore(id, it);
    }
    found
}

/// Cheapest path from `pad` to `far` with movable copper passable at a penalty. Reuses the cached
/// fixed-obstacle map per net class and checks terminals before building `others` so keepout-walled
/// pins exit immediately.
#[allow(clippy::too_many_arguments)]
fn find_soft_path(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &ShoveOptions,
    fixed: &mut FixedWalls,
    net: u32,
    pad: ItemId,
    far: ItemId,
) -> Option<(RouteGrid, RouteGrid, Vec<Terminal>, Vec<Terminal>, crate::search::GridPath, Vec<Point>)> {
    let movable = |_: ItemId, it: &Item| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked && it.net != net;
    let profile = Profile::for_net(board, net);
    let fixed_ids = &fixed.ids;
    let mut walls = fixed
        .maps
        .entry(profile.class.clone())
        .or_insert_with(|| {
            ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| !fixed_ids.contains(&id))
        })
        .clone();
    let non_fixed_walls: Vec<ItemId> = board
        .items()
        .filter(|(id, it)| !fixed.ids.contains(id) && !movable(*id, it))
        .map(|(id, _)| id)
        .collect();
    for id in non_fixed_walls {
        walls.stamp(board, id);
    }
    let mut grid = walls.view(net);
    let end_with_pads = |board: &mut Board, grid: &RouteGrid, root: ItemId| -> Vec<Terminal> {
        let base = terminals_through(board, grid, root, &movable);
        if board.item(root).is_none_or(|it| matches!(it.kind, Kind::Pad { .. })) {
            return base;
        }
        let joined = local_cluster(board, root);
        let mut pads: Vec<ItemId> = joined
            .into_iter()
            .filter(|&id| id != root && board.item(id).is_some_and(|it| matches!(it.kind, Kind::Pad { .. })))
            .collect();
        if pads.is_empty() {
            return base;
        }
        if let Some(origin) = board.item(root).map(|it| {
            let b = it.shape.bbox();
            Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2)
        }) {
            pads.sort_by(|&a, &b| {
                let da = board.item(a).map_or(f64::INFINITY, |it| {
                    let bb = it.shape.bbox();
                    origin.distance(Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2))
                });
                let db = board.item(b).map_or(f64::INFINITY, |it| {
                    let bb = it.shape.bbox();
                    origin.distance(Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2))
                });
                da.total_cmp(&db)
            });
        }
        pads.truncate(4);
        let mut by_cell: std::collections::BTreeMap<usize, Option<(Point, Nm)>> = base.into_iter().collect();
        for pad_id in pads {
            for (cell, leg) in terminals_through(board, grid, pad_id, &movable) {
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
    };
    let sources = end_with_pads(board, &grid, pad);
    if sources.is_empty() {
        return None;
    }
    let targets = end_with_pads(board, &grid, far);
    if targets.is_empty() {
        return None;
    }
    let plane_nets = if options.pours_block { HashSet::new() } else { plane_nets_of(board) };
    let sig_movable = |id: ItemId, it: &Item| movable(id, it) && !plane_nets.contains(&it.net);
    let others = ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| {
        !board.item(id).is_some_and(|it| sig_movable(id, it))
    });
    let soft_grid = others.view(net);
    let plane_soft = (!plane_nets.is_empty()).then(|| {
        ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| {
            !board.item(id).is_some_and(|it| movable(id, it) && plane_nets.contains(&it.net))
        })
        .view(net)
    });
    let plane_clear_cost = (STRAIGHT / 2).max(1);
    grid.penalty = (0..soft_grid.blocked.len())
        .map(|idx| {
            if soft_grid.blocked[idx] {
                options.soft_cost
            } else if plane_soft.as_ref().is_some_and(|pg| pg.blocked[idx]) {
                plane_clear_cost
            } else {
                0
            }
        })
        .collect();
    let via_pen: Vec<u32> = (0..soft_grid.via_blocked.len())
        .map(|c| {
            if soft_grid.via_blocked[c] {
                options.soft_cost
            } else if plane_soft.as_ref().is_some_and(|pg| pg.via_blocked[c]) {
                plane_clear_cost
            } else {
                0
            }
        })
        .collect();
    for (id, it) in board.items().filter(|(id, it)| movable(*id, it)) {
        let _ = id;
        let board::Shape::Capsule { a, b, r } = it.shape else { continue };
        let cross_pen = if plane_nets.contains(&it.net) { STRAIGHT } else { options.cross_cost };
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
                        grid.penalty[n] = grid.penalty[n].max(cross_pen);
                    }
                }
            }
        }
    }
    let (src, tgt): (Vec<usize>, Vec<usize>) = (sources.iter().map(|t| t.0).collect(), targets.iter().map(|t| t.0).collect());
    let path = solver.route_with_via_penalty(&grid, &src, &tgt, options.via_cost, &via_pen)?;
    let mut pinch: Vec<Point> = Vec::new();
    for &(k, i, j) in &path {
        if soft_grid.blocked[grid.index(k, i, j)] {
            pinch.push(grid.centre(i, j));
        }
    }
    for w in path.windows(2) {
        if w[0].0 != w[1].0 && soft_grid.via_blocked[w[1].2 * grid.nx + w[1].1] {
            pinch.push(grid.centre(w[1].1, w[1].2));
        }
    }
    if let Some(cand) = crate::route::pieces(&grid, &path, &sources, &targets) {
        let margin = profile.clearance.max(board::nm(board.rules.min_clearance)) + profile.via_radius;
        for piece in &cand {
            let search = piece.shape.bbox().grow(margin);
            let mut hit = false;
            for l in (0..board.layers.len()).filter(|l| piece.on(*l)) {
                if board.near(l, search).into_iter().any(|id| {
                    board.item(id).is_some_and(|it| {
                        sig_movable(id, it) && piece.shape.gap(&it.shape) < profile.clearance.max(Profile::for_net(board, it.net).clearance) as f64
                    })
                }) {
                    hit = true;
                    break;
                }
            }
            if hit {
                if let board::Shape::Capsule { a, b, .. } = piece.shape {
                    pinch.push(a);
                    if b != a {
                        pinch.push(b);
                        pinch.push(Point::new((a.x + b.x) / 2, (a.y + b.y) / 2));
                    }
                }
            }
        }
    }
    Some((grid, soft_grid, sources, targets, path, pinch))
}

fn rip_local_pieces(
    board: &mut Board,
    ids: Vec<ItemId>,
    pours_block: bool,
    soft: &mut Vec<ItemId>,
    spans: &mut Vec<Span>,
    round: &mut ShoveRound,
) -> HashSet<u32> {
    let mut nets = HashSet::new();
    for id in ids {
        if let Some(it) = board.remove(id) {
            nets.insert(it.net);
            spans.extend(it.span.clone());
            round.ripped_items.push(it);
        }
    }
    for &n in &nets {
        let had: Vec<(ItemId, Item)> = board.items().filter(|(_, it)| it.net == n).map(|(id, it)| (id, it.clone())).collect();
        spans.extend(rip_floating(board, soft, n, pours_block));
        round.ripped_items.extend(had.into_iter().filter(|(id, _)| board.item(*id).is_none()).map(|(_, it)| it));
    }
    soft.retain(|id| board.item(*id).is_some());
    nets
}

/// Local region renegotiation: rips movable copper within `reach` of the pinched sections of the
/// soft path (rather than along clear cross-board spans), negotiates the ripped signal nets with
/// exact grid-step zone widths, widens once round any remaining collision cells or displaced-net
/// blockers if needed, and rejoins plane vias directly to their planes.
#[allow(clippy::too_many_arguments)]
fn renegotiate_region(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &ShoveOptions,
    soft: &mut Vec<ItemId>,
    fixed: &mut FixedWalls,
    pad: ItemId,
    far: ItemId,
    net: u32,
    pinch: &[Point],
    out_of_time: &dyn Fn() -> bool,
    spans: &mut Vec<Span>,
    round: &mut ShoveRound,
) {
    let plane_nets = if options.pours_block { HashSet::new() } else { plane_nets_of(board) };
    let start_airlines = open_now(board, options.pours_block);
    let mut open_at_start: HashMap<u32, usize> = HashMap::new();
    let mut initial_pairs: HashSet<(ItemId, ItemId)> = HashSet::with_capacity(start_airlines.len());
    for a in &start_airlines {
        if options.pours_block || !plane_nets.contains(&a.net) {
            *open_at_start.entry(a.net).or_insert(0) += 1;
        }
        initial_pairs.insert((a.from_item.min(a.to_item), a.from_item.max(a.to_item)));
    }
    let mut full_path = Vec::with_capacity(round.path.len() + 2);
    if let (Some(&(l0, p0)), Some(it)) = (round.path.first(), board.item(pad)) {
        full_path.push((l0, crate::route::anchor(it, p0)));
    }
    full_path.extend_from_slice(&round.path);
    if let (Some(&(l1, p1)), Some(it)) = (round.path.last(), board.item(far)) {
        full_path.push((l1, crate::route::anchor(it, p1)));
    }
    // On a long cross-board path that runs clear of movable copper for most of its length, ripping
    // round the clear middle scrambles unrelated parallel buses. Keep only the path sections near
    // actual soft-copper pinches when pinches are known, and cap Stage 1's corridor radius so wide
    // late reaches expand surgically via Stage 2 rather than cratering unrelated buses.
    let s1_reach = options.reach.min(2_200_000);
    let window = (s1_reach + 450_000) as f64;
    let active_corridors: Vec<Corridor> = if pinch.is_empty() {
        vec![Corridor::of(&full_path)]
    } else {
        let mut chains: Vec<Corridor> = Vec::new();
        let mut cur: Vec<(usize, Point)> = Vec::new();
        for &(l, p) in &full_path {
            if pinch.iter().any(|q| p.distance(*q) <= window) {
                cur.push((l, p));
            } else if !cur.is_empty() {
                chains.push(Corridor::of(&cur));
                cur.clear();
            }
        }
        if !cur.is_empty() {
            chains.push(Corridor::of(&cur));
        }
        if chains.is_empty() {
            vec![Corridor::of(&full_path)]
        } else {
            chains
        }
    };
    let reach = s1_reach as f64;
    let movable = |it: &Item| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked && it.net != net;
    let near: Vec<ItemId> = board
        .items()
        .filter(|(_, it)| movable(it))
        .filter(|(_, it)| match (&it.kind, &it.shape) {
            (Kind::Via, board::Shape::Capsule { a, .. }) => {
                active_corridors.iter().any(|c| c.distance(*a, *a, None) < reach)
                    || pinch.iter().any(|q| q.distance(*a) < reach)
            }
            (Kind::Track, board::Shape::Capsule { a, b, .. }) => {
                let layer = Some(it.layers.trailing_zeros() as usize);
                active_corridors.iter().any(|c| c.distance(*a, *b, layer) < reach)
                    || pinch.iter().any(|q| board::point_segment(*q, *a, *b) < reach)
            }
            _ => false,
        })
        .map(|(id, _)| id)
        .collect();
    let mut touched = rip_local_pieces(board, near, options.pours_block, soft, spans, round);
    round.crossed = round.ripped_items.len();

    touched.insert(net);
    let mut neg_nets: HashSet<u32> = touched.iter().copied().filter(|&n| n == net || !plane_nets.contains(&n)).collect();
    tell(
        &options.log,
        "shoving",
        || format!("{}: {} pieces of {} nets ripped round the way out; negotiating them again", board.net_name(net), round.crossed, touched.len().saturating_sub(1)),
        None,
    );

    let after_rip1 = (board.clone(), soft.clone(), spans.clone(), round.clone());
    let mut best_c = usize::MAX;
    let mut hotspots: Vec<Point> = Vec::new();
    let patience = if neg_nets.len() <= 6 { 10 } else { 6 };
    let n1 = crate::negotiate::route_negotiated_rounds(
        board,
        solver,
        &crate::negotiate::NegotiationOptions {
            cell: options.cell,
            via_cost: options.via_cost,
            max_rounds: 32,
            patience,
            present: 20.0,
            present_growth: 1.55,
            history_step: 25,
            max_seconds: Some(options.reroute_seconds),
            nets: Some(neg_nets.clone()),
            net_weight: HashMap::from([(net, 0.45)]),
            tight_zones: true,
            pours_block: options.pours_block,
            seed: options.seed,
            cancel: options.cancel.clone(),
            ..Default::default()
        },
        &mut |_| {},
        &mut |_, _| {},
        Some(&mut |view| {
            if view.colliding > 0 && view.colliding <= best_c {
                best_c = view.colliding;
                hotspots.clear();
                let (nx, ny) = (view.grid.nx, view.grid.ny);
                let cells = nx * ny;
                let layers = view.grid.layers.len();
                for j in 0..ny {
                    for i in 0..nx {
                        let c = j * nx + i;
                        if (0..layers).any(|l| view.overlap[l * cells + c] >= 2) {
                            hotspots.push(view.grid.centre(i, j));
                        }
                    }
                }
            }
        }),
    );
    soft.extend(n1.laid);
    // Finish any uncommitted negotiated nets and prune dead cut stubs before rejoining plane vias,
    // so signal escapes claim tight pin-row lanes before movable plane vias drop back in.
    let r1 = route_open(
        board,
        solver,
        &Options {
            cell: options.cell,
            via_cost: options.via_cost,
            max_seconds: Some(options.reroute_seconds.min(5.0)),
            nets: Some(neg_nets.clone()),
            pours_block: options.pours_block,
            seed: options.seed,
            cancel: options.cancel.clone(),
            log: None,
        },
        &mut |_| {},
    );
    soft.extend(r1.laid);
    spans.extend(clean_closed_stubs(board, soft, options.pours_block, &plane_nets, Some(&neg_nets)));
    if board.planes != 0 && !options.pours_block {
        let jp = crate::planes::join_planes(board, &None);
        soft.extend(jp.laid);
    }
    let start_count: usize = neg_nets.iter().map(|n| open_at_start.get(n).copied().unwrap_or(0)).sum();
    let open1 = open_now(board, options.pours_block);
    let open_neg1 = open1.iter().filter(|a| neg_nets.contains(&a.net) && (options.pours_block || !plane_nets.contains(&a.net))).count();
    let pad_open1 = connection_of(board, pad, options.pours_block).is_some();
    let score1 = (pad_open1, score_open(&open1, options.pours_block, &plane_nets));

    // When stage 1 still pinches or displaces one of the negotiated nets, trace the newly opened
    // airlines (ignoring pre-existing vendor-walled airlines on the same net) to find any external
    // blocker just outside `s1_reach`, or re-pack at equal weight when the ripped nets collided
    // only with each other. Keep stage 2 only when strictly better than stage 1.
    if (open_neg1 >= start_count || pad_open1) && neg_nets.len() <= 12 && !out_of_time() {
        let mut blocker_pts: Vec<Point> = Vec::new();
        let mut displaced: Vec<Airline> = open1
            .iter()
            .filter(|a| {
                neg_nets.contains(&a.net)
                    && (options.pours_block || !plane_nets.contains(&a.net))
                    && (!initial_pairs.contains(&(a.from_item.min(a.to_item), a.from_item.max(a.to_item)))
                        || a.from_item == pad
                        || a.to_item == pad
                        || a.from_item == far
                        || a.to_item == far)
            })
            .cloned()
            .collect();
        displaced.sort_by_key(|a| !(a.from_item == pad || a.to_item == pad || a.from_item == far || a.to_item == far));
        displaced.truncate(3);
        if !displaced.is_empty() {
            let held_stage1 = hold_since(board, soft, &after_rip1.1, &plane_nets);
            for a in &displaced {
                if out_of_time() {
                    break;
                }
                if let Some((_, _, _, _, _, b_pinch)) = find_soft_path(board, solver, options, fixed, a.net, a.from_item, a.to_item) {
                    blocker_pts.extend(b_pinch);
                }
            }
            release(board, held_stage1);
            // When Stage 1's own routes filled the sole mouth of a pocket, no path exists with
            // them held; search once with them passable too so the soft path crosses out through
            // the mouth and names any external copper blocking its exit.
            if blocker_pts.is_empty() {
                for a in &displaced {
                    if out_of_time() {
                        break;
                    }
                    if let Some((_, _, _, _, _, b_pinch)) = find_soft_path(board, solver, options, fixed, a.net, a.from_item, a.to_item) {
                        blocker_pts.extend(b_pinch);
                    }
                }
            }
        }
        if !hotspots.is_empty() || !blocker_pts.is_empty() || (!displaced.is_empty() && neg_nets.len() <= 8) {
            let stage1_done = (board.clone(), soft.clone(), spans.clone(), round.clone());
            (*board, *soft, *spans, *round) = after_rip1;
            let h_reach = (options.cell * 12).max(400_000);
            let b_reach = options.reach.clamp(450_000, 1_200_000);
            let (h_dist, b_dist) = (h_reach as f64, b_reach as f64);
            let all_pts: Vec<Point> = hotspots.iter().chain(&blocker_pts).copied().collect();
            let extra: Vec<ItemId> = if all_pts.is_empty() {
                Vec::new()
            } else {
                let search_box = board::BBox::of(all_pts).grow(h_reach.max(b_reach));
                board
                    .items()
                    .filter(|(_, it)| movable(it) && it.shape.bbox().overlaps(search_box))
                    .filter(|(_, it)| match (&it.kind, &it.shape) {
                        (Kind::Via, board::Shape::Capsule { a, .. }) => {
                            hotspots.iter().any(|p| p.distance(*a) < h_dist)
                                || blocker_pts.iter().any(|p| p.distance(*a) < b_dist)
                        }
                        (Kind::Track, board::Shape::Capsule { a, b, .. }) => {
                            hotspots.iter().any(|p| board::point_segment(*p, *a, *b) < h_dist)
                                || blocker_pts.iter().any(|p| board::point_segment(*p, *a, *b) < b_dist)
                        }
                        _ => false,
                    })
                    .map(|(id, _)| id)
                    .collect()
            };
            let had_extra = !extra.is_empty();
            if had_extra {
                let more = rip_local_pieces(board, extra, options.pours_block, soft, spans, round);
                round.crossed = round.ripped_items.len();
                for m in more {
                    if m == net || !plane_nets.contains(&m) {
                        neg_nets.insert(m);
                    }
                }
            }
            if had_extra || (!displaced.is_empty() && neg_nets.len() <= 8) {
                let stage2_weight = if had_extra { HashMap::from([(net, 0.7)]) } else { HashMap::new() };
                let n2 = crate::negotiate::route_negotiated(
                    board,
                    solver,
                    &crate::negotiate::NegotiationOptions {
                        cell: options.cell,
                        via_cost: options.via_cost,
                        max_rounds: 32,
                        patience: 10,
                        present: 20.0,
                        present_growth: 1.55,
                        history_step: 25,
                        max_seconds: Some(options.reroute_seconds),
                        nets: Some(neg_nets.clone()),
                        net_weight: stage2_weight,
                        tight_zones: true,
                        pours_block: options.pours_block,
                        seed: options.seed.wrapping_add(31),
                        cancel: options.cancel.clone(),
                        ..Default::default()
                    },
                    &mut |_| {},
                    &mut |_, _| {},
                );
                soft.extend(n2.laid);
                let r2 = route_open(
                    board,
                    solver,
                    &Options {
                        cell: options.cell,
                        via_cost: options.via_cost,
                        max_seconds: Some(options.reroute_seconds.min(5.0)),
                        nets: Some(neg_nets.clone()),
                        pours_block: options.pours_block,
                        seed: options.seed.wrapping_add(31),
                        cancel: options.cancel.clone(),
                        log: None,
                    },
                    &mut |_| {},
                );
                soft.extend(r2.laid);
                spans.extend(clean_closed_stubs(board, soft, options.pours_block, &plane_nets, Some(&neg_nets)));
                if board.planes != 0 && !options.pours_block {
                    let jp = crate::planes::join_planes(board, &None);
                    soft.extend(jp.laid);
                }
                let open2 = open_now(board, options.pours_block);
                let pad_open2 = connection_of(board, pad, options.pours_block).is_some();
                let score2 = (pad_open2, score_open(&open2, options.pours_block, &plane_nets));
                if score2 >= score1 {
                    (*board, *soft, *spans, *round) = stage1_done;
                }
            } else {
                (*board, *soft, *spans, *round) = stage1_done;
            }
        }
    }
    round.routed = connection_of(board, pad, options.pours_block).is_none();
}

/// The gentle plough: `steps` small pushes, each followed by a try at the route, with any net a
/// push leaves breaking a rule put back as it was. Rips nothing.
#[allow(clippy::too_many_arguments)]
fn gentle(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &ShoveOptions,
    soft: &mut Vec<ItemId>,
    pad: ItemId,
    net: u32,
    movable: &dyn Fn(&Item) -> bool,
    spans: &mut Vec<Span>,
    round: &mut ShoveRound,
    on_step: &mut dyn FnMut(&Board, &ShoveRound, usize),
) {
    let profile = Profile::for_net(board, net);
    let half = profile.half_width.max(profile.via_radius);
    let widest = board.items().filter(|(_, it)| movable(it)).filter_map(|(_, it)| match it.shape {
        board::Shape::Capsule { r, .. } => Some(r),
        _ => None,
    });
    let widest = widest.max().unwrap_or(0);
    let clearance = board.rules.classes.iter().map(|c| board::nm(c.clearance)).max().unwrap_or(0).max(profile.clearance);
    let full = (half + clearance + widest + options.margin) as f64;
    let steps = options.steps.max(1);
    let reach = options.reach as f64;
    let centre = board.item(pad).map(|it| {
        let b = it.shape.bbox();
        Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2)
    });
    let window = round.path.iter().map(|(_, p)| board::BBox::of([*p])).reduce(board::BBox::union).map(|b| b.grow(options.reach + 1_000_000));
    for step_idx in 0..steps {
        let fields = match options.plough {
            Plough::Gentle => plough_fields(board, &round.path, full / steps as f64, reach, net),
            _ => HashMap::new(),
        };
        let push = |p: Point, n: u32, layer: Option<usize>| -> Option<(Point, bool)> {
            // Vias hold still, and copper ending in one with them.
            let layer = layer?;
            if n == net {
                return None;
            }
            let (dx, dy) = match options.plough {
                Plough::Dipole => {
                    let c = centre?;
                    let (x, y) = ((p.x - c.x) as f64, (p.y - c.y) as f64);
                    let d = x.hypot(y);
                    if d == 0.0 || d > reach {
                        return None;
                    }
                    let by = full / steps as f64 * (-d / (reach / 3.0)).exp();
                    (x / d * by, y / d * by)
                }
                _ => fields.get(&layer)?.displacement(p),
            };
            (dx.hypot(dy) >= 1_000.0).then(|| (Point::new(dx.round() as i64, dy.round() as i64), false))
        };
        let before: Vec<(ItemId, Item)> = board
            .items()
            .filter(|(_, it)| movable(it) && window.is_none_or(|w| it.shape.bbox().overlaps(w)))
            .map(|(id, it)| (id, it.clone()))
            .collect();
        let soft_before = soft.clone();
        let first_new = board.items().map(|(id, _)| id).max().map_or(0, |m| m + 1);
        let (_, moved, moved_spans) = morph::shift_copper(board, soft, 75_000.0, &push);
        spans.extend(moved_spans);
        round.moved += moved;
        put_back_broken(board, soft, &soft_before, &before, first_new, options.pours_block, spans);
        if route_one(board, solver, options, soft, pad) {
            round.routed = true;
            on_step(board, round, step_idx);
            return;
        }
        on_step(board, round, step_idx);
    }
}

/// Puts every net whose copper now breaks a rule back as it was in `before`, over and over, since
/// a net put back can land on another that moved into its room.
/// Pieces with ids from `first_new` on were made by the step.
fn put_back_broken(board: &mut Board, soft: &mut Vec<ItemId>, soft_before: &[ItemId], before: &[(ItemId, Item)], first_new: ItemId, pours_block: bool, spans: &mut Vec<Span>) {
    let was: HashMap<ItemId, &Item> = before.iter().map(|(id, it)| (*id, it)).collect();
    let nets_before: HashSet<u32> = before.iter().map(|(_, it)| it.net).collect();
    let mut done: HashSet<u32> = HashSet::new();
    for _ in 0..6 {
        let broken: HashSet<u32> = board
            .items()
            .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked && nets_before.contains(&it.net) && !done.contains(&it.net))
            .filter(|(id, _)| !(if pours_block { board::check_item_with_pours(board, *id) } else { board::check_item(board, *id) }).is_empty())
            .map(|(_, it)| it.net)
            .collect();
        if broken.is_empty() {
            break;
        }
        for &net in &broken {
            // What the step made of the net's copper goes, new pieces and moved vias; what it
            // was comes back.
            let changed: Vec<ItemId> = board
                .items()
                .filter(|(_, it)| it.net == net && matches!(it.kind, Kind::Track | Kind::Via) && !it.locked)
                .filter(|(id, it)| *id >= first_new || was.get(id).is_some_and(|w| w.shape != it.shape))
                .map(|(id, _)| id)
                .collect();
            for id in changed {
                board.remove(id);
            }
            for (id, it) in before.iter().filter(|(_, it)| it.net == net) {
                if board.item(*id).is_none() {
                    if let Some(span) = it.span {
                        spans.retain(|s| *s != span);
                    }
                    board.restore(*id, (*it).clone());
                }
            }
        }
        done.extend(broken);
    }
    soft.retain(|id| board.item(*id).is_some());
    for id in soft_before {
        if board.item(*id).is_some() && !soft.contains(id) {
            soft.push(*id);
        }
    }
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

/// The warp field on each layer the path runs on, over the path's extent and `reach` beyond:
/// copper that stays put holds still, a strip `r` wide on either side of the path moves `r` away
/// from it, and between them the field is harmonic.
pub fn plough_fields(board: &Board, path: &[(usize, Point)], r: f64, reach: f64, net: u32) -> HashMap<usize, morph::WarpField> {
    const CELL: f64 = 60_000.0;
    let mut out = HashMap::new();
    let Some(bounds) = path.iter().map(|(_, p)| board::BBox::of([*p])).reduce(board::BBox::union) else { return out };
    let domain = bounds.grow(reach as Nm);
    // The path's straight runs on each layer.
    let mut runs: HashMap<usize, Vec<(Point, Point)>> = HashMap::new();
    let mut k = 0;
    while k + 1 < path.len() {
        let (layer, a) = path[k];
        let mut e = k + 1;
        if path[e].0 != layer {
            k += 1;
            continue;
        }
        let dir = (path[e].1.x - a.x, path[e].1.y - a.y);
        while e + 1 < path.len() && path[e + 1].0 == layer && (path[e + 1].1.x - path[e].1.x, path[e + 1].1.y - path[e].1.y) == dir {
            e += 1;
        }
        runs.entry(layer).or_default().push((a, path[e].1));
        k = e;
    }
    let still: Vec<(u64, board::Shape)> = board
        .items()
        .filter(|(_, it)| it.shape.bbox().overlaps(domain))
        .filter(|(_, it)| match it.kind {
            Kind::Pad { .. } | Kind::Keepout { .. } => true,
            Kind::Track => it.locked || it.net == net,
            // A via spans every layer, and the tracks on layers the path does not touch stay
            // where they are: so vias hold still, and tracks flow around them.
            Kind::Via => true,
            Kind::Pour { kept, .. } => kept,
        })
        .map(|(_, it)| (if it.hole.is_some() { u64::MAX } else { it.layers }, it.shape.clone()))
        .collect();
    for (layer, segments) in runs {
        let mut field = morph::WarpField::new(bounds, CELL, reach);
        for (layers, shape) in &still {
            if layers & (1 << layer) != 0 {
                field.add_inclusion(std::slice::from_ref(shape), (0.0, 0.0), false);
            }
        }
        for (a, b) in segments {
            let (dx, dy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
            let len = dx.hypot(dy);
            if len == 0.0 {
                continue;
            }
            let (nx, ny) = (-dy / len, dx / len);
            for side in [1.0, -1.0] {
                let off = |p: Point| Point::new(p.x + (side * nx * r).round() as i64, p.y + (side * ny * r).round() as i64);
                let strip = board::Shape::Polygon(vec![a, b, off(b), off(a)]);
                field.add_inclusion(&[strip], (side * nx * r, side * ny * r), false);
            }
        }
        field.solve();
        out.insert(layer, field);
    }
    out
}

/// Routes the open connection at `pad` on the ordinary grid; whether it was laid.
fn route_one(board: &mut Board, solver: &mut dyn FieldSolver, options: &ShoveOptions, soft: &mut Vec<ItemId>, pad: ItemId) -> bool {
    let Some(airline) = connection_of(board, pad, options.pours_block) else { return true };
    let far = if airline.from_item == pad { airline.to_item } else { airline.from_item };
    let profile = Profile::for_net(board, airline.net);
    let grid = ObstacleMap::new(board, profile, options.cell, options.pours_block).view(airline.net);
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
    fn region_whole_skips_duplicate_far_end_when_walled_and_frees_multi_net_pocket() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal))
             (net 0 \"\") (net 1 \"A\") (net 2 \"B\") (net 3 \"C\")
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
        // First check a locked-wall connection: region_whole tries only one end per pass.
        let mut walled = board.clone();
        walled.insert(pad(1, 5.0, 10.0));
        walled.insert(pad(1, 15.0, 10.0));
        walled.insert(Item {
            kind: Kind::Track,
            net: 0,
            layers: 1,
            shape: board::Shape::Capsule { a: Point::new(mm(10.0), mm(0.0)), b: Point::new(mm(10.0), mm(20.0)), r: mm(0.5) },
            hole: None,
            locked: true,
            span: None,
        });
        let w_opts = ShoveOptions { plough: Plough::Region, region_whole: true, reach: mm(0.5), ..Default::default() };
        let w_stats = shove_escape(&mut walled, &mut crate::route::CpuSolver, &w_opts, &mut Vec::new(), &mut |_, _| {});
        assert_eq!(w_stats.tries.len(), 1, "symmetrical undone try marks the far end tried too");

        // Now trap A behind B's track at x = 6.3 mm and C's track at x = 7.05 mm (> 0.5 mm reach
        // from A's soft crossing through B when B cannot step east without C moving).
        board.insert(pad(1, 5.0, 10.0));
        board.insert(pad(1, 15.0, 10.0));
        board.insert(pad(2, 2.0, 3.0));
        board.insert(pad(2, 2.0, 17.0));
        board.insert(pad(3, 2.0, 1.5));
        board.insert(pad(3, 2.0, 18.5));
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
        let opts = ShoveOptions { cell: 35_000, plough: Plough::Region, reach: mm(0.5), ..Default::default() };
        assert!(opts.region_whole);
        let stats = shove_escape(&mut board, &mut crate::route::CpuSolver, &opts, &mut soft, &mut |_, _| {});
        assert!(stats.tries.iter().any(|t| t.kept && t.routed), "{stats:?}");
        assert!(connectivity_without_pours(&board).airlines.is_empty());
        for &id in &soft {
            assert!(board::check_item(&board, id).is_empty(), "{:?}", board.item(id));
        }
    }
}
