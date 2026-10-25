//! Negotiated congestion (PathFinder, McMurchie and Ebeling 1995) on the conservative grid.
//!
//! Fixed copper stays hard-blocked. Routes found during negotiation are not: each net claims a
//! zone of cells around its paths, wide enough that two nets outside each other's zones are
//! legal on the board, and entering a cell inside another net's zone costs a present-congestion
//! price plus an accumulated history price. A net's own zone is lifted before it re-routes.
//! Rounds repeat, the present price rising and history accumulating where nets still collide,
//! until no path lies in another net's zone; then routes are committed, each checked exactly.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use board::{connectivity, connectivity_without_pours, nm, Board, ItemId, Kind, Nm};

use crate::grid::{ObstacleMap, Profile, RouteGrid};
use crate::route::{lay, terminals, FieldSolver, Terminal};
use crate::search::{cost_field, trace_back, GridPath};

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
    pub pours_block: bool,
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
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
            pours_block: false,
            cancel: None,
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
    zone: Vec<usize>,
}

/// Which nets cover each node, through per-net multiplicities so that two routes of one net
/// may overlap freely.
struct Zones {
    count: Vec<u16>,
    per_net: BTreeMap<u32, BTreeMap<usize, u32>>,
}

impl Zones {
    fn new(nodes: usize) -> Zones {
        Zones { count: vec![0; nodes], per_net: BTreeMap::new() }
    }

    fn add(&mut self, net: u32, cells: &[usize]) {
        let mine = self.per_net.entry(net).or_default();
        for &c in cells {
            let m = mine.entry(c).or_insert(0);
            if *m == 0 {
                self.count[c] = self.count[c].saturating_add(1);
            }
            *m += 1;
        }
    }

    fn remove(&mut self, net: u32, cells: &[usize]) {
        let Some(mine) = self.per_net.get_mut(&net) else { return };
        for &c in cells {
            if let Some(m) = mine.get_mut(&c) {
                *m -= 1;
                if *m == 0 {
                    mine.remove(&c);
                    self.count[c] = self.count[c].saturating_sub(1);
                }
            }
        }
    }

    /// Nets other than `net` covering node `c`.
    fn others(&self, net: u32, c: usize) -> u16 {
        let own = self.per_net.get(&net).is_some_and(|m| m.contains_key(&c));
        self.count[c] - u16::from(own)
    }
}

/// A connection's current trial route, as (board layer, point) corners, for showing negotiation
/// as it goes.
pub type Trial = (u32, Vec<(usize, board::Point)>);

pub fn route_negotiated(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &NegotiationOptions,
    watch: &mut dyn FnMut(&Board),
    trials: &mut dyn FnMut(usize, &[Trial]),
) -> NegotiationStats {
    let started = std::time::Instant::now();
    let out_of_time = || {
        options.max_seconds.is_some_and(|limit| started.elapsed().as_secs_f64() > limit)
            || options.cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    };
    let mut stats = NegotiationStats::default();
    let open = if options.pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines;
    let mut connections: Vec<Connection> = open
        .iter()
        .map(|a| Connection {
            net: a.net,
            class: Profile::for_net(board, a.net).class,
            from: a.from_item,
            to: a.to_item,
            path: None,
            zone: Vec::new(),
        })
        .collect();
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
    let (trace_disc, via_disc) = zone_discs(board, &maps, options.cell);

    let mut zones = Zones::new(layers * cells);
    let mut history = vec![0u32; layers * cells];
    let mut present = options.present;
    let ends: Vec<(Vec<Terminal>, Vec<Terminal>)> = connections
        .iter()
        .map(|c| {
            let grid = maps[&c.class].view(c.net);
            (terminals(board, &grid, c.from), terminals(board, &grid, c.to))
        })
        .collect();
    let cells_of = |t: &[Terminal]| t.iter().map(|t| t.0).collect::<Vec<usize>>();

    let mut shown = std::time::Instant::now();
    for round in 0..options.max_rounds {
        if out_of_time() {
            break;
        }
        stats.rounds = round + 1;
        if options.threads > 1 {
            route_round_in_parallel(&mut connections, &maps, &ends, &zones, &history, present, options);
            zones = Zones::new(layers * cells);
            for c in connections.iter_mut() {
                c.zone = c.path.as_ref().map(|p| zone_of(p, nx, ny, layers, &trace_disc, &via_disc)).unwrap_or_default();
                zones.add(c.net, &c.zone);
            }
        } else {
            for k in 0..connections.len() {
                if out_of_time() {
                    break;
                }
                let c = &mut connections[k];
                zones.remove(c.net, &c.zone);
                let mut grid = maps[&c.class].view(c.net);
                grid.penalty = penalties(&zones, &history, present, c.net);
                let (sources, targets) = &ends[k];
                let cost = solver.solve(&grid, &cells_of(sources), options.via_cost);
                c.path = trace_back(&grid, &cost, &cells_of(targets), options.via_cost);
                c.zone = c.path.as_ref().map(|p| zone_of(p, nx, ny, layers, &trace_disc, &via_disc)).unwrap_or_default();
                zones.add(c.net, &c.zone);
                if shown.elapsed() > TRIAL_INTERVAL {
                    trials(round, &trial_list(&connections, &sample));
                    shown = std::time::Instant::now();
                }
            }
        }
        trials(round, &trial_list(&connections, &sample));
        shown = std::time::Instant::now();
        let mut colliding = 0;
        for c in &connections {
            let Some(path) = &c.path else { continue };
            let mut collides = false;
            for &(layer, i, j) in path {
                let n = layer * cells + j * nx + i;
                if zones.others(c.net, n) > 0 {
                    history[n] += options.history_step;
                    collides = true;
                }
            }
            colliding += usize::from(collides);
        }
        stats.conflicted = colliding;
        if colliding == 0 {
            break;
        }
        present *= options.present_growth;
    }

    // Commit greedily: a route whose cells lie in no committed net's zone is legal against
    // everything committed so far, converged or not; each piece is still checked exactly.
    let mut committed = Zones::new(layers * cells);
    for (k, c) in connections.iter().enumerate() {
        let Some(path) = &c.path else {
            stats.unroutable += 1;
            continue;
        };
        if path.iter().any(|&(l, i, j)| committed.others(c.net, l * cells + j * nx + i) > 0) {
            continue;
        }
        let grid = maps[&c.class].view(c.net);
        let (sources, targets) = &ends[k];
        let Some(items) = lay(board, &grid, path, sources, targets) else { continue };
        committed.add(c.net, &c.zone);
        stats.committed += 1;
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
            Some((c.net, corners(path).into_iter().map(|(l, i, j)| (grid.layers[l], grid.centre(i, j))).collect()))
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

fn penalties(zones: &Zones, history: &[u32], present: f64, net: u32) -> Vec<u32> {
    (0..history.len()).map(|n| history[n] + (present * f64::from(zones.others(net, n))).round() as u32).collect()
}

/// One Jacobi round: every connection routes on the CPU against the same congestion snapshot.
fn route_round_in_parallel(
    connections: &mut [Connection],
    maps: &HashMap<String, ObstacleMap>,
    ends: &[(Vec<Terminal>, Vec<Terminal>)],
    zones: &Zones,
    history: &[u32],
    present: f64,
    options: &NegotiationOptions,
) {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let jobs: Vec<(u32, &str)> = connections.iter().map(|c| (c.net, c.class.as_str())).collect();
    let paths: Vec<std::sync::Mutex<Option<GridPath>>> = (0..connections.len()).map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..options.threads {
            scope.spawn(|| loop {
                let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if k >= jobs.len() {
                    break;
                }
                let (net, class) = jobs[k];
                let mut grid: RouteGrid = maps[class].view(net);
                grid.penalty = penalties(zones, history, present, net);
                let (sources, targets) = &ends[k];
                let s: Vec<usize> = sources.iter().map(|t| t.0).collect();
                let t: Vec<usize> = targets.iter().map(|t| t.0).collect();
                let cost = cost_field(&grid, &s, options.via_cost);
                *paths[k].lock().expect("no worker panics") = trace_back(&grid, &cost, &t, options.via_cost);
            });
        }
    });
    for (c, path) in connections.iter_mut().zip(paths) {
        c.path = path.into_inner().expect("no worker panics");
    }
}

/// Cell offsets within the zone radius of a trace cell and of a via cell: the widest trace and
/// via on the board against the widest trace, at the largest clearance, plus each side's step
/// reach.
fn zone_discs(board: &Board, maps: &HashMap<String, ObstacleMap>, h: Nm) -> (Vec<(i32, i32)>, Vec<(i32, i32)>) {
    let widest = maps.values().map(|m| m.profile.half_width).max().unwrap_or(0) as f64;
    let via = maps.values().map(|m| m.profile.via_radius).max().unwrap_or(0) as f64;
    let clearance = board.rules.classes.iter().map(|c| nm(c.clearance)).chain([nm(board.rules.min_clearance)]).max().unwrap_or(0) as f64;
    let h = h as f64;
    let trace_reach = 2.0 * widest + clearance + 2.0 * STEP_REACH * h;
    let via_reach = via + widest.max(via) + clearance + 2.0 * STEP_REACH * h;
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
fn zone_of(path: &GridPath, nx: usize, ny: usize, layers: usize, trace_disc: &[(i32, i32)], via_disc: &[(i32, i32)]) -> Vec<usize> {
    let cells = nx * ny;
    let mut out = BTreeSet::new();
    let mut stamp = |layer: usize, i: usize, j: usize, disc: &[(i32, i32)]| {
        for &(di, dj) in disc {
            let (a, b) = (i as i32 + di, j as i32 + dj);
            if a >= 0 && b >= 0 && (a as usize) < nx && (b as usize) < ny {
                out.insert(layer * cells + b as usize * nx + a as usize);
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
    out.into_iter().collect()
}
