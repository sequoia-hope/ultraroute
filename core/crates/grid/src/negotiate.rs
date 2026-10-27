//! Negotiated congestion (PathFinder, McMurchie and Ebeling 1995) on the conservative grid.
//!
//! Fixed copper stays hard-blocked. Routes found during negotiation are not: each net claims a
//! zone of cells around its paths, wide enough that two nets outside each other's zones are
//! legal on the board, and entering a cell inside another net's zone costs a present-congestion
//! price plus an accumulated history price. A net's own zone is lifted before it re-routes.
//! Rounds repeat, the present price rising and history accumulating where nets still collide,
//! until no path lies in another net's zone; then routes are committed, each checked exactly.
//!
//! Vias are negotiated too: a via stands on every layer, so the sites where one would come too
//! near another net's trace or via carry their own price, and a net's own vias keep their holes
//! apart unless two routes share one. What one pass could not commit is negotiated again against
//! what it laid, and a connection walled off by this call's own routes may have them ripped and
//! negotiated over with it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use board::{anchors, connectivity, connectivity_without_pours, nm, Airline, Board, Connectivity, ItemId, Kind, Nm, Point};

use crate::grid::{ObstacleMap, Profile, RouteGrid};
use crate::report::{tell, Log};
use crate::route::{lay, pieces, terminals, FieldSolver, PricedJob, RoundPrices, Terminal};
use crate::search::{route_astar, GridPath};

/// How often the trial routes are shown while a round runs one connection after another.
const TRIAL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Half a cell diagonal, as a fraction of the cell: how far a step strays from its nearer end.
const STEP_REACH: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// Kept beyond the exact geometry in zones, so that rounding to cell centres never lets through
/// a pair closer than the rules by less than KiCad's half micron of slack.
const ZONE_MARGIN: f64 = 1_000.0;

/// A pass of at most this many connections is a small one (a region's renegotiation, a rescue),
/// and a net of fewer open connections than this has its tree spanned again round pockets.
const SMALL: usize = 24;

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
    /// Zones as tight as the grid allows, and vias negotiated: the sites where a via would come
    /// too near another net's trace or via are priced and checked, and a net's own vias keep
    /// hole-to-hole clearance unless two of its routes share one. Off, the older zones, a whole
    /// step wider each side, and vias free of congestion.
    pub via_zones: bool,
    /// Passes after the first over the connections it left open that still have a way, routed
    /// against the copper the passes before laid.
    pub repasses: usize,
    /// When a connection that had a way before negotiation is walled off by routes it laid, rip
    /// those routes and negotiate them over with it, kept only when fewer connections stay open.
    pub rip_blocked: bool,
    /// Span a net of several clusters by pairs that can meet before pairs that cannot, so that a
    /// pin walled into a pocket in the middle does not keep the open pins either side apart.
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

impl NegotiationOptions {
    /// Every addition above off: negotiation as it was before them, its commit order and
    /// patience too, to compare against.
    fn legacy(&self) -> bool {
        !self.via_zones && self.repasses == 0 && !self.rip_blocked && !self.reachable_tree
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
    /// Cells its full-width legs cross outside its ends' own copper: claimed as its path is.
    legs: Vec<(usize, usize, usize)>,
    /// The cells where its path changes layer, each once.
    vias: Vec<usize>,
    /// Cells round its vias where another via of its net would break hole-to-hole clearance.
    hole_ring: Vec<u32>,
    /// Whether its path or vias ran into another claim in the last round.
    colliding: bool,
}

impl Connection {
    fn route(&self) -> Option<Route<'_>> {
        Some(Route { net: self.net, class: &self.class, path: self.path.as_ref()?, legs: &self.legs, vias: &self.vias, hole_ring: &self.hole_ring })
    }
}

/// A route this call laid, and what it laid: kept so that a rescue can price and rip it.
#[derive(Clone)]
struct LaidRoute {
    net: u32,
    class: String,
    path: GridPath,
    legs: Vec<(usize, usize, usize)>,
    vias: Vec<usize>,
    hole_ring: Vec<u32>,
    items: Vec<ItemId>,
}

impl LaidRoute {
    fn route(&self) -> Route<'_> {
        Route { net: self.net, class: &self.class, path: &self.path, legs: &self.legs, vias: &self.vias, hole_ring: &self.hole_ring }
    }
}

/// What a route's claims are worked out from.
#[derive(Clone, Copy)]
struct Route<'a> {
    net: u32,
    class: &'a str,
    path: &'a GridPath,
    legs: &'a [(usize, usize, usize)],
    vias: &'a [usize],
    hole_ring: &'a [u32],
}

/// Every route's claims as each class of route sees them: a wide trace keeps further from a
/// path than a thin one needs to. Classes that see alike share one set of claims, through
/// `alias`: on a board of many classes, most of the memory.
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

/// Per net, how many of its routes' hole rings cover each cell: where another of its vias may
/// not stand.
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

    fn count(&self, net: u32, cell: usize) -> u16 {
        self.per_net.get(&net).and_then(|m| m.get(&(cell as u32))).copied().unwrap_or(0)
    }
}

/// A pass's claims: on the nodes traces may take, on the cells vias may take, and each net's
/// hole rings.
struct PassClaims {
    trace: Claims,
    via: Claims,
    holes: SameNetHoles,
}

impl PassClaims {
    fn new(claimer: &Claimer) -> PassClaims {
        let cells = claimer.nx * claimer.ny;
        PassClaims {
            trace: Claims::new(&claimer.trace_alias, claimer.layers * cells),
            via: Claims::new(&claimer.via_alias, if claimer.exact { cells } else { 0 }),
            holes: SameNetHoles::default(),
        }
    }

    fn add(&mut self, claimer: &mut Claimer, r: Route) {
        self.trace.add(r.net, &claimer.trace(r));
        if claimer.exact {
            self.via.add(r.net, &claimer.via(r));
            self.holes.add(r.net, r.hole_ring);
        }
    }

    fn remove(&mut self, claimer: &mut Claimer, r: Route) {
        self.trace.remove(r.net, &claimer.trace(r));
        if claimer.exact {
            self.via.remove(r.net, &claimer.via(r));
            self.holes.remove(r.net, r.hole_ring);
        }
    }

    fn clear(&mut self) {
        self.trace.clear();
        self.via.clear();
        self.holes.per_net.clear();
    }

    /// Whether a route runs into a claim: its cells, other than its own ends', into another
    /// net's, or a via of it into another net's or a hole ring of its own net.
    fn hit(&self, r: Route, own_ends: &BTreeSet<usize>, nx: usize, cells: usize, exact: bool) -> bool {
        let trace = self.trace.seen_by(r.class);
        let traces = r.path.iter().chain(r.legs).map(|&(l, i, j)| l * cells + j * nx + i).any(|n| !own_ends.contains(&n) && trace.others(r.net, n) > 0);
        traces || exact && r.vias.iter().any(|&c| self.via.seen_by(r.class).others(r.net, c) > 0 || self.holes.count(r.net, c) > 0)
    }
}

/// Works out what a route claims from its path, whenever it is needed, rather than keeping it:
/// kept, every route's claims for every viewer were hundreds of megabytes on a large board.
struct Claimer {
    nx: usize,
    ny: usize,
    layers: usize,
    exact: bool,
    /// Per owner class and viewer class.
    discs: HashMap<(String, String), AllDiscs>,
    /// Per class: offsets round a via where another of its net's vias may not stand.
    hole_discs: HashMap<String, Vec<(i32, i32)>>,
    /// Each class to the first class that sees traces as it does (same width and clearance),
    /// and vias as it does (same clearance and via).
    trace_alias: HashMap<String, String>,
    via_alias: HashMap<String, String>,
    trace_viewers: Vec<String>,
    via_viewers: Vec<String>,
    /// One bit per node, set while a zone is stamped and cleared after, so that no node is
    /// claimed twice: an eighth of the memory a byte per node would take.
    seen: Vec<u64>,
}

impl Claimer {
    fn new(board: &Board, maps: &HashMap<String, ObstacleMap>, sample: &RouteGrid, exact: bool) -> Claimer {
        let mut discs = HashMap::new();
        let mut hole_discs = HashMap::new();
        for (owner, a) in maps {
            hole_discs.insert(owner.clone(), same_hole_disc(board, &a.profile, sample.h));
            for (viewer, b) in maps {
                discs.insert((owner.clone(), viewer.clone()), all_zone_discs(board, &a.profile, &b.profile, sample.h, exact));
            }
        }
        let mut sorted: Vec<&String> = maps.keys().collect();
        sorted.sort();
        let alias = |alike: &dyn Fn(&Profile, &Profile) -> bool| -> HashMap<String, String> {
            sorted
                .iter()
                .map(|&c| {
                    let like = sorted.iter().find(|o| alike(&maps[**o].profile, &maps[c].profile)).unwrap_or(&c);
                    (c.clone(), (*like).clone())
                })
                .collect()
        };
        let trace_alias = alias(&|a, b| a.half_width == b.half_width && a.clearance == b.clearance);
        let via_alias = alias(&|a, b| a.clearance == b.clearance && a.via_radius == b.via_radius && a.drill_radius == b.drill_radius);
        let viewers = |alias: &HashMap<String, String>| alias.values().cloned().collect::<BTreeSet<_>>().into_iter().collect();
        let (trace_viewers, via_viewers) = (viewers(&trace_alias), viewers(&via_alias));
        let (nx, ny, layers) = (sample.nx, sample.ny, sample.layers.len());
        let seen = vec![0; (layers * nx * ny).div_ceil(64)];
        Claimer { nx, ny, layers, exact, discs, hole_discs, trace_alias, via_alias, trace_viewers, via_viewers, seen }
    }

    /// The nodes a route claims against each viewer's traces.
    fn trace(&mut self, r: Route) -> Vec<(String, Vec<u32>)> {
        let Claimer { nx, ny, layers, discs, trace_viewers, seen, .. } = self;
        trace_viewers
            .iter()
            .map(|v| {
                let d = &discs[&(r.class.to_string(), v.clone())];
                (v.clone(), zone_of_fast(r.path, r.legs, r.vias, *nx, *ny, *layers, &d.trace_to_trace, &d.via_to_trace, seen))
            })
            .collect()
    }

    /// The cells a route claims against each viewer's vias; none unless vias are negotiated.
    fn via(&mut self, r: Route) -> Vec<(String, Vec<u32>)> {
        if !self.exact {
            return Vec::new();
        }
        let Claimer { nx, ny, discs, via_viewers, seen, .. } = self;
        via_viewers
            .iter()
            .map(|v| {
                let d = &discs[&(r.class.to_string(), v.clone())];
                (v.clone(), via_zone_of_fast(r.path, r.legs, r.vias, *nx, *ny, &d.trace_to_via, &d.via_to_via, seen))
            })
            .collect()
    }

    /// The hole ring round a route's vias, sorted; none unless vias are negotiated.
    fn holes(&mut self, class: &str, vias: &[usize]) -> Vec<u32> {
        if !self.exact {
            return Vec::new();
        }
        stamp_cells_2d(vias, self.nx, self.ny, &self.hole_discs[class], &mut self.seen)
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
    let conn = joined(board, options.pours_block);
    let mut open = open_of(&conn, &options.nets);
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
        open = reachable_airlines(board, &conn, open, &maps, &mut grid_buf, options.via_cost);
    }
    drop(conn);
    let mut claimer = Claimer::new(board, &maps, &sample, options.via_zones);
    let mut laid: Vec<LaidRoute> = Vec::new();
    let keep_laid = options.repasses > 0 || options.rip_blocked;

    let first = run_pass(
        board, solver, options, &mut maps, &mut claimer, &sample, &mut grid_buf, open, options.max_rounds, &options.net_weight, keep_laid, true,
        &out_of_time, watch, trials, &mut on_round, &mut laid,
    );
    stats.rounds = first.rounds;
    stats.conflicted = first.conflicted;
    stats.unroutable = first.unroutable;
    stats.committed = first.committed;
    stats.vias = first.vias;
    stats.laid = first.laid;

    for repass in 0..options.repasses {
        if out_of_time() {
            break;
        }
        let left = routable_open(board, options, &maps, &mut grid_buf);
        if left.is_empty() {
            break;
        }
        // Another order, so that a pass does not lose the same races again.
        let again = NegotiationOptions { seed: options.seed.wrapping_add((repass as u64 + 1).wrapping_mul(0x9e37_79b9)), ..options.clone() };
        let pass = run_pass(
            board, solver, &again, &mut maps, &mut claimer, &sample, &mut grid_buf, left, options.max_rounds.min(16), &options.net_weight, keep_laid, false,
            &out_of_time, watch, &mut |_, _| {}, &mut None, &mut laid,
        );
        if pass.committed == 0 {
            break;
        }
    }

    if options.rip_blocked && !out_of_time() && open_of(&joined(board, options.pours_block), &options.nets).len() > stats.unroutable {
        rescue_walled_off(board, solver, options, &mut maps, &mut claimer, &sample, &mut grid_buf, &out_of_time, watch, &mut laid);
    }

    if keep_laid {
        let left = open_of(&joined(board, options.pours_block), &options.nets).len();
        stats.committed = stats.connections.saturating_sub(left);
        stats.laid = laid.iter().flat_map(|r| r.items.iter().copied()).collect();
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

/// Negotiates `open` to the end and commits what it can. With `keep_laid`, what it lays is also
/// stamped into `maps`, for passes after it to route round, and kept in `laid`.
#[allow(clippy::too_many_arguments)]
fn run_pass(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &NegotiationOptions,
    maps: &mut HashMap<String, ObstacleMap>,
    claimer: &mut Claimer,
    sample: &RouteGrid,
    grid_buf: &mut RouteGrid,
    open: Vec<Airline>,
    max_rounds: usize,
    weights: &HashMap<u32, f64>,
    keep_laid: bool,
    report: bool,
    out_of_time: &dyn Fn() -> bool,
    watch: &mut dyn FnMut(&Board),
    trials: &mut dyn FnMut(usize, &[Trial]),
    on_round: &mut Option<&mut dyn FnMut(RoundView)>,
    laid: &mut Vec<LaidRoute>,
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
            hole_ring: Vec::new(),
            colliding: false,
        })
        .collect();
    crate::jitter::shuffle(&mut connections, options.seed);
    // Favoured nets first; the sort is stable, so with no weights the order is the seed's.
    connections.sort_by(|a, b| weight_of(a.net).total_cmp(&weight_of(b.net)));

    let (nx, ny, layers) = (sample.nx, sample.ny, sample.layers.len());
    let cells = nx * ny;
    // A via stands on every layer: crowding at its site is priced, and remembered, as often.
    let via_scale = layers.max(2) as f64;
    let via_step = options.history_step * layers.max(2) as u32;

    let mut zones = PassClaims::new(claimer);
    let mut history = vec![0u32; layers * cells];
    let mut via_history = if options.via_zones { vec![0u32; cells] } else { Vec::new() };
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
    let half_widths: HashMap<&String, Nm> = maps.iter().map(|(class, m)| (class, m.profile.half_width)).collect();
    // What a connection's claims are worked out from, once its path is found.
    let settle = |k: usize, c: &mut Connection, claimer: &mut Claimer| {
        c.legs.clear();
        c.vias.clear();
        c.hole_ring.clear();
        let Some(path) = &c.path else { return };
        if options.via_zones {
            c.legs = leg_cells(sample, path, &ends[k].0, &ends[k].1, half_widths[&c.class], &own_ends[k]);
        }
        c.vias = via_cells_of(path, nx);
        c.hole_ring = claimer.holes(&c.class, &c.vias);
    };

    // A small pass often starts with a collision or two and nothing between them and none, so
    // that `patience` would give up while a via pair or a short detour still costs more than
    // crossing; it waits until the present price has outgrown them.
    let patience = if !options.legacy() && connections.len() <= SMALL { options.patience.max(8) } else { options.patience };
    let mut shown = crate::clock::Instant::now();
    let (mut best_colliding, mut stale) = (usize::MAX, 0);
    for round in 0..max_rounds {
        if out_of_time() {
            break;
        }
        summary.rounds = round + 1;
        if solver.batch_size() > 1 || options.threads > 1 {
            if solver.batch_size() > 1 {
                route_round_in_batches(&mut connections, maps, &ends, &zones, &history, &via_history, present, via_scale, weights, options, solver);
            } else {
                route_round_in_parallel(&mut connections, maps, &ends, &zones, &history, &via_history, present, via_scale, weights, options, round, sample, report, trials);
            }
            zones.clear();
            for (k, c) in connections.iter_mut().enumerate() {
                settle(k, c, claimer);
                if let Some(r) = c.route() {
                    zones.add(claimer, r);
                }
            }
        } else {
            for k in 0..connections.len() {
                if out_of_time() {
                    break;
                }
                let c = &mut connections[k];
                if report {
                    tell(&options.log, "negotiating", || format!("round {}: rerouting {}", round + 1, board.net_name(c.net)), None);
                }
                if let Some(r) = c.route() {
                    zones.remove(claimer, r);
                }
                maps[&c.class].view_into(c.net, grid_buf);
                let w = weight_of(c.net);
                penalties_into(zones.trace.seen_by(&c.class), &history, present, c.net, w, &mut grid_buf.penalty);
                if options.via_zones {
                    via_penalties_into(&zones, &c.class, &[], &via_history, present * via_scale, c.net, w, &mut grid_buf.via_penalty);
                }
                let (sources, targets) = &ends[k];
                c.path = solver.route(grid_buf, &cells_of(sources), &cells_of(targets), options.via_cost);
                settle(k, c, claimer);
                if let Some(r) = c.route() {
                    zones.add(claimer, r);
                }
                if report && shown.elapsed() > TRIAL_INTERVAL {
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
                if !own_ends[k].contains(&n) && zones.trace.seen_by(&c.class).others(c.net, n) > 0 {
                    history[n] += options.history_step;
                    collides = true;
                }
            }
            if options.via_zones {
                for &cell in &c.vias {
                    if zones.via.seen_by(&c.class).others(c.net, cell) > 0 || zones.holes.count(c.net, cell) > 0 {
                        via_history[cell] += via_step;
                        collides = true;
                    }
                }
            }
            colliding += usize::from(collides);
            c.colliding = collides;
        }
        if report {
            let list = trial_list(&connections, sample);
            trials(round, &list);
            if let Some(cb) = on_round.as_deref_mut() {
                let mut overlap = vec![0u16; layers * cells];
                for z in zones.trace.by_viewer.values() {
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
    drop(zones);
    drop(history);
    summary.unroutable = connections.iter().filter(|c| c.path.is_none()).count();

    // Commit greedily: a route whose cells lie in no committed net's zone is legal against
    // everything committed so far, converged or not; each piece is still checked exactly.
    let order = if options.legacy() {
        (0..connections.len()).filter(|&k| connections[k].path.is_some()).collect()
    } else {
        commit_order(&connections, &own_ends, nx, cells, claimer, &weight_of)
    };
    let mut committed = PassClaims::new(claimer);
    for k in order {
        let c = &connections[k];
        let Some(r) = c.route() else { continue };
        if committed.hit(r, &own_ends[k], nx, cells, options.via_zones) {
            continue;
        }
        maps[&c.class].view_into(c.net, grid_buf);
        let (sources, targets) = &ends[k];
        let Some(items) = lay(board, grid_buf, r.path, sources, targets) else { continue };
        committed.add(claimer, r);
        if keep_laid {
            for map in maps.values_mut() {
                for &id in &items {
                    map.stamp(board, id);
                }
            }
            laid.push(LaidRoute {
                net: c.net,
                class: c.class.clone(),
                path: r.path.clone(),
                legs: c.legs.clone(),
                vias: c.vias.clone(),
                hole_ring: c.hole_ring.clone(),
                items: items.clone(),
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

/// The order to commit in: routes that collide with nothing first, then the rest one at a
/// time, each the route that runs into the fewest of those still to go (favoured nets before
/// others, short routes before long), so that one sprawling route does not shut out three short
/// ones.
fn commit_order(
    connections: &[Connection],
    own_ends: &[BTreeSet<usize>],
    nx: usize,
    cells: usize,
    claimer: &mut Claimer,
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
    let meet = |k: usize, m: usize, conflicts: &mut Vec<BTreeSet<usize>>| {
        conflicts[k].insert(m);
        conflicts[m].insert(k);
    };
    for (k, c) in connections.iter().enumerate() {
        let Some(r) = c.route() else { continue };
        for (viewer, zone) in claimer.trace(r) {
            for m in zone.iter().filter_map(|n| trace_at.get(n)).flatten().copied() {
                if m != k && connections[m].net != c.net && claimer.trace_alias[&connections[m].class] == viewer {
                    meet(k, m, &mut conflicts);
                }
            }
        }
        for (viewer, zone) in claimer.via(r) {
            for m in zone.iter().filter_map(|n| via_at.get(n)).flatten().copied() {
                if m != k && connections[m].net != c.net && claimer.via_alias[&connections[m].class] == viewer {
                    meet(k, m, &mut conflicts);
                }
            }
        }
        for m in c.hole_ring.iter().filter_map(|n| via_at.get(n)).flatten().copied() {
            if m != k && connections[m].net == c.net {
                meet(k, m, &mut conflicts);
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
        let degree = |k: usize| conflicts[k].iter().filter(|&&m| active[m]).count();
        let len = |k: usize| connections[k].path.as_ref().map_or(0, Vec::len);
        let best = (0..remaining.len())
            .min_by(|&x, &y| {
                let (a, b) = (remaining[x], remaining[y]);
                let (ca, cb) = (&connections[a], &connections[b]);
                ca.colliding
                    .cmp(&cb.colliding)
                    .then_with(|| weight_of(ca.net).total_cmp(&weight_of(cb.net)))
                    .then_with(|| degree(a).cmp(&degree(b)))
                    .then_with(|| len(a).cmp(&len(b)))
                    .then_with(|| a.cmp(&b))
            })
            .expect("some remain");
        let chosen = remaining.swap_remove(best);
        active[chosen] = false;
        out.push(chosen);
    }
    out
}

fn joined(board: &Board, pours_block: bool) -> Connectivity {
    if pours_block { connectivity(board) } else { connectivity_without_pours(board) }
}

fn open_of(conn: &Connectivity, nets: &Option<HashSet<u32>>) -> Vec<Airline> {
    let mut open = conn.airlines.clone();
    if let Some(only) = nets {
        open.retain(|a| only.contains(&a.net));
    }
    open
}

/// The open connections that still have a way on `maps`, as the copper laid so far leaves them.
fn routable_open(board: &mut Board, options: &NegotiationOptions, maps: &HashMap<String, ObstacleMap>, grid_buf: &mut RouteGrid) -> Vec<Airline> {
    let conn = joined(board, options.pours_block);
    let mut open = open_of(&conn, &options.nets);
    if options.reachable_tree {
        open = reachable_airlines(board, &conn, open, maps, grid_buf, options.via_cost);
    }
    open.into_iter().filter(|a| has_way(board, maps, grid_buf, a, options.via_cost)).collect()
}

/// Whether `a` has a way on its class's map, if it has one.
fn has_way(board: &mut Board, maps: &HashMap<String, ObstacleMap>, grid_buf: &mut RouteGrid, a: &Airline, via_cost: u32) -> bool {
    let Some(map) = maps.get(&Profile::for_net(board, a.net).class) else { return false };
    map.view_into(a.net, grid_buf);
    let s: Vec<usize> = terminals(board, grid_buf, a.from_item).into_iter().map(|t| t.0).collect();
    let t: Vec<usize> = terminals(board, grid_buf, a.to_item).into_iter().map(|t| t.0).collect();
    !s.is_empty() && !t.is_empty() && route_astar(grid_buf, &s, &t, via_cost).is_some()
}

/// The present price a rescue puts on each laid route claiming a node: high enough that the
/// walled-off connection crosses as few of them as it can.
const RESCUE_PRICE: f64 = 120.0;

/// At most so many walled-off connections tried, and so many routes ripped for one.
const RESCUES: usize = 6;
const RESCUE_RIPS: usize = 8;

/// A connection that had a way before this call laid anything and has none now is walled off
/// by routes this call laid. Its cheapest way across them shows which: those routes' nets are
/// ripped whole (a net's routes may share vias, and a net of several pins can span again round
/// the rescued route), and negotiated over with it, the walled-off net favoured. The change is
/// kept only when fewer connections are left open than before.
#[allow(clippy::too_many_arguments)]
fn rescue_walled_off(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &NegotiationOptions,
    maps: &mut HashMap<String, ObstacleMap>,
    claimer: &mut Claimer,
    sample: &RouteGrid,
    grid_buf: &mut RouteGrid,
    out_of_time: &dyn Fn() -> bool,
    watch: &mut dyn FnMut(&Board),
    laid: &mut Vec<LaidRoute>,
) {
    // The maps as they stood before this call laid anything: made again here, not kept from the
    // start, where they would double the maps' memory for a rescue most calls never need.
    let ours: HashSet<ItemId> = laid.iter().flat_map(|r| r.items.iter().copied()).collect();
    let base_maps: HashMap<String, ObstacleMap> = maps
        .iter()
        .map(|(class, m)| (class.clone(), ObstacleMap::without(board, m.profile.clone(), options.cell, options.pours_block, &|id| ours.contains(&id))))
        .collect();
    drop(ours);
    let base_maps = &base_maps;
    // The maps as `base_maps` with every route in `laid` stamped back in.
    let rebuild = |board: &Board, maps: &mut HashMap<String, ObstacleMap>, laid: &[LaidRoute]| {
        maps.clone_from(base_maps);
        for map in maps.values_mut() {
            for &id in laid.iter().flat_map(|r| &r.items) {
                map.stamp(board, id);
            }
        }
    };
    let mut tried: HashSet<u32> = HashSet::new();
    for _ in 0..RESCUES {
        if out_of_time() {
            break;
        }
        let conn = joined(board, options.pours_block);
        let mut open = open_of(&conn, &options.nets);
        if options.reachable_tree {
            open = reachable_airlines(board, &conn, open, maps, grid_buf, options.via_cost);
        }
        let open_before = open.len();
        let Some(walled) = open
            .into_iter()
            .filter(|a| !tried.contains(&a.net))
            .find(|a| !has_way(board, maps, grid_buf, a, options.via_cost) && has_way(board, base_maps, grid_buf, a, options.via_cost))
        else {
            break;
        };
        tried.insert(walled.net);

        // Its cheapest way on the bare board, every laid route's claims priced.
        let class = Profile::for_net(board, walled.net).class;
        let mut owner: HashMap<ItemId, usize> = HashMap::new();
        let found = {
            let mut soft = PassClaims::new(claimer);
            for (idx, r) in laid.iter().enumerate() {
                soft.add(claimer, r.route());
                for &id in &r.items {
                    owner.insert(id, idx);
                }
            }
            base_maps[&class].view_into(walled.net, grid_buf);
            crowd_into(soft.trace.seen_by(&class), RESCUE_PRICE, walled.net, &mut grid_buf.penalty);
            if options.via_zones {
                let calm = vec![0; sample.nx * sample.ny];
                via_penalties_into(&soft, &class, &[], &calm, RESCUE_PRICE * sample.layers.len().max(2) as f64, walled.net, 1.0, &mut grid_buf.via_penalty);
            }
            let (s, t) = (terminals(board, grid_buf, walled.from_item), terminals(board, grid_buf, walled.to_item));
            let path = solver.route(grid_buf, &s.iter().map(|t| t.0).collect::<Vec<_>>(), &t.iter().map(|t| t.0).collect::<Vec<_>>(), options.via_cost);
            path.and_then(|path| pieces(grid_buf, &path, &s, &t))
        };
        let Some(trial) = found else { continue };
        // The laid routes it breaks a rule against; it must break none against anything else.
        let mut victims: BTreeSet<usize> = BTreeSet::new();
        let mut hard = false;
        for piece in trial {
            let temp = board.insert(piece);
            for v in grid_buf.violations(board, temp) {
                match v.b.and_then(|b| owner.get(&b).copied()) {
                    Some(idx) => {
                        victims.insert(idx);
                    }
                    None => hard = true,
                }
            }
            board.discard(temp);
        }
        if hard || victims.is_empty() {
            continue;
        }
        let nets: HashSet<u32> = victims.iter().map(|&i| laid[i].net).collect();
        victims.extend((0..laid.len()).filter(|&i| nets.contains(&laid[i].net)));
        if victims.len() > RESCUE_RIPS {
            continue;
        }

        let snapshot = (board.clone(), laid.clone());
        let mut kept = Vec::with_capacity(laid.len());
        for (idx, r) in laid.drain(..).enumerate() {
            if victims.contains(&idx) {
                for &id in r.items.iter().rev() {
                    board.discard(id);
                }
            } else {
                kept.push(r);
            }
        }
        *laid = kept;
        rebuild(board, maps, laid);

        let mut weights = options.net_weight.clone();
        weights.insert(walled.net, options.net_weight.get(&walled.net).copied().unwrap_or(1.0) * 0.5);
        for pass in 0..2u64 {
            if out_of_time() {
                break;
            }
            let left = routable_open(board, options, maps, grid_buf);
            if left.is_empty() {
                break;
            }
            let again = NegotiationOptions { seed: options.seed.wrapping_add((pass + 17).wrapping_mul(0x517c_c1b7)), ..options.clone() };
            let done = run_pass(
                board, solver, &again, maps, claimer, sample, grid_buf, left, options.max_rounds.min(16), &weights, true, false, out_of_time,
                &mut |_| {}, &mut |_, _| {}, &mut None, laid,
            );
            if done.committed == 0 {
                break;
            }
        }

        if open_of(&joined(board, options.pours_block), &options.nets).len() < open_before {
            watch(board);
        } else {
            (*board, *laid) = snapshot;
            rebuild(board, maps, laid);
        }
    }
}

/// A point a connection may start from, and the copper there.
type Anchor = (Point, ItemId);

/// A net of several open clusters is spanned by the closest pairs of clusters, blind to whether
/// a pair can meet at all: a pin walled into a pocket in the middle of the tree takes both its
/// airlines with it, and the clusters either side, which could meet past it, stay apart. Where
/// an airline of such a net has no way on the grid, its tree is spanned again by pairs that have
/// one first, nearest first, the pocketed clusters joined last: as many airlines as before.
fn reachable_airlines(
    board: &mut Board,
    conn: &Connectivity,
    mut open: Vec<Airline>,
    maps: &HashMap<String, ObstacleMap>,
    grid_buf: &mut RouteGrid,
    via_cost: u32,
) -> Vec<Airline> {
    let mut by_net: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (idx, a) in open.iter().enumerate() {
        by_net.entry(a.net).or_default().push(idx);
    }
    by_net.retain(|_, idxs| (2..SMALL).contains(&idxs.len()));
    if by_net.is_empty() {
        return open;
    }
    // Each cluster's anchors, as the airlines were spanned from.
    let mut clusters_of: HashMap<u32, BTreeMap<usize, Vec<Anchor>>> = HashMap::new();
    for (id, item) in board.items() {
        if !item.is_copper() || matches!(item.kind, Kind::Pour { .. }) || !by_net.contains_key(&item.net) {
            continue;
        }
        let Some(&cluster) = conn.cluster.get(&id) else { continue };
        let (pts, len) = anchors(board, id);
        clusters_of.entry(item.net).or_default().entry(cluster).or_default().extend(pts[..len].iter().map(|&p| (p, id)));
    }

    for (net, idxs) in by_net {
        let Some(map) = maps.get(&Profile::for_net(board, net).class) else { continue };
        let Some(by_cluster) = clusters_of.get(&net) else { continue };
        let clusters: Vec<&Vec<Anchor>> = by_cluster.values().filter(|v| !v.is_empty()).collect();
        let m = clusters.len();
        if m != idxs.len() + 1 {
            continue;
        }
        map.view_into(net, grid_buf);
        let mut terms: HashMap<ItemId, Vec<usize>> = HashMap::new();
        let mut meet = |board: &mut Board, a: ItemId, b: ItemId| {
            for id in [a, b] {
                terms.entry(id).or_insert_with(|| terminals(board, grid_buf, id).into_iter().map(|t| t.0).collect());
            }
            let (s, t) = (&terms[&a], &terms[&b]);
            !s.is_empty() && !t.is_empty() && route_astar(grid_buf, s, t, via_cost).is_some()
        };
        if idxs.iter().all(|&i| meet(board, open[i].from_item, open[i].to_item)) {
            continue;
        }

        let closest = |ca: &[Anchor], cb: &[Anchor]| {
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
        let mut edges: Vec<(f64, usize, usize, Anchor, Anchor)> = Vec::with_capacity(m * (m - 1) / 2);
        for u in 0..m {
            for v in u + 1..m {
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
        // Kruskal's tree: pairs that meet on the grid first, then any to finish it.
        let mut chosen: Vec<Airline> = Vec::with_capacity(m - 1);
        for must_meet in [true, false] {
            for &(_, u, v, pa, pb) in &edges {
                if chosen.len() == m - 1 {
                    break;
                }
                let (ru, rv) = (find(&mut parent, u), find(&mut parent, v));
                if ru != rv && (!must_meet || meet(board, pa.1, pb.1)) {
                    parent[ru] = rv;
                    chosen.push(Airline { net, from: pa.0, to: pb.0, from_item: pa.1, to_item: pb.1 });
                }
            }
        }
        if chosen.len() == idxs.len() {
            for (&slot, airline) in idxs.iter().zip(chosen) {
                open[slot] = airline;
            }
        }
    }
    open
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

/// Each node's present price for `net` alone, without history: for a rescue, which has none.
fn crowd_into(zones: &Zones, present: f64, net: u32, out: &mut Vec<u32>) {
    out.clear();
    out.extend((0..zones.count.len()).map(|n| price(0, zones.others(net, n), present, 1.0)));
}

/// A node's price: its history and the present price of the `others` nets covering it, scaled
/// by `weight`.
fn price(history: u32, others: u16, present: f64, weight: f64) -> u32 {
    if (weight - 1.0).abs() < 1e-12 {
        history + (present * f64::from(others)).round() as u32
    } else {
        ((f64::from(history) + present * f64::from(others)) * weight).round().max(0.0) as u32
    }
}

/// The hole rings of `net`'s other routes: in a Jacobi round its own, `own_ring`, is still among
/// them and is taken back out.
fn sibling_hole_ring(holes: &SameNetHoles, net: u32, own_ring: &[u32]) -> Vec<(u32, u16)> {
    let Some(mine) = holes.per_net.get(&net) else { return Vec::new() };
    mine.iter()
        .filter_map(|(&c, &n)| {
            let others = n.saturating_sub(u16::from(own_ring.binary_search(&c).is_ok()));
            (others > 0).then_some((c, others))
        })
        .collect()
}

/// Adds the via sites' prices for a connection of `class` and `net` to `out`, its grid's own.
#[allow(clippy::too_many_arguments)]
fn via_penalties_into(zones: &PassClaims, class: &str, own_ring: &[u32], via_history: &[u32], via_present: f64, net: u32, weight: f64, out: &mut Vec<u32>) {
    let seen = zones.via.seen_by(class);
    let own = seen.per_net.get(&net).into_iter().flat_map(|m| m.keys().copied());
    via_price_into(via_history, &seen.count, own, &sibling_hole_ring(&zones.holes, net, own_ring), via_present, weight, out);
}

/// Adds each via site's price, from a round's `via_history` and `count` of nets claiming it
/// against a via of this class (the sites `own` its own net claims counted one fewer), and
/// `hole_ring`, the times its net's other routes keep a via from it, onto `out`: the grid's own
/// via costs (its via-avoid areas), or nothing.
pub fn via_price_into(via_history: &[u32], count: &[u16], own: impl Iterator<Item = u32>, hole_ring: &[(u32, u16)], via_present: f64, weight: f64, out: &mut Vec<u32>) {
    if out.len() != count.len() {
        out.clear();
        out.resize(count.len(), 0);
    }
    for ((o, &h), &n) in out.iter_mut().zip(via_history).zip(count) {
        *o = o.saturating_add(price(h, n, via_present, weight));
    }
    for c in own {
        let (c, h, n) = (c as usize, via_history[c as usize], count[c as usize]);
        out[c] = out[c].saturating_sub(price(h, n, via_present, weight)).saturating_add(price(h, n.saturating_sub(1), via_present, weight));
    }
    for &(c, n) in hole_ring {
        out[c as usize] = out[c as usize].saturating_add(price(0, n, via_present, weight));
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
        out.extend(history.iter().zip(count).map(|(&h, &n)| price(h, n, present, weight)));
        for c in own {
            let c = c as usize;
            out[c] = price(history[c], count[c] - 1, present, weight);
        }
    }
}

/// One Jacobi round with the fields solved together, a batch at a time, on the solver's device.
#[allow(clippy::too_many_arguments)]
fn route_round_in_batches(
    connections: &mut [Connection],
    maps: &HashMap<String, ObstacleMap>,
    ends: &[(Vec<Terminal>, Vec<Terminal>)],
    zones: &PassClaims,
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
        counts: classes.iter().map(|c| zones.trace.seen_by(c).count.as_slice()).collect(),
        present,
        via_history,
        via_counts: if options.via_zones { classes.iter().map(|c| zones.via.seen_by(c).count.as_slice()).collect() } else { Vec::new() },
        via_present: present * via_scale,
    };
    let weight_of = |net: u32| weights.get(&net).copied().unwrap_or(1.0);
    let mut priced = true;
    for start in (0..connections.len()).step_by(size) {
        let end = (start + size).min(connections.len());
        let count = end - start;
        let sources: Vec<Vec<usize>> = ends[start..end].iter().map(|(s, _)| s.iter().map(|t| t.0).collect()).collect();
        let targets: Vec<Vec<usize>> = ends[start..end].iter().map(|(_, t)| t.iter().map(|t| t.0).collect()).collect();
        if priced {
            for (c, grid) in connections[start..end].iter().zip(grids[..count].iter_mut()) {
                maps[&c.class].view_into(c.net, grid);
            }
            let jobs: Vec<PricedJob> = connections[start..end]
                .iter()
                .zip(&grids[..count])
                .zip(sources.iter().zip(&targets))
                .map(|((c, grid), (s, t))| PricedJob {
                    grid,
                    class: classes.iter().position(|k| **k == c.class).expect("a map per class"),
                    weight: weight_of(c.net),
                    own: zones.trace.seen_by(&c.class).per_net.get(&c.net).map(|m| m.keys().copied().collect()).unwrap_or_default(),
                    own_vias: if options.via_zones { zones.via.seen_by(&c.class).per_net.get(&c.net).map(|m| m.keys().copied().collect()).unwrap_or_default() } else { Vec::new() },
                    hole_ring: if options.via_zones { sibling_hole_ring(&zones.holes, c.net, &c.hole_ring) } else { Vec::new() },
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
        for (c, grid) in connections[start..end].iter().zip(grids[..count].iter_mut()) {
            maps[&c.class].view_into(c.net, grid);
            penalties_into(zones.trace.seen_by(&c.class), history, present, c.net, weight_of(c.net), &mut grid.penalty);
            if options.via_zones {
                via_penalties_into(zones, &c.class, &c.hole_ring, via_history, present * via_scale, c.net, weight_of(c.net), &mut grid.via_penalty);
            }
        }
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
    zones: &PassClaims,
    history: &[u32],
    via_history: &[u32],
    present: f64,
    via_scale: f64,
    weights: &HashMap<u32, f64>,
    options: &NegotiationOptions,
    round: usize,
    sample: &RouteGrid,
    report: bool,
    trials: &mut dyn FnMut(usize, &[Trial]),
) {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let jobs: Vec<(u32, &str, f64, &[u32])> =
        connections.iter().map(|c| (c.net, c.class.as_str(), weights.get(&c.net).copied().unwrap_or(1.0), c.hole_ring.as_slice())).collect();
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
                    let (net, class, weight, own_ring) = jobs[k];
                    let grid = match &mut grid {
                        Some(g) => {
                            maps[class].view_into(net, g);
                            g
                        }
                        None => grid.insert(maps[class].view(net)),
                    };
                    penalties_into(zones.trace.seen_by(class), history, present, net, weight, &mut grid.penalty);
                    if options.via_zones {
                        via_penalties_into(zones, class, own_ring, via_history, present * via_scale, net, weight, &mut grid.via_penalty);
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
            if !report {
                continue;
            }
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

/// A disc of cell offsets, and for each of the eight steps to a neighbour the offsets a step
/// that way newly covers: a path's zone is stamped a rim at a time, not a disc at a time.
struct FastDisc {
    full: Vec<(i32, i32)>,
    rims: [Vec<(i32, i32)>; 8],
}

/// The eight steps, in the order of [`FastDisc::rims`].
const STEPS: [(i32, i32); 8] = [(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)];

impl FastDisc {
    fn of(full: Vec<(i32, i32)>) -> FastDisc {
        let set: HashSet<(i32, i32)> = full.iter().copied().collect();
        let rims = std::array::from_fn(|k| {
            let (di, dj) = STEPS[k];
            full.iter().copied().filter(|&(oi, oj)| !set.contains(&(oi + di, oj + dj))).collect()
        });
        FastDisc { full, rims }
    }

    /// The offsets to stamp at path cell `k`: a rim where the path just stepped there from a
    /// neighbouring cell already stamped (`along`), the whole disc otherwise.
    fn at(&self, path: &GridPath, k: usize, along: bool) -> &[(i32, i32)] {
        let step = (k > 0 && along).then(|| (path[k].1 as i32 - path[k - 1].1 as i32, path[k].2 as i32 - path[k - 1].2 as i32));
        step.and_then(|s| STEPS.iter().position(|&d| d == s)).map_or(&self.full, |d| &self.rims[d])
    }
}

/// Offsets claimed round an owner's trace cell and via cell, against a viewer's trace and via.
struct AllDiscs {
    trace_to_trace: FastDisc,
    via_to_trace: Vec<(i32, i32)>,
    trace_to_via: FastDisc,
    via_to_via: Vec<(i32, i32)>,
}

/// Cell offsets within the zone radius of an owner's trace cell and via cell, as a viewer class
/// sees them. Exact, the radius is what two grid segments need: segments no longer than a cell
/// diagonal whose ends keep `sqrt(d^2 + h^2 / 2)` apart keep `d` apart all along, where the
/// older zones added a whole step reach each side, `1.4 h`, and shut fine-pitch lanes. Exact
/// zones also claim against vias, for copper, hole-to-copper and hole-to-hole clearance.
fn all_zone_discs(board: &Board, owner: &Profile, viewer: &Profile, h: Nm, exact: bool) -> AllDiscs {
    let clearance = owner.clearance.max(viewer.clearance).max(nm(board.rules.min_clearance)) as f64;
    let h = h as f64;
    if !exact {
        let (half_width, via, widest) = (owner.half_width as f64, owner.via_radius as f64, viewer.half_width as f64);
        let disc = |reach: f64| -> Vec<(i32, i32)> {
            let r = (reach / h).ceil() as i32;
            (-r..=r).flat_map(|dj| (-r..=r).map(move |di| (di, dj))).filter(|&(di, dj)| f64::from(di).hypot(f64::from(dj)) * h < reach + h).collect()
        };
        return AllDiscs {
            trace_to_trace: FastDisc::of(disc(half_width + widest + clearance + 2.0 * STEP_REACH * h)),
            via_to_trace: disc(via + widest + clearance + 2.0 * STEP_REACH * h),
            trace_to_via: FastDisc::of(Vec::new()),
            via_to_via: Vec::new(),
        };
    }
    let hole = nm(board.rules.min_hole_clearance) as f64;
    let hole_to_hole = nm(board.rules.min_hole_to_hole) as f64;
    let segment = |d: f64| (d.hypot(STEP_REACH * h) + ZONE_MARGIN).max(h + ZONE_MARGIN);
    let (ohw, ovia, odrill) = (owner.half_width as f64, owner.via_radius as f64, owner.drill_radius as f64);
    let (vhw, vvia, vdrill) = (viewer.half_width as f64, viewer.via_radius as f64, viewer.drill_radius as f64);
    let trace_trace = ohw + vhw + clearance;
    let via_trace = (ovia + vhw + clearance).max(odrill + vhw + hole);
    let trace_via = (ohw + vvia + clearance).max(ohw + vdrill + hole);
    let via_via = (ovia + vvia + clearance).max(odrill + vdrill + hole_to_hole).max(ovia + vdrill + hole).max(odrill + vvia + hole);
    AllDiscs {
        trace_to_trace: FastDisc::of(disc_within(segment(trace_trace), h)),
        via_to_trace: disc_within(segment(via_trace), h),
        trace_to_via: FastDisc::of(disc_within(segment(trace_via), h)),
        via_to_via: disc_within(via_via + ZONE_MARGIN, h),
    }
}

/// Cell offsets whose centres lie closer than `reach`.
fn disc_within(reach: f64, h: f64) -> Vec<(i32, i32)> {
    let r = (reach / h).ceil() as i32;
    (-r..=r).flat_map(|dj| (-r..=r).map(move |di| (di, dj))).filter(|&(di, dj)| f64::from(di).hypot(f64::from(dj)) * h < reach).collect()
}

/// Offsets round a via where another via of its net would break hole-to-hole clearance; not its
/// own cell, where two routes of the net share the one via.
fn same_hole_disc(board: &Board, profile: &Profile, h: Nm) -> Vec<(i32, i32)> {
    let reach = 2.0 * profile.drill_radius as f64 + nm(board.rules.min_hole_to_hole) as f64 + ZONE_MARGIN;
    disc_within(reach, h as f64).into_iter().filter(|&o| o != (0, 0)).collect()
}

fn via_cells_of(path: &GridPath, nx: usize) -> Vec<usize> {
    let mut vias: Vec<usize> = path.windows(2).filter(|w| w[0].0 != w[1].0).map(|w| w[1].2 * nx + w[1].1).collect();
    vias.sort_unstable();
    vias.dedup();
    vias
}

/// The cells a path's full-width legs cross outside its ends' own copper, sampled half a cell
/// apart. A leg necked narrower than the route stays within its pad's reach and is left out.
fn leg_cells(grid: &RouteGrid, path: &GridPath, sources: &[Terminal], targets: &[Terminal], half_width: Nm, own: &BTreeSet<usize>) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let (Some(&first), Some(&last)) = (path.first(), path.last()) else { return out };
    let cells = grid.nx * grid.ny;
    let leg_of = |terms: &[Terminal], (l, i, j): (usize, usize, usize)| {
        let n = l * cells + j * grid.nx + i;
        // An end that may also start on the copper itself needs no leg.
        if terms.iter().any(|t| t.0 == n && t.1.is_none()) {
            None
        } else {
            terms.iter().find_map(|t| (t.0 == n).then_some(t.1).flatten())
        }
    };
    for (end, leg) in [(first, leg_of(sources, first)), (last, leg_of(targets, last))] {
        let Some((anchor, hw)) = leg else { continue };
        if hw < half_width {
            continue;
        }
        let to = grid.centre(end.1, end.2);
        let steps = ((anchor.distance(to) / (grid.h as f64 * 0.5)).ceil() as usize).max(1);
        for s in 0..steps {
            let t = s as f64 / steps as f64;
            let p = Point::new(anchor.x + ((to.x - anchor.x) as f64 * t).round() as Nm, anchor.y + ((to.y - anchor.y) as f64 * t).round() as Nm);
            if let Some((i, j)) = grid.cell_at(p) {
                if !own.contains(&(end.0 * cells + j * grid.nx + i)) && (end.0, i, j) != end {
                    out.push((end.0, i, j));
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Clears what a stamp marked, ready for the next.
fn forget(seen: &mut [u64], marked: &[u32]) {
    for &n in marked {
        seen[n as usize / 64] &= !(1u64 << (n % 64));
    }
}

/// Stamps `disc` round cell `(i, j)` of a grid `nx` by `ny`, `base` the layer's first node, into
/// `out`, each node once.
#[allow(clippy::too_many_arguments)]
fn stamp(out: &mut Vec<u32>, seen: &mut [u64], base: usize, nx: usize, ny: usize, i: usize, j: usize, disc: &[(i32, i32)]) {
    for &(di, dj) in disc {
        let (a, b) = (i as i32 + di, j as i32 + dj);
        if a >= 0 && b >= 0 && (a as usize) < nx && (b as usize) < ny {
            let n = base + b as usize * nx + a as usize;
            let (word, bit) = (n / 64, 1u64 << (n % 64));
            if seen[word] & bit == 0 {
                seen[word] |= bit;
                out.push(n as u32);
            }
        }
    }
}

/// The `(layer, i, j)` nodes a path claims against a viewer's trace: a trace disc round each
/// cell of it and of its legs on their layer, and a via disc on every layer at each via.
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
    seen: &mut [u64],
) -> Vec<u32> {
    let cells = nx * ny;
    let mut out = Vec::new();
    for (k, &(layer, i, j)) in path.iter().enumerate() {
        let along = k > 0 && path[k - 1].0 == layer;
        stamp(&mut out, seen, layer * cells, nx, ny, i, j, trace_disc.at(path, k, along));
    }
    for &(layer, i, j) in legs {
        stamp(&mut out, seen, layer * cells, nx, ny, i, j, &trace_disc.full);
    }
    for &cell in vias {
        for l in 0..layers {
            stamp(&mut out, seen, l * cells, nx, ny, cell % nx, cell / nx, via_disc);
        }
    }
    forget(seen, &out);
    out
}

/// The `(i, j)` cells a path claims against a viewer's via: round every cell of it and of its
/// legs, on whatever layer, and round each of its vias.
#[allow(clippy::too_many_arguments)]
fn via_zone_of_fast(
    path: &GridPath,
    legs: &[(usize, usize, usize)],
    vias: &[usize],
    nx: usize,
    ny: usize,
    trace_to_via: &FastDisc,
    via_to_via: &[(i32, i32)],
    seen: &mut [u64],
) -> Vec<u32> {
    let mut out = Vec::new();
    for (k, &(_, i, j)) in path.iter().enumerate() {
        // A change of layer stays on its cell, already stamped.
        if k > 0 && (path[k - 1].1, path[k - 1].2) == (i, j) {
            continue;
        }
        stamp(&mut out, seen, 0, nx, ny, i, j, trace_to_via.at(path, k, true));
    }
    for &(_, i, j) in legs {
        stamp(&mut out, seen, 0, nx, ny, i, j, &trace_to_via.full);
    }
    for &cell in vias {
        stamp(&mut out, seen, 0, nx, ny, cell % nx, cell / nx, via_to_via);
    }
    forget(seen, &out);
    out
}

/// The cells within `disc` of any of `centres`, sorted.
fn stamp_cells_2d(centres: &[usize], nx: usize, ny: usize, disc: &[(i32, i32)], seen: &mut [u64]) -> Vec<u32> {
    let mut out = Vec::new();
    for &cell in centres {
        stamp(&mut out, seen, 0, nx, ny, cell % nx, cell / nx, disc);
    }
    forget(seen, &out);
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use board::Shape;

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

            // Via prices are the same prices, added onto the grid's own via costs.
            let base: Vec<u32> = (0..nodes).map(|n| (n % 3) as u32 * 1000).collect();
            let mut via = base.clone();
            via_price_into(&history, &z.count, z.per_net.get(&net).into_iter().flat_map(|m| m.keys().copied()), &[], 23.7, 1.0, &mut via);
            assert_eq!(via, slow.iter().zip(&base).map(|(p, b)| p + b).collect::<Vec<_>>());
        }
    }

    #[test]
    fn a_zone_is_sorted_unique_and_on_the_grid() {
        let (nx, ny, layers): (usize, usize, usize) = (8, 6, 2);
        let disc: Vec<(i32, i32)> = (-1..=1).flat_map(|dj| (-1..=1).map(move |di| (di, dj))).collect();
        let path: GridPath = vec![(0, 0, 0), (0, 1, 0), (1, 1, 0), (1, 7, 5)];
        let mut seen = vec![0; (layers * nx * ny).div_ceil(64)];
        let mut z = zone_of_fast(&path, &[], &via_cells_of(&path, nx), nx, ny, layers, &FastDisc::of(disc.clone()), &disc, &mut seen);
        assert!(seen.iter().all(|&w| w == 0), "a stamp leaves nothing marked");
        let n = z.len();
        z.sort_unstable();
        z.dedup();
        assert_eq!(z.len(), n, "each node once");
        assert!(z.iter().all(|&n| (n as usize) < nx * ny * layers));
        // The via at (1, 0) claims its disc on both layers, the corner cell clipped to the grid.
        assert!(z.contains(&0) && z.contains(&((nx * ny) as u32)));
        assert!(z.contains(&((nx * ny + 5 * nx + 7) as u32)));
    }

    fn test_board(layers: &str, nets: &[&str]) -> Board {
        let mut defs = String::from("(net 0 \"\")");
        for (i, n) in nets.iter().enumerate() {
            defs.push_str(&format!(" (net {} \"{n}\")", i + 1));
        }
        let src = format!(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers {layers}) {defs}
             (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1)))"
        );
        let (pcb, copper) = kicad_pcb::parse_board(&src).unwrap();
        Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap())
    }

    fn smd_pad(net: u32, layer: usize, x: f64, y: f64) -> board::Item {
        let p = Point::new((x * 1e6).round() as Nm, (y * 1e6).round() as Nm);
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
    fn rims_stamp_what_whole_discs_would() {
        let (nx, ny, layers): (usize, usize, usize) = (24, 24, 2);
        let raw: Vec<(i32, i32)> = (-4..=4).flat_map(|dj| (-4..=4).map(move |di| (di, dj))).filter(|&(di, dj)| di * di + dj * dj <= 18).collect();
        let vdisc: Vec<(i32, i32)> = (-5..=5).flat_map(|dj| (-5..=5).map(move |di| (di, dj))).filter(|&(di, dj)| di * di + dj * dj <= 25).collect();
        let fd = FastDisc::of(raw.clone());
        let path: GridPath = vec![(0, 8, 8), (0, 9, 8), (0, 10, 9), (0, 10, 10), (0, 9, 11), (0, 8, 11), (1, 8, 11), (1, 7, 10), (1, 7, 9), (1, 8, 8)];
        let legs = vec![(0, 7, 8), (1, 9, 8)];
        let vias = via_cells_of(&path, nx);
        let mut seen = vec![0; (layers * nx * ny).div_ceil(64)];
        let cells = nx * ny;
        let on_grid = |i: usize, j: usize, (di, dj): (i32, i32)| {
            let (a, b) = (i as i32 + di, j as i32 + dj);
            (a >= 0 && b >= 0 && (a as usize) < nx && (b as usize) < ny).then(|| b as usize * nx + a as usize)
        };

        let mut fast = zone_of_fast(&path, &legs, &vias, nx, ny, layers, &fd, &vdisc, &mut seen);
        fast.sort_unstable();
        let mut slow = BTreeSet::new();
        for &(l, i, j) in path.iter().chain(&legs) {
            slow.extend(raw.iter().filter_map(|&o| on_grid(i, j, o)).map(|c| (l * cells + c) as u32));
        }
        for &c in &vias {
            for l in 0..layers {
                slow.extend(vdisc.iter().filter_map(|&o| on_grid(c % nx, c / nx, o)).map(|c| (l * cells + c) as u32));
            }
        }
        assert_eq!(fast, slow.into_iter().collect::<Vec<_>>());

        let mut fast = via_zone_of_fast(&path, &legs, &vias, nx, ny, &fd, &vdisc, &mut seen);
        fast.sort_unstable();
        let mut slow = BTreeSet::new();
        for &(_, i, j) in path.iter().chain(&legs) {
            slow.extend(raw.iter().filter_map(|&o| on_grid(i, j, o)).map(|c| c as u32));
        }
        for &c in &vias {
            slow.extend(vdisc.iter().filter_map(|&o| on_grid(c % nx, c / nx, o)).map(|c| c as u32));
        }
        assert_eq!(fast, slow.into_iter().collect::<Vec<_>>());
        assert!(seen.iter().all(|&w| w == 0), "a stamp leaves nothing marked");
    }

    #[test]
    fn via_zones_keep_crossing_routes_clear_around_vias() {
        let mut board = test_board("(0 \"F.Cu\" signal) (31 \"B.Cu\" signal)", &["A", "B", "WALL"]);
        board.insert(smd_pad(1, 0, 3.0, 9.6));
        board.insert(smd_pad(1, 0, 13.0, 9.6));
        board.insert(smd_pad(2, 0, 3.0, 10.4));
        board.insert(smd_pad(2, 0, 13.0, 10.4));
        // A wall on the top layer sends both nets through vias in the same corridor.
        board.insert(locked_wall(3, 0, (8.0, 3.0), (8.0, 17.0)));
        let options = NegotiationOptions { cell: 50_000, repasses: 0, rip_blocked: false, ..Default::default() };
        let stats = route_negotiated(&mut board, &mut crate::route::CpuSolver, &options, &mut |_| {}, &mut |_, _| {});
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
        let options = NegotiationOptions { cell: 50_000, repasses: 0, rip_blocked: false, ..Default::default() };
        let stats = route_negotiated(&mut board, &mut crate::route::CpuSolver, &options, &mut |_| {}, &mut |_, _| {});
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
        let options = NegotiationOptions { cell: 50_000, reachable_tree: false, repasses: 0, rip_blocked: false, ..Default::default() };
        let stats = route_negotiated(&mut blind, &mut crate::route::CpuSolver, &options, &mut |_| {}, &mut |_, _| {});
        assert_eq!(stats.committed, 0, "the closest pairs both run into the boxed middle pad");
        assert_eq!(connectivity_without_pours(&blind).airlines.len(), 2);

        let options = NegotiationOptions { reachable_tree: true, ..options };
        let stats = route_negotiated(&mut board, &mut crate::route::CpuSolver, &options, &mut |_| {}, &mut |_, _| {});
        assert_eq!(stats.committed, 1, "the two open outer pads are joined past it");
        assert_eq!(connectivity_without_pours(&board).airlines.len(), 1);
        assert!(board::check(&board).is_empty());
    }

    #[test]
    fn repasses_and_rescue_finish_routes_one_pass_left() {
        let mut board = test_board("(0 \"F.Cu\" signal)", &["A", "B", "C"]);
        // Three pairs that cross one another on a single layer: one round commits one of them.
        board.insert(smd_pad(1, 0, 4.0, 10.0));
        board.insert(smd_pad(1, 0, 12.0, 10.0));
        board.insert(smd_pad(2, 0, 8.0, 6.0));
        board.insert(smd_pad(2, 0, 8.0, 14.0));
        board.insert(smd_pad(3, 0, 5.5, 7.5));
        board.insert(smd_pad(3, 0, 10.5, 12.5));
        let mut single = board.clone();
        let options = NegotiationOptions { cell: 50_000, max_rounds: 1, repasses: 0, rip_blocked: false, ..Default::default() };
        let one = route_negotiated(&mut single, &mut crate::route::CpuSolver, &options, &mut |_| {}, &mut |_, _| {});
        assert_eq!(one.committed, 1);

        let options = NegotiationOptions { repasses: 2, rip_blocked: true, ..options };
        let all = route_negotiated(&mut board, &mut crate::route::CpuSolver, &options, &mut |_| {}, &mut |_, _| {});
        assert_eq!(all.committed, 3, "{all:?}");
        assert!(connectivity_without_pours(&board).airlines.is_empty());
        assert!(board::check(&board).is_empty());
    }

    #[test]
    fn a_batch_priced_by_the_solver_routes_as_one_priced_here() {
        struct BatchCpu {
            priced: bool,
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
                if !self.priced {
                    return None;
                }
                let route = |job: &PricedJob| {
                    let mut g = job.grid.clone();
                    price_into(prices.history, prices.counts[job.class], job.own.iter().copied(), prices.present, job.weight, &mut g.penalty);
                    if !prices.via_history.is_empty() {
                        let own = job.own_vias.iter().copied();
                        via_price_into(prices.via_history, prices.via_counts[job.class], own, &job.hole_ring, prices.via_present, job.weight, &mut g.via_penalty);
                    }
                    route_astar(&g, job.sources, job.targets, via_cost)
                };
                Some(jobs.iter().map(route).collect())
            }
            fn route_batch(&mut self, jobs: &[(&RouteGrid, &[usize], &[usize])], via_cost: u32) -> Vec<Option<GridPath>> {
                jobs.iter().map(|&(g, s, t)| route_astar(g, s, t, via_cost)).collect()
            }
        }

        let mut board = test_board("(0 \"F.Cu\" signal) (31 \"B.Cu\" signal)", &["A", "B", "C"]);
        // A of three pins crosses B and C: via counts and hole rings both come into it.
        board.insert(smd_pad(1, 0, 4.0, 10.0));
        board.insert(smd_pad(1, 1, 12.0, 9.8));
        board.insert(smd_pad(1, 1, 12.0, 10.2));
        board.insert(smd_pad(2, 0, 8.0, 7.0));
        board.insert(smd_pad(2, 0, 8.0, 13.0));
        board.insert(smd_pad(3, 1, 9.0, 7.0));
        board.insert(smd_pad(3, 1, 9.0, 13.0));
        // A via-avoid area across the middle, which both ways must price alike.
        let mm = |x: f64, y: f64| Point::new((x * 1e6) as Nm, (y * 1e6) as Nm);
        board.via_avoid.push(Shape::Polygon(vec![mm(6.0, 0.0), mm(10.0, 0.0), mm(10.0, 20.0), mm(6.0, 20.0)]));
        let options = NegotiationOptions { cell: 50_000, ..Default::default() };
        let (mut priced, mut batch) = (board.clone(), board);
        let a = route_negotiated(&mut priced, &mut BatchCpu { priced: true }, &options, &mut |_| {}, &mut |_, _| {});
        let b = route_negotiated(&mut batch, &mut BatchCpu { priced: false }, &options, &mut |_| {}, &mut |_, _| {});
        assert_eq!((a.committed, a.rounds, a.vias), (b.committed, b.rounds, b.vias));
        let copper = |board: &Board| board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via)).map(|(_, it)| format!("{:?}", it.shape)).collect::<BTreeSet<_>>();
        assert_eq!(copper(&priced), copper(&batch));
        assert!(connectivity_without_pours(&priced).airlines.is_empty());
        assert!(board::check(&priced).is_empty());
    }
}
