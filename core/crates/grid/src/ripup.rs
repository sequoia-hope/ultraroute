//! Rip-up and reroute for the connections the other passes left open.
//!
//! Copper the router laid itself is soft: a blocked connection may route through it at a price.
//! Whatever soft copper its new route actually conflicts with is ripped, the connection laid, and
//! the ripped connections routed again. The board is kept only if fewer connections are open
//! than before; otherwise it is put back as it was.

use std::collections::{HashMap, HashSet};

use board::{check_item, connectivity, connectivity_without_pours, Airline, Board, Connectivity, Item, ItemId, Nm, Point};

use crate::grid::{ObstacleMap, Profile};
use crate::report::{tell, Focus, Log};
use crate::route::{
    cluster_pads, corridor_terminals, corridor_widths, corridor_window, ends_with_pads, pieces,
    pieces_corridor, route_open, terminals, FieldSolver, Options, FINE_CELL_FLOOR, FINE_NODES,
    NARROW_CELL_PENALTY,
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
    pub log: Option<Log>,
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
            log: None,
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
    /// Attempts kept that run narrowed to their pins the whole way (`route::narrow_run`), by
    /// their two ends, with the width.
    pub narrowed: Vec<(ItemId, ItemId, Nm)>,
    /// Attempts kept that narrowed only across tight gaps (`route::corridor_widths`).
    pub corridor: usize,
}

fn joined(board: &Board, pours_block: bool) -> Connectivity {
    if pours_block { connectivity(board) } else { connectivity_without_pours(board) }
}

/// At most so many nets ripped for one connection are negotiated over together.
const NEGOTIATED: usize = 8;

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
    let mut full_grid = grid.clone();
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
    let mut corridor_tried: HashSet<(ItemId, ItemId)> = HashSet::new();
    let mut corridor_nets: HashSet<u32> = HashSet::new();
    let mut corridor_rescue = false;
    loop {
        let mut open = joined(board, options.pours_block).airlines;
        open.retain(|a| !plane_nets.contains(&a.net));
        open.sort_by(|a, b| a.from.distance(a.to).total_cmp(&b.from.distance(b.to)));
        if corridor_rescue {
            open.retain(|a| !corridor_tried.contains(&(a.from_item, a.to_item)));
        }
        open.retain(|a| !tried.contains(&(a.from_item, a.to_item)));
        if open.is_empty() || out_of_time() {
            if corridor_rescue || out_of_time() {
                break;
            }
            // Wait until the end of the round check if open became empty only because tried held them.
            break;
        }
        let kept_before = stats.kept;
        for airline in open {
            if out_of_time() {
                break;
            }
            tried.insert((airline.from_item, airline.to_item));
            if board.item(airline.from_item).is_none() || board.item(airline.to_item).is_none() {
                continue;
            }
            let base = Profile::for_net(board, airline.net);
            let corr_widths = corridor_widths(board, &base);
            if corridor_rescue && corr_widths.is_empty() {
                corridor_tried.insert((airline.from_item, airline.to_item));
                continue;
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
            enum Candidate {
                Whole { profile: Profile, key: String, narrowed: bool },
                Corridor { profile: Profile, key: String },
                CorridorFine { profile: Profile, cell: Nm, window: board::BBox },
            }
            // The net's own width, then, for a short current net that finds no way at it, its
            // pins' width the whole way (`route::narrow_run`), then legal corridor widths that
            // narrow only through tight gaps and return to full width outside them.
            let mut candidates = Vec::new();
            if !corridor_rescue {
                candidates.push(Candidate::Whole { profile: base.clone(), key: base.class.clone(), narrowed: false });
                if let Some(hw) = crate::route::narrow_run(board, &base, &airline) {
                    let mut narrow = base.clone();
                    (narrow.half_width, narrow.narrow_half_width) = (hw, hw);
                    candidates.push(Candidate::Whole { profile: narrow, key: format!("{}\u{0}{hw}", base.class), narrowed: true });
                }
            }
            let fine = options.cell / 2;
            let cwin = corridor_window(&airline);
            let cnodes = ((cwin.max.x - cwin.min.x) / fine.max(1)) as f64 * ((cwin.max.y - cwin.min.y) / fine.max(1)) as f64 * board.layers.len() as f64;
            for hw in corr_widths {
                let mut narrow = base.clone();
                (narrow.half_width, narrow.narrow_half_width) = (hw, base.narrow_half_width.min(hw));
                candidates.push(Candidate::Corridor { profile: narrow.clone(), key: format!("{}\u{0}c{hw}", base.class) });
                if fine >= FINE_CELL_FLOOR && cnodes <= FINE_NODES {
                    candidates.push(Candidate::CorridorFine { profile: narrow, cell: fine, window: cwin });
                }
            }
            let plane_soft: HashSet<ItemId> = if plane_nets.is_empty() {
                HashSet::new()
            } else {
                soft_set.iter().copied().filter(|&id| board.item(id).is_some_and(|it| plane_nets.contains(&it.net))).collect()
            };
            let plane_cost = (options.rip_cost / 40).max(5);
            let price = |soft_b: bool, plane_b: bool| if soft_b { options.rip_cost } else if plane_b { plane_cost } else { 0 };
            let mut found = None;
            for cand in candidates {
                if out_of_time() {
                    break;
                }
                match cand {
                    Candidate::Whole { profile, key, narrowed } => {
                        // The narrow run's maps cover its neighbourhood only, made for this try alone.
                        let window = narrowed.then(|| crate::route::narrow_window(&airline));
                        let made = |b: &Board, skip: &dyn Fn(ItemId) -> bool| match window {
                            Some(w) => ObstacleMap::window_without(b, profile.clone(), options.cell, w, options.pours_block, skip),
                            None => ObstacleMap::without(b, profile.clone(), options.cell, options.pours_block, skip),
                        };
                        let (hard_own, soft_own, plane_own);
                        let hard_map: &ObstacleMap = if window.is_some() {
                            hard_own = made(board, &|id| soft_set.contains(&id));
                            &hard_own
                        } else {
                            hard.entry(key.clone()).or_insert_with(|| made(board, &|id| soft_set.contains(&id)))
                        };
                        hard_map.view_into(airline.net, &mut grid);
                        let soft_map: &ObstacleMap = if window.is_some() {
                            soft_own = made(board, &|id| !soft_set.contains(&id) || plane_soft.contains(&id));
                            &soft_own
                        } else {
                            soft_maps.entry(key.clone()).or_insert_with(|| made(board, &|id| !soft_set.contains(&id) || plane_soft.contains(&id)))
                        };
                        // A plane net's soft copper is mostly vias joining pads to the planes, which
                        // `join_planes` puts back elsewhere: cheap to rip.
                        let plane_map: Option<&ObstacleMap> = if plane_nets.is_empty() {
                            None
                        } else if window.is_some() {
                            plane_own = made(board, &|id| !plane_soft.contains(&id));
                            Some(&plane_own)
                        } else {
                            Some(&*plane_maps.entry(key.clone()).or_insert_with(|| made(board, &|id| !plane_soft.contains(&id))))
                        };
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
                        if sources.is_empty() || targets.is_empty() {
                            continue;
                        }
                        let source_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
                        let target_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
                        if let Some(path) = solver.route(&grid, &source_cells, &target_cells, options.via_cost) {
                            if let Some(new) = pieces(&grid, &path, &sources, &targets) {
                                let path_pts = path.iter().map(|&(l, i, j)| (grid.layers[l], grid.centre(i, j))).collect();
                                found = Some((path_pts, new, narrowed.then_some(2 * grid.half_width), false));
                                break;
                            }
                        }
                    }
                    Candidate::Corridor { profile, key } => {
                        corridor_tried.insert((airline.from_item, airline.to_item));
                        for (prof, k) in [(&base, &base.class), (&profile, &key)] {
                            hard.entry(k.clone()).or_insert_with(|| {
                                ObstacleMap::without(board, prof.clone(), options.cell, options.pours_block, &|id| soft_set.contains(&id))
                            });
                            soft_maps.entry(k.clone()).or_insert_with(|| {
                                ObstacleMap::without(board, prof.clone(), options.cell, options.pours_block, &|id| !soft_set.contains(&id) || plane_soft.contains(&id))
                            });
                            if !plane_nets.is_empty() {
                                plane_maps.entry(k.clone()).or_insert_with(|| {
                                    ObstacleMap::without(board, prof.clone(), options.cell, options.pours_block, &|id| !plane_soft.contains(&id))
                                });
                            }
                        }
                        let full_hard = &hard[&base.class];
                        let full_soft = &soft_maps[&base.class];
                        let full_plane = (!plane_nets.is_empty()).then(|| &plane_maps[&base.class]);
                        let hard_map = &hard[&key];
                        let soft_map = &soft_maps[&key];
                        let plane_map = (!plane_nets.is_empty()).then(|| &plane_maps[&key]);
                        full_hard.view_into(airline.net, &mut full_grid);
                        hard_map.view_into(airline.net, &mut grid);
                        let net = airline.net;
                        let full_free = |n: usize| {
                            !full_grid.blocked[n] && !full_soft.blocked_at(net, n) && !full_plane.is_some_and(|m| m.blocked_at(net, n))
                        };
                        grid.penalty.clear();
                        let nodes = grid.blocked.len();
                        grid.penalty.extend((0..nodes).map(|n| {
                            price(soft_map.blocked_at(net, n), plane_map.is_some_and(|m| m.blocked_at(net, n)))
                                + if full_free(n) { 0 } else { NARROW_CELL_PENALTY }
                        }));
                        if options.region_repair {
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
                            let ends_fn = |b: &mut Board, g: &crate::grid::RouteGrid, item: ItemId| {
                                corridor_terminals(b, &full_grid, g, &full_free, item)
                            };
                            let mut end = |id: ItemId| {
                                let pads = conn.cluster.get(&id).and_then(|k| pads.get(k)).map_or(&[][..], Vec::as_slice);
                                ends_with_pads(board, &grid, id, pads, &ends_fn)
                            };
                            (end(airline.from_item), end(airline.to_item))
                        } else {
                            (
                                corridor_terminals(board, &full_grid, &grid, &full_free, airline.from_item),
                                corridor_terminals(board, &full_grid, &grid, &full_free, airline.to_item),
                            )
                        };
                        if sources.is_empty() || targets.is_empty() {
                            continue;
                        }
                        let source_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
                        let target_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
                        if let Some(path) = solver.route(&grid, &source_cells, &target_cells, options.via_cost) {
                            if let Some(new) = pieces_corridor(&grid, base.half_width, &full_free, &path, &sources, &targets) {
                                let path_pts = path.iter().map(|&(l, i, j)| (grid.layers[l], grid.centre(i, j))).collect();
                                found = Some((path_pts, new, None, true));
                                break;
                            }
                        }
                    }
                    Candidate::CorridorFine { profile, cell, window } => {
                        corridor_tried.insert((airline.from_item, airline.to_item));
                        let hard_map = ObstacleMap::window_without(board, profile.clone(), cell, window, options.pours_block, &|id| soft_set.contains(&id));
                        hard_map.view_into(airline.net, &mut grid);
                        if !options.region_repair
                            && (terminals(board, &grid, airline.from_item).is_empty() || terminals(board, &grid, airline.to_item).is_empty())
                        {
                            continue;
                        }
                        let soft_map = ObstacleMap::window_without(board, profile, cell, window, options.pours_block, &|id| !soft_set.contains(&id) || plane_soft.contains(&id));
                        let plane_map = (!plane_nets.is_empty()).then(|| {
                            ObstacleMap::window_without(board, base.clone(), cell, window, options.pours_block, &|id| !plane_soft.contains(&id))
                        });
                        let full_hard = ObstacleMap::window_without(board, base.clone(), cell, window, options.pours_block, &|id| soft_set.contains(&id));
                        let full_soft = ObstacleMap::window_without(board, base.clone(), cell, window, options.pours_block, &|id| !soft_set.contains(&id) || plane_soft.contains(&id));
                        full_hard.view_into(airline.net, &mut full_grid);
                        let net = airline.net;
                        let full_free = |n: usize| {
                            !full_grid.blocked[n] && !full_soft.blocked_at(net, n) && !plane_map.as_ref().is_some_and(|m| m.blocked_at(net, n))
                        };
                        grid.penalty.clear();
                        let nodes = grid.blocked.len();
                        grid.penalty.extend((0..nodes).map(|n| {
                            price(soft_map.blocked_at(net, n), plane_map.as_ref().is_some_and(|m| m.blocked_at(net, n)))
                                + if full_free(n) { 0 } else { NARROW_CELL_PENALTY }
                        }));
                        if options.region_repair {
                            grid.via_penalty.resize(grid.cell_count(), 0);
                            for (c, p) in grid.via_penalty.iter_mut().enumerate() {
                                *p += price(soft_map.via_blocked_at(net, c), plane_map.as_ref().is_some_and(|m| m.via_blocked_at(net, c)));
                            }
                        }
                        let (sources, targets) = if options.region_repair {
                            let (conn, pads) = near.get_or_insert_with(|| {
                                let conn = joined(board, options.pours_block);
                                let pads = cluster_pads(board, &conn);
                                (conn, pads)
                            });
                            let ends_fn = |b: &mut Board, g: &crate::grid::RouteGrid, item: ItemId| {
                                corridor_terminals(b, &full_grid, g, &full_free, item)
                            };
                            let mut end = |id: ItemId| {
                                let pads = conn.cluster.get(&id).and_then(|k| pads.get(k)).map_or(&[][..], Vec::as_slice);
                                ends_with_pads(board, &grid, id, pads, &ends_fn)
                            };
                            (end(airline.from_item), end(airline.to_item))
                        } else {
                            (
                                corridor_terminals(board, &full_grid, &grid, &full_free, airline.from_item),
                                corridor_terminals(board, &full_grid, &grid, &full_free, airline.to_item),
                            )
                        };
                        if sources.is_empty() || targets.is_empty() {
                            continue;
                        }
                        let source_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
                        let target_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
                        if let Some(path) = solver.route(&grid, &source_cells, &target_cells, options.via_cost) {
                            if let Some(new) = pieces_corridor(&grid, base.half_width, &full_free, &path, &sources, &targets) {
                                let path_pts = path.iter().map(|&(l, i, j)| (grid.layers[l], grid.centre(i, j))).collect();
                                found = Some((path_pts, new, None, true));
                                break;
                            }
                        }
                    }
                }
            }
            let Some((path_pts, new, narrowed_width, is_corridor)) = found else {
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
            soft.extend(laid.iter().copied());
            let reroute_corridor = ripped_items.iter().any(|it| corridor_nets.contains(&it.net));
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
                corridor: reroute_corridor,
            };
            let mut extra_corridor = 0;
            let mut extra_narrowed = Vec::new();
            let mut again = if is_corridor && ripped == 0 {
                Vec::new()
            } else {
                tell(&options.log, "ripping up", || format!("{net_name}: ripped {ripped} pieces, rerouting them"), None);
                let r = route_open(board, solver, &reroute, &mut |_| {});
                extra_corridor += r.corridor;
                extra_narrowed.extend(r.narrowed);
                r.laid
            };
            if !plane_nets.is_empty() && !(is_corridor && ripped == 0) {
                again.extend(crate::planes::join_planes(board, &None, &options.deadline, None).laid);
            }
            let mut score_after = score(board);
            // One after another, the first rerouted may take the only lane of the next. The
            // ripped nets and this connection's are negotiated together instead, at one weight.
            let ripped_nets: HashSet<u32> = ripped_items.iter().map(|it| it.net).filter(|n| *n != 0 && !plane_nets.contains(n)).collect();
            if options.region_repair && score_after >= score_before && (1..=NEGOTIATED).contains(&ripped_nets.len()) && !out_of_time() {
                for &id in laid.iter().chain(&again) {
                    board.remove(id);
                }
                laid.clear();
                again.clear();
                extra_corridor = 0;
                extra_narrowed.clear();
                soft.retain(|id| board.item(*id).is_some());
                let mut nets = ripped_nets;
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
                let r = route_open(board, solver, &reroute, &mut |_| {});
                extra_corridor += r.corridor;
                extra_narrowed.extend(r.narrowed);
                again.extend(r.laid);
                if !plane_nets.is_empty() {
                    again.extend(crate::planes::join_planes(board, &None, &options.deadline, None).laid);
                }
                score_after = score(board);
            }
            let mut all_laid = laid;
            all_laid.extend(again.iter().copied());
            soft.extend(again);
            let after = score_after.1;
            if score_after < score_before {
                if options.region_repair {
                    let open: HashSet<u32> = joined(board, options.pours_block).airlines.iter().map(|a| a.net).collect();
                    crate::cleanup::prune_dangling_of(board, soft, &|it| !it.locked && !open.contains(&it.net), &mut |_, _| {});
                    near = None;
                }
                stats.kept += 1;
                stats.ripped += ripped;
                if let Some(w) = narrowed_width {
                    stats.narrowed.push((airline.from_item, airline.to_item, w));
                }
                stats.narrowed.extend(extra_narrowed);
                if is_corridor {
                    stats.corridor += 1;
                    corridor_nets.insert(airline.net);
                }
                stats.corridor += extra_corridor;
                corridor_tried.clear();
                soft_maps.clear();
                plane_maps.clear();
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
            if !corridor_rescue && !out_of_time() {
                corridor_rescue = true;
                tried.clear();
                continue;
            }
            break;
        }
        corridor_rescue = false;
        // What was ripped and rerouted may have opened room for connections tried before.
        tried.clear();
    }
    soft.retain(|id| board.item(*id).is_some());
    stats
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
        let stats = rip_up(&mut board, &mut crate::route::CpuSolver, &RipOptions::default(), &mut soft, &mut |_| {});
        assert_eq!(stats.kept, 1, "{stats:?}");
        assert_eq!(stats.corridor, 1, "{stats:?}");
        assert!(connectivity_without_pours(&board).airlines.is_empty());
        for &id in &soft {
            assert!(check_item(&board, id).is_empty(), "{:?}", board.item(id));
        }
    }
}
