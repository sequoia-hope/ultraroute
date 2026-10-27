//! Rip-up and reroute for the connections the other passes left open.
//!
//! Copper the router laid itself is soft: a blocked connection may route through it at a price.
//! Whatever soft copper its new route actually conflicts with is ripped, the connection laid, and
//! the ripped connections routed again. The board is kept only if fewer connections are open
//! than before; otherwise it is put back as it was.

use std::collections::{HashMap, HashSet};

use board::{check_item, connectivity, connectivity_without_pours, Airline, Board, Item, ItemId, Point};

use crate::grid::{ObstacleMap, Profile};
use crate::report::{tell, Focus, Log};
use crate::route::{pieces, route_open, terminals, FieldSolver, Options};

#[derive(Debug, Clone)]
pub struct RipOptions {
    pub cell: board::Nm,
    pub via_cost: u32,
    /// Price of entering a cell that soft copper of another net claims.
    pub rip_cost: u32,
    pub max_seconds: Option<f64>,
    /// Time allowed for rerouting what one attempt ripped.
    pub reroute_seconds: f64,
    pub pours_block: bool,
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub log: Option<Log>,
}

impl Default for RipOptions {
    fn default() -> RipOptions {
        RipOptions {
            cell: 50_000,
            via_cost: 80,
            rip_cost: 400,
            max_seconds: None,
            reroute_seconds: 10.0,
            pours_block: false,
            cancel: None,
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
}

fn open_count(board: &Board, pours_block: bool) -> usize {
    if pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines.len()
}

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
    let started = crate::clock::Instant::now();
    let out_of_time = || {
        options.max_seconds.is_some_and(|limit| started.elapsed().as_secs_f64() > limit)
            || options.cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    };
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
        pours_block: options.pours_block,
    };
    let mut tried: HashSet<(ItemId, ItemId)> = HashSet::new();
    loop {
        let mut open = if options.pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines;
        open.sort_by(|a, b| a.from.distance(a.to).total_cmp(&b.from.distance(b.to)));
        open.retain(|a| !tried.contains(&(a.from_item, a.to_item)));
        if open.is_empty() || out_of_time() {
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
            stats.attempts += 1;
            let before = open_count(board, options.pours_block);
            let net_name = board.net_name(airline.net).to_string();
            tell(
                &options.log,
                "ripping up",
                || format!("{net_name}: looking for a way through the router's own copper"),
                Some(Focus::Connection { net: airline.net, from: airline.from, to: airline.to }),
            );
            // Locked copper stays, even the router's own: a caller holds still what it must keep.
            let soft_set: HashSet<ItemId> = soft.iter().copied().filter(|id| board.item(*id).is_some_and(|it| !it.locked)).collect();
            let profile = Profile::for_net(board, airline.net);
            let hard_map = hard.entry(profile.class.clone()).or_insert_with(|| {
                ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| soft_set.contains(&id))
            });
            hard_map.view_into(airline.net, &mut grid);
            let soft_map = soft_maps.entry(profile.class.clone()).or_insert_with(|| {
                ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| !soft_set.contains(&id))
            });
            grid.penalty.clear();
            let nodes = grid.blocked.len();
            grid.penalty.extend((0..nodes).map(|idx| if soft_map.blocked_at(airline.net, idx) { options.rip_cost } else { 0 }));
            let sources = terminals(board, &grid, airline.from_item);
            let targets = terminals(board, &grid, airline.to_item);
            if sources.is_empty() || targets.is_empty() {
                continue;
            }
            let source_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
            let target_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
            let Some(path) = solver.route(&grid, &source_cells, &target_cells, options.via_cost) else {
                stats.no_path += 1;
                tell(&options.log, "ripping up", || format!("{net_name}: no way through, even ripping"), None);
                watch(board, RipAttempt { airline: &airline, path: Vec::new(), ripped: Vec::new(), laid: Vec::new(), open_before: before, open_after: before, outcome: "no path" });
                continue;
            };
            let path_pts: Vec<(usize, Point)> = path.iter().map(|&(l, i, j)| (grid.layers[l], grid.centre(i, j))).collect();
            let Some(new) = pieces(&grid, &path, &sources, &targets) else { continue };

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
            let reroute = Options {
                seed: 0,
                cell: options.cell,
                via_cost: options.via_cost,
                max_seconds: Some(options.reroute_seconds),
                nets: None,
                pours_block: options.pours_block,
                cancel: options.cancel.clone(),
                log: options.log.clone(),
            };
            tell(&options.log, "ripping up", || format!("{net_name}: ripped {ripped} pieces, rerouting them"), None);
            let again = route_open(board, solver, &reroute, &mut |_| {});
            let mut all_laid = laid;
            all_laid.extend(again.laid.iter().copied());
            soft.extend(again.laid);
            let after = open_count(board, options.pours_block);
            if after < before {
                stats.kept += 1;
                stats.ripped += ripped;
                soft_maps.clear();
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
            break;
        }
        // What was ripped and rerouted may have opened room for connections tried before.
        tried.clear();
    }
    soft.retain(|id| board.item(*id).is_some());
    stats
}
