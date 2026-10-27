//! Negotiated congestion (PathFinder, McMurchie and Ebeling 1995) on the conservative grid.
//!
//! Fixed copper stays hard-blocked. Routes found during negotiation are not: each net claims a
//! zone of cells around its paths, wide enough that two nets outside each other's zones are
//! legal on the board, and entering a cell inside another net's zone costs a present-congestion
//! price plus an accumulated history price. A net's own zone is lifted before it re-routes.
//! Rounds repeat, the present price rising and history accumulating where nets still collide,
//! until no path lies in another net's zone; then routes are committed, each checked exactly.

use std::collections::{BTreeSet, HashMap, HashSet};

use board::{connectivity, connectivity_without_pours, nm, Airline, Board, Connectivity, ItemId, Kind, Nm, Point, Shape};

use crate::grid::{ObstacleMap, Profile, RouteGrid};
use crate::report::{tell, Log};
use crate::route::{lay, pieces, terminals, FieldSolver, PricedJob, RoundPrices, Terminal};
use crate::search::{route_astar, GridPath};

/// How often the trial routes are shown while a round runs one connection after another.
const TRIAL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Half a cell diagonal, as a fraction of the cell: how far a step strays from its nearer end.
const STEP_REACH: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// Margin kept beyond exact geometry in zone discs so cell-centre rounding never lets a violating
/// pair slip past KiCad's 0.5 um slack.
const ZONE_MARGIN: f64 = 1_000.0;

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
    /// Price and check via-to-trace, via-to-via, and same-net hole-to-hole claims on layer
    /// changes, with exact segment-to-segment zone radii.
    pub via_zones: bool,
    /// Follow-up negotiation passes on the routable connections left uncommitted after laying a
    /// pass's routes.
    pub repasses: usize,
    /// When a connection that was routable before committing is walled off by newly committed
    /// copper, rip the conflicting routes from this call and renegotiate them together, keeping
    /// the result only when strictly fewer connections remain open.
    pub rip_blocked: bool,
    /// Build each multi-cluster net's spanning tree across grid-reachable pairs before pocketed
    /// ones, so a trapped middle pin does not split open pins apart.
    pub reachable_tree: bool,
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
            via_zones: true,
            repasses: 2,
            rip_blocked: true,
            reachable_tree: true,
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
    /// Extra cells covered by full-width terminal legs outside the terminals' own copper.
    legs: Vec<(usize, usize, usize)>,
    /// Unique `(i, j)` cell indices where `path` changes layer.
    vias: Vec<usize>,
    /// The `(layer, i, j)` nodes its path claims against a viewer's trace, per viewer class.
    zone: Vec<(String, Vec<u32>)>,
    /// The `(i, j)` cells its path claims against a viewer's via, per viewer class.
    via_zone: Vec<(String, Vec<u32>)>,
    /// Nearby `(i, j)` cells around its vias where another via of the same net would break
    /// hole-to-hole clearance; `(0, 0)` is excluded so two routes of one net may share a via.
    hole_ring: Vec<u32>,
    /// Whether its path or vias ran into another claim in the last round.
    colliding: bool,
}

#[derive(Clone)]
struct LaidRoute {
    net: u32,
    items: Vec<ItemId>,
    zone: Vec<(String, Vec<u32>)>,
    via_zone: Vec<(String, Vec<u32>)>,
    hole_ring: Vec<u32>,
}

/// Every route's claims as each class of route sees them: a wide trace or via keeps further from
/// a path than a thin trace needs to.
struct Claims {
    by_viewer: HashMap<String, Zones>,
}

impl Claims {
    fn new(classes: impl Iterator<Item = String>, nodes: usize) -> Claims {
        Claims { by_viewer: classes.map(|c| (c, Zones::new(nodes))).collect() }
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
        &self.by_viewer[viewer]
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

/// Same-net hole-to-hole annulus counts per net: non-coincident cells around each route's vias
/// where a sibling route of the same net may not drill a second hole.
#[derive(Default)]
struct SameNetHoles {
    per_net: HashMap<u32, HashMap<u32, u16>>,
}

impl SameNetHoles {
    fn add(&mut self, net: u32, cells: &[u32]) {
        if cells.is_empty() {
            return;
        }
        let mine = self.per_net.entry(net).or_default();
        for &c in cells {
            let m = mine.entry(c).or_insert(0);
            *m = m.saturating_add(1);
        }
    }

    fn remove(&mut self, net: u32, cells: &[u32]) {
        if cells.is_empty() {
            return;
        }
        let Some(mine) = self.per_net.get_mut(&net) else { return };
        for &c in cells {
            if let Some(m) = mine.get_mut(&c) {
                *m -= 1;
                if *m == 0 {
                    mine.remove(&c);
                }
            }
        }
        if mine.is_empty() {
            self.per_net.remove(&net);
        }
    }

    fn clear(&mut self) {
        self.per_net.clear();
    }

    fn count(&self, net: u32, cell: usize) -> u16 {
        self.per_net.get(&net).and_then(|m| m.get(&(cell as u32))).copied().unwrap_or(0)
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
    let conn = board_conn(board, options.pours_block);
    let mut open = filtered_airlines(&conn, &options.nets);
    stats.connections = open.len();
    if open.is_empty() {
        return stats;
    }

    let mut maps: HashMap<String, ObstacleMap> = HashMap::new();
    for a in &open {
        let p = Profile::for_net(board, a.net);
        if !maps.contains_key(&p.class) {
            maps.insert(p.class.clone(), ObstacleMap::new(board, p, options.cell, options.pours_block));
        }
    }
    let sample = maps.values().next().expect("one map per class").view(0);
    let mut grid_buf = sample.clone();
    if options.reachable_tree {
        open = reachable_airlines(board, &conn, open, &maps, &mut grid_buf);
    }

    let mut discs: HashMap<(String, String), AllDiscs> = HashMap::new();
    let mut hole_discs: HashMap<String, Vec<(i32, i32)>> = HashMap::new();
    for (owner, a) in &maps {
        hole_discs.insert(owner.clone(), same_hole_disc(board, &a.profile, options.cell));
        for (viewer, b) in &maps {
            discs.insert((owner.clone(), viewer.clone()), all_zone_discs(board, &a.profile, &b.profile, options.cell, options.via_zones));
        }
    }
    let mut classes: Vec<String> = maps.keys().cloned().collect();
    classes.sort();

    let base_maps = options.rip_blocked.then(|| maps.clone());
    let mut laid_routes: Vec<LaidRoute> = Vec::new();
    let stamp_laid = options.repasses > 0 || options.rip_blocked;

    let pass0 = run_pass(
        board,
        solver,
        options,
        &mut maps,
        &classes,
        &discs,
        &hole_discs,
        &sample,
        &mut grid_buf,
        open,
        options.max_rounds,
        &options.net_weight,
        stamp_laid,
        true,
        &out_of_time,
        watch,
        trials,
        &mut on_round,
        &mut laid_routes,
    );
    stats.rounds = pass0.rounds;
    stats.conflicted = pass0.conflicted;
    stats.unroutable = pass0.unroutable;
    stats.committed = pass0.committed;
    stats.vias = pass0.vias;
    stats.laid = pass0.laid;

    for repass in 0..options.repasses {
        if out_of_time() {
            break;
        }
        let next_open = routable_open_airlines(board, options, &mut maps, &discs, &mut hole_discs, &mut classes, &mut grid_buf);
        if next_open.is_empty() {
            break;
        }
        let mut sub_opts = options.clone();
        sub_opts.seed = options.seed.wrapping_add((repass as u64 + 1).wrapping_mul(0x9e37_79b9));
        let sub = run_pass(
            board,
            solver,
            &sub_opts,
            &mut maps,
            &classes,
            &discs,
            &hole_discs,
            &sample,
            &mut grid_buf,
            next_open,
            options.max_rounds.min(16),
            &options.net_weight,
            stamp_laid,
            false,
            &out_of_time,
            watch,
            &mut |_, _| {},
            &mut None,
            &mut laid_routes,
        );
        if sub.committed == 0 {
            break;
        }
    }

    if let Some(base_maps) = &base_maps {
        if !out_of_time() {
            let cur_open = filtered_airlines(&board_conn(board, options.pours_block), &options.nets).len();
            if cur_open > stats.unroutable {
                rescue_walled_off(
                    board,
                    solver,
                    options,
                    base_maps,
                    &mut maps,
                    &mut classes,
                    &discs,
                    &mut hole_discs,
                    &sample,
                    &mut grid_buf,
                    &out_of_time,
                    watch,
                    &mut laid_routes,
                );
            }
        }
    }

    if options.repasses > 0 || options.rip_blocked {
        let left = filtered_airlines(&board_conn(board, options.pours_block), &options.nets).len();
        stats.committed = stats.connections.saturating_sub(left);
        stats.laid = laid_routes.iter().flat_map(|r| r.items.iter().copied()).collect();
        stats.vias = stats.laid.iter().filter(|id| board.item(**id).is_some_and(|it| it.kind == Kind::Via)).count();
    }
    stats
}

struct PassSummary {
    rounds: usize,
    conflicted: usize,
    unroutable: usize,
    committed: usize,
    vias: usize,
    laid: Vec<ItemId>,
}

#[allow(clippy::too_many_arguments)]
fn run_pass(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &NegotiationOptions,
    maps: &mut HashMap<String, ObstacleMap>,
    classes: &[String],
    discs: &HashMap<(String, String), AllDiscs>,
    hole_discs: &HashMap<String, Vec<(i32, i32)>>,
    sample: &RouteGrid,
    grid_buf: &mut RouteGrid,
    open: Vec<Airline>,
    max_rounds: usize,
    weights: &HashMap<u32, f64>,
    stamp_laid: bool,
    report_trials: bool,
    out_of_time: &dyn Fn() -> bool,
    watch: &mut dyn FnMut(&Board),
    trials: &mut dyn FnMut(usize, &[Trial]),
    on_round: &mut Option<&mut dyn FnMut(RoundView)>,
    laid_routes: &mut Vec<LaidRoute>,
) -> PassSummary {
    let mut summary = PassSummary { rounds: 0, conflicted: 0, unroutable: 0, committed: 0, vias: 0, laid: Vec::new() };
    if open.is_empty() || max_rounds == 0 {
        return summary;
    }
    let weight_of = |net: u32| weights.get(&net).copied().unwrap_or(1.0);
    let mut connections: Vec<Connection> = open
        .iter()
        .map(|a| Connection {
            net: a.net,
            class: Profile::for_net(board, a.net).class,
            from: a.from_item,
            to: a.to_item,
            path: None,
            legs: Vec::new(),
            vias: Vec::new(),
            zone: Vec::new(),
            via_zone: Vec::new(),
            hole_ring: Vec::new(),
            colliding: false,
        })
        .collect();
    crate::jitter::shuffle(&mut connections, options.seed);
    // Favoured nets first; the sort is stable, so with no weights the order is the seed's.
    connections.sort_by(|a, b| weight_of(a.net).total_cmp(&weight_of(b.net)));

    let (nx, ny, layers) = (sample.nx, sample.ny, sample.layers.len());
    let cells = nx * ny;
    let via_scale = layers.max(2) as f64;
    let via_hist_step = options.history_step * layers.max(2) as u32;

    let mut zones = Claims::new(classes.iter().cloned(), layers * cells);
    let mut via_zones = Claims::new(classes.iter().cloned(), cells);
    let mut same_holes = SameNetHoles::default();
    let mut history = vec![0u32; layers * cells];
    let mut via_history = vec![0u32; cells];
    let mut present = options.present;

    let ends: Vec<(Vec<Terminal>, Vec<Terminal>)> = connections
        .iter()
        .map(|c| {
            maps[&c.class].view_into(c.net, grid_buf);
            (terminals(board, grid_buf, c.from), terminals(board, grid_buf, c.to))
        })
        .collect();
    let cells_of = |t: &[Terminal]| t.iter().map(|t| t.0).collect::<Vec<usize>>();
    // A path must start and end on its own pads, however near a neighbour's route runs: those
    // cells are the footprint's doing and not counted as collisions.
    let own_ends: Vec<BTreeSet<usize>> =
        ends.iter().map(|(s, t)| s.iter().chain(t).filter(|t| t.1.is_none()).map(|t| t.0).collect()).collect();

    let mut stamp_buf = StampBuf::new(layers * cells);
    let update_claims = |k: usize, c: &mut Connection, stamp_buf: &mut StampBuf| {
        let Some(path) = &c.path else {
            c.legs.clear();
            c.vias.clear();
            c.zone.clear();
            c.via_zone.clear();
            c.hole_ring.clear();
            return;
        };
        let owner_hw = maps[&c.class].profile.half_width;
        c.legs = if options.via_zones {
            leg_cells(sample, path, &ends[k].0, &ends[k].1, owner_hw, &own_ends[k])
        } else {
            Vec::new()
        };
        c.vias = via_cells_of(path, nx);
        c.zone = classes
            .iter()
            .map(|viewer| {
                let d = &discs[&(c.class.clone(), viewer.clone())];
                (viewer.clone(), zone_of_fast(path, &c.legs, &c.vias, nx, ny, layers, &d.trace_to_trace, &d.via_to_trace, stamp_buf))
            })
            .collect();
        if options.via_zones {
            c.via_zone = classes
                .iter()
                .map(|viewer| {
                    let d = &discs[&(c.class.clone(), viewer.clone())];
                    (viewer.clone(), via_zone_of_fast(path, &c.legs, &c.vias, nx, ny, &d.trace_to_via, &d.via_to_via, stamp_buf))
                })
                .collect();
            c.hole_ring = stamp_cells_2d(&c.vias, nx, ny, &hole_discs[&c.class], stamp_buf);
        }
    };

    // On a full board `colliding` steps down gradually and resets `stale` several times while
    // `present` warms up; on a small sub-problem (`<= 24` connections) `colliding` often starts
    // at 1..3 with no intermediate steps before 0, so `patience = 4` would quit at `present = 131`
    // before a pair of vias (`2 * via_cost`) or a short detour looks cheaper than overlapping.
    let patience = if (options.via_zones || options.repasses > 0 || options.rip_blocked) && connections.len() <= 24 {
        options.patience.max(8)
    } else {
        options.patience
    };
    let mut shown = crate::clock::Instant::now();
    let (mut best_colliding, mut stale) = (usize::MAX, 0);
    for round in 0..max_rounds {
        if out_of_time() {
            break;
        }
        summary.rounds = round + 1;
        if solver.batch_size() > 1 {
            route_round_in_batches(&mut connections, maps, &ends, &zones, &via_zones, &same_holes, &history, &via_history, present, via_scale, weights, options, solver);
            zones.clear();
            via_zones.clear();
            same_holes.clear();
            for (k, c) in connections.iter_mut().enumerate() {
                update_claims(k, c, &mut stamp_buf);
                zones.add(c.net, &c.zone);
                if options.via_zones {
                    via_zones.add(c.net, &c.via_zone);
                    same_holes.add(c.net, &c.hole_ring);
                }
            }
        } else if options.threads > 1 {
            route_round_in_parallel(
                &mut connections,
                maps,
                &ends,
                &zones,
                &via_zones,
                &same_holes,
                &history,
                &via_history,
                present,
                via_scale,
                weights,
                options,
                round,
                sample,
                report_trials,
                trials,
            );
            zones.clear();
            via_zones.clear();
            same_holes.clear();
            for (k, c) in connections.iter_mut().enumerate() {
                update_claims(k, c, &mut stamp_buf);
                zones.add(c.net, &c.zone);
                if options.via_zones {
                    via_zones.add(c.net, &c.via_zone);
                    same_holes.add(c.net, &c.hole_ring);
                }
            }
        } else {
            for k in 0..connections.len() {
                if out_of_time() {
                    break;
                }
                let c = &mut connections[k];
                if report_trials {
                    tell(&options.log, "negotiating", || format!("round {}: rerouting {}", round + 1, board.net_name(c.net)), None);
                }
                zones.remove(c.net, &c.zone);
                if options.via_zones {
                    via_zones.remove(c.net, &c.via_zone);
                    same_holes.remove(c.net, &c.hole_ring);
                }
                maps[&c.class].view_into(c.net, grid_buf);
                let w = weight_of(c.net);
                penalties_into(zones.seen_by(&c.class), &history, present, c.net, w, &mut grid_buf.penalty);
                if options.via_zones {
                    via_penalties_into(via_zones.seen_by(&c.class), &same_holes, &[], &via_history, present * via_scale, c.net, w, &mut grid_buf.via_penalty);
                }
                let (sources, targets) = &ends[k];
                c.path = solver.route(grid_buf, &cells_of(sources), &cells_of(targets), options.via_cost);
                update_claims(k, c, &mut stamp_buf);
                zones.add(c.net, &c.zone);
                if options.via_zones {
                    via_zones.add(c.net, &c.via_zone);
                    same_holes.add(c.net, &c.hole_ring);
                }
                if report_trials && shown.elapsed() > TRIAL_INTERVAL {
                    trials(round, &trial_list(&connections, sample));
                    shown = crate::clock::Instant::now();
                }
            }
        }
        let mut colliding = 0;
        for (k, c) in connections.iter_mut().enumerate() {
            let Some(path) = &c.path else { continue };
            let mut collides = false;
            for &(layer, i, j) in path.iter().chain(&c.legs) {
                let n = layer * cells + j * nx + i;
                if !own_ends[k].contains(&n) && zones.seen_by(&c.class).others(c.net, n) > 0 {
                    history[n] += options.history_step;
                    collides = true;
                }
            }
            if options.via_zones {
                for &cell in &c.vias {
                    if via_zones.seen_by(&c.class).others(c.net, cell) > 0 || same_holes.count(c.net, cell) > 0 {
                        via_history[cell] += via_hist_step;
                        collides = true;
                    }
                }
            }
            colliding += usize::from(collides);
            c.colliding = collides;
        }
        if report_trials {
            let list = trial_list(&connections, sample);
            trials(round, &list);
            if let Some(cb) = on_round.as_deref_mut() {
                let mut overlap = vec![0u16; layers * cells];
                for z in zones.by_viewer.values() {
                    for (o, &c) in overlap.iter_mut().zip(&z.count) {
                        *o = (*o).max(c);
                    }
                }
                cb(RoundView { round, colliding, present, grid: sample, overlap, history: &history, trials: &list });
            }
            tell(&options.log, "negotiating", || format!("round {}: {colliding} of {} routes still collide", round + 1, connections.len()), None);
        }
        shown = crate::clock::Instant::now();
        summary.conflicted = colliding;
        if colliding == 0 {
            break;
        }
        if colliding < best_colliding {
            best_colliding = colliding;
            stale = 0;
        } else {
            stale += 1;
            if stale >= patience {
                break;
            }
        }
        present *= options.present_growth;
    }

    summary.unroutable = connections.iter().filter(|c| c.path.is_none()).count();

    // Commit clean routes first, then colliding ones in minimum-degree conflict order so one
    // sprawling route does not displace three short ones. Legacy mode keeps raw index order.
    let order = if options.via_zones || options.repasses > 0 || options.rip_blocked {
        commit_order(&connections, &own_ends, nx, cells, &weight_of)
    } else {
        (0..connections.len()).filter(|&k| connections[k].path.is_some()).collect()
    };

    let mut committed = Claims::new(classes.iter().cloned(), layers * cells);
    let mut committed_vias = Claims::new(classes.iter().cloned(), cells);
    let mut committed_holes = SameNetHoles::default();
    for k in order {
        let c = &connections[k];
        let Some(path) = &c.path else { continue };
        let trace_hit = path
            .iter()
            .chain(&c.legs)
            .map(|&(l, i, j)| l * cells + j * nx + i)
            .any(|n| !own_ends[k].contains(&n) && committed.seen_by(&c.class).others(c.net, n) > 0);
        if trace_hit {
            continue;
        }
        if options.via_zones {
            let via_hit = c
                .vias
                .iter()
                .any(|&cell| committed_vias.seen_by(&c.class).others(c.net, cell) > 0 || committed_holes.count(c.net, cell) > 0);
            if via_hit {
                continue;
            }
        }
        maps[&c.class].view_into(c.net, grid_buf);
        let (sources, targets) = &ends[k];
        let Some(items) = lay(board, grid_buf, path, sources, targets) else { continue };
        committed.add(c.net, &c.zone);
        if options.via_zones {
            committed_vias.add(c.net, &c.via_zone);
            committed_holes.add(c.net, &c.hole_ring);
        }
        if stamp_laid {
            for map in maps.values_mut() {
                for &id in &items {
                    map.stamp(board, id);
                }
            }
            laid_routes.push(LaidRoute {
                net: c.net,
                items: items.clone(),
                zone: c.zone.clone(),
                via_zone: c.via_zone.clone(),
                hole_ring: c.hole_ring.clone(),
            });
        }
        summary.committed += 1;
        tell(&options.log, "committing", || format!("{}: laid its negotiated route", board.net_name(c.net)), None);
        summary.vias += items.iter().filter(|id| board.item(**id).is_some_and(|it| it.kind == Kind::Via)).count();
        summary.laid.extend(items);
        watch(board);
    }
    summary
}

/// Orders routable candidates so clean routes commit before colliding ones, and colliding ones
/// commit fewest-conflicts-first while respecting `weight_of`.
fn commit_order(
    connections: &[Connection],
    own_ends: &[BTreeSet<usize>],
    nx: usize,
    cells: usize,
    weight_of: &dyn Fn(u32) -> f64,
) -> Vec<usize> {
    let mut trace_at: HashMap<u32, Vec<usize>> = HashMap::new();
    let mut via_at: HashMap<u32, Vec<usize>> = HashMap::new();
    for (k, c) in connections.iter().enumerate() {
        let Some(path) = &c.path else { continue };
        for &(l, i, j) in path.iter().chain(&c.legs) {
            let n = l * cells + j * nx + i;
            if !own_ends[k].contains(&n) {
                trace_at.entry(n as u32).or_default().push(k);
            }
        }
        for &cell in &c.vias {
            via_at.entry(cell as u32).or_default().push(k);
        }
    }
    let mut conflicts: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); connections.len()];
    for (k, c) in connections.iter().enumerate() {
        if c.path.is_none() {
            continue;
        }
        for (viewer, zcells) in &c.zone {
            for &n in zcells {
                if let Some(others) = trace_at.get(&n) {
                    for &m in others {
                        if m != k && connections[m].net != c.net && &connections[m].class == viewer {
                            conflicts[k].insert(m);
                            conflicts[m].insert(k);
                        }
                    }
                }
            }
        }
        for (viewer, vcells) in &c.via_zone {
            for &cell in vcells {
                if let Some(others) = via_at.get(&cell) {
                    for &m in others {
                        if m != k && connections[m].net != c.net && &connections[m].class == viewer {
                            conflicts[k].insert(m);
                            conflicts[m].insert(k);
                        }
                    }
                }
            }
        }
        for &cell in &c.hole_ring {
            if let Some(others) = via_at.get(&cell) {
                for &m in others {
                    if m != k && connections[m].net == c.net {
                        conflicts[k].insert(m);
                        conflicts[m].insert(k);
                    }
                }
            }
        }
    }

    let mut remaining: Vec<usize> = (0..connections.len()).filter(|&k| connections[k].path.is_some()).collect();
    let mut active = vec![false; connections.len()];
    for &k in &remaining {
        active[k] = true;
    }
    let mut out = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let best_pos = (0..remaining.len())
            .min_by(|&ia, &ib| {
                let (a, b) = (remaining[ia], remaining[ib]);
                let (ca, cb) = (&connections[a], &connections[b]);
                let deg_a = conflicts[a].iter().filter(|&&m| active[m]).count();
                let deg_b = conflicts[b].iter().filter(|&&m| active[m]).count();
                ca.colliding
                    .cmp(&cb.colliding)
                    .then_with(|| weight_of(ca.net).total_cmp(&weight_of(cb.net)))
                    .then_with(|| deg_a.cmp(&deg_b))
                    .then_with(|| ca.path.as_ref().map_or(0, Vec::len).cmp(&cb.path.as_ref().map_or(0, Vec::len)))
                    .then_with(|| a.cmp(&b))
            })
            .expect("non-empty");
        let chosen = remaining.swap_remove(best_pos);
        active[chosen] = false;
        out.push(chosen);
    }
    out
}

fn board_conn(board: &Board, pours_block: bool) -> Connectivity {
    if pours_block { connectivity(board) } else { connectivity_without_pours(board) }
}

fn filtered_airlines(conn: &Connectivity, nets: &Option<HashSet<u32>>) -> Vec<Airline> {
    let mut open = conn.airlines.clone();
    if let Some(only) = nets {
        open.retain(|a| only.contains(&a.net));
    }
    open
}

/// Open airlines on `board` that still have a free route on `maps`, ensuring any newly appearing
/// net class (unlikely, since classes come from nets) has its map and discs ready.
#[allow(clippy::too_many_arguments)]
fn routable_open_airlines(
    board: &mut Board,
    options: &NegotiationOptions,
    maps: &mut HashMap<String, ObstacleMap>,
    discs: &HashMap<(String, String), AllDiscs>,
    hole_discs: &mut HashMap<String, Vec<(i32, i32)>>,
    classes: &mut Vec<String>,
    grid_buf: &mut RouteGrid,
) -> Vec<Airline> {
    let conn = board_conn(board, options.pours_block);
    let mut open = filtered_airlines(&conn, &options.nets);
    for a in &open {
        let p = Profile::for_net(board, a.net);
        if !maps.contains_key(&p.class) && discs.keys().any(|(k, _)| k == &p.class) {
            hole_discs.entry(p.class.clone()).or_insert_with(|| same_hole_disc(board, &p, options.cell));
            maps.insert(p.class.clone(), ObstacleMap::new(board, p.clone(), options.cell, options.pours_block));
            if !classes.contains(&p.class) {
                classes.push(p.class);
                classes.sort();
            }
        }
    }
    if options.reachable_tree {
        open = reachable_airlines(board, &conn, open, maps, grid_buf);
    }
    let mut cur_net = u32::MAX;
    let mut cur_class = String::new();
    open.into_iter()
        .filter(|a| {
            let class = Profile::for_net(board, a.net).class;
            let Some(map) = maps.get(&class) else { return false };
            if a.net != cur_net || class != cur_class {
                map.view_into(a.net, grid_buf);
                cur_net = a.net;
                cur_class = class;
            }
            let sc: Vec<usize> = terminals(board, grid_buf, a.from_item).into_iter().map(|t| t.0).collect();
            let tc: Vec<usize> = terminals(board, grid_buf, a.to_item).into_iter().map(|t| t.0).collect();
            !sc.is_empty() && !tc.is_empty() && route_astar(grid_buf, &sc, &tc, options.via_cost).is_some()
        })
        .collect()
}

/// For any connection that was routable before negotiation laid copper and is now walled off by
/// routes laid in this call, finds the fewest laid routes blocking it, rips them, and negotiates
/// them together with the blocked net favoured. Keeps the change only when strictly fewer
/// connections remain open.
#[allow(clippy::too_many_arguments)]
fn rescue_walled_off(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &NegotiationOptions,
    base_maps: &HashMap<String, ObstacleMap>,
    maps: &mut HashMap<String, ObstacleMap>,
    classes: &mut Vec<String>,
    discs: &HashMap<(String, String), AllDiscs>,
    hole_discs: &mut HashMap<String, Vec<(i32, i32)>>,
    sample: &RouteGrid,
    grid_buf: &mut RouteGrid,
    out_of_time: &dyn Fn() -> bool,
    watch: &mut dyn FnMut(&Board),
    laid_routes: &mut Vec<LaidRoute>,
) {
    let cells = sample.nx * sample.ny;
    let nodes = sample.layers.len() * cells;
    let mut tried_nets: HashSet<u32> = HashSet::new();

    for _attempt in 0..6 {
        if out_of_time() {
            break;
        }
        let conn = board_conn(board, options.pours_block);
        let mut open = filtered_airlines(&conn, &options.nets);
        if options.reachable_tree {
            open = reachable_airlines(board, &conn, open, maps, grid_buf);
        }
        let open_before = open.len();

        // Find an open airline that is walled off on `maps` now, yet routable on `base_maps`.
        let mut target: Option<Airline> = None;
        for a in open {
            if tried_nets.contains(&a.net) {
                continue;
            }
            let class = Profile::for_net(board, a.net).class;
            maps[&class].view_into(a.net, grid_buf);
            let sc_now: Vec<usize> = terminals(board, grid_buf, a.from_item).into_iter().map(|t| t.0).collect();
            let tc_now: Vec<usize> = terminals(board, grid_buf, a.to_item).into_iter().map(|t| t.0).collect();
            if !sc_now.is_empty() && !tc_now.is_empty() && route_astar(grid_buf, &sc_now, &tc_now, options.via_cost).is_some() {
                continue;
            }
            base_maps[&class].view_into(a.net, grid_buf);
            let sc_base: Vec<usize> = terminals(board, grid_buf, a.from_item).into_iter().map(|t| t.0).collect();
            let tc_base: Vec<usize> = terminals(board, grid_buf, a.to_item).into_iter().map(|t| t.0).collect();
            if !sc_base.is_empty() && !tc_base.is_empty() && route_astar(grid_buf, &sc_base, &tc_base, options.via_cost).is_some() {
                target = Some(a);
                break;
            }
        }
        let Some(blocked) = target else { break };
        tried_nets.insert(blocked.net);

        // Price every currently laid route's zone on `base_maps` so `blocked` chooses a path
        // crossing as few laid routes as possible.
        let class = Profile::for_net(board, blocked.net).class;
        let mut soft_zones = Claims::new(classes.iter().cloned(), nodes);
        let mut soft_vias = Claims::new(classes.iter().cloned(), cells);
        let mut soft_holes = SameNetHoles::default();
        let mut owner_by_item: HashMap<ItemId, usize> = HashMap::new();
        for (idx, r) in laid_routes.iter().enumerate() {
            soft_zones.add(r.net, &r.zone);
            soft_vias.add(r.net, &r.via_zone);
            soft_holes.add(r.net, &r.hole_ring);
            for &id in &r.items {
                owner_by_item.insert(id, idx);
            }
        }
        base_maps[&class].view_into(blocked.net, grid_buf);
        let zero_hist = vec![0u32; nodes];
        let zero_vhist = vec![0u32; cells];
        penalties_into(soft_zones.seen_by(&class), &zero_hist, 120.0, blocked.net, 1.0, &mut grid_buf.penalty);
        if options.via_zones {
            via_penalties_into(soft_vias.seen_by(&class), &soft_holes, &[], &zero_vhist, 480.0, blocked.net, 1.0, &mut grid_buf.via_penalty);
        }
        let src_terms = terminals(board, grid_buf, blocked.from_item);
        let tgt_terms = terminals(board, grid_buf, blocked.to_item);
        let sc: Vec<usize> = src_terms.iter().map(|t| t.0).collect();
        let tc: Vec<usize> = tgt_terms.iter().map(|t| t.0).collect();
        let Some(trial_path) = solver.route(grid_buf, &sc, &tc, options.via_cost) else { continue };
        let Some(trial_pieces) = pieces(grid_buf, &trial_path, &src_terms, &tgt_terms) else { continue };

        let mut victims: BTreeSet<usize> = BTreeSet::new();
        let mut hit_hard = false;
        for piece in trial_pieces {
            let temp = board.insert(piece);
            let vs = grid_buf.violations(board, temp);
            board.discard(temp);
            for v in vs {
                match v.b.and_then(|bid| owner_by_item.get(&bid).copied()) {
                    Some(r_idx) => {
                        victims.insert(r_idx);
                    }
                    None => {
                        hit_hard = true;
                    }
                }
            }
        }
        if hit_hard || victims.is_empty() {
            continue;
        }
        // Rip whole victim nets so two sibling routes sharing a via are never split apart and
        // multi-pin victim nets can re-span around the rescued route.
        let victim_nets: HashSet<u32> = victims.iter().map(|&i| laid_routes[i].net).collect();
        for (idx, r) in laid_routes.iter().enumerate() {
            if victim_nets.contains(&r.net) {
                victims.insert(idx);
            }
        }
        if victims.len() > 8 {
            continue;
        }

        let snap_board = board.clone();
        let snap_maps = maps.clone();
        let snap_laid = laid_routes.clone();

        let mut kept_routes = Vec::with_capacity(laid_routes.len());
        for (idx, r) in laid_routes.drain(..).enumerate() {
            if victims.contains(&idx) {
                for &id in r.items.iter().rev() {
                    board.discard(id);
                }
            } else {
                kept_routes.push(r);
            }
        }
        *laid_routes = kept_routes;

        // Rebuild maps from `base_maps` plus the kept routes, faster than restamping every pad.
        *maps = base_maps.clone();
        for r in laid_routes.iter() {
            for map in maps.values_mut() {
                for &id in &r.items {
                    map.stamp(board, id);
                }
            }
        }

        let mut weights = options.net_weight.clone();
        let base_w = weights.get(&blocked.net).copied().unwrap_or(1.0);
        weights.insert(blocked.net, base_w * 0.5);

        for sub_pass in 0..2 {
            if out_of_time() {
                break;
            }
            let sub_open = routable_open_airlines(board, options, maps, discs, hole_discs, classes, grid_buf);
            if sub_open.is_empty() {
                break;
            }
            let mut sub_opts = options.clone();
            sub_opts.seed = options.seed.wrapping_add((sub_pass as u64 + 17).wrapping_mul(0x517c_c1b7));
            let res = run_pass(
                board,
                solver,
                &sub_opts,
                maps,
                classes,
                discs,
                hole_discs,
                sample,
                grid_buf,
                sub_open,
                options.max_rounds.min(16),
                &weights,
                true,
                false,
                out_of_time,
                &mut |_| {},
                &mut |_, _| {},
                &mut None,
                laid_routes,
            );
            if res.committed == 0 {
                break;
            }
        }

        let open_after = filtered_airlines(&board_conn(board, options.pours_block), &options.nets).len();
        if open_after < open_before {
            watch(board);
        } else {
            *board = snap_board;
            *maps = snap_maps;
            *laid_routes = snap_laid;
        }
    }
}

/// Replaces any multi-cluster net's Euclidean spanning tree when an intermediate pin sits in a
/// disconnected grid pocket: reachable cluster pairs are spanned first in shortest-first order,
/// then pocketed clusters bridge in to complete the tree with the same airline count.
fn reachable_airlines(
    board: &mut Board,
    conn: &Connectivity,
    mut open: Vec<Airline>,
    maps: &HashMap<String, ObstacleMap>,
    grid_buf: &mut RouteGrid,
) -> Vec<Airline> {
    let mut by_net: HashMap<u32, Vec<usize>> = HashMap::new();
    for (idx, a) in open.iter().enumerate() {
        by_net.entry(a.net).or_default().push(idx);
    }
    let mut multi_nets: Vec<u32> = by_net.iter().filter(|(_, idxs)| idxs.len() >= 2).map(|(&n, _)| n).collect();
    multi_nets.sort_unstable();
    if multi_nets.is_empty() {
        return open;
    }

    // Gather non-pour copper anchors per cluster for only those multi-airline nets.
    let multi_set: HashSet<u32> = multi_nets.iter().copied().collect();
    let mut net_clusters: HashMap<u32, std::collections::BTreeMap<usize, Vec<(Point, ItemId)>>> = HashMap::new();
    for (id, item) in board.items() {
        if !item.is_copper() || matches!(item.kind, Kind::Pour { .. }) || !multi_set.contains(&item.net) {
            continue;
        }
        let Some(&cid) = conn.cluster.get(&id) else { continue };
        let (pts, len) = item_anchors(item);
        if len > 0 {
            net_clusters.entry(item.net).or_default().entry(cid).or_default().extend(pts[..len].iter().map(|&p| (p, id)));
        }
    }

    for net in multi_nets {
        let idxs = &by_net[&net];
        let class = Profile::for_net(board, net).class;
        let Some(map) = maps.get(&class) else { continue };
        map.view_into(net, grid_buf);

        let mut term_cache: HashMap<ItemId, Vec<usize>> = HashMap::new();
        let mut terms_of = |board: &mut Board, id: ItemId| -> Vec<usize> {
            if let Some(v) = term_cache.get(&id) {
                return v.clone();
            }
            let v: Vec<usize> = terminals(board, grid_buf, id).into_iter().map(|t| t.0).collect();
            term_cache.insert(id, v.clone());
            v
        };

        let all_routable = idxs.iter().all(|&i| {
            let a = &open[i];
            let sc = terms_of(board, a.from_item);
            let tc = terms_of(board, a.to_item);
            !sc.is_empty() && !tc.is_empty() && route_astar(grid_buf, &sc, &tc, 200).is_some()
        });
        if all_routable {
            continue;
        }

        let Some(cmap) = net_clusters.get(&net) else { continue };
        let clusters: Vec<&Vec<(Point, ItemId)>> = cmap.values().filter(|v| !v.is_empty()).collect();
        let m = clusters.len();
        if m != idxs.len() + 1 || m > 48 {
            continue;
        }

        let closest = |ca: &[(Point, ItemId)], cb: &[(Point, ItemId)]| {
            let mut best = (f64::INFINITY, ca[0], cb[0]);
            for &pa in ca {
                for &pb in cb {
                    let d = pa.0.distance(pb.0);
                    if d < best.0 {
                        best = (d, pa, pb);
                    }
                }
            }
            best
        };
        let mut edges: Vec<(f64, usize, usize, (Point, ItemId), (Point, ItemId))> = Vec::with_capacity(m * (m - 1) / 2);
        for u in 0..m {
            for v in (u + 1)..m {
                let (d, pa, pb) = closest(clusters[u], clusters[v]);
                edges.push((d, u, v, pa, pb));
            }
        }
        edges.sort_by(|a, b| a.0.total_cmp(&b.0));

        let mut parent: Vec<usize> = (0..m).collect();
        let find = |parent: &mut [usize], mut x: usize| {
            while parent[x] != x {
                parent[x] = parent[parent[x]];
                x = parent[x];
            }
            x
        };

        let mut chosen: Vec<Airline> = Vec::with_capacity(m - 1);
        for &(_, u, v, pa, pb) in &edges {
            let (ru, rv) = (find(&mut parent, u), find(&mut parent, v));
            if ru == rv {
                continue;
            }
            let sc = terms_of(board, pa.1);
            let tc = terms_of(board, pb.1);
            if !sc.is_empty() && !tc.is_empty() && route_astar(grid_buf, &sc, &tc, 200).is_some() {
                parent[ru] = rv;
                chosen.push(Airline { net, from: pa.0, to: pb.0, from_item: pa.1, to_item: pb.1 });
                if chosen.len() == m - 1 {
                    break;
                }
            }
        }
        if chosen.len() < m - 1 {
            for &(_, u, v, pa, pb) in &edges {
                let (ru, rv) = (find(&mut parent, u), find(&mut parent, v));
                if ru != rv {
                    parent[ru] = rv;
                    chosen.push(Airline { net, from: pa.0, to: pb.0, from_item: pa.1, to_item: pb.1 });
                    if chosen.len() == m - 1 {
                        break;
                    }
                }
            }
        }
        if chosen.len() == idxs.len() {
            for (&slot, replacement) in idxs.iter().zip(chosen) {
                open[slot] = replacement;
            }
        }
    }
    open
}

fn item_anchors(item: &board::Item) -> ([Point; 2], usize) {
    let z = Point::new(0, 0);
    match (&item.kind, &item.shape) {
        (_, Shape::Capsule { a, b, .. }) if a == b => ([*a, z], 1),
        (Kind::Track, Shape::Capsule { a, b, .. }) => ([*a, *b], 2),
        (Kind::Pad { .. }, Shape::Polygon(p)) => {
            let n = p.len().max(1) as i64;
            ([Point::new(p.iter().map(|q| q.x).sum::<i64>() / n, p.iter().map(|q| q.y).sum::<i64>() / n), z], 1)
        }
        _ => ([z, z], 0),
    }
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

/// Sibling via-drill annulus counts for `net`, leaving out this connection's sorted `own_hole_ring`
/// when `same_holes` still holds the full Jacobi snapshot.
fn sibling_hole_ring(same_holes: &SameNetHoles, net: u32, own_hole_ring: &[u32]) -> Vec<(u32, u16)> {
    let Some(holes) = same_holes.per_net.get(&net) else { return Vec::new() };
    holes
        .iter()
        .filter_map(|(&c, &n)| {
            let sib = n.saturating_sub(u16::from(own_hole_ring.binary_search(&c).is_ok()));
            (sib > 0).then_some((c, sib))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn via_penalties_into(
    via_zones: &Zones,
    same_holes: &SameNetHoles,
    own_hole_ring: &[u32],
    via_history: &[u32],
    via_present: f64,
    net: u32,
    weight: f64,
    out: &mut Vec<u32>,
) {
    let own = via_zones.per_net.get(&net).into_iter().flat_map(|m| m.keys().copied());
    let holes = sibling_hole_ring(same_holes, net, own_hole_ring);
    via_price_into(via_history, &via_zones.count, own, &holes, via_present, weight, out);
}

/// Each 2D cell's via price from a round's `via_history`, `count` of nets claiming the cell
/// against a via of this class, `own` cells claimed by this net itself, and `hole_ring`
/// `(cell, count)` pairs from sibling routes' via-drill annuli on the same net.
pub fn via_price_into(
    via_history: &[u32],
    count: &[u16],
    own: impl Iterator<Item = u32>,
    hole_ring: &[(u32, u16)],
    via_present: f64,
    weight: f64,
    out: &mut Vec<u32>,
) {
    price_into(via_history, count, own, via_present, weight, out);
    for &(cell, cnt) in hole_ring {
        let c = cell as usize;
        let extra = ((f64::from(via_history[c]) + via_present * f64::from(cnt)) * weight).round().max(0.0) as u32;
        out[c] = out[c].saturating_add(extra);
    }
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
    via_zones: &Claims,
    same_holes: &SameNetHoles,
    history: &[u32],
    via_history: &[u32],
    present: f64,
    via_scale: f64,
    weights: &HashMap<u32, f64>,
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
    let prices = RoundPrices {
        generation,
        history,
        counts: classes.iter().map(|c| zones.seen_by(c).count.as_slice()).collect(),
        present,
        via_history: if options.via_zones { via_history } else { &[] },
        via_counts: if options.via_zones { classes.iter().map(|c| via_zones.seen_by(c).count.as_slice()).collect() } else { Vec::new() },
        via_present: present * via_scale,
    };
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
                    weight: weights.get(&c.net).copied().unwrap_or(1.0),
                    own: zones.seen_by(&c.class).per_net.get(&c.net).map(|m| m.keys().copied().collect()).unwrap_or_default(),
                    own_vias: if options.via_zones {
                        via_zones.seen_by(&c.class).per_net.get(&c.net).map(|m| m.keys().copied().collect()).unwrap_or_default()
                    } else {
                        Vec::new()
                    },
                    hole_ring: if options.via_zones { sibling_hole_ring(same_holes, c.net, &c.hole_ring) } else { Vec::new() },
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
            let w = weights.get(&c.net).copied().unwrap_or(1.0);
            penalties_into(zones.seen_by(&c.class), history, present, c.net, w, &mut grid.penalty);
            if options.via_zones {
                via_penalties_into(via_zones.seen_by(&c.class), same_holes, &c.hole_ring, via_history, present * via_scale, c.net, w, &mut grid.via_penalty);
            }
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
    via_zones: &Claims,
    same_holes: &SameNetHoles,
    history: &[u32],
    via_history: &[u32],
    present: f64,
    via_scale: f64,
    weights: &HashMap<u32, f64>,
    options: &NegotiationOptions,
    round: usize,
    sample: &RouteGrid,
    report_trials: bool,
    trials: &mut dyn FnMut(usize, &[Trial]),
) {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let jobs: Vec<(u32, &str, f64, &[u32])> = connections
        .iter()
        .map(|c| (c.net, c.class.as_str(), weights.get(&c.net).copied().unwrap_or(1.0), c.hole_ring.as_slice()))
        .collect();
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
                    let (net, class, weight, own_hole_ring) = jobs[k];
                    let grid = match &mut grid {
                        Some(g) => {
                            maps[class].view_into(net, g);
                            g
                        }
                        None => grid.insert(maps[class].view(net)),
                    };
                    penalties_into(zones.seen_by(class), history, present, net, weight, &mut grid.penalty);
                    if options.via_zones {
                        via_penalties_into(via_zones.seen_by(class), same_holes, own_hole_ring, via_history, present * via_scale, net, weight, &mut grid.via_penalty);
                    }
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
            if report_trials {
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
        }
    });
    for (c, path) in connections.iter_mut().zip(paths) {
        if let Some(path) = path.into_inner().expect("no worker panics") {
            c.path = path;
        }
    }
}

/// A disc of cell offsets together with its incremental 1-step rims for each of the 8 grid
/// neighbour directions, so walking along a path stamps only newly entered cells.
struct FastDisc {
    full: Vec<(i32, i32)>,
    rims: [Vec<(i32, i32)>; 8],
}

impl FastDisc {
    fn of(full: Vec<(i32, i32)>) -> FastDisc {
        let set: HashSet<(i32, i32)> = full.iter().copied().collect();
        let dirs: [(i32, i32); 8] = [(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)];
        let rims = std::array::from_fn(|k| {
            let (di, dj) = dirs[k];
            full.iter().copied().filter(|&(ox, oy)| !set.contains(&(ox + di, oy + dj))).collect()
        });
        FastDisc { full, rims }
    }
}

fn step_dir(di: i32, dj: i32) -> Option<usize> {
    match (di, dj) {
        (1, 0) => Some(0),
        (-1, 0) => Some(1),
        (0, 1) => Some(2),
        (0, -1) => Some(3),
        (1, 1) => Some(4),
        (1, -1) => Some(5),
        (-1, 1) => Some(6),
        (-1, -1) => Some(7),
        _ => None,
    }
}

/// Reusable epoch marker for O(1) deduplication of stamped node and cell indices.
struct StampBuf {
    seen: Vec<u32>,
    epoch: u32,
}

impl StampBuf {
    fn new(cap: usize) -> StampBuf {
        StampBuf { seen: vec![0; cap], epoch: 0 }
    }

    fn next_epoch(&mut self, needed: usize) -> u32 {
        if self.seen.len() < needed {
            self.seen.resize(needed, 0);
        }
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.seen.fill(0);
            self.epoch = 1;
        }
        self.epoch
    }
}

/// Cell offsets claimed around an owner's trace and via against a viewer's trace and via.
struct AllDiscs {
    trace_to_trace: FastDisc,
    via_to_trace: Vec<(i32, i32)>,
    trace_to_via: FastDisc,
    via_to_via: Vec<(i32, i32)>,
}

/// Cell offsets within the zone radius of an owner's trace cell and via cell, as a viewer class
/// sees them. In exact mode, two non-crossing grid segments of length at most `sqrt(2) * h` whose
/// endpoints keep at least `sqrt(d^2 + 0.5 * h^2)` apart never come closer than `d` anywhere
/// along their runs, avoiding the extra `~2.4 * h` slack that otherwise chokes fine-pitch lanes
/// while adding full via-viewer discs for copper, hole-to-copper, and hole-to-hole rules.
fn all_zone_discs(board: &Board, owner: &Profile, viewer: &Profile, h: Nm, exact: bool) -> AllDiscs {
    let clearance = owner.clearance.max(viewer.clearance).max(nm(board.rules.min_clearance)) as f64;
    let hf = h as f64;
    if !exact {
        let (half_width, via, widest) = (owner.half_width as f64, owner.via_radius as f64, viewer.half_width as f64);
        let trace_reach = half_width + widest + clearance + 2.0 * STEP_REACH * hf;
        let via_reach = via + widest + clearance + 2.0 * STEP_REACH * hf;
        let disc = |reach: f64| -> Vec<(i32, i32)> {
            let r = (reach / hf).ceil() as i32;
            (-r..=r)
                .flat_map(|dj| (-r..=r).map(move |di| (di, dj)))
                .filter(|&(di, dj)| f64::from(di).hypot(f64::from(dj)) * hf < reach + hf)
                .collect()
        };
        return AllDiscs {
            trace_to_trace: FastDisc::of(disc(trace_reach)),
            via_to_trace: disc(via_reach),
            trace_to_via: FastDisc::of(Vec::new()),
            via_to_via: Vec::new(),
        };
    }

    let hole_clr = nm(board.rules.min_hole_clearance) as f64;
    let hole_to_hole = nm(board.rules.min_hole_to_hole) as f64;
    let half_diag = STEP_REACH * hf;
    let seg_reach = |d: f64| (d.hypot(half_diag) + ZONE_MARGIN).max(hf + ZONE_MARGIN);

    let (ohw, ovia, odrill) = (owner.half_width as f64, owner.via_radius as f64, owner.drill_radius as f64);
    let (vhw, vvia, vdrill) = (viewer.half_width as f64, viewer.via_radius as f64, viewer.drill_radius as f64);

    let d_tt = ohw + vhw + clearance;
    let d_vt = (ovia + vhw + clearance).max(odrill + vhw + hole_clr);
    let d_tv = (ohw + vvia + clearance).max(ohw + vdrill + hole_clr);
    let d_vv = (ovia + vvia + clearance)
        .max(odrill + vdrill + hole_to_hole)
        .max(ovia + vdrill + hole_clr)
        .max(odrill + vvia + hole_clr);

    let disc = |reach: f64| -> Vec<(i32, i32)> {
        let r = (reach / hf).ceil() as i32;
        (-r..=r)
            .flat_map(|dj| (-r..=r).map(move |di| (di, dj)))
            .filter(|&(di, dj)| f64::from(di).hypot(f64::from(dj)) * hf < reach)
            .collect()
    };
    AllDiscs {
        trace_to_trace: FastDisc::of(disc(seg_reach(d_tt))),
        via_to_trace: disc(seg_reach(d_vt)),
        trace_to_via: FastDisc::of(disc(seg_reach(d_tv))),
        via_to_via: disc(d_vv + ZONE_MARGIN),
    }
}

/// Non-zero cell offsets where a second via of the same net would break hole-to-hole clearance;
/// `(0, 0)` is left out because two routes of one net at the same cell share one via.
fn same_hole_disc(board: &Board, profile: &Profile, h: Nm) -> Vec<(i32, i32)> {
    let hf = h as f64;
    let reach = 2.0 * profile.drill_radius as f64 + nm(board.rules.min_hole_to_hole) as f64 + ZONE_MARGIN;
    let r = (reach / hf).ceil() as i32;
    (-r..=r)
        .flat_map(|dj| (-r..=r).map(move |di| (di, dj)))
        .filter(|&(di, dj)| (di != 0 || dj != 0) && f64::from(di).hypot(f64::from(dj)) * hf < reach)
        .collect()
}

fn via_cells_of(path: &GridPath, nx: usize) -> Vec<usize> {
    let mut vias: Vec<usize> = path
        .windows(2)
        .filter(|w| w[0].0 != w[1].0)
        .map(|w| w[1].2 * nx + w[1].1)
        .collect();
    vias.sort_unstable();
    vias.dedup();
    vias
}

/// Cells along full-width terminal legs outside the terminals' own copper, sampled densely enough
/// that consecutive samples are grid neighbours.
fn leg_cells(
    grid: &RouteGrid,
    path: &GridPath,
    sources: &[Terminal],
    targets: &[Terminal],
    owner_hw: Nm,
    own: &BTreeSet<usize>,
) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let (Some(&first), Some(&last)) = (path.first(), path.last()) else { return out };
    let cells = grid.nx * grid.ny;
    let idx = |&(l, i, j): &(usize, usize, usize)| l * cells + j * grid.nx + i;
    let find_leg = |terms: &[Terminal], n: usize| {
        if terms.iter().any(|t| t.0 == n && t.1.is_none()) {
            None
        } else {
            terms.iter().find_map(|t| (t.0 == n).then_some(t.1).flatten())
        }
    };
    for (end_cell, leg) in [(first, find_leg(sources, idx(&first))), (last, find_leg(targets, idx(&last)))] {
        let Some((anchor, hw)) = leg else { continue };
        if hw < owner_hw {
            continue;
        }
        let to = grid.centre(end_cell.1, end_cell.2);
        let dist = anchor.distance(to);
        let steps = ((dist / (grid.h as f64 * 0.5)).ceil() as usize).max(1);
        for s in 0..steps {
            let t = s as f64 / steps as f64;
            let p = Point::new(
                anchor.x + ((to.x - anchor.x) as f64 * t).round() as Nm,
                anchor.y + ((to.y - anchor.y) as f64 * t).round() as Nm,
            );
            if let Some((i, j)) = grid.cell_at(p) {
                let n = end_cell.0 * cells + j * grid.nx + i;
                if !own.contains(&n) && (end_cell.0, i, j) != end_cell {
                    out.push((end_cell.0, i, j));
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The `(layer, i, j)` nodes a path claims against a viewer's trace: a trace disc around each
/// trace cell on its layer, and a via disc on every layer where it changes layer. Deduplicated.
#[allow(clippy::too_many_arguments)]
fn zone_of_fast(
    path: &GridPath,
    legs: &[(usize, usize, usize)],
    vias: &[usize],
    nx: usize,
    ny: usize,
    layers: usize,
    trace_disc: &FastDisc,
    via_disc: &[(i32, i32)],
    buf: &mut StampBuf,
) -> Vec<u32> {
    let cells = nx * ny;
    let epoch = buf.next_epoch(layers * cells);
    let mut out = Vec::new();
    let mut stamp = |layer: usize, i: usize, j: usize, disc: &[(i32, i32)]| {
        let base = layer * cells;
        let (ci, cj) = (i as i32, j as i32);
        for &(di, dj) in disc {
            let (a, b) = (ci + di, cj + dj);
            if a >= 0 && b >= 0 && (a as usize) < nx && (b as usize) < ny {
                let idx = base + b as usize * nx + a as usize;
                if buf.seen[idx] != epoch {
                    buf.seen[idx] = epoch;
                    out.push(idx as u32);
                }
            }
        }
    };
    for (k, &(layer, i, j)) in path.iter().enumerate() {
        let slice = if k > 0 && path[k - 1].0 == layer {
            step_dir(i as i32 - path[k - 1].1 as i32, j as i32 - path[k - 1].2 as i32)
                .map_or(trace_disc.full.as_slice(), |d| trace_disc.rims[d].as_slice())
        } else {
            trace_disc.full.as_slice()
        };
        stamp(layer, i, j, slice);
    }
    for &(layer, i, j) in legs {
        stamp(layer, i, j, &trace_disc.full);
    }
    for &cell in vias {
        let (i, j) = (cell % nx, cell / nx);
        for l in 0..layers {
            stamp(l, i, j, via_disc);
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn zone_of(
    path: &GridPath,
    legs: &[(usize, usize, usize)],
    vias: &[usize],
    nx: usize,
    ny: usize,
    layers: usize,
    trace_disc: &[(i32, i32)],
    via_disc: &[(i32, i32)],
) -> Vec<u32> {
    let mut buf = StampBuf::new(layers * nx * ny);
    let fd = FastDisc::of(trace_disc.to_vec());
    let mut out = zone_of_fast(path, legs, vias, nx, ny, layers, &fd, via_disc, &mut buf);
    out.sort_unstable();
    out
}

/// The `(i, j)` cells a path claims against a viewer's via: every trace cell blocks vias across
/// all layers, and every via blocks other vias within copper and hole rules. Deduplicated.
#[allow(clippy::too_many_arguments)]
fn via_zone_of_fast(
    path: &GridPath,
    legs: &[(usize, usize, usize)],
    vias: &[usize],
    nx: usize,
    ny: usize,
    trace_to_via: &FastDisc,
    via_to_via: &[(i32, i32)],
    buf: &mut StampBuf,
) -> Vec<u32> {
    let epoch = buf.next_epoch(nx * ny);
    let mut out = Vec::new();
    let mut stamp = |i: usize, j: usize, disc: &[(i32, i32)]| {
        let (ci, cj) = (i as i32, j as i32);
        for &(di, dj) in disc {
            let (a, b) = (ci + di, cj + dj);
            if a >= 0 && b >= 0 && (a as usize) < nx && (b as usize) < ny {
                let idx = b as usize * nx + a as usize;
                if buf.seen[idx] != epoch {
                    buf.seen[idx] = epoch;
                    out.push(idx as u32);
                }
            }
        }
    };
    for (k, &(_, i, j)) in path.iter().enumerate() {
        if k > 0 && path[k - 1].1 == i && path[k - 1].2 == j {
            continue;
        }
        let slice = if k > 0 {
            step_dir(i as i32 - path[k - 1].1 as i32, j as i32 - path[k - 1].2 as i32)
                .map_or(trace_to_via.full.as_slice(), |d| trace_to_via.rims[d].as_slice())
        } else {
            trace_to_via.full.as_slice()
        };
        stamp(i, j, slice);
    }
    for &(_, i, j) in legs {
        stamp(i, j, &trace_to_via.full);
    }
    for &cell in vias {
        stamp(cell % nx, cell / nx, via_to_via);
    }
    out
}

fn stamp_cells_2d(centres: &[usize], nx: usize, ny: usize, disc: &[(i32, i32)], buf: &mut StampBuf) -> Vec<u32> {
    if centres.is_empty() || disc.is_empty() {
        return Vec::new();
    }
    let epoch = buf.next_epoch(nx * ny);
    let mut out = Vec::new();
    for &cell in centres {
        let (i, j) = ((cell % nx) as i32, (cell / nx) as i32);
        for &(di, dj) in disc {
            let (a, b) = (i + di, j + dj);
            if a >= 0 && b >= 0 && (a as usize) < nx && (b as usize) < ny {
                let idx = b as usize * nx + a as usize;
                if buf.seen[idx] != epoch {
                    buf.seen[idx] = epoch;
                    out.push(idx as u32);
                }
            }
        }
    }
    out.sort_unstable();
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
        let vias = via_cells_of(&path, nx);
        let z = zone_of(&path, &[], &vias, nx, ny, layers, &disc, &disc);
        assert!(z.windows(2).all(|w| w[0] < w[1]));
        assert!(z.iter().all(|&n| (n as usize) < nx * ny * layers));
        // The via at (1, 0) claims its disc on both layers, the corner cell clipped to the grid.
        assert!(z.contains(&0) && z.contains(&((nx * ny) as u32)));
        assert!(z.contains(&((nx * ny + 5 * nx + 7) as u32)));
    }

    fn test_board(layers_spec: &str, nets: &[&str]) -> Board {
        let mut net_defs = String::from("(net 0 \"\")");
        for (i, n) in nets.iter().enumerate() {
            net_defs.push_str(&format!(" (net {} \"{n}\")", i + 1));
        }
        let src = format!(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers {layers_spec}) {net_defs}
             (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1)))"
        );
        let (pcb, copper) = kicad_pcb::parse_board(&src).unwrap();
        Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap())
    }

    fn smd_pad(net: u32, layer: usize, x_mm: f64, y_mm: f64) -> board::Item {
        let p = Point::new((x_mm * 1e6).round() as Nm, (y_mm * 1e6).round() as Nm);
        board::Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net,
            layers: 1u64 << layer,
            shape: Shape::Capsule { a: p, b: p, r: 250_000 },
            hole: None,
            locked: true,
            span: None,
        }
    }

    fn locked_wall(net: u32, layer: usize, a: (f64, f64), b: (f64, f64)) -> board::Item {
        let pt = |(x, y): (f64, f64)| Point::new((x * 1e6).round() as Nm, (y * 1e6).round() as Nm);
        board::Item {
            kind: Kind::Track,
            net,
            layers: 1u64 << layer,
            shape: Shape::Capsule { a: pt(a), b: pt(b), r: 100_000 },
            hole: None,
            locked: true,
            span: None,
        }
    }

    #[test]
    fn fast_disc_rims_match_full_disc_at_every_step() {
        let (nx, ny, layers) = (24, 24, 2);
        let raw: Vec<(i32, i32)> = (-4..=4)
            .flat_map(|dj| (-4..=4).map(move |di| (di, dj)))
            .filter(|&(di, dj)| di * di + dj * dj <= 18)
            .collect();
        let vdisc: Vec<(i32, i32)> = (-5..=5)
            .flat_map(|dj| (-5..=5).map(move |di| (di, dj)))
            .filter(|&(di, dj)| di * di + dj * dj <= 25)
            .collect();
        let fd = FastDisc::of(raw.clone());
        let path: GridPath = vec![
            (0, 8, 8),
            (0, 9, 8),
            (0, 10, 9),
            (0, 10, 10),
            (0, 9, 11),
            (0, 8, 11),
            (1, 8, 11),
            (1, 7, 10),
            (1, 7, 9),
            (1, 8, 8),
        ];
        let legs = vec![(0, 7, 8), (1, 9, 8)];
        let vias = via_cells_of(&path, nx);
        let mut buf = StampBuf::new(layers * nx * ny);

        let mut fast_3d = zone_of_fast(&path, &legs, &vias, nx, ny, layers, &fd, &vdisc, &mut buf);
        fast_3d.sort_unstable();
        let mut slow_3d = BTreeSet::new();
        let cells = nx * ny;
        for &(l, i, j) in path.iter().chain(&legs) {
            for &(di, dj) in &raw {
                let (a, b) = (i as i32 + di, j as i32 + dj);
                if a >= 0 && b >= 0 && (a as usize) < nx && (b as usize) < ny {
                    slow_3d.insert((l * cells + b as usize * nx + a as usize) as u32);
                }
            }
        }
        for &c in &vias {
            let (i, j) = ((c % nx) as i32, (c / nx) as i32);
            for l in 0..layers {
                for &(di, dj) in &vdisc {
                    let (a, b) = (i + di, j + dj);
                    if a >= 0 && b >= 0 && (a as usize) < nx && (b as usize) < ny {
                        slow_3d.insert((l * cells + b as usize * nx + a as usize) as u32);
                    }
                }
            }
        }
        assert_eq!(fast_3d, slow_3d.into_iter().collect::<Vec<_>>());

        let mut fast_2d = via_zone_of_fast(&path, &legs, &vias, nx, ny, &fd, &vdisc, &mut buf);
        fast_2d.sort_unstable();
        let mut slow_2d = BTreeSet::new();
        for &(_, i, j) in path.iter().chain(&legs) {
            for &(di, dj) in &raw {
                let (a, b) = (i as i32 + di, j as i32 + dj);
                if a >= 0 && b >= 0 && (a as usize) < nx && (b as usize) < ny {
                    slow_2d.insert((b as usize * nx + a as usize) as u32);
                }
            }
        }
        for &c in &vias {
            let (i, j) = ((c % nx) as i32, (c / nx) as i32);
            for &(di, dj) in &vdisc {
                let (a, b) = (i + di, j + dj);
                if a >= 0 && b >= 0 && (a as usize) < nx && (b as usize) < ny {
                    slow_2d.insert((b as usize * nx + a as usize) as u32);
                }
            }
        }
        assert_eq!(fast_2d, slow_2d.into_iter().collect::<Vec<_>>());
    }

    #[test]
    fn via_zones_keep_crossing_routes_clear_around_vias() {
        let mut board = test_board("(0 \"F.Cu\" signal) (31 \"B.Cu\" signal)", &["A", "B", "WALL"]);
        board.insert(smd_pad(1, 0, 3.0, 9.6));
        board.insert(smd_pad(1, 0, 13.0, 9.6));
        board.insert(smd_pad(2, 0, 3.0, 10.4));
        board.insert(smd_pad(2, 0, 13.0, 10.4));
        // A top-layer barrier forces both nets through vias in the same corridor.
        board.insert(locked_wall(3, 0, (8.0, 3.0), (8.0, 17.0)));

        let stats = route_negotiated(
            &mut board,
            &mut crate::route::CpuSolver,
            &NegotiationOptions { cell: 50_000, via_zones: true, repasses: 0, rip_blocked: false, ..Default::default() },
            &mut |_| {},
            &mut |_, _| {},
        );
        assert_eq!(stats.committed, 2, "{stats:?}");
        assert!(connectivity_without_pours(&board).airlines.is_empty());
        assert!(board::check(&board).is_empty());
    }

    #[test]
    fn same_net_routes_share_a_via_or_keep_hole_to_hole_clearance() {
        let mut board = test_board("(0 \"F.Cu\" signal) (31 \"B.Cu\" signal)", &["A"]);
        board.insert(smd_pad(1, 0, 4.0, 10.0));
        board.insert(smd_pad(1, 1, 8.0, 10.0));
        board.insert(smd_pad(1, 0, 12.0, 10.0));

        let stats = route_negotiated(
            &mut board,
            &mut crate::route::CpuSolver,
            &NegotiationOptions { cell: 50_000, via_zones: true, repasses: 0, rip_blocked: false, ..Default::default() },
            &mut |_| {},
            &mut |_, _| {},
        );
        assert_eq!(stats.committed, 2, "{stats:?}");
        assert!(connectivity_without_pours(&board).airlines.is_empty());
        assert!(board::check(&board).is_empty());
    }

    #[test]
    fn reachable_tree_spans_open_pins_past_a_pocketed_middle_pin() {
        let mut board = test_board("(0 \"F.Cu\" signal)", &["SIG", "WALL"]);
        board.insert(smd_pad(1, 0, 3.0, 10.0));
        board.insert(smd_pad(1, 0, 8.0, 10.0));
        board.insert(smd_pad(1, 0, 13.0, 10.0));
        for (a, b) in [((7.0, 9.0), (9.0, 9.0)), ((9.0, 9.0), (9.0, 11.0)), ((9.0, 11.0), (7.0, 11.0)), ((7.0, 11.0), (7.0, 9.0))] {
            board.insert(locked_wall(2, 0, a, b));
        }

        let mut blind = board.clone();
        let blind_stats = route_negotiated(
            &mut blind,
            &mut crate::route::CpuSolver,
            &NegotiationOptions { cell: 50_000, reachable_tree: false, repasses: 0, rip_blocked: false, ..Default::default() },
            &mut |_| {},
            &mut |_, _| {},
        );
        assert_eq!(blind_stats.committed, 0, "blind Euclidean tree routes both edges into the boxed middle pad");
        assert_eq!(connectivity_without_pours(&blind).airlines.len(), 2);

        let stats = route_negotiated(
            &mut board,
            &mut crate::route::CpuSolver,
            &NegotiationOptions { cell: 50_000, reachable_tree: true, repasses: 0, rip_blocked: false, ..Default::default() },
            &mut |_| {},
            &mut |_, _| {},
        );
        assert_eq!(stats.committed, 1, "reachable spanning connects the two open outer pads");
        assert_eq!(connectivity_without_pours(&board).airlines.len(), 1);
        assert!(board::check(&board).is_empty());
    }

    #[test]
    fn repasses_and_rip_blocked_finish_routes_left_by_one_pass() {
        let mut board = test_board("(0 \"F.Cu\" signal)", &["A", "B", "C"]);
        // Three crossing pairs on a single layer collide in round 1; follow-up passes detour around
        // whichever committed first.
        board.insert(smd_pad(1, 0, 4.0, 10.0));
        board.insert(smd_pad(1, 0, 12.0, 10.0));
        board.insert(smd_pad(2, 0, 8.0, 6.0));
        board.insert(smd_pad(2, 0, 8.0, 14.0));
        board.insert(smd_pad(3, 0, 5.5, 7.5));
        board.insert(smd_pad(3, 0, 10.5, 12.5));

        let mut single = board.clone();
        let s0 = route_negotiated(
            &mut single,
            &mut crate::route::CpuSolver,
            &NegotiationOptions { cell: 50_000, max_rounds: 1, repasses: 0, rip_blocked: false, ..Default::default() },
            &mut |_| {},
            &mut |_, _| {},
        );
        assert_eq!(s0.committed, 1, "one round on one layer can commit only one of three mutually crossing routes");

        let s1 = route_negotiated(
            &mut board,
            &mut crate::route::CpuSolver,
            &NegotiationOptions { cell: 50_000, max_rounds: 1, repasses: 2, rip_blocked: true, ..Default::default() },
            &mut |_| {},
            &mut |_, _| {},
        );
        assert!(s1.committed > s0.committed, "{s1:?} vs {s0:?}");
        assert_eq!(s1.committed, 3, "{s1:?}");
        assert!(connectivity_without_pours(&board).airlines.is_empty());
        assert!(board::check(&board).is_empty());
    }

    #[test]
    fn priced_batch_solver_matches_unpriced_batch_with_via_zones() {
        struct BatchCpu {
            use_priced: bool,
        }
        impl FieldSolver for BatchCpu {
            fn solve(&mut self, grid: &RouteGrid, sources: &[usize], via_cost: u32) -> Vec<u32> {
                crate::search::cost_field(grid, sources, via_cost)
            }
            fn route(&mut self, grid: &RouteGrid, sources: &[usize], targets: &[usize], via_cost: u32) -> Option<GridPath> {
                route_astar(grid, sources, targets, via_cost)
            }
            fn batch_size(&self) -> usize {
                4
            }
            fn route_priced(&mut self, prices: &RoundPrices, jobs: &[PricedJob], via_cost: u32) -> Option<Vec<Option<GridPath>>> {
                if !self.use_priced {
                    return None;
                }
                Some(
                    jobs.iter()
                        .map(|job| {
                            let mut g = job.grid.clone();
                            price_into(prices.history, prices.counts[job.class], job.own.iter().copied(), prices.present, job.weight, &mut g.penalty);
                            if !prices.via_history.is_empty() {
                                via_price_into(
                                    prices.via_history,
                                    prices.via_counts[job.class],
                                    job.own_vias.iter().copied(),
                                    &job.hole_ring,
                                    prices.via_present,
                                    job.weight,
                                    &mut g.via_penalty,
                                );
                            }
                            route_astar(&g, job.sources, job.targets, via_cost)
                        })
                        .collect(),
                )
            }
            fn route_batch(&mut self, jobs: &[(&RouteGrid, &[usize], &[usize])], via_cost: u32) -> Vec<Option<GridPath>> {
                jobs.iter().map(|&(g, s, t)| route_astar(g, s, t, via_cost)).collect()
            }
        }

        let mut board = test_board("(0 \"F.Cu\" signal) (31 \"B.Cu\" signal)", &["A", "B", "C"]);
        // Net A is 3-pin and crosses B and C, exercising both `via_counts` and `hole_ring`.
        board.insert(smd_pad(1, 0, 4.0, 10.0));
        board.insert(smd_pad(1, 1, 12.0, 9.8));
        board.insert(smd_pad(1, 1, 12.0, 10.2));
        board.insert(smd_pad(2, 0, 8.0, 7.0));
        board.insert(smd_pad(2, 0, 8.0, 13.0));
        board.insert(smd_pad(3, 1, 9.0, 7.0));
        board.insert(smd_pad(3, 1, 9.0, 13.0));

        let opts = NegotiationOptions { cell: 50_000, via_zones: true, ..Default::default() };
        let mut b_priced = board.clone();
        let mut b_batch = board;
        let s_priced = route_negotiated(&mut b_priced, &mut BatchCpu { use_priced: true }, &opts, &mut |_| {}, &mut |_, _| {});
        let s_batch = route_negotiated(&mut b_batch, &mut BatchCpu { use_priced: false }, &opts, &mut |_| {}, &mut |_, _| {});
        assert_eq!(s_priced.committed, s_batch.committed);
        assert_eq!(s_priced.rounds, s_batch.rounds);
        assert_eq!(s_priced.vias, s_batch.vias);
        assert!(connectivity_without_pours(&b_priced).airlines.is_empty());
        assert!(board::check(&b_priced).is_empty());
    }
}
