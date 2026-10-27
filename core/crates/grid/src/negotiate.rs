//! Negotiated congestion (PathFinder, McMurchie and Ebeling 1995) on the conservative grid.
//!
//! Fixed copper stays hard-blocked. Routes found during negotiation are not: each net claims a
//! zone of cells around its paths, wide enough that two nets outside each other's zones are
//! legal on the board, and entering a cell inside another net's zone costs a present-congestion
//! price plus an accumulated history price. A net's own zone is lifted before it re-routes.
//! Rounds repeat, the present price rising and history accumulating where nets still collide,
//! until no path lies in another net's zone; then routes are committed, each checked exactly.

use std::collections::{BTreeSet, HashMap, HashSet};

use board::{connectivity, connectivity_without_pours, nm, Board, ItemId, Kind, Nm};

use crate::grid::{ObstacleMap, Profile, RouteGrid};
use crate::report::{tell, Log};
use crate::route::{lay, terminals, FieldSolver, PricedJob, RoundPrices, Terminal};
use crate::search::{route_astar, GridPath};

/// How often the trial routes are shown while a round runs one connection after another.
const TRIAL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Half a cell diagonal, as a fraction of the cell: how far a step strays from its centre.
const STEP_REACH: f64 = std::f64::consts::FRAC_1_SQRT_2;

#[derive(Debug, Clone)]
pub struct NegotiationOptions {
    pub cell: Nm,
    pub via_cost: u32,
    pub max_rounds: usize,
    /// Price per other net covering a cell, in the first round; it grows by `present_growth`.
    pub present: f64,
    pub present_growth: f64,
    /// Added to a cell's history each round a path there collides.
    pub history_step: u32,
    pub max_seconds: Option<f64>,
    /// Above one, every connection routes against the previous round's congestion at once on
    /// this many CPU threads (Jacobi PathFinder) instead of one after another (Gauss-Seidel).
    pub threads: usize,
    /// Rounds without fewer collisions than the best so far before negotiation gives up.
    pub patience: usize,
    /// Nets whose open connections participate; every net when `None`.
    pub nets: Option<HashSet<u32>>,
    /// Per-net multiplier on history and present congestion prices (`1.0` when absent). Nets
    /// priced below `1.0` also route and commit before the rest, so a favoured escape keeps its
    /// way out of a pad while neighbours pay the full price to go round it.
    pub net_weight: HashMap<u32, f64>,
    pub pours_block: bool,
    /// Reorders the connections, for a search over many runs; 0 keeps the board's order.
    pub seed: u64,
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub log: Option<Log>,
}

impl Default for NegotiationOptions {
    fn default() -> Self {
        NegotiationOptions {
            cell: 50_000,
            via_cost: 200,
            max_rounds: 30,
            present: 20.0,
            present_growth: 1.6,
            history_step: 10,
            max_seconds: None,
            threads: 1,
            patience: 4,
            nets: None,
            net_weight: HashMap::new(),
            pours_block: false,
            seed: 0,
            cancel: None,
            log: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NegotiationStats {
    pub connections: usize,
    pub rounds: usize,
    /// Connections whose paths still collided when negotiation stopped.
    pub conflicted: usize,
    pub committed: usize,
    pub unroutable: usize,
    pub vias: usize,
    pub laid: Vec<ItemId>,
}

struct Connection {
    net: u32,
    class: String,
    from: ItemId,
    to: ItemId,
    path: Option<GridPath>,
    /// Whether its path ran into another net's claim in the last round.
    colliding: bool,
}

/// Every route's claims as each class of route sees them: a wide trace keeps further from a
/// path than a thin one needs to. Classes that see alike (the same width and clearance) share
/// one set of claims, through `alias`: on a board of many classes, most of the memory.
struct Claims {
    by_viewer: HashMap<String, Zones>,
    alias: HashMap<String, String>,
}

impl Claims {
    fn new(alias: &HashMap<String, String>, nodes: usize) -> Claims {
        let viewers: BTreeSet<&String> = alias.values().collect();
        Claims { by_viewer: viewers.into_iter().map(|c| (c.clone(), Zones::new(nodes))).collect(), alias: alias.clone() }
    }

    fn add(&mut self, net: u32, zone: &[(String, Vec<u32>)]) {
        for (viewer, cells) in zone {
            if let Some(z) = self.by_viewer.get_mut(viewer) {
                z.add(net, cells);
            }
        }
    }

    fn remove(&mut self, net: u32, zone: &[(String, Vec<u32>)]) {
        for (viewer, cells) in zone {
            if let Some(z) = self.by_viewer.get_mut(viewer) {
                z.remove(net, cells);
            }
        }
    }

    /// Every claim dropped, in place: building fresh claims beside the old ones held both at
    /// once, which on a large board is a hundred megabytes for nothing.
    fn clear(&mut self) {
        for z in self.by_viewer.values_mut() {
            z.count.fill(0);
            z.per_net.clear();
        }
    }

    fn seen_by(&self, viewer: &str) -> &Zones {
        &self.by_viewer[&self.alias[viewer]]
    }
}

/// Which nets cover each node, through per-net multiplicities so that two routes of one net
/// may overlap freely. On a large board these maps hold millions of entries, so nodes are kept
/// as `u32` in hash maps, about a third of the memory of the ordered maps they replaced.
struct Zones {
    count: Vec<u16>,
    per_net: HashMap<u32, HashMap<u32, u16>>,
}

impl Zones {
    fn new(nodes: usize) -> Zones {
        assert!(u32::try_from(nodes).is_ok(), "{nodes} grid nodes do not fit a u32 index");
        Zones { count: vec![0; nodes], per_net: HashMap::new() }
    }

    fn add(&mut self, net: u32, cells: &[u32]) {
        let mine = self.per_net.entry(net).or_default();
        for &c in cells {
            let m = mine.entry(c).or_insert(0);
            if *m == 0 {
                self.count[c as usize] = self.count[c as usize].saturating_add(1);
            }
            *m = m.saturating_add(1);
        }
    }

    fn remove(&mut self, net: u32, cells: &[u32]) {
        let Some(mine) = self.per_net.get_mut(&net) else { return };
        for &c in cells {
            if let Some(m) = mine.get_mut(&c) {
                *m -= 1;
                if *m == 0 {
                    mine.remove(&c);
                    self.count[c as usize] = self.count[c as usize].saturating_sub(1);
                }
            }
        }
        if mine.is_empty() {
            self.per_net.remove(&net);
        }
    }

    /// Nets other than `net` covering node `c`.
    fn others(&self, net: u32, c: usize) -> u16 {
        let own = self.per_net.get(&net).is_some_and(|m| m.contains_key(&(c as u32)));
        self.count[c] - u16::from(own)
    }
}

/// A connection's current trial route, as (board layer, point) corners, for showing negotiation
/// as it goes, and whether it collided with another net's route in the last round.
pub type Trial = (u32, Vec<(usize, board::Point)>, bool);

/// One round's congestion snapshot, for watching PathFinder settle.
pub struct RoundView<'a> {
    pub round: usize,
    pub colliding: usize,
    pub present: f64,
    pub grid: &'a RouteGrid,
    /// Maximum across viewer classes of how many nets claim each `(layer, i, j)` node.
    pub overlap: Vec<u16>,
    pub history: &'a [u32],
    pub trials: &'a [Trial],
}

pub fn route_negotiated(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &NegotiationOptions,
    watch: &mut dyn FnMut(&Board),
    trials: &mut dyn FnMut(usize, &[Trial]),
) -> NegotiationStats {
    route_negotiated_rounds(board, solver, options, watch, trials, None)
}

/// As [`route_negotiated`], optionally reporting each round's congestion map and trials.
pub fn route_negotiated_rounds(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &NegotiationOptions,
    watch: &mut dyn FnMut(&Board),
    trials: &mut dyn FnMut(usize, &[Trial]),
    mut on_round: Option<&mut dyn FnMut(RoundView)>,
) -> NegotiationStats {
    let started = crate::clock::Instant::now();
    let out_of_time = || {
        options.max_seconds.is_some_and(|limit| started.elapsed().as_secs_f64() > limit)
            || options.cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    };
    let mut stats = NegotiationStats::default();
    let mut open = if options.pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines;
    if let Some(only) = &options.nets {
        open.retain(|a| only.contains(&a.net));
    }
    let weight_of = |net: u32| options.net_weight.get(&net).copied().unwrap_or(1.0);
    let mut connections: Vec<Connection> = open
        .iter()
        .map(|a| Connection {
            net: a.net,
            class: Profile::for_net(board, a.net).class,
            from: a.from_item,
            to: a.to_item,
            path: None,
            colliding: false,
        })
        .collect();
    crate::jitter::shuffle(&mut connections, options.seed);
    // Favoured nets first; the sort is stable, so with no weights the order is the seed's.
    connections.sort_by(|a, b| weight_of(a.net).total_cmp(&weight_of(b.net)));
    stats.connections = connections.len();
    if connections.is_empty() {
        return stats;
    }

    let mut maps: HashMap<String, ObstacleMap> = HashMap::new();
    for c in &connections {
        if !maps.contains_key(&c.class) {
            maps.insert(c.class.clone(), ObstacleMap::new(board, Profile::for_net(board, c.net), options.cell, options.pours_block));
        }
    }
    let sample = maps.values().next().expect("one map per class").view(0);
    let (nx, ny, layers) = (sample.nx, sample.ny, sample.layers.len());
    let cells = nx * ny;
    let mut discs: HashMap<(String, String), Discs> = HashMap::new();
    for (owner, a) in &maps {
        for (viewer, b) in &maps {
            discs.insert((owner.clone(), viewer.clone()), zone_discs(board, &a.profile, &b.profile, options.cell));
        }
    }
    let classes: Vec<String> = maps.keys().cloned().collect();
    // Each class sees claims as the first class of its width and clearance does.
    let mut alias: HashMap<String, String> = HashMap::new();
    let mut sorted = classes.clone();
    sorted.sort();
    for c in &sorted {
        let p = &maps[c].profile;
        let like = sorted.iter().find(|o| maps[*o].profile.half_width == p.half_width && maps[*o].profile.clearance == p.clearance).unwrap_or(c);
        alias.insert(c.clone(), like.clone());
    }
    let viewers: Vec<String> = alias.values().cloned().collect::<BTreeSet<_>>().into_iter().collect();
    // A route's claims are worked out again from its path whenever they are needed, rather than
    // kept: kept, every route's claims for every viewer were hundreds of megabytes.
    let claims_of = |path: &GridPath, owner: &str| -> Vec<(String, Vec<u32>)> {
        viewers
            .iter()
            .map(|viewer| {
                let (trace, via) = &discs[&(owner.to_string(), viewer.clone())];
                (viewer.clone(), zone_of(path, nx, ny, layers, trace, via))
            })
            .collect()
    };
    let mut zones = Claims::new(&alias, layers * cells);
    let mut history = vec![0u32; layers * cells];
    let mut present = options.present;
    let mut grid_buf = sample.clone();
    let ends: Vec<(Vec<Terminal>, Vec<Terminal>)> = connections
        .iter()
        .map(|c| {
            maps[&c.class].view_into(c.net, &mut grid_buf);
            (terminals(board, &grid_buf, c.from), terminals(board, &grid_buf, c.to))
        })
        .collect();
    let cells_of = |t: &[Terminal]| t.iter().map(|t| t.0).collect::<Vec<usize>>();
    // A path must start and end on its own pads, however near a neighbour's route runs: those
    // cells are the footprint's doing and not counted as collisions.
    let own_ends: Vec<BTreeSet<usize>> =
        ends.iter().map(|(s, t)| s.iter().chain(t).filter(|t| t.1.is_none()).map(|t| t.0).collect()).collect();

    let mut shown = crate::clock::Instant::now();
    let (mut best_colliding, mut stale) = (usize::MAX, 0);
    for round in 0..options.max_rounds {
        if out_of_time() {
            break;
        }
        stats.rounds = round + 1;
        if solver.batch_size() > 1 {
            route_round_in_batches(&mut connections, &maps, &ends, &zones, &history, present, options, solver);
            zones.clear();
            for c in &connections {
                if let Some(p) = &c.path {
                    zones.add(c.net, &claims_of(p, &c.class));
                }
            }
        } else if options.threads > 1 {
            route_round_in_parallel(&mut connections, &maps, &ends, &zones, &history, present, options, round, &sample, trials);
            zones.clear();
            for c in &connections {
                if let Some(p) = &c.path {
                    zones.add(c.net, &claims_of(p, &c.class));
                }
            }
        } else {
            for k in 0..connections.len() {
                if out_of_time() {
                    break;
                }
                let c = &mut connections[k];
                tell(&options.log, "negotiating", || format!("round {}: rerouting {}", round + 1, board.net_name(c.net)), None);
                if let Some(p) = &c.path {
                    zones.remove(c.net, &claims_of(p, &c.class));
                }
                maps[&c.class].view_into(c.net, &mut grid_buf);
                penalties_into(zones.seen_by(&c.class), &history, present, c.net, weight_of(c.net), &mut grid_buf.penalty);
                let (sources, targets) = &ends[k];
                c.path = solver.route(&grid_buf, &cells_of(sources), &cells_of(targets), options.via_cost);
                if let Some(p) = &c.path {
                    zones.add(c.net, &claims_of(p, &c.class));
                }
                if shown.elapsed() > TRIAL_INTERVAL {
                    trials(round, &trial_list(&connections, &sample));
                    shown = crate::clock::Instant::now();
                }
            }
        }
        let mut colliding = 0;
        for (k, c) in connections.iter_mut().enumerate() {
            let Some(path) = &c.path else { continue };
            let mut collides = false;
            for &(layer, i, j) in path {
                let n = layer * cells + j * nx + i;
                if !own_ends[k].contains(&n) && zones.seen_by(&c.class).others(c.net, n) > 0 {
                    history[n] += options.history_step;
                    collides = true;
                }
            }
            colliding += usize::from(collides);
            c.colliding = collides;
        }
        let list = trial_list(&connections, &sample);
        trials(round, &list);
        if let Some(cb) = on_round.as_deref_mut() {
            let mut overlap = vec![0u16; layers * cells];
            for z in zones.by_viewer.values() {
                for (o, &c) in overlap.iter_mut().zip(&z.count) {
                    *o = (*o).max(c);
                }
            }
            cb(RoundView { round, colliding, present, grid: &sample, overlap, history: &history, trials: &list });
        }
        shown = crate::clock::Instant::now();
        stats.conflicted = colliding;
        tell(&options.log, "negotiating", || format!("round {}: {colliding} of {} routes still collide", round + 1, connections.len()), None);
        if colliding == 0 {
            break;
        }
        if colliding < best_colliding {
            best_colliding = colliding;
            stale = 0;
        } else {
            stale += 1;
            if stale >= options.patience {
                break;
            }
        }
        present *= options.present_growth;
    }

    // Commit greedily: a route whose cells lie in no committed net's zone is legal against
    // everything committed so far, converged or not; each piece is still checked exactly.
    let mut committed = Claims::new(&alias, layers * cells);
    for (k, c) in connections.iter().enumerate() {
        let Some(path) = &c.path else {
            stats.unroutable += 1;
            continue;
        };
        if path.iter().map(|&(l, i, j)| l * cells + j * nx + i).any(|n| !own_ends[k].contains(&n) && committed.seen_by(&c.class).others(c.net, n) > 0) {
            continue;
        }
        maps[&c.class].view_into(c.net, &mut grid_buf);
        let (sources, targets) = &ends[k];
        let Some(items) = lay(board, &grid_buf, path, sources, targets) else { continue };
        committed.add(c.net, &claims_of(path, &c.class));
        stats.committed += 1;
        tell(&options.log, "committing", || format!("{}: laid its negotiated route", board.net_name(c.net)), None);
        stats.vias += items.iter().filter(|id| board.item(**id).is_some_and(|it| it.kind == Kind::Via)).count();
        stats.laid.extend(items);
        watch(board);
    }
    stats
}

fn trial_list(connections: &[Connection], grid: &RouteGrid) -> Vec<Trial> {
    connections
        .iter()
        .filter_map(|c| {
            let path = c.path.as_ref()?;
            Some((c.net, corners(path).into_iter().map(|(l, i, j)| (grid.layers[l], grid.centre(i, j))).collect(), c.colliding))
        })
        .collect()
}

/// The path's cells where it turns or changes layer, and its ends.
fn corners(path: &GridPath) -> Vec<(usize, usize, usize)> {
    let mut out: Vec<(usize, usize, usize)> = Vec::new();
    for (k, &cell) in path.iter().enumerate() {
        if k > 0 && k + 1 < path.len() {
            let (a, b) = (path[k - 1], path[k + 1]);
            let straight = a.0 == cell.0 && b.0 == cell.0 && (cell.1 as i64 - a.1 as i64, cell.2 as i64 - a.2 as i64) == (b.1 as i64 - cell.1 as i64, b.2 as i64 - cell.2 as i64);
            if straight {
                continue;
            }
        }
        out.push(cell);
    }
    out
}

/// Each node's price for `net`: its history plus the present price of every other net covering
/// it, scaled by `weight`. Filled in place, so a worker reuses one buffer, and counted for all
/// nets at once before the net's own cells are taken back out, instead of asking the net's map
/// about every node.
fn penalties_into(zones: &Zones, history: &[u32], present: f64, net: u32, weight: f64, out: &mut Vec<u32>) {
    let own = zones.per_net.get(&net).into_iter().flat_map(|m| m.keys().copied());
    price_into(history, &zones.count, own, present, weight, out);
}

/// Each node's price from a round's `history` and `count` of nets covering it, the nodes `own`
/// covers counted one fewer, scaled by `weight`: what [`penalties_into`] gives a connection, for
/// a worker handed the round's counts once instead of every connection's prices.
pub fn price_into(history: &[u32], count: &[u16], own: impl Iterator<Item = u32>, present: f64, weight: f64, out: &mut Vec<u32>) {
    out.clear();
    if (weight - 1.0).abs() < 1e-12 {
        // The present price of a few covering nets, looked up rather than multiplied out.
        let table: [u32; 32] = std::array::from_fn(|n| (present * n as f64).round() as u32);
        let price = |h: u32, others: u16| h + table.get(others as usize).copied().unwrap_or_else(|| (present * f64::from(others)).round() as u32);
        out.extend(history.iter().zip(count).map(|(&h, &n)| price(h, n)));
        for c in own {
            let c = c as usize;
            out[c] = price(history[c], count[c] - 1);
        }
    } else {
        let price = |h: u32, others: u16| ((f64::from(h) + present * f64::from(others)) * weight).round().max(0.0) as u32;
        out.extend(history.iter().zip(count).map(|(&h, &n)| price(h, n)));
        for c in own {
            let c = c as usize;
            out[c] = price(history[c], count[c] - 1);
        }
    }
}

/// One Jacobi round with the fields solved together, a batch at a time, on the solver's device.
#[allow(clippy::too_many_arguments)]
fn route_round_in_batches(
    connections: &mut [Connection],
    maps: &HashMap<String, ObstacleMap>,
    ends: &[(Vec<Terminal>, Vec<Terminal>)],
    zones: &Claims,
    history: &[u32],
    present: f64,
    options: &NegotiationOptions,
    solver: &mut dyn FieldSolver,
) {
    let size = solver.batch_size().max(1);
    let sample = maps.values().next().expect("one map per class").view(0);
    let mut grids: Vec<RouteGrid> = (0..size.min(connections.len())).map(|_| sample.clone()).collect();
    // The round's prices are the same for every connection: a solver that can price them itself
    // is handed them once, and each connection's grid without prices.
    static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut classes: Vec<&String> = maps.keys().collect();
    classes.sort();
    let prices = RoundPrices { generation, history, counts: classes.iter().map(|c| zones.seen_by(c).count.as_slice()).collect(), present };
    let mut priced = true;
    for start in (0..connections.len()).step_by(size) {
        let end = (start + size).min(connections.len());
        if priced {
            let count = end - start;
            for (c, grid) in connections[start..end].iter().zip(grids[..count].iter_mut()) {
                maps[&c.class].view_into(c.net, grid);
            }
            let sources: Vec<Vec<usize>> = ends[start..end].iter().map(|(s, _)| s.iter().map(|t| t.0).collect()).collect();
            let targets: Vec<Vec<usize>> = ends[start..end].iter().map(|(_, t)| t.iter().map(|t| t.0).collect()).collect();
            let jobs: Vec<PricedJob> = connections[start..end]
                .iter()
                .zip(&grids[..count])
                .zip(sources.iter().zip(&targets))
                .map(|((c, grid), (s, t))| PricedJob {
                    grid,
                    class: classes.iter().position(|k| **k == c.class).expect("a map per class"),
                    weight: options.net_weight.get(&c.net).copied().unwrap_or(1.0),
                    own: zones.seen_by(&c.class).per_net.get(&c.net).map(|m| m.keys().copied().collect()).unwrap_or_default(),
                    sources: s,
                    targets: t,
                })
                .collect();
            if let Some(routed) = solver.route_priced(&prices, &jobs, options.via_cost) {
                for (k, path) in routed.into_iter().enumerate() {
                    connections[start + k].path = path;
                }
                continue;
            }
            priced = false;
        }
        let count = end - start;
        for (c, grid) in connections[start..end].iter().zip(grids[..count].iter_mut()) {
            maps[&c.class].view_into(c.net, grid);
            let w = options.net_weight.get(&c.net).copied().unwrap_or(1.0);
            penalties_into(zones.seen_by(&c.class), history, present, c.net, w, &mut grid.penalty);
        }
        let sources: Vec<Vec<usize>> = ends[start..end].iter().map(|(s, _)| s.iter().map(|t| t.0).collect()).collect();
        let targets: Vec<Vec<usize>> = ends[start..end].iter().map(|(_, t)| t.iter().map(|t| t.0).collect()).collect();
        let jobs: Vec<(&RouteGrid, &[usize], &[usize])> = grids[..count]
            .iter()
            .zip(&sources)
            .zip(&targets)
            .map(|((g, s), t)| (g, s.as_slice(), t.as_slice()))
            .collect();
        let routed = solver.route_batch(&jobs, options.via_cost);
        for (k, path) in routed.into_iter().enumerate() {
            connections[start + k].path = path;
        }
    }
}

/// One Jacobi round: every connection routes on the CPU against the same congestion snapshot.
/// While the workers route, this thread shows what they have found so far.
#[allow(clippy::too_many_arguments)]
fn route_round_in_parallel(
    connections: &mut [Connection],
    maps: &HashMap<String, ObstacleMap>,
    ends: &[(Vec<Terminal>, Vec<Terminal>)],
    zones: &Claims,
    history: &[u32],
    present: f64,
    options: &NegotiationOptions,
    round: usize,
    sample: &RouteGrid,
    trials: &mut dyn FnMut(usize, &[Trial]),
) {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let jobs: Vec<(u32, &str, f64)> =
        connections.iter().map(|c| (c.net, c.class.as_str(), options.net_weight.get(&c.net).copied().unwrap_or(1.0))).collect();
    let paths: Vec<std::sync::Mutex<Option<Option<GridPath>>>> = (0..connections.len()).map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..options.threads {
            scope.spawn(|| {
                // One grid per worker, refilled for each connection: a large board's views run
                // to tens of megabytes, too much to allocate and free hundreds of times a round.
                let mut grid: Option<RouteGrid> = None;
                loop {
                    let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if k >= jobs.len() {
                        break;
                    }
                    let (net, class, weight) = jobs[k];
                    let grid = match &mut grid {
                        Some(g) => {
                            maps[class].view_into(net, g);
                            g
                        }
                        None => grid.insert(maps[class].view(net)),
                    };
                    penalties_into(zones.seen_by(class), history, present, net, weight, &mut grid.penalty);
                    let (sources, targets) = &ends[k];
                    let s: Vec<usize> = sources.iter().map(|t| t.0).collect();
                    let t: Vec<usize> = targets.iter().map(|t| t.0).collect();
                    *paths[k].lock().expect("no worker panics") = Some(route_astar(grid, &s, &t, options.via_cost));
                    done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            });
        }
        while done.load(std::sync::atomic::Ordering::Relaxed) < jobs.len() {
            std::thread::sleep(TRIAL_INTERVAL);
            let shown: Vec<Trial> = connections
                .iter()
                .zip(&paths)
                .filter_map(|(c, fresh)| {
                    let fresh = fresh.lock().expect("no worker panics").clone();
                    let path = fresh.unwrap_or_else(|| c.path.clone())?;
                    Some((c.net, corners(&path).into_iter().map(|(l, i, j)| (sample.layers[l], sample.centre(i, j))).collect(), c.colliding))
                })
                .collect();
            trials(round, &shown);
            let n = done.load(std::sync::atomic::Ordering::Relaxed);
            tell(&options.log, "negotiating", || format!("round {}: {n} of {} rerouted", round + 1, jobs.len()), None);
        }
    });
    for (c, path) in connections.iter_mut().zip(paths) {
        if let Some(path) = path.into_inner().expect("no worker panics") {
            c.path = path;
        }
    }
}

/// Cell offsets claimed around a trace cell and around a via cell.
type Discs = (Vec<(i32, i32)>, Vec<(i32, i32)>);

/// Cell offsets within the zone radius of an owner's trace cell and via cell, as a viewer class
/// sees them: the owner's trace or via against the viewer's trace, at the clearance between the
/// two classes, plus each side's step reach.
fn zone_discs(board: &Board, owner: &Profile, viewer: &Profile, h: Nm) -> Discs {
    let (half_width, via, widest) = (owner.half_width as f64, owner.via_radius as f64, viewer.half_width as f64);
    let clearance = owner.clearance.max(viewer.clearance).max(nm(board.rules.min_clearance)) as f64;
    let h = h as f64;
    let trace_reach = half_width + widest + clearance + 2.0 * STEP_REACH * h;
    let via_reach = via + widest + clearance + 2.0 * STEP_REACH * h;
    let disc = |reach: f64| {
        let r = (reach / h).ceil() as i32;
        (-r..=r)
            .flat_map(|dj| (-r..=r).map(move |di| (di, dj)))
            .filter(|&(di, dj)| f64::from(di).hypot(f64::from(dj)) * h < reach + h)
            .collect()
    };
    (disc(trace_reach), disc(via_reach))
}

/// The nodes a path claims: a disc around each cell on its layer, and a via disc on every layer
/// where it changes layer. Deduplicated.
fn zone_of(path: &GridPath, nx: usize, ny: usize, layers: usize, trace_disc: &[(i32, i32)], via_disc: &[(i32, i32)]) -> Vec<u32> {
    let cells = nx * ny;
    let mut out = Vec::new();
    let mut stamp = |layer: usize, i: usize, j: usize, disc: &[(i32, i32)]| {
        for &(di, dj) in disc {
            let (a, b) = (i as i32 + di, j as i32 + dj);
            if a >= 0 && b >= 0 && (a as usize) < nx && (b as usize) < ny {
                out.push((layer * cells + b as usize * nx + a as usize) as u32);
            }
        }
    };
    for (k, &(layer, i, j)) in path.iter().enumerate() {
        stamp(layer, i, j, trace_disc);
        if k > 0 && path[k - 1].0 != layer {
            for l in 0..layers {
                stamp(l, i, j, via_disc);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out.shrink_to_fit();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift(mut state: u64) -> impl FnMut() -> u64 {
        move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        }
    }

    #[test]
    fn zones_count_each_net_once_however_often_its_routes_overlap() {
        let mut z = Zones::new(10);
        z.add(1, &[2, 3]);
        z.add(1, &[3, 4]);
        z.add(2, &[3]);
        assert_eq!(z.count[3], 2);
        assert_eq!((z.others(1, 3), z.others(2, 3), z.others(3, 3)), (1, 1, 2));
        z.remove(1, &[2, 3]);
        assert_eq!(z.count[3], 2, "net 1's other route still covers 3");
        assert_eq!(z.count[2], 0);
        z.remove(1, &[3, 4]);
        assert_eq!((z.count[3], z.count[4]), (1, 0));
        assert!(!z.per_net.contains_key(&1), "an emptied net leaves no map behind");
        z.remove(9, &[3]);
        assert_eq!(z.count[3], 1, "removing what was never added changes nothing");
    }

    #[test]
    fn penalties_match_asking_every_node() {
        let mut next = xorshift(0x5eed);
        let nodes = 500;
        let mut z = Zones::new(nodes);
        let mut added: Vec<(u32, Vec<u32>)> = Vec::new();
        for _ in 0..300 {
            if added.len() > 5 && next() % 3 == 0 {
                let (net, cells) = added.swap_remove((next() % added.len() as u64) as usize);
                z.remove(net, &cells);
            } else {
                let net = (next() % 6) as u32;
                let mut cells: Vec<u32> = (0..next() % 40).map(|_| (next() % nodes as u64) as u32).collect();
                cells.sort_unstable();
                cells.dedup();
                z.add(net, &cells);
                added.push((net, cells));
            }
            let history: Vec<u32> = (0..nodes).map(|_| (next() % 50) as u32).collect();
            let net = (next() % 7) as u32;
            let mut fast = vec![7; 3];
            penalties_into(&z, &history, 23.7, net, 1.0, &mut fast);
            let slow: Vec<u32> = (0..nodes).map(|n| history[n] + (23.7 * f64::from(z.others(net, n))).round() as u32).collect();
            assert_eq!(fast, slow);

            let mut scaled = Vec::new();
            penalties_into(&z, &history, 23.7, net, 0.25, &mut scaled);
            let slow_scaled: Vec<u32> =
                (0..nodes).map(|n| ((f64::from(history[n]) + 23.7 * f64::from(z.others(net, n))) * 0.25).round() as u32).collect();
            assert_eq!(scaled, slow_scaled);
        }
    }

    #[test]
    fn a_zone_is_sorted_unique_and_on_the_grid() {
        let (nx, ny, layers) = (8, 6, 2);
        let disc: Vec<(i32, i32)> = (-1..=1).flat_map(|dj| (-1..=1).map(move |di| (di, dj))).collect();
        let path: GridPath = vec![(0, 0, 0), (0, 1, 0), (1, 1, 0), (1, 7, 5)];
        let z = zone_of(&path, nx, ny, layers, &disc, &disc);
        assert!(z.windows(2).all(|w| w[0] < w[1]));
        assert!(z.iter().all(|&n| (n as usize) < nx * ny * layers));
        // The via at (1, 0) claims its disc on both layers, the corner cell clipped to the grid.
        assert!(z.contains(&0) && z.contains(&((nx * ny) as u32)));
        assert!(z.contains(&((nx * ny + 5 * nx + 7) as u32)));
    }
}
