//! Rip-up and reroute for the connections the other passes left open.
//!
//! Copper the router laid itself is soft: a blocked connection may route through it at a price.
//! Whatever soft copper its new route actually conflicts with is ripped, the connection laid, and
//! the ripped connections routed again. The board is kept only if fewer connections are open
//! than before; otherwise it is put back as it was.

use std::collections::{BTreeMap, HashMap, HashSet};

use board::{check_item, connectivity, connectivity_without_pours, Airline, Board, Item, ItemId, Kind, Point};

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
    /// When repairing after a region plough, gather cluster pad terminals for locally cut stubs,
    /// skip plane airlines, and rejoin any ripped plane vias before judging the attempt.
    pub region_repair: bool,
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
            region_repair: false,
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
    let mut plane_soft_maps: HashMap<String, ObstacleMap> = HashMap::new();
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
        pours_block: options.pours_block,
    };
    let plane_nets: HashSet<u32> = if options.region_repair && !options.pours_block && board.planes != 0 {
        board.items().filter(|(_, it)| board.is_plane(it) && it.net != 0).map(|(_, it)| it.net).collect()
    } else {
        HashSet::new()
    };
    let score = |board: &Board| -> (usize, usize) {
        let open = if options.pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines;
        let sig = if plane_nets.is_empty() { open.len() } else { open.iter().filter(|a| !plane_nets.contains(&a.net)).count() };
        (sig, open.len())
    };
    let mut tried: HashSet<(ItemId, ItemId)> = HashSet::new();
    loop {
        let conn = if options.pours_block { connectivity(board) } else { connectivity_without_pours(board) };
        let mut open = conn.airlines;
        if !plane_nets.is_empty() {
            open.retain(|a| !plane_nets.contains(&a.net));
        }
        open.sort_by(|a, b| a.from.distance(a.to).total_cmp(&b.from.distance(b.to)));
        open.retain(|a| !tried.contains(&(a.from_item, a.to_item)));
        if open.is_empty() || out_of_time() {
            break;
        }
        let mut pads_by_cluster: HashMap<usize, Vec<ItemId>> = HashMap::new();
        if options.region_repair {
            for (id, it) in board.items() {
                if matches!(it.kind, Kind::Pad { .. }) {
                    if let Some(&cl) = conn.cluster.get(&id) {
                        pads_by_cluster.entry(cl).or_default().push(id);
                    }
                }
            }
        }
        let end_terminals = |board: &mut Board, grid: &crate::grid::RouteGrid, root: ItemId, pads_map: &HashMap<usize, Vec<ItemId>>, cl_map: &HashMap<ItemId, usize>| {
            let base = terminals(board, grid, root);
            if !options.region_repair {
                return base;
            }
            let Some(pads) = cl_map.get(&root).and_then(|cl| pads_map.get(cl)) else {
                return base;
            };
            let origin = board.item(root).map(|it| {
                let b = it.shape.bbox();
                Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2)
            });
            let mut near_pads: Vec<ItemId> = pads.iter().copied().filter(|&id| id != root).collect();
            if let Some(o) = origin {
                near_pads.sort_by(|&a, &b| {
                    let da = board.item(a).map_or(f64::INFINITY, |it| {
                        let bb = it.shape.bbox();
                        o.distance(Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2))
                    });
                    let db = board.item(b).map_or(f64::INFINITY, |it| {
                        let bb = it.shape.bbox();
                        o.distance(Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2))
                    });
                    da.total_cmp(&db)
                });
            }
            near_pads.truncate(4);
            let mut by_cell: BTreeMap<usize, Option<(Point, board::Nm)>> = base.into_iter().collect();
            for pad_id in near_pads {
                for (cell, leg) in terminals(board, grid, pad_id) {
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
            let profile = Profile::for_net(board, airline.net);
            let hard_map = hard.entry(profile.class.clone()).or_insert_with(|| {
                ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| soft_set.contains(&id))
            });
            hard_map.view_into(airline.net, &mut grid);
            let soft_map = soft_maps.entry(profile.class.clone()).or_insert_with(|| {
                ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| {
                    !soft_set.contains(&id) || board.item(id).is_some_and(|it| plane_nets.contains(&it.net))
                })
            });
            let plane_soft_map = (!plane_nets.is_empty()).then(|| {
                plane_soft_maps.entry(profile.class.clone()).or_insert_with(|| {
                    ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| {
                        !soft_set.contains(&id) || board.item(id).is_none_or(|it| !plane_nets.contains(&it.net))
                    })
                })
            });
            let plane_cost = (options.rip_cost / 40).max(5);
            grid.penalty.clear();
            let nodes = grid.blocked.len();
            grid.penalty.extend((0..nodes).map(|idx| {
                if soft_map.blocked_at(airline.net, idx) {
                    options.rip_cost
                } else if plane_soft_map.as_ref().is_some_and(|m| m.blocked_at(airline.net, idx)) {
                    plane_cost
                } else {
                    0
                }
            }));
            let via_pen: Vec<u32> = if options.region_repair {
                let cells = grid.cell_count();
                (0..cells)
                    .map(|c| {
                        if soft_map.via_blocked_at(airline.net, c) {
                            options.rip_cost
                        } else if plane_soft_map.as_ref().is_some_and(|m| m.via_blocked_at(airline.net, c)) {
                            plane_cost
                        } else {
                            0
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let cur_conn = if options.region_repair && stats.kept > kept_before {
                Some(if options.pours_block { connectivity(board) } else { connectivity_without_pours(board) })
            } else {
                None
            };
            let cl_ref = cur_conn.as_ref().map_or(&conn.cluster, |c| &c.cluster);
            let sources = end_terminals(board, &grid, airline.from_item, &pads_by_cluster, cl_ref);
            let targets = end_terminals(board, &grid, airline.to_item, &pads_by_cluster, cl_ref);
            if sources.is_empty() || targets.is_empty() {
                continue;
            }
            let source_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
            let target_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
            let routed_path = if options.region_repair {
                solver.route_with_via_penalty(&grid, &source_cells, &target_cells, options.via_cost, &via_pen)
            } else {
                solver.route(&grid, &source_cells, &target_cells, options.via_cost)
            };
            let Some(path) = routed_path else {
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
            let mut extra_laid = again.laid;
            if !plane_nets.is_empty() {
                let jp = crate::planes::join_planes(board, &None);
                extra_laid.extend(jp.laid);
            }
            let mut score_after = score(board);
            // When signal nets were ripped in a shared corridor, sequential routing often lets the
            // greedy new route or the first ripped net steal the only lane of the next; pull the
            // new route back up too and negotiate `airline.net` and the ripped nets together at
            // equal weight before giving up.
            if options.region_repair && score_after.0 >= score_before.0 && !out_of_time() {
                let mut neg_set: HashSet<u32> = ripped_items
                    .iter()
                    .map(|it| it.net)
                    .filter(|n| *n != 0 && !plane_nets.contains(n))
                    .collect();
                if (1..=8).contains(&neg_set.len()) {
                    neg_set.insert(airline.net);
                    for &id in &extra_laid {
                        board.remove(id);
                    }
                    extra_laid.clear();
                    for &id in &laid {
                        board.remove(id);
                    }
                    soft.retain(|id| board.item(*id).is_some());
                    laid.clear();
                    let n = crate::negotiate::route_negotiated(
                        board,
                        solver,
                        &crate::negotiate::NegotiationOptions {
                            cell: options.cell,
                            via_cost: options.via_cost,
                            max_rounds: 24,
                            patience: 6,
                            present: 20.0,
                            present_growth: 1.55,
                            history_step: 25,
                            max_seconds: Some(options.reroute_seconds),
                            nets: Some(neg_set.clone()),
                            tight_zones: true,
                            pours_block: options.pours_block,
                            cancel: options.cancel.clone(),
                            ..Default::default()
                        },
                        &mut |_| {},
                        &mut |_, _| {},
                    );
                    extra_laid.extend(n.laid);
                    let finish = route_open(board, solver, &reroute, &mut |_| {});
                    extra_laid.extend(finish.laid);
                    if !plane_nets.is_empty() {
                        let jp = crate::planes::join_planes(board, &None);
                        extra_laid.extend(jp.laid);
                    }
                    score_after = score(board);
                }
            }
            let mut all_laid = laid;
            all_laid.extend(extra_laid.iter().copied());
            soft.extend(extra_laid);
            let after = score_after.1;
            let improved = if options.region_repair { score_after.0 < score_before.0 } else { after < before };
            if improved {
                if options.region_repair {
                    let still_open: HashSet<u32> = (if options.pours_block { connectivity(board) } else { connectivity_without_pours(board) })
                        .airlines
                        .into_iter()
                        .filter(|a| !plane_nets.contains(&a.net))
                        .map(|a| a.net)
                        .collect();
                    crate::cleanup::prune_dangling_filtered(board, soft, None, &still_open, &mut |_, _| {});
                }
                stats.kept += 1;
                stats.ripped += ripped;
                soft_maps.clear();
                plane_soft_maps.clear();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_repair_negotiates_multiple_ripped_nets_in_a_narrow_corridor() {
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
}
