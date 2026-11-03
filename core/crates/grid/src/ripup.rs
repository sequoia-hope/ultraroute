//! Rip-up and reroute for the connections the other passes left open.
//!
//! Copper the router laid itself is soft: a blocked connection may route through it at a price.
//! Whatever soft copper its new route actually conflicts with is ripped, the connection laid, and
//! the ripped connections routed again. The board is kept only if fewer connections are open
//! than before; otherwise it is put back as it was.

use std::collections::{HashMap, HashSet};

use board::{check_item, connectivity, connectivity_without_pours, Airline, BBox, Board, Connectivity, Item, ItemId, Nm, Point};

use crate::grid::{ObstacleMap, Profile, RouteGrid};
use crate::report::{tell, Focus, Log};
use crate::route::{
    cluster_pads, corridor_terminals, corridor_widths, corridor_window, ends_with_pads, narrow_clipped, pieces, pieces_corridor, route_open,
    stretches, terminals, FieldSolver, Options, Stats, Stretch, CORRIDOR_STRETCH, FINE_CELL_FLOOR, FINE_NODES, NARROW_CELL_PENALTY,
};

#[derive(Debug, Clone)]
pub struct RipOptions {
    pub cell: board::Nm,
    pub via_cost: u32,
    /// Price of entering a cell that soft copper of another net claims.
    pub rip_cost: u32,
    pub deadline: crate::clock::Deadline,
    /// Time allowed for rerouting what one attempt ripped.
    pub reroute_seconds: f64,
    pub pours_block: bool,
    /// The repair after a region plough: a connection may end at a pad of its cluster near the
    /// cut it ends at; on a board with planes, plane nets' copper is cheap to rip and joined to
    /// its planes again after, and only signal connections are tried and counted first; when
    /// rerouting one after another leaves the ripped nets no better, they are negotiated
    /// together, the connection's net with them; a change kept prunes closed nets' stubs.
    pub region_repair: bool,
    /// A connection boxed in, one whose end has no terminal until the soft copper round it is
    /// lifted or that finds no way even ripping, is tried with that copper lifted and then on
    /// cells half the size in a window round it (vias priced where they would rip there); one so
    /// laid is held while what it ripped is rerouted, negotiated, and given one more rip-up.
    /// Connections not boxed in are tried as without it.
    pub rip_boxed: bool,
    pub log: Option<Log>,
    /// Rip-up of the last resort, once every stage that keeps the nets' own widths is done: a
    /// round of corridors only first, narrowing the connections still open across the tight
    /// gaps their own width cannot pass (`route::corridor_widths`), and another whenever a
    /// round keeps nothing; ordinary rounds after any kept. Off, every net keeps its width.
    pub corridor: bool,
}

impl Default for RipOptions {
    fn default() -> RipOptions {
        RipOptions {
            cell: 50_000,
            via_cost: 80,
            rip_cost: 400,
            deadline: Default::default(),
            reroute_seconds: 10.0,
            pours_block: false,
            region_repair: false,
            rip_boxed: false,
            log: None,
            corridor: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RipStats {
    pub attempts: usize,
    /// Attempts kept because they left fewer connections open.
    pub kept: usize,
    pub ripped: usize,
    /// Attempts lost by cause: no route even through soft copper, a route that met copper that
    /// may not be ripped, and a result no better than before.
    pub no_path: usize,
    pub hard_conflict: usize,
    pub no_better: usize,
    /// Connections laid narrowed to their pins the whole way (`route::narrow_run`), by their
    /// two ends, with the width: by the attempts kept, and by the rerouting after them.
    pub narrowed: Vec<(ItemId, ItemId, Nm)>,
    /// Connections laid narrowed only across tight gaps (`RipOptions::corridor`), and each such
    /// stretch, by the connection's two ends, with its width and length.
    pub corridor: usize,
    pub corridors: Vec<(ItemId, ItemId, Nm, Nm)>,
    /// Attempts at boxed-in connections (`RipOptions::rip_boxed`): those that found a way, and
    /// those of them found only on the finer window.
    pub boxed: usize,
    pub fine: usize,
}

/// At most so many connections a corridor round tries: each may build a fine map round itself.
const CORRIDOR_TRIES: usize = 32;

fn joined(board: &Board, pours_block: bool) -> Connectivity {
    if pours_block { connectivity(board) } else { connectivity_without_pours(board) }
}

/// At most so many nets ripped for one connection are negotiated over together.
const NEGOTIATED: usize = 8;

/// The work a boxed-in connection may take, counted rather than timed so the result is the same
/// on any machine: finer windows searched in one rip-up, negotiation rounds over what one ripped,
/// connections routed on finer windows after, and attempts in the rip-up that follows.
const FINE_RIPS: usize = 12;
const BOXED_ROUNDS: usize = 10;
const FINE_REROUTES: usize = 6;
const HOP_ATTEMPTS: usize = 4;

/// One connection's rip-up attempt, shown to [`rip_up_watch`] before an unsuccessful try reverts.
pub struct RipAttempt<'a> {
    pub airline: &'a Airline,
    pub path: Vec<(usize, Point)>,
    pub ripped: Vec<Item>,
    pub laid: Vec<ItemId>,
    pub open_before: usize,
    pub open_after: usize,
    /// `"kept"`, `"no better"`, `"hard conflict"`, or `"no path"`.
    pub outcome: &'static str,
}

/// Tries every open connection once per round, while rounds keep closing connections; a round
/// that closes any lets the next try them all again. `soft`
/// holds the copper the router laid; it is updated as copper is ripped and laid.
pub fn rip_up(board: &mut Board, solver: &mut dyn FieldSolver, options: &RipOptions, soft: &mut Vec<ItemId>, watch: &mut dyn FnMut(&Board)) -> RipStats {
    rip_up_watch(board, solver, options, soft, &mut |b, a| {
        if a.outcome == "kept" {
            watch(b);
        }
    })
}

/// As [`rip_up`], reporting every attempt and its board state before any revert.
pub fn rip_up_watch(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &RipOptions,
    soft: &mut Vec<ItemId>,
    watch: &mut dyn FnMut(&Board, RipAttempt),
) -> RipStats {
    rip_up_inner(board, solver, options, soft, None, None, watch)
}

/// As [`rip_up_watch`], trying only `only_nets`' connections when given, and with `attempts`, at
/// most so many in one round.
fn rip_up_inner(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &RipOptions,
    soft: &mut Vec<ItemId>,
    only_nets: Option<&HashSet<u32>>,
    attempts: Option<usize>,
    watch: &mut dyn FnMut(&Board, RipAttempt),
) -> RipStats {
    let out_of_time = || options.deadline.expired();
    let mut stats = RipStats::default();
    let mut hard: HashMap<String, ObstacleMap> = HashMap::new();
    let mut soft_maps: HashMap<String, ObstacleMap> = HashMap::new();
    let mut grid = crate::grid::RouteGrid {
        net: 0,
        x0: 0,
        y0: 0,
        h: options.cell,
        nx: 0,
        ny: 0,
        layers: Vec::new(),
        half_width: 0,
        via: None,
        blocked: Vec::new(),
        via_blocked: Vec::new(),
        penalty: Vec::new(),
        via_penalty: Vec::new(),
        region_cost: Vec::new(),
        pours_block: options.pours_block,
        directional: false,
        deadline: options.deadline.clone(),
        spokes: None,
    };
    // Plane nets' connections are the planes' to make (`join_planes`), not rip-up's.
    let plane_nets: HashSet<u32> = if options.region_repair && !options.pours_block && board.planes != 0 {
        board.items().filter(|(_, it)| board.is_plane(it) && it.net != 0).map(|(_, it)| it.net).collect()
    } else {
        HashSet::new()
    };
    // Signal connections open, then all: what an attempt must leave fewer of.
    let score = |board: &Board| {
        let open = joined(board, options.pours_block).airlines;
        (open.iter().filter(|a| !plane_nets.contains(&a.net)).count(), open.len())
    };
    let mut plane_maps: HashMap<String, ObstacleMap> = HashMap::new();
    // The clusters and their pads, for ends at pads; made again after a kept attempt.
    let mut near: Option<(Connectivity, HashMap<usize, Vec<ItemId>>)> = None;
    let mut tried: HashSet<(ItemId, ItemId)> = HashSet::new();
    // A round of corridors only, first and once a round keeps nothing: the connections it has
    // tried since the last kept attempt, the nets laid by one (their ripped copper is rerouted
    // with corridors too), and its maps, good until an attempt is kept.
    let mut corridors = options.corridor;
    let mut cornered: HashSet<(ItemId, ItemId)> = HashSet::new();
    let mut corridor_nets: HashSet<u32> = HashSet::new();
    let mut corridor_maps: HashMap<String, ObstacleMap> = HashMap::new();
    let mut full_grid = grid.clone();
    // Connections given a finer window once, and the windows left to give.
    let mut fine_tried: HashSet<(ItemId, ItemId)> = HashSet::new();
    let mut fine_left = FINE_RIPS;
    loop {
        let mut open = joined(board, options.pours_block).airlines;
        open.retain(|a| !plane_nets.contains(&a.net) && only_nets.is_none_or(|ns| ns.contains(&a.net)));
        open.sort_by(|a, b| a.from.distance(a.to).total_cmp(&b.from.distance(b.to)));
        open.retain(|a| !tried.contains(&(a.from_item, a.to_item)));
        if corridors {
            open.retain(|a| !cornered.contains(&(a.from_item, a.to_item)));
        }
        if open.is_empty() || out_of_time() {
            break;
        }
        let kept_before = stats.kept;
        let mut corridor_tries = 0;
        for airline in open {
            if out_of_time() || attempts.is_some_and(|n| stats.attempts >= n) {
                break;
            }
            tried.insert((airline.from_item, airline.to_item));
            if board.item(airline.from_item).is_none() || board.item(airline.to_item).is_none() {
                continue;
            }
            let base = Profile::for_net(board, airline.net);
            let corridor_hws = if corridors { corridor_widths(board, &base, &airline) } else { Vec::new() };
            if corridors {
                if corridor_hws.is_empty() {
                    continue;
                }
                if corridor_tries == CORRIDOR_TRIES {
                    break;
                }
                corridor_tries += 1;
                cornered.insert((airline.from_item, airline.to_item));
            }
            stats.attempts += 1;
            let score_before = score(board);
            let before = score_before.1;
            let net_name = board.net_name(airline.net).to_string();
            tell(
                &options.log,
                "ripping up",
                || format!("{net_name}: looking for a way through the router's own copper"),
                Some(Focus::Connection { net: airline.net, from: airline.from, to: airline.to }),
            );
            // Locked copper stays, even the router's own: a caller holds still what it must keep.
            let soft_set: HashSet<ItemId> = soft.iter().copied().filter(|id| board.item(*id).is_some_and(|it| !it.locked)).collect();
            // A corridor round's try (`try_corridor`), or the net's own width and the narrow run.
            let mut boxed = false;
            let found = if corridors {
                let ctx = CorridorTry { options, plane_nets: &plane_nets, soft_set: &soft_set, base: &base, airline: &airline };
                try_corridor(board, solver, &ctx, &corridor_hws, &mut corridor_maps, &mut grid, &mut full_grid, &mut near)
                    .map(|(path, new, stretches)| (path, new, None, Some(stretches)))
            } else {
                // The net's own width, then, for a short current net that finds no way at it, its
                // pins' width the whole way (`route::narrow_run`), each on maps of its own.
                let mut widths = vec![(base.clone(), base.class.clone())];
                if let Some(hw) = crate::route::narrow_run(board, &base, &airline) {
                    let mut narrow = base.clone();
                    (narrow.half_width, narrow.narrow_half_width) = (hw, hw);
                    widths.push((narrow, format!("{}\u{0}{hw}", base.class)));
                }
                let mut found = None;
                for (profile, key) in widths {
                    // The narrow run's maps cover its neighbourhood only, made for this try alone.
                    let window = (profile.half_width < base.half_width).then(|| crate::route::narrow_window(&airline));
                    let plane = |id: ItemId| board.item(id).is_some_and(|it| plane_nets.contains(&it.net));
                    let made = |skip: &dyn Fn(ItemId) -> bool| match window {
                        Some(b) => ObstacleMap::window_without(board, profile.clone(), options.cell, b, options.pours_block, skip),
                        None => ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, skip),
                    };
                    let (hard_own, soft_own, plane_own);
                    let hard_map: &ObstacleMap = if window.is_some() {
                        hard_own = made(&|id| soft_set.contains(&id));
                        &hard_own
                    } else {
                        hard.entry(key.clone()).or_insert_with(|| made(&|id| soft_set.contains(&id)))
                    };
                    hard_map.view_into(airline.net, &mut grid);
                    let soft_map: &ObstacleMap = if window.is_some() {
                        soft_own = made(&|id| !soft_set.contains(&id) || plane(id));
                        &soft_own
                    } else {
                        soft_maps.entry(key.clone()).or_insert_with(|| made(&|id| !soft_set.contains(&id) || plane(id)))
                    };
                    // A plane net's soft copper is mostly vias joining pads to the planes, which
                    // `join_planes` puts back elsewhere: cheap to rip.
                    let plane_map: Option<&ObstacleMap> = if plane_nets.is_empty() {
                        None
                    } else if window.is_some() {
                        plane_own = made(&|id| !soft_set.contains(&id) || !plane(id));
                        Some(&plane_own)
                    } else {
                        Some(&*plane_maps.entry(key.clone()).or_insert_with(|| made(&|id| !soft_set.contains(&id) || !plane(id))))
                    };
                    let plane_cost = (options.rip_cost / 40).max(5);
                    let price = |soft: bool, plane: bool| if soft { options.rip_cost } else if plane { plane_cost } else { 0 };
                    grid.penalty.clear();
                    let nodes = grid.blocked.len();
                    let net = airline.net;
                    grid.penalty.extend((0..nodes).map(|n| price(soft_map.blocked_at(net, n), plane_map.is_some_and(|m| m.blocked_at(net, n)))));
                    if options.region_repair {
                        // A via is priced where it would rip, as a track is.
                        grid.via_penalty.resize(grid.cell_count(), 0);
                        for (c, p) in grid.via_penalty.iter_mut().enumerate() {
                            *p += price(soft_map.via_blocked_at(net, c), plane_map.is_some_and(|m| m.via_blocked_at(net, c)));
                        }
                    }
                    let (sources, targets) = if options.region_repair {
                        let (conn, pads) = near.get_or_insert_with(|| {
                            let conn = joined(board, options.pours_block);
                            let pads = cluster_pads(board, &conn);
                            (conn, pads)
                        });
                        let mut end = |id: ItemId| {
                            let pads = conn.cluster.get(&id).and_then(|k| pads.get(k)).map_or(&[][..], Vec::as_slice);
                            ends_with_pads(board, &grid, id, pads, &terminals)
                        };
                        (end(airline.from_item), end(airline.to_item))
                    } else {
                        (terminals(board, &grid, airline.from_item), terminals(board, &grid, airline.to_item))
                    };
                    // An end walled in by the router's own copper has no terminal of its own.
                    let (sources, targets) = if options.rip_boxed && (sources.is_empty() || targets.is_empty()) {
                        boxed = true;
                        lifted_ends(board, &grid, &airline, &soft_set, &mut near, options.pours_block)
                    } else {
                        (sources, targets)
                    };
                    if sources.is_empty() || targets.is_empty() {
                        continue;
                    }
                    let source_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
                    let target_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
                    if let Some(path) = solver.route(&grid, &source_cells, &target_cells, options.via_cost) {
                        found = Some((path, sources, targets, profile.half_width < base.half_width));
                        break;
                    }
                }
                if found.is_none() && options.rip_boxed && fine_left > 0 && !out_of_time() && fine_tried.insert((airline.from_item, airline.to_item)) {
                    fine_left -= 1;
                    found = fine_rip_path(board, solver, options, &airline, &base, &soft_set, &plane_nets, &mut near, &mut grid);
                    boxed = found.is_some();
                    stats.fine += usize::from(boxed);
                }
                stats.boxed += usize::from(boxed && found.is_some());
                match found {
                    None => None,
                    Some((path, sources, targets, narrowed)) => {
                        let path_pts: Vec<(usize, Point)> = path.iter().map(|&(l, i, j)| (grid.layers[l], grid.centre(i, j))).collect();
                        let Some(new) = pieces(&grid, &path, &sources, &targets) else { continue };
                        Some((path_pts, new, narrowed.then_some(2 * grid.half_width), None))
                    }
                }
            };
            let Some((path_pts, new, narrowed, stretches)) = found else {
                stats.no_path += 1;
                tell(&options.log, "ripping up", || format!("{net_name}: no way through, even ripping"), None);
                watch(board, RipAttempt { airline: &airline, path: Vec::new(), ripped: Vec::new(), laid: Vec::new(), open_before: before, open_after: before, outcome: "no path" });
                continue;
            };

            let snapshot = board.clone();
            let soft_before = soft.clone();
            let mut ripped_items = Vec::new();
            let mut laid = Vec::new();
            let mut ok = true;
            'pieces: for piece in new {
                let id = board.insert(piece);
                laid.push(id);
                for violation in check_item(board, id) {
                    let Some(other) = violation.b else {
                        ok = false;
                        break 'pieces;
                    };
                    // The router's own copper of any net, this net's too (a via of the net may stand
                    // too near another of its holes); what comes loose is rerouted below.
                    let rippable = soft_set.contains(&other) && !laid.contains(&other);
                    if !rippable {
                        ok = false;
                        break 'pieces;
                    }
                    if let Some(it) = board.remove(other) {
                        ripped_items.push(it);
                    }
                }
                if !check_item(board, id).is_empty() {
                    ok = false;
                    break;
                }
            }
            if !ok {
                stats.hard_conflict += 1;
                tell(&options.log, "ripping up", || format!("{net_name}: blocked by copper that may not be ripped"), None);
                *board = snapshot;
                watch(board, RipAttempt { airline: &airline, path: path_pts, ripped: ripped_items, laid: Vec::new(), open_before: before, open_after: before, outcome: "hard conflict" });
                continue;
            }
            let ripped = ripped_items.len();
            let reroute = Options {
                seed: 0,
                cell: options.cell,
                via_cost: options.via_cost,
                deadline: options.deadline.seconds(options.reroute_seconds),
                nets: None,
                pours_block: options.pours_block,
                directional: false,
                log: options.log.clone(),
                net_via_cost: Default::default(),
                fine: true,
                corridor: false,
            };
            let ripped_nets: HashSet<u32> = ripped_items.iter().map(|it| it.net).filter(|n| *n != 0 && !plane_nets.contains(n)).collect();
            let mut rerouted = Stats::default();
            let (all_laid, score_after) = if boxed {
                tell(&options.log, "ripping up", || format!("{net_name}: boxed in; ripped {ripped} pieces, rerouting them behind it"), None);
                priority_restart(board, solver, options, soft, &soft_before, &laid, &ripped_nets, &airline, &reroute, &plane_nets, score_before, only_nets.is_none(), &score, &out_of_time)
            } else {
                soft.extend(laid.iter().copied());
                // The nets a corridor ripped, and ripped nets laid through a corridor before, may take
                // one again, they alone: the others reroute as with no corridor anywhere.
                let renarrow: HashSet<u32> =
                    ripped_items.iter().map(|it| it.net).filter(|n| *n != 0 && (stretches.is_some() || corridor_nets.contains(n))).collect();
                // A corridor that ripped nothing left nothing to reroute.
                let quiet = stretches.is_some() && ripped == 0;
                if !quiet {
                    tell(&options.log, "ripping up", || format!("{net_name}: ripped {ripped} pieces, rerouting them"), None);
                    reroute_into(board, solver, &reroute, &renarrow, &mut rerouted);
                }
                let mut again = std::mem::take(&mut rerouted.laid);
                if !plane_nets.is_empty() && !quiet {
                    again.extend(crate::planes::join_planes(board, &None, &options.deadline, None).laid);
                }
                let mut score_after = score(board);
                // One after another, the first rerouted may take the only lane of the next. The
                // ripped nets and this connection's are negotiated together instead, at one weight.
                if options.region_repair && score_after >= score_before && (1..=NEGOTIATED).contains(&ripped_nets.len()) && !out_of_time() {
                    for &id in laid.iter().chain(&again) {
                        board.remove(id);
                    }
                    laid.clear();
                    again.clear();
                    rerouted = Stats::default();
                    soft.retain(|id| board.item(*id).is_some());
                    let mut nets = ripped_nets.clone();
                    nets.insert(airline.net);
                    let negotiated = crate::negotiate::route_negotiated(
                        board,
                        solver,
                        &crate::negotiate::NegotiationOptions {
                            cell: options.cell,
                            via_cost: options.via_cost,
                            deadline: options.deadline.seconds(options.reroute_seconds),
                            nets: Some(nets),
                            pours_block: options.pours_block,
                            rip_blocked: false,
                            pad_ends: true,
                            directional: false,
                            ..Default::default()
                        },
                        &mut |_| {},
                        &mut |_, _| {},
                    );
                    again.extend(negotiated.laid);
                    reroute_into(board, solver, &reroute, &renarrow, &mut rerouted);
                    again.append(&mut rerouted.laid);
                    if !plane_nets.is_empty() {
                        again.extend(crate::planes::join_planes(board, &None, &options.deadline, None).laid);
                    }
                    score_after = score(board);
                }
                let mut all_laid = laid;
                all_laid.extend(again.iter().copied());
                soft.extend(again);
                (all_laid, score_after)
            };
            let after = score_after.1;
            if score_after < score_before {
                if options.region_repair || boxed {
                    let open: HashSet<u32> = joined(board, options.pours_block).airlines.iter().map(|a| a.net).collect();
                    crate::cleanup::prune_dangling_of(board, soft, &|it| !it.locked && !open.contains(&it.net), &mut |_, _| {});
                    near = None;
                }
                stats.kept += 1;
                stats.ripped += ripped;
                if let Some(w) = narrowed {
                    stats.narrowed.push((airline.from_item, airline.to_item, w));
                }
                stats.narrowed.append(&mut rerouted.narrowed);
                if let Some(stretches) = stretches {
                    stats.corridor += 1;
                    stats.corridors.extend(stretches.into_iter().map(|(w, l)| (airline.from_item, airline.to_item, 2 * w, l)));
                    corridor_nets.insert(airline.net);
                }
                stats.corridor += rerouted.corridor;
                corridor_nets.extend(rerouted.corridors.iter().filter_map(|c| board.item(c.0).map(|it| it.net)));
                stats.corridors.append(&mut rerouted.corridors);
                soft_maps.clear();
                plane_maps.clear();
                corridor_maps.clear();
                cornered.clear();
                tell(&options.log, "ripping up", || format!("{net_name}: kept, {after} connections open"), None);
                watch(board, RipAttempt { airline: &airline, path: path_pts, ripped: ripped_items, laid: all_laid, open_before: before, open_after: after, outcome: "kept" });
            } else {
                stats.no_better += 1;
                tell(&options.log, "ripping up", || format!("{net_name}: no better, put back"), None);
                watch(board, RipAttempt { airline: &airline, path: path_pts, ripped: ripped_items, laid: all_laid, open_before: before, open_after: after, outcome: "no better" });
                *board = snapshot;
                *soft = soft_before;
            }
        }
        if stats.kept == kept_before {
            if options.corridor && !corridors && !out_of_time() {
                corridors = true;
                tried.clear();
                continue;
            }
            break;
        }
        if attempts.is_some() {
            break;
        }
        corridors = false;
        // What was ripped and rerouted may have opened room for connections tried before.
        tried.clear();
    }
    soft.retain(|id| board.item(*id).is_some());
    stats
}

/// Reroutes what an attempt ripped, as `reroute` says, into `into`; then the nets of `renarrow`
/// still open, which were laid through a corridor before, with corridors and alone: every
/// other net reroutes as it would with no corridor anywhere.
fn reroute_into(board: &mut Board, solver: &mut dyn FieldSolver, reroute: &Options, renarrow: &HashSet<u32>, into: &mut Stats) {
    let mut add = |s: Stats| {
        into.laid.extend(s.laid);
        into.narrowed.extend(s.narrowed);
        into.corridor += s.corridor;
        into.corridors.extend(s.corridors);
    };
    add(route_open(board, solver, reroute, &mut |_| {}));
    if renarrow.is_empty() || reroute.deadline.expired() {
        return;
    }
    let corridor = Options { nets: Some(renarrow.clone()), corridor: true, ..reroute.clone() };
    add(route_open(board, solver, &corridor, &mut |_| {}));
}

/// A corridor try's find: its way, its pieces, and their narrowed stretches.
type Corridor = (Vec<(usize, Point)>, Vec<Item>, Vec<Stretch>);

/// One map a corridor try needs: its key, what it leaves out, and the widths it is made for.
type Build<'a> = (&'a String, &'a dyn Fn(ItemId) -> bool, &'a Profile);

/// What a corridor try reads of its attempt.
struct CorridorTry<'a> {
    options: &'a RipOptions,
    plane_nets: &'a HashSet<u32>,
    soft_set: &'a HashSet<ItemId>,
    base: &'a Profile,
    airline: &'a Airline,
}

/// The maps a corridor try of `ctx` routes on at one width: copper that may not be ripped, soft
/// copper, and plane nets' soft copper at the narrow half width `hw` (priced as a whole-width
/// try prices them), and all copper at the net's own width, for where the track may widen
/// again. On the board's own grid `cached` keeps them for the round, the full-width one for
/// every width; a finer `window`'s serve one try, and a window whose ends have no way out of
/// its narrow map builds no more. `None` then, or when the deadline passes first.
fn corridor_maps<'m>(
    board: &mut Board,
    ctx: &CorridorTry,
    hw: Nm,
    window: Option<(Nm, BBox)>,
    cached: &'m mut HashMap<String, ObstacleMap>,
) -> Option<[&'m ObstacleMap; 4]> {
    let options = ctx.options;
    let mut narrow = ctx.base.clone();
    (narrow.half_width, narrow.narrow_half_width) = (hw, ctx.base.narrow_half_width.min(hw));
    let plane_soft: HashSet<ItemId> =
        ctx.soft_set.iter().copied().filter(|&id| board.item(id).is_some_and(|it| ctx.plane_nets.contains(&it.net))).collect();
    let hard = |id: ItemId| ctx.soft_set.contains(&id);
    let soft = |id: ItemId| !ctx.soft_set.contains(&id) || plane_soft.contains(&id);
    let plane = |id: ItemId| !plane_soft.contains(&id);
    let all = |_: ItemId| false;
    if window.is_some() {
        cached.retain(|k, _| !k.starts_with("window"));
    }
    let (at, full) = match window {
        Some(_) => ("window".to_string(), "window\u{0}all".to_string()),
        None => (format!("{}\u{0}c{hw}", ctx.base.class), format!("{}\u{0}all", ctx.base.class)),
    };
    let (hard_key, soft_key) = (format!("{at}\u{0}hard"), format!("{at}\u{0}soft"));
    // With no plane nets the plane map would be empty: the soft one stands in, priced at nothing.
    let plane_key = if ctx.plane_nets.is_empty() { soft_key.clone() } else { format!("{at}\u{0}plane") };
    let builds: [Build; 4] =
        [(&hard_key, &hard, &narrow), (&soft_key, &soft, &narrow), (&plane_key, &plane, &narrow), (&full, &all, ctx.base)];
    for (k, (key, skip, profile)) in builds.into_iter().enumerate() {
        if cached.contains_key(key) {
            continue;
        }
        if options.deadline.expired() {
            return None;
        }
        let map = match window {
            Some((cell, bounds)) => ObstacleMap::window_without(board, profile.clone(), cell, bounds, options.pours_block, skip),
            None => ObstacleMap::without_by(board, profile.clone(), options.cell, options.pours_block, skip, &options.deadline)?,
        };
        cached.insert(key.clone(), map);
        if k == 0 && window.is_some() {
            let grid = cached[key].view(ctx.airline.net);
            if terminals(board, &grid, ctx.airline.from_item).is_empty() || terminals(board, &grid, ctx.airline.to_item).is_empty() {
                return None;
            }
        }
    }
    Some([&cached[&hard_key], &cached[&soft_key], &cached[&plane_key], &cached[&full]])
}

/// One corridor try at `ctx`'s connection, widest corridor width first, each on the board's own
/// grid and then on a finer one round the connection: a route narrowed only where the net's own
/// width finds no room, priced through soft copper as any attempt is. The way, its pieces, and
/// their narrowed stretches; `None` when no corridor goes, or the deadline passes first.
#[allow(clippy::too_many_arguments)]
fn try_corridor(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    ctx: &CorridorTry,
    widths: &[Nm],
    cached: &mut HashMap<String, ObstacleMap>,
    grid: &mut RouteGrid,
    full_grid: &mut RouteGrid,
    near: &mut Option<(Connectivity, HashMap<usize, Vec<ItemId>>)>,
) -> Option<Corridor> {
    let (options, airline) = (ctx.options, ctx.airline);
    let fine = options.cell / 2;
    let cwin = corridor_window(airline);
    let cnodes = ((cwin.max.x - cwin.min.x) / fine.max(1)) as f64 * ((cwin.max.y - cwin.min.y) / fine.max(1)) as f64 * board.layers.len() as f64;
    let plane_cost = (options.rip_cost / 40).max(5);
    let price = |soft: bool, plane: bool| if soft { options.rip_cost } else if plane { plane_cost } else { 0 };
    let net = airline.net;
    let planes = !ctx.plane_nets.is_empty();
    for &hw in widths {
        let mut windows = vec![None];
        if fine >= FINE_CELL_FLOOR && cnodes <= FINE_NODES {
            windows.push(Some((fine, cwin)));
        }
        for window in windows {
            if options.deadline.expired() {
                return None;
            }
            let Some([hard, soft, plane, all]) = corridor_maps(board, ctx, hw, window, cached) else { continue };
            hard.view_into(net, grid);
            all.view_into(net, full_grid);
            let full_free = |n: usize| !full_grid.blocked[n];
            grid.penalty.clear();
            let nodes = grid.blocked.len();
            grid.penalty.extend((0..nodes).map(|n| {
                price(soft.blocked_at(net, n), planes && plane.blocked_at(net, n)) + if full_free(n) { 0 } else { NARROW_CELL_PENALTY }
            }));
            if options.region_repair {
                grid.via_penalty.resize(grid.cell_count(), 0);
                for (c, p) in grid.via_penalty.iter_mut().enumerate() {
                    *p += price(soft.via_blocked_at(net, c), planes && plane.via_blocked_at(net, c));
                }
            }
            let ends = |b: &mut Board, g: &RouteGrid, item: ItemId| corridor_terminals(b, full_grid, g, &full_free, item);
            let (sources, targets) = if options.region_repair {
                let (conn, pads) = near.get_or_insert_with(|| {
                    let conn = joined(board, options.pours_block);
                    let pads = cluster_pads(board, &conn);
                    (conn, pads)
                });
                let mut end = |id: ItemId| {
                    let pads = conn.cluster.get(&id).and_then(|k| pads.get(k)).map_or(&[][..], Vec::as_slice);
                    ends_with_pads(board, grid, id, pads, &ends)
                };
                (end(airline.from_item), end(airline.to_item))
            } else {
                (ends(board, grid, airline.from_item), ends(board, grid, airline.to_item))
            };
            if sources.is_empty() || targets.is_empty() {
                continue;
            }
            let source_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
            let target_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
            let Some(path) = solver.route(grid, &source_cells, &target_cells, options.via_cost) else { continue };
            let Some(mut new) = pieces_corridor(grid, ctx.base.half_width, &full_free, &path, &sources, &targets) else { continue };
            narrow_clipped(board, &mut new, ctx.base.half_width, hw, &|b: &Board, id| check_item(b, id).is_empty());
            let Some(narrowed) = stretches(&new, ctx.base.half_width) else {
                let name = board.net_name(net).to_string();
                let long = CORRIDOR_STRETCH as f64 * 1e-6;
                tell(&options.log, "ripping up", || format!("{name}: a corridor {:.2} mm wide narrows for over {long:.1} mm, refused", 2.0 * hw as f64 * 1e-6), None);
                continue;
            };
            let path_pts = path.iter().map(|&(l, i, j)| (grid.layers[l], grid.centre(i, j))).collect();
            return Some((path_pts, new, narrowed));
        }
    }
    None
}

/// Terminals of `end`, lifting unlocked soft copper right around `end` when `terminals` finds
/// none so a pad crowded by router copper can still launch a rip-up path.
fn terminals_lifting_soft(
    board: &mut Board,
    grid: &crate::grid::RouteGrid,
    end: ItemId,
    soft_set: &HashSet<ItemId>,
) -> Vec<crate::route::Terminal> {
    let found = terminals(board, grid, end);
    let Some(bb) = board.bbox(end).filter(|_| found.is_empty()) else { return found };
    let around = bb.grow((grid.h * 12).max(450_000));
    let near: HashSet<ItemId> = (0..board.layers.len())
        .flat_map(|l| board.near(l, around))
        .filter(|id| soft_set.contains(id))
        .collect();
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

/// The ends of `airline` on `grid`, from the pads of its clusters nearest each end too, each
/// with the soft copper round it lifted where it has no terminal otherwise.
fn lifted_ends(
    board: &mut Board,
    grid: &crate::grid::RouteGrid,
    airline: &Airline,
    soft_set: &HashSet<ItemId>,
    near: &mut Option<(Connectivity, HashMap<usize, Vec<ItemId>>)>,
    pours_block: bool,
) -> (Vec<crate::route::Terminal>, Vec<crate::route::Terminal>) {
    let (conn, pads) = near.get_or_insert_with(|| {
        let conn = joined(board, pours_block);
        let pads = cluster_pads(board, &conn);
        (conn, pads)
    });
    let lift = |b: &mut Board, g: &crate::grid::RouteGrid, id: ItemId| terminals_lifting_soft(b, g, id, soft_set);
    let mut end = |id: ItemId| {
        let pads = conn.cluster.get(&id).and_then(|k| pads.get(k)).map_or(&[][..], Vec::as_slice);
        ends_with_pads(board, grid, id, pads, &lift)
    };
    (end(airline.from_item), end(airline.to_item))
}

/// A local window at `fine` cell size around `airline` wide enough to detour past neighbour
/// header pins, falling back to `narrow_window` when the wider window exceeds `FINE_NODES`.
fn rip_fine_window(airline: &Airline, fine: Nm, layers: usize) -> Option<board::BBox> {
    let fits = |b: board::BBox| {
        let nodes = ((b.max.x - b.min.x) / fine.max(1)) as f64
            * ((b.max.y - b.min.y) / fine.max(1)) as f64
            * layers as f64;
        nodes <= crate::route::FINE_NODES
    };
    let wide = board::BBox::of([airline.from, airline.to]).grow(6_000_000);
    if fits(wide) {
        return Some(wide);
    }
    let narrow = crate::route::narrow_window(airline);
    fits(narrow).then_some(narrow)
}

type FoundRip = (crate::search::GridPath, Vec<crate::route::Terminal>, Vec<crate::route::Terminal>, bool);

/// Tries `airline` on cells half the standard size inside a local window, for pin gaps (such as
/// 2.54 mm headers) that only open once the grid margin halves.
#[allow(clippy::too_many_arguments)]
fn fine_rip_path(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &RipOptions,
    airline: &Airline,
    base: &Profile,
    soft_set: &HashSet<ItemId>,
    plane_nets: &HashSet<u32>,
    near: &mut Option<(Connectivity, HashMap<usize, Vec<ItemId>>)>,
    grid: &mut crate::grid::RouteGrid,
) -> Option<FoundRip> {
    let fine = options.cell / 2;
    if fine < crate::route::FINE_CELL_FLOOR {
        return None;
    }
    let win = rip_fine_window(airline, fine, board.layers.len())?;
    let plane = |id: ItemId| board.item(id).is_some_and(|it| plane_nets.contains(&it.net));
    let hard_own = ObstacleMap::window_without(board, base.clone(), fine, win, options.pours_block, &|id| soft_set.contains(&id));
    let soft_own = ObstacleMap::window_without(board, base.clone(), fine, win, options.pours_block, &|id| !soft_set.contains(&id) || plane(id));
    let plane_own = (!plane_nets.is_empty()).then(|| {
        ObstacleMap::window_without(board, base.clone(), fine, win, options.pours_block, &|id| !soft_set.contains(&id) || !plane(id))
    });
    hard_own.view_into(airline.net, grid);
    let plane_cost = (options.rip_cost / 40).max(5);
    let price = |s: bool, p: bool| if s { options.rip_cost } else if p { plane_cost } else { 0 };
    grid.penalty.clear();
    let nodes = grid.blocked.len();
    let net = airline.net;
    grid.penalty.extend((0..nodes).map(|n| price(soft_own.blocked_at(net, n), plane_own.as_ref().is_some_and(|m| m.blocked_at(net, n)))));
    grid.via_penalty.resize(grid.cell_count(), 0);
    for (c, p) in grid.via_penalty.iter_mut().enumerate() {
        *p += price(soft_own.via_blocked_at(net, c), plane_own.as_ref().is_some_and(|m| m.via_blocked_at(net, c)));
    }
    let (sources, targets) = lifted_ends(board, grid, airline, soft_set, near, options.pours_block);
    if sources.is_empty() || targets.is_empty() {
        return None;
    }
    let source_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
    let target_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
    let via_cost = ((options.via_cost as i64 * options.cell) / fine.max(1)) as u32;
    let path = solver.route(grid, &source_cells, &target_cells, via_cost)?;
    Some((path, sources, targets, false))
}

fn set_locked(board: &mut Board, ids: &[ItemId], locked: bool) {
    for &id in ids {
        if let Some(mut it) = board.remove(id) {
            it.locked = locked;
            board.restore(id, it);
        }
    }
}

/// Routes the open connections of `nets`, at most `FINE_REROUTES` of them, shortest first, each
/// on a wider `cell / 2` window round it, so a net pushed out of one 2.54 mm header gap can
/// detour through the next gap over.
fn route_open_fine_windows(board: &mut Board, solver: &mut dyn FieldSolver, options: &RipOptions, nets: &HashSet<u32>) -> Vec<ItemId> {
    let fine = options.cell / 2;
    if fine < crate::route::FINE_CELL_FLOOR || options.deadline.expired() {
        return Vec::new();
    }
    let via_cost = ((options.via_cost as i64 * options.cell) / fine.max(1)) as u32;
    let mut laid = Vec::new();
    let mut open = joined(board, options.pours_block).airlines;
    open.retain(|a| nets.contains(&a.net));
    open.sort_by(|a, b| a.from.distance(a.to).total_cmp(&b.from.distance(b.to)));
    for airline in open.into_iter().take(FINE_REROUTES) {
        if options.deadline.expired() {
            break;
        }
        let Some(win) = rip_fine_window(&airline, fine, board.layers.len()) else { continue };
        let profile = Profile::for_net(board, airline.net);
        let map = ObstacleMap::window(board, profile, fine, win, options.pours_block);
        let mut grid = map.view(airline.net);
        grid.deadline = options.deadline.clone();
        let sources = terminals(board, &grid, airline.from_item);
        let targets = terminals(board, &grid, airline.to_item);
        if sources.is_empty() || targets.is_empty() {
            continue;
        }
        let src: Vec<usize> = sources.iter().map(|t| t.0).collect();
        let tgt: Vec<usize> = targets.iter().map(|t| t.0).collect();
        let Some(path) = solver.route(&grid, &src, &tgt, via_cost) else { continue };
        if let Some(items) = crate::route::lay(board, &grid, &path, &sources, &targets) {
            laid.extend(items);
        }
    }
    laid
}

/// Priority restart after laying a boxed connection's path: hold the freed route locked, prune
/// cut stubs on the nets it ripped, reroute them (negotiating or stepping one soft lane aside
/// while the freed route stays locked), and fall back to joint negotiation only if needed. Each
/// step's work is counted (`BOXED_ROUNDS`, `FINE_REROUTES`, `HOP_ATTEMPTS`); only the stage's
/// deadline backs them.
#[allow(clippy::too_many_arguments)]
fn priority_restart(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &RipOptions,
    soft: &mut Vec<ItemId>,
    soft_before: &[ItemId],
    laid: &[ItemId],
    ripped_nets: &HashSet<u32>,
    airline: &Airline,
    reroute: &Options,
    plane_nets: &HashSet<u32>,
    score_before: (usize, usize),
    allow_hop: bool,
    score: &dyn Fn(&Board) -> (usize, usize),
    out_of_time: &dyn Fn() -> bool,
) -> (Vec<ItemId>, (usize, usize)) {
    soft.extend(laid.iter().copied());
    set_locked(board, laid, true);
    if !ripped_nets.is_empty() {
        crate::cleanup::prune_dangling_of(board, soft, &|it| !it.locked && ripped_nets.contains(&it.net), &mut |_, _| {});
    }
    let (board_held, soft_held) = (board.clone(), soft.clone());
    let mut involved = ripped_nets.clone();
    involved.insert(airline.net);

    let finish_pass = |board: &mut Board, solver: &mut dyn FieldSolver, soft: &mut Vec<ItemId>| {
        soft.extend(route_open(board, solver, reroute, &mut |_| {}).laid);
        if score(board) >= score_before && !out_of_time() {
            soft.extend(route_open_fine_windows(board, solver, options, &involved));
        }
        if !plane_nets.is_empty() {
            soft.extend(crate::planes::join_planes(board, &None, &options.deadline, None).laid);
        }
    };

    finish_pass(board, solver, soft);
    let mut score_after = score(board);

    if score_after >= score_before && (1..=NEGOTIATED).contains(&ripped_nets.len()) && !out_of_time() {
        (*board, *soft) = (board_held.clone(), soft_held.clone());
        let negotiated = crate::negotiate::route_negotiated(
            board,
            solver,
            &crate::negotiate::NegotiationOptions {
                cell: options.cell,
                via_cost: options.via_cost,
                max_rounds: BOXED_ROUNDS,
                deadline: options.deadline.clone(),
                nets: Some(ripped_nets.clone()),
                pours_block: options.pours_block,
                rip_blocked: false,
                pad_ends: true,
                directional: false,
                ..Default::default()
            },
            &mut |_| {},
            &mut |_, _| {},
        );
        soft.extend(negotiated.laid);
        finish_pass(board, solver, soft);
        score_after = score(board);
    }

    if allow_hop && score_after >= score_before && score_after.0 <= score_before.0 + 1 && !ripped_nets.is_empty() && !out_of_time() {
        let still_ripped: HashSet<u32> = joined(board, options.pours_block)
            .airlines
            .into_iter()
            .map(|a| a.net)
            .filter(|n| ripped_nets.contains(n))
            .collect();
        if (1..=2).contains(&still_ripped.len()) {
            let hop = rip_up_inner(
                board,
                solver,
                &RipOptions { region_repair: true, corridor: false, log: None, ..options.clone() },
                soft,
                Some(&still_ripped),
                Some(HOP_ATTEMPTS),
                &mut |_, _| {},
            );
            if hop.kept > 0 && !plane_nets.is_empty() {
                soft.extend(crate::planes::join_planes(board, &None, &options.deadline, None).laid);
            }
            score_after = score(board);
        }
    }

    set_locked(board, laid, false);

    if score_after >= score_before && (1..=NEGOTIATED).contains(&ripped_nets.len()) && !out_of_time() {
        (*board, *soft) = (board_held, soft_held);
        for &id in laid {
            board.remove(id);
        }
        soft.retain(|id| board.item(*id).is_some());
        let negotiated = crate::negotiate::route_negotiated(
            board,
            solver,
            &crate::negotiate::NegotiationOptions {
                cell: options.cell,
                via_cost: options.via_cost,
                max_rounds: BOXED_ROUNDS,
                deadline: options.deadline.clone(),
                nets: Some(involved.clone()),
                pours_block: options.pours_block,
                rip_blocked: false,
                pad_ends: true,
                directional: false,
                ..Default::default()
            },
            &mut |_| {},
            &mut |_, _| {},
        );
        soft.extend(negotiated.laid);
        finish_pass(board, solver, soft);
        score_after = score(board);
    }

    let was: HashSet<ItemId> = soft_before.iter().copied().collect();
    let all_laid: Vec<ItemId> = soft.iter().copied().filter(|id| !was.contains(id) && board.item(*id).is_some()).collect();
    (all_laid, score_after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use board::Kind;

    #[test]
    fn region_repair_frees_a_connection_crossed_by_two_ripped_nets() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal))
             (net 0 \"\") (net 1 \"A\") (net 2 \"B\") (net 3 \"C\")
             (gr_rect (start 0 0) (end 14 14) (layer \"Edge.Cuts\") (width 0.1)))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let mm = |v: f64| (v * 1e6) as board::Nm;
        let smd = |net, layer: usize, x, y| Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net,
            layers: 1u64 << layer,
            shape: board::Shape::Capsule { a: Point::new(mm(x), mm(y)), b: Point::new(mm(x), mm(y)), r: mm(0.25) },
            hole: None,
            locked: true,
            span: None,
        };
        // Net 1 is open horizontally across y = 7 mm on F.Cu; nets 2 and 3 cross vertically in
        // soft tracks blocking net 1's direct F.Cu path while B.Cu has room to detour via vias.
        board.insert(smd(1, 0, 3.0, 7.0));
        board.insert(smd(1, 0, 11.0, 7.0));
        board.insert(smd(2, 0, 6.6, 2.0));
        board.insert(smd(2, 0, 6.6, 12.0));
        board.insert(smd(3, 0, 7.4, 2.0));
        board.insert(smd(3, 0, 7.4, 12.0));
        let mut soft = Vec::new();
        for (net, x) in [(2, 6.6), (3, 7.4)] {
            soft.push(board.insert(Item {
                kind: Kind::Track,
                net,
                layers: 1,
                shape: board::Shape::Capsule { a: Point::new(mm(x), mm(2.0)), b: Point::new(mm(x), mm(12.0)), r: mm(0.1) },
                hole: None,
                locked: false,
                span: None,
            }));
        }
        assert_eq!(connectivity_without_pours(&board).airlines.len(), 1);
        let stats = rip_up(
            &mut board,
            &mut crate::route::CpuSolver,
            &RipOptions { cell: 35_000, region_repair: true, ..Default::default() },
            &mut soft,
            &mut |_| {},
        );
        assert!(stats.kept >= 1, "{stats:?}");
        assert!(connectivity_without_pours(&board).airlines.is_empty());
        for &id in &soft {
            assert!(check_item(&board, id).is_empty(), "{:?}", board.item(id));
        }
    }

    #[test]
    fn rip_up_corridor_clears_soft_track_blocking_narrow_gap() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6))
             (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal))
             (net 0 \"\") (net 1 \"PWR\") (net 2 \"SIG\") (net 3 \"WALL\")
             (gr_rect (start 0 0) (end 16 14) (layer \"Edge.Cuts\") (width 0.1)))",
        )
        .unwrap();
        let mut rules = kicad_pcb::Rules::parse("{}").unwrap();
        rules.min_track_width = 0.0002;
        rules.min_clearance = 0.00025;
        rules.min_copper_edge_clearance = 0.0003;
        rules.classes[0].track_width = 0.0005;
        rules.classes[0].clearance = 0.00025;
        let mut board = Board::new(&pcb, &copper, rules);
        let mm = |v: f64| (v * 1e6) as board::Nm;
        let smd = |net, x, y| Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net,
            layers: 1,
            shape: board::Shape::Capsule { a: Point::new(mm(x), mm(y)), b: Point::new(mm(x), mm(y)), r: mm(0.35) },
            hole: None,
            locked: true,
            span: None,
        };
        board.insert(smd(1, 2.0, 7.0));
        board.insert(smd(1, 14.0, 7.0));
        // Unnetted hard wall on both layers at x = 8.0 leaves only an 0.80 mm gap at y = 7.0 on
        // F.Cu (and blocks B.Cu completely across x = 8.0 so net 1 must pass the F.Cu gap).
        for (y0, y1) in [(0.4, 6.2), (7.8, 13.6)] {
            board.insert(Item {
                kind: Kind::Track,
                net: 0,
                layers: 1,
                shape: board::Shape::Capsule { a: Point::new(mm(8.0), mm(y0)), b: Point::new(mm(8.0), mm(y1)), r: mm(0.4) },
                hole: None,
                locked: true,
                span: None,
            });
        }
        board.insert(Item {
            kind: Kind::Track,
            net: 0,
            layers: 2,
            shape: board::Shape::Capsule { a: Point::new(mm(8.0), mm(0.4)), b: Point::new(mm(8.0), mm(13.6)), r: mm(0.4) },
            hole: None,
            locked: true,
            span: None,
        });
        // Net 2 lies to the left of the wall (x = 5.5) where B.Cu is open for a via detour,
        // and currently sits as a soft track on F.Cu across y = 7.0 blocking net 1's approach.
        board.insert(smd(2, 5.5, 2.0));
        board.insert(smd(2, 5.5, 12.0));
        let mut soft = vec![board.insert(Item {
            kind: Kind::Track,
            net: 2,
            layers: 1,
            shape: board::Shape::Capsule { a: Point::new(mm(5.5), mm(2.0)), b: Point::new(mm(5.5), mm(12.0)), r: mm(0.25) },
            hole: None,
            locked: false,
            span: None,
        })];
        // Off, rip-up finds no way, as before corridors.
        let (mut plain, mut plain_soft) = (board.clone(), soft.clone());
        let stats = rip_up(&mut plain, &mut crate::route::CpuSolver, &RipOptions::default(), &mut plain_soft, &mut |_| {});
        assert_eq!((stats.kept, stats.corridor), (0, 0), "{stats:?}");
        let options = RipOptions { corridor: true, ..Default::default() };
        let stats = rip_up(&mut board, &mut crate::route::CpuSolver, &options, &mut soft, &mut |_| {});
        assert_eq!(stats.kept, 1, "{stats:?}");
        assert_eq!(stats.corridor, 1, "{stats:?}");
        assert!(!stats.corridors.is_empty(), "{stats:?}");
        assert!(connectivity_without_pours(&board).airlines.is_empty());
        for &id in &soft {
            assert!(check_item(&board, id).is_empty(), "{:?}", board.item(id));
        }
    }

    #[test]
    fn rip_boxed_routes_a_boxed_connection_through_a_header_gap_on_finer_cells() {
        let make = || {
            let (pcb, copper) = kicad_pcb::parse_board(
                "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal))
                 (net 0 \"\") (net 1 \"A\") (net 2 \"B\") (net 3 \"H1\") (net 4 \"H2\") (net 5 \"H3\") (net 6 \"G\")
                 (gr_rect (start 0 0) (end 12 8) (layer \"Edge.Cuts\") (width 0.1)))",
            )
            .unwrap();
            let rules = kicad_pcb::Rules::parse(
                "{\"board\":{\"design_settings\":{\"rules\":{\"min_track_width\":0.25,\"min_clearance\":0.2}}},
                  \"net_settings\":{\"classes\":[{\"name\":\"Default\",\"track_width\":0.25,\"clearance\":0.2}]}}",
            )
            .unwrap();
            let mut board = Board::new(&pcb, &copper, rules);
            let mm = |v: f64| (v * 1e6) as board::Nm;
            let pad = |net, x, y, r| Item {
                kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
                net,
                layers: 1,
                shape: board::Shape::Capsule { a: Point::new(mm(x), mm(y)), b: Point::new(mm(x), mm(y)), r: mm(r) },
                hole: None,
                locked: true,
                span: None,
            };
            // Three 2.54 mm-pitch pads with 0.716 mm gaps: blocked on 0.05 mm cells, open on 0.025 mm.
            for (net, y) in [(3, 1.46), (4, 4.00), (5, 6.54)] {
                board.insert(pad(net, 6.0, y, 0.912));
            }
            // Fixed guard above A's left pad so A can only exit through the y = 2.73 header gap.
            board.insert(Item {
                kind: Kind::Track,
                net: 6,
                layers: 1,
                shape: board::Shape::Capsule { a: Point::new(mm(2.0), mm(3.45)), b: Point::new(mm(5.1), mm(3.45)), r: mm(0.15) },
                hole: None,
                locked: true,
                span: None,
            });
            board.insert(pad(1, 3.0, 2.73, 0.2));
            board.insert(pad(1, 9.0, 2.73, 0.2));
            board.insert(pad(2, 1.2, 4.2, 0.2));
            board.insert(pad(2, 10.0, 5.27, 0.2));
            let mut soft = Vec::new();
            for (a, b) in [((1.2, 4.2), (1.2, 2.05)), ((1.2, 2.05), (3.8, 2.05)), ((3.8, 2.05), (3.8, 2.73)), ((3.8, 2.73), (10.0, 2.73)), ((10.0, 2.73), (10.0, 5.27))] {
                soft.push(board.insert(Item {
                    kind: Kind::Track,
                    net: 2,
                    layers: 1,
                    shape: board::Shape::Capsule { a: Point::new(mm(a.0), mm(a.1)), b: Point::new(mm(b.0), mm(b.1)), r: mm(0.125) },
                    hole: None,
                    locked: false,
                    span: None,
                }));
            }
            (board, soft)
        };

        let (mut base, mut soft_base) = make();
        let off = rip_up(&mut base, &mut crate::route::CpuSolver, &RipOptions { cell: 50_000, rip_boxed: false, ..Default::default() }, &mut soft_base, &mut |_| {});
        assert_eq!(off.kept, 0, "{off:?}");

        let (mut fixed, mut soft_fixed) = make();
        let on = rip_up(&mut fixed, &mut crate::route::CpuSolver, &RipOptions { cell: 50_000, rip_boxed: true, ..Default::default() }, &mut soft_fixed, &mut |_| {});
        assert_eq!((on.kept, on.boxed, on.fine), (1, 1, 1), "{on:?}");
        assert!(connectivity_without_pours(&fixed).airlines.is_empty());
        for &id in &soft_fixed {
            assert!(check_item(&fixed, id).is_empty(), "{:?}", fixed.item(id));
        }
    }
}
