//! Open connections routed on the grid, one at a time, and laid as tracks and vias.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use board::{connectivity, connectivity_without_pours, joined, BBox, Board, Item, ItemId, Kind, Nm, Point, Shape};

use crate::grid::{ObstacleMap, Profile, RouteGrid};
use crate::report::{item_name, tell, Focus, Log};
use crate::search::{cost_field, route_astar, trace_back, GridPath};

/// How many nearby free cells are tried as the far end of a straight leg out of a terminal.
const LEG_CANDIDATES: usize = 16;
/// How far beyond a terminal's own extent a leg may reach, in cells.
const LEG_CELLS: Nm = 3;
/// How far a necked-down leg may reach out of a pad too crowded for a full-width one, in cells.
const NECK_CELLS: Nm = 20;
/// How far a forced neck, one narrower than the rules allow, may reach beyond its pad, in
/// nanometres: out from among the part's pins and no further.
const FORCED_NECK_REACH: Nm = 1_000_000;

pub trait FieldSolver {
    fn solve(&mut self, grid: &RouteGrid, sources: &[usize], via_cost: u32) -> Vec<u32>;

    /// Fields for several grids of one shape; a GPU solves them together.
    fn solve_batch(&mut self, jobs: &[(&RouteGrid, &[usize])], via_cost: u32) -> Vec<Vec<u32>> {
        jobs.iter().map(|(grid, sources)| self.solve(grid, sources, via_cost)).collect()
    }

    /// How many fields to hand `solve_batch` at once; one means no batching.
    fn batch_size(&self) -> usize {
        1
    }

    /// The cheapest route from the sources to any target.
    fn route(&mut self, grid: &RouteGrid, sources: &[usize], targets: &[usize], via_cost: u32) -> Option<GridPath> {
        let cost = self.solve(grid, sources, via_cost);
        trace_back(grid, &cost, targets, via_cost)
    }

    /// Cheapest routes for a batch of a negotiation round's connections, each priced by the
    /// solver itself from the round's shared `prices` (see [`crate::negotiate::price_into`]),
    /// as a pool of workers does without being sent every connection's prices; `None` when the
    /// solver cannot, and the batch goes through [`FieldSolver::route_batch`] instead.
    fn route_priced(&mut self, _prices: &RoundPrices, _jobs: &[PricedJob], _via_cost: u32) -> Option<Vec<Option<GridPath>>> {
        None
    }

    /// Cheapest routes for several grids of one shape at once.
    fn route_batch(&mut self, jobs: &[(&RouteGrid, &[usize], &[usize])], via_cost: u32) -> Vec<Option<GridPath>> {
        let solve_jobs: Vec<(&RouteGrid, &[usize])> = jobs.iter().map(|&(g, s, _)| (g, s)).collect();
        let fields = self.solve_batch(&solve_jobs, via_cost);
        jobs.iter()
            .zip(fields)
            .map(|(&(grid, _, targets), cost)| trace_back(grid, &cost, targets, via_cost))
            .collect()
    }
}

/// What a negotiation round prices every connection from: the nodes' history, how many nets
/// cover each node as each class sees them (classes in name order), the present price per net,
/// and the corresponding 2D via history, via-viewer counts, and via present price when via
/// zones are active (`via_history` is empty otherwise). `generation` changes each round.
pub struct RoundPrices<'a> {
    pub generation: u64,
    pub history: &'a [u32],
    pub counts: Vec<&'a [u16]>,
    pub present: f64,
    pub via_history: &'a [u32],
    pub via_counts: Vec<&'a [u16]>,
    pub via_present: f64,
}

/// One connection of a round to price and route: its grid without prices, the class whose
/// counts it sees, its weight, the nodes and 2D via cells its own net covers, sibling via-drill
/// annulus counts on the same net, and its ends.
pub struct PricedJob<'a> {
    pub grid: &'a RouteGrid,
    pub class: usize,
    pub weight: f64,
    pub own: Vec<u32>,
    pub own_vias: Vec<u32>,
    pub hole_ring: Vec<(u32, u16)>,
    pub sources: &'a [usize],
    pub targets: &'a [usize],
}

pub struct CpuSolver;

impl FieldSolver for CpuSolver {
    fn solve(&mut self, grid: &RouteGrid, sources: &[usize], via_cost: u32) -> Vec<u32> {
        cost_field(grid, sources, via_cost)
    }

    fn route(&mut self, grid: &RouteGrid, sources: &[usize], targets: &[usize], via_cost: u32) -> Option<GridPath> {
        route_astar(grid, sources, targets, via_cost)
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    pub cell: Nm,
    pub via_cost: u32,
    /// When to stop, at the next connection and inside a search.
    pub deadline: crate::clock::Deadline,
    /// Nets whose open connections are routed; every net when `None`.
    pub nets: Option<HashSet<u32>>,
    /// Keep other nets out of poured copper instead of routing through it to be refilled.
    pub pours_block: bool,
    /// Routes on a two-layer board prefer each layer's own direction (`RouteGrid::directional`).
    pub directional: bool,
    /// Reorders connections of nearly equal length, for a search over many runs; 0 keeps the
    /// shortest-first order exact.
    pub seed: u64,
    pub log: Option<Log>,
    /// Nets whose vias cost other than `via_cost`: a net a length rule limits pays for each the
    /// track its barrel counts as.
    pub net_via_cost: HashMap<u32, u32>,
    /// A connection with no way on the grid is tried once more on a finer one round it.
    pub fine: bool,
    /// A connection with no way at its class width may narrow to a legal thinner width across
    /// tight gaps only, returning to full width outside them ([`corridor_widths`]). Off unless
    /// a stage asks: the last stage, for what nothing before it could join, and rip-up's
    /// rerouting there; every other caller routes as it did before corridors.
    pub corridor: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            cell: 50_000,
            via_cost: 80,
            deadline: Default::default(),
            nets: None,
            pours_block: false,
            directional: true,
            seed: 0,
            log: None,
            net_via_cost: HashMap::new(),
            fine: true,
            corridor: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    pub routed: usize,
    pub failed: usize,
    /// Failures by cause: an end with no free cell, no path between them, a piece refused.
    pub no_terminal: usize,
    pub no_path: usize,
    pub refused: usize,
    pub vias: usize,
    /// Every track and via laid, for writing back.
    pub laid: Vec<ItemId>,
    /// The connections given up, by their two ends, and why.
    pub failures: Vec<(ItemId, ItemId, &'static str)>,
    /// The connections laid narrowed to their pins the whole way ([`narrow_run`]), by their two
    /// ends, with the width.
    pub narrowed: Vec<(ItemId, ItemId, Nm)>,
    /// The connections laid on a finer grid round them, where the run's own found no way.
    pub fine: usize,
    /// The connections laid narrowed only across tight gaps ([`corridor_widths`]).
    pub corridor: usize,
    /// Each narrowed stretch of those, by the connection's two ends, with its width and length.
    pub corridors: Vec<(ItemId, ItemId, Nm, Nm)>,
    pub setup_ns: u64,
    pub routed_ns: u64,
    pub no_terminal_ns: u64,
    pub no_path_ns: u64,
    pub refused_ns: u64,
}

/// How a path reaches a terminal's copper when its cell's centre is not on it: from `from` on the
/// copper (through `elbow`, when a straight line clips a neighbour) to the cell's centre, at
/// `half_width`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Leg {
    pub(crate) from: Point,
    pub(crate) elbow: Option<Point>,
    pub(crate) half_width: Nm,
}

/// A place a path may start or end: a free cell, and any leg needed to reach the copper.
pub(crate) type Terminal = (usize, Option<Leg>);

/// Routes the board's open connections, shortest first, until a round joins nothing more.
/// `watch` sees the board after every connection laid.
pub fn route_open(board: &mut Board, solver: &mut dyn FieldSolver, options: &Options, watch: &mut dyn FnMut(&Board)) -> Stats {
    let out_of_time = || options.deadline.expired();
    let mut stats = Stats::default();
    let mut maps: HashMap<String, ObstacleMap> = HashMap::new();
    let mut grid = RouteGrid {
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
    // The GPU's fields know nothing of directions.
    let directional = options.directional && board.layers.len() == 2 && solver.batch_size() <= 1;
    let mut given_up: BTreeSet<(ItemId, ItemId)> = BTreeSet::new();
    // Once nothing more routes, a net of several clusters is spanned again by pairs that can meet
    // first: an airline into a pin walled in a pocket hides the way between the pins either side.
    let mut respan = false;
    loop {
        let sep = board::work::begin_event();
        let st0 = board::work::timed_run_ns();
        // Unless pours are kept as planes, they will be refilled around what is routed, and may
        // no longer join what they join now; so every connection is made in tracks.
        let conn = if options.pours_block { connectivity(board) } else { connectivity_without_pours(board) };
        let mut est_open = conn.airlines.len();
        let mut open = conn.airlines.clone();
        if let Some(only) = &options.nets {
            open.retain(|a| only.contains(&a.net));
        }
        if respan {
            open = crate::negotiate::reachable_airlines(board, &conn, open, &maps, &mut grid, options.via_cost);
        }
        drop(conn);
        open.sort_by(|a, b| a.from.distance(a.to).total_cmp(&b.from.distance(b.to)));
        if options.seed != 0 {
            // Shortest first still, roughly: shuffled within runs of eight.
            for (k, chunk) in open.chunks_mut(8).enumerate() {
                crate::jitter::shuffle(chunk, options.seed.wrapping_add(k as u64));
            }
        }
        open.retain(|a| !given_up.contains(&(a.from_item, a.to_item)));
        stats.setup_ns = stats.setup_ns.saturating_add(board::work::timed_run_ns().saturating_sub(st0));
        if open.is_empty() {
            sep.finish("route_setup", "empty", options.nets.is_none().then_some(est_open));
            if respan || given_up.is_empty() {
                break;
            }
            respan = true;
            continue;
        }
        sep.finish("route_setup", "setup", options.nets.is_none().then_some(est_open));
        let routed_before = stats.routed;
        for airline in open {
            if out_of_time() {
                return stats;
            }
            let ev = board::work::begin_event();
            let t0 = board::work::timed_run_ns();
            let mut map_build_ns = 0u64;
            let net_name = board.net_name(airline.net).to_string();
            tell(
                &options.log,
                "routing",
                || format!("{net_name}: {} to {}", item_name(board, airline.from_item), item_name(board, airline.to_item)),
                Some(Focus::Connection { net: airline.net, from: airline.from, to: airline.to }),
            );
            let profile = Profile::for_net(board, airline.net);
            if !maps.contains_key(&profile.class) {
                let mt0 = board::work::timed_run_ns();
                let Some(map) = ObstacleMap::new_by(board, profile.clone(), options.cell, options.pours_block, &options.deadline) else {
                    stats.setup_ns = stats.setup_ns.saturating_add(board::work::timed_run_ns().saturating_sub(t0));
                    ev.finish("route_setup", "cut", None);
                    return stats;
                };
                map_build_ns = map_build_ns.saturating_add(board::work::timed_run_ns().saturating_sub(mt0));
                maps.insert(profile.class.clone(), map);
            }
            let mut tried = attempt(board, solver, options, &maps[&profile.class], &mut grid, directional, &airline);
            // A short connection of a net that carries current, which finds no way at its width,
            // may go narrowed to its pin's width the whole way: as a designer runs a regulator's
            // switch node from a fine-pitch pin through the gap beside its capacitors. Its map
            // covers the connection's neighbourhood only.
            if let (Err(_), Some(hw)) = (&tried, narrow_run(board, &profile, &airline)) {
                let mut narrow = profile.clone();
                (narrow.half_width, narrow.narrow_half_width) = (hw, hw);
                let map = ObstacleMap::window(board, narrow, options.cell, narrow_window(&airline), options.pours_block);
                if let Ok(items) = attempt(board, solver, options, &map, &mut grid, directional, &airline) {
                    stats.narrowed.push((airline.from_item, airline.to_item, 2 * hw));
                    tried = Ok(items);
                }
            }
            // A way that fits by less than the grid's margin (a track between header pins at the
            // rules' clearance, which a step between cell centres may stray into) shows on a finer
            // grid: the connection's neighbourhood on cells half the size, once.
            let fine = options.cell / 2;
            let window = narrow_window(&airline);
            let nodes = ((window.max.x - window.min.x) / fine.max(1)) as f64 * ((window.max.y - window.min.y) / fine.max(1)) as f64 * board.layers.len() as f64;
            if tried.is_err() && options.fine && fine >= FINE_CELL_FLOOR && nodes <= FINE_NODES {
                let map = ObstacleMap::window(board, profile.clone(), fine, window, options.pours_block);
                if let Ok(items) = attempt(board, solver, options, &map, &mut grid, directional, &airline) {
                    stats.fine += 1;
                    tried = Ok(items);
                }
            }
            // Where a tight gap between pins or obstacles blocks the class's full width, route on
            // a narrower legal map while pricing cells blocked at full width, so the track narrows
            // only through the pinch and returns to full width outside it. A gap whose free band
            // at the narrower width is narrower than a cell (as 2.54 mm header rows are on 50 um
            // cells) falls back to a half-cell window round the connection.
            if tried.is_err() && options.corridor {
                let cwin = corridor_window(&airline);
                let cnodes = ((cwin.max.x - cwin.min.x) / fine.max(1)) as f64 * ((cwin.max.y - cwin.min.y) / fine.max(1)) as f64 * board.layers.len() as f64;
                for hw in corridor_widths(board, &profile, &airline) {
                    // A map the deadline cuts short gives up the corridor, not the run: the
                    // connection fails as any other, and the loop's own check stops it.
                    if out_of_time() {
                        break;
                    }
                    let mut narrow = profile.clone();
                    (narrow.half_width, narrow.narrow_half_width) = (hw, profile.narrow_half_width.min(hw));
                    let key = format!("{}\u{0}c{hw}", profile.class);
                    if !maps.contains_key(&key) {
                        let mt0 = board::work::timed_run_ns();
                        let Some(map) = ObstacleMap::new_by(board, narrow.clone(), options.cell, options.pours_block, &options.deadline) else {
                            map_build_ns = map_build_ns.saturating_add(board::work::timed_run_ns().saturating_sub(mt0));
                            break;
                        };
                        map_build_ns = map_build_ns.saturating_add(board::work::timed_run_ns().saturating_sub(mt0));
                        maps.insert(key.clone(), map);
                    }
                    let (full_map, narrow_map) = (&maps[&profile.class], &maps[&key]);
                    if let Ok((items, stretches)) = attempt_corridor(board, solver, options, full_map, narrow_map, &mut grid, &mut full_grid, directional, &airline) {
                        stats.corridor += 1;
                        stats.corridors.extend(stretches.into_iter().map(|(w, l)| (airline.from_item, airline.to_item, 2 * w, l)));
                        tried = Ok(items);
                        break;
                    }
                    if !(options.fine && fine >= FINE_CELL_FLOOR && cnodes <= FINE_NODES) || out_of_time() {
                        continue;
                    }
                    let fine_narrow = ObstacleMap::window(board, narrow, fine, cwin, options.pours_block);
                    fine_narrow.view_into(airline.net, &mut grid);
                    if terminals(board, &grid, airline.from_item).is_empty() || terminals(board, &grid, airline.to_item).is_empty() || out_of_time() {
                        continue;
                    }
                    let fine_full = ObstacleMap::window(board, profile.clone(), fine, cwin, options.pours_block);
                    if let Ok((items, stretches)) = attempt_corridor(board, solver, options, &fine_full, &fine_narrow, &mut grid, &mut full_grid, directional, &airline) {
                        stats.corridor += 1;
                        stats.fine += 1;
                        stats.corridors.extend(stretches.into_iter().map(|(w, l)| (airline.from_item, airline.to_item, 2 * w, l)));
                        tried = Ok(items);
                        break;
                    }
                }
            }
            let mut cause_tag: &'static str = "closed";
            let laid = match tried {
                Ok(items) => Some(items),
                Err(missed) => {
                    let why = match missed {
                        Missed::Start => "no terminal at the start",
                        Missed::End => "no terminal at the end",
                        Missed::Path => "no path",
                        Missed::Refused => "refused",
                    };
                    match missed {
                        Missed::Start | Missed::End => {
                            stats.no_terminal += 1;
                            cause_tag = "no_terminal";
                        }
                        Missed::Path => {
                            stats.no_path += 1;
                            cause_tag = "no_path";
                        }
                        _ => {
                            stats.refused += 1;
                            cause_tag = "refused";
                        }
                    }
                    if why != "refused" {
                        stats.failures.push((airline.from_item, airline.to_item, why));
                    }
                    None
                }
            };
            match &laid {
                Some(items) => tell(&options.log, "routed", || {
                    let vias = items.iter().filter(|id| board.item(**id).is_some_and(|it| it.kind == Kind::Via)).count();
                    format!("{net_name}: laid {} pieces, {vias} vias", items.len())
                }, None),
                None => tell(&options.log, "gave up", || format!("{net_name}: {}", stats.failures.last().map_or("refused", |f| f.2)), None),
            }
            match laid {
                Some(items) => {
                    stats.routed += 1;
                    est_open = est_open.saturating_sub(1);
                    stats.vias += items.iter().filter(|id| board.item(**id).is_some_and(|it| it.kind == Kind::Via)).count();
                    for map in maps.values_mut() {
                        for &id in &items {
                            map.stamp(board, id);
                        }
                    }
                    stats.laid.extend(items);
                    watch(board);
                    let total_dt = board::work::timed_run_ns().saturating_sub(t0);
                    stats.setup_ns = stats.setup_ns.saturating_add(map_build_ns.min(total_dt));
                    stats.routed_ns = stats.routed_ns.saturating_add(total_dt.saturating_sub(map_build_ns));
                    ev.finish("route_conn", "closed", options.nets.is_none().then_some(est_open));
                }
                None => {
                    stats.failed += 1;
                    given_up.insert((airline.from_item, airline.to_item));
                    let total_dt = board::work::timed_run_ns().saturating_sub(t0);
                    stats.setup_ns = stats.setup_ns.saturating_add(map_build_ns.min(total_dt));
                    let conn_dt = total_dt.saturating_sub(map_build_ns);
                    match cause_tag {
                        "no_terminal" => stats.no_terminal_ns = stats.no_terminal_ns.saturating_add(conn_dt),
                        "no_path" => stats.no_path_ns = stats.no_path_ns.saturating_add(conn_dt),
                        _ => stats.refused_ns = stats.refused_ns.saturating_add(conn_dt),
                    }
                    ev.finish("route_conn", cause_tag, None);
                }
            }
        }
        if stats.routed == routed_before {
            if respan || given_up.is_empty() {
                break;
            }
            respan = true;
        }
    }
    stats
}

/// Why one try at a connection laid nothing.
enum Missed {
    Start,
    End,
    Path,
    Refused,
}

/// Extra cost per cell entered where the class's full width is blocked, so a corridor-necked
/// route leaves the pinch and returns to full width as soon as room allows.
pub(crate) const NARROW_CELL_PENALTY: u32 = 60;

/// Below this minimum half-width (a 0.05 mm track, under 2 mil), the project file holds zero or
/// an Eagle import placeholder rather than a fabrication rule; corridor necking stays at or above
/// [`FALLBACK_MIN_HALF_WIDTH`] (a 0.15 mm track).
const SANE_MIN_HALF_WIDTH: Nm = 25_000;
const FALLBACK_MIN_HALF_WIDTH: Nm = 75_000;

/// The longest a corridor's narrowed stretch may run, end to end: a gap between pins, a pin
/// walled in by its neighbours, and out; not a thin track along a header row.
pub(crate) const CORRIDOR_STRETCH: Nm = 5_000_000;

/// A corridor's narrowed stretch: its half width and its length.
pub(crate) type Stretch = (Nm, Nm);

/// Legal narrower half-widths a connection of `profile` may squeeze through tight gaps at. A
/// signal, which necks to its minimum near its pads already, goes to the net's minimum. A net
/// that carries current (a class wider than the default) keeps its width as the router's rule
/// has it ([`Profile::narrow_half_width`]), and narrows only as [`narrow_run`] allows it: for
/// ends no further apart than [`NARROW_RUN_REACH`], never thinner than its pins.
pub(crate) fn corridor_widths(board: &Board, profile: &Profile, airline: &board::Airline) -> Vec<Nm> {
    if profile.narrow_half_width >= profile.half_width {
        return narrow_run(board, profile, airline).into_iter().collect();
    }
    let floor = if profile.min_half_width < SANE_MIN_HALF_WIDTH {
        FALLBACK_MIN_HALF_WIDTH.max(profile.min_half_width)
    } else {
        profile.min_half_width
    };
    if floor + 1_000 >= profile.half_width {
        return Vec::new();
    }
    vec![floor]
}

/// Terminals for a corridor-necked try: full-width ones where full-free cells reach the end within
/// the rules, plus narrow ones for ends boxed inside a tight gap; for a pad no straight leg
/// leaves at either width, bent ones ([`bent_terminals`]). Only a corridor try takes a bent leg,
/// so no board a corridor does not join sees one.
pub(crate) fn corridor_terminals(
    board: &mut Board,
    full_grid: &RouteGrid,
    narrow_grid: &RouteGrid,
    full_free: &dyn Fn(usize) -> bool,
    id: ItemId,
) -> Vec<Terminal> {
    let mut by_cell: BTreeMap<usize, Option<Leg>> = BTreeMap::new();
    let mut full = terminals_within_rules(board, full_grid, id);
    if full.is_empty() {
        full = bent_terminals(board, full_grid, id);
    }
    for (cell, leg) in full {
        if !narrow_grid.blocked[cell] && full_free(cell) {
            by_cell.insert(cell, leg);
        }
    }
    let mut narrow = terminals(board, narrow_grid, id);
    if narrow.is_empty() {
        narrow = bent_terminals(board, narrow_grid, id);
    }
    for (cell, leg) in narrow {
        if leg.is_none() {
            by_cell.insert(cell, None);
        } else {
            by_cell.entry(cell).or_insert(leg);
        }
    }
    by_cell.into_iter().collect()
}

/// One try at `airline` on `narrow_map`, widening every step whose both ends are free on
/// `full_map` back to `full_map`'s width.
#[allow(clippy::too_many_arguments)]
fn attempt_corridor(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &Options,
    full_map: &ObstacleMap,
    narrow_map: &ObstacleMap,
    grid: &mut RouteGrid,
    full_grid: &mut RouteGrid,
    directional: bool,
    airline: &board::Airline,
) -> Result<(Vec<ItemId>, Vec<Stretch>), Missed> {
    full_map.view_into(airline.net, full_grid);
    narrow_map.view_into(airline.net, grid);
    grid.directional = directional;
    let net = airline.net;
    {
        let _b = board::work::enter(board::work::Bucket::MapPatch);
        let nodes = grid.blocked.len();
        grid.penalty.clear();
        grid.penalty.extend((0..nodes).map(|n| if full_grid.blocked[n] { NARROW_CELL_PENALTY } else { 0 }));
    }
    let full_free = |n: usize| !full_grid.blocked[n];
    let sources = corridor_terminals(board, full_grid, grid, &full_free, airline.from_item);
    if sources.is_empty() {
        return Err(Missed::Start);
    }
    let targets = corridor_terminals(board, full_grid, grid, &full_free, airline.to_item);
    if targets.is_empty() {
        return Err(Missed::End);
    }
    let source_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
    let target_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
    let via_cost = options.net_via_cost.get(&net).copied().unwrap_or(options.via_cost);
    let path = solver.route(grid, &source_cells, &target_cells, via_cost).ok_or(Missed::Path)?;
    let full_hw = full_map.profile.half_width;
    let mut pieces = pieces_corridor(grid, full_hw, &full_free, &path, &sources, &targets).ok_or(Missed::Refused)?;
    narrow_clipped(board, &mut pieces, full_hw, grid.half_width, &|board: &Board, id| grid.violations(board, id).is_empty());
    let stretches = stretches(&pieces, full_hw).ok_or(Missed::Refused)?;
    Ok((lay_pieces(board, grid, pieces).ok_or(Missed::Refused)?, stretches))
}

/// Narrows back each widened track of a corridor that `fits` refuses at `full_hw`: a full-width
/// step between two cells free at full width may still clip a corner beside them, and that step
/// alone goes narrow rather than the whole corridor refused.
pub(crate) fn narrow_clipped(board: &mut Board, pieces: &mut [Item], full_hw: Nm, narrow_hw: Nm, fits: &dyn Fn(&Board, ItemId) -> bool) {
    let _b = board::work::enter(board::work::Bucket::Geometry);
    if narrow_hw >= full_hw {
        return;
    }
    for piece in pieces.iter_mut() {
        if piece.kind != Kind::Track || !matches!(piece.shape, Shape::Capsule { r, .. } if r == full_hw) {
            continue;
        }
        let id = board.insert(piece.clone());
        let ok = fits(board, id);
        board.discard(id);
        if !ok {
            if let Shape::Capsule { r, .. } = &mut piece.shape {
                *r = narrow_hw;
            }
        }
    }
}

/// A corridor's narrowed stretches, in order: each run of tracks narrower than `full_hw`, as its
/// narrowest half width and its length; a via or a full-width track ends one. `None` when one is
/// longer than [`CORRIDOR_STRETCH`].
pub(crate) fn stretches(pieces: &[Item], full_hw: Nm) -> Option<Vec<Stretch>> {
    let mut out: Vec<Stretch> = Vec::new();
    let mut open = false;
    for piece in pieces {
        match (&piece.kind, &piece.shape) {
            (Kind::Track, Shape::Capsule { a, b, r }) if *r < full_hw => {
                let len = a.distance(*b).round() as Nm;
                match out.last_mut() {
                    Some(last) if open => *last = (last.0.min(*r), last.1 + len),
                    _ => out.push((*r, len)),
                }
                open = true;
            }
            _ => open = false,
        }
    }
    out.iter().all(|s| s.1 <= CORRIDOR_STRETCH).then_some(out)
}

/// One try at `airline` on `map`.
fn attempt(board: &mut Board, solver: &mut dyn FieldSolver, options: &Options, map: &ObstacleMap, grid: &mut RouteGrid, directional: bool, airline: &board::Airline) -> Result<Vec<ItemId>, Missed> {
    map.view_into(airline.net, grid);
    grid.directional = directional;
    let sources = terminals(board, grid, airline.from_item);
    if sources.is_empty() {
        return Err(Missed::Start);
    }
    let targets = terminals(board, grid, airline.to_item);
    if targets.is_empty() {
        return Err(Missed::End);
    }
    let source_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
    let target_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
    let via_cost = options.net_via_cost.get(&airline.net).copied().unwrap_or(options.via_cost);
    let path = solver.route(grid, &source_cells, &target_cells, via_cost).ok_or(Missed::Path)?;
    lay(board, grid, &path, &sources, &targets).ok_or(Missed::Refused)
}

/// The finest cell a connection is tried again on, and the most nodes its map may have: a
/// connection across the board is not worth a fine map of the board.
pub(crate) const FINE_CELL_FLOOR: Nm = 10_000;
pub(crate) const FINE_NODES: f64 = 4e6;

/// How far round a narrow run's two ends its map reaches, for the way between them to bend.
const NARROW_RUN_MARGIN: Nm = 2_000_000;
/// How far round a fine corridor try's two ends its window reaches, so a path exiting a header
/// gap has room to step around the connector row.
const CORRIDOR_FINE_MARGIN: Nm = 4_000_000;

/// The part of the board a narrow run of `airline` is routed in.
pub(crate) fn narrow_window(airline: &board::Airline) -> BBox {
    BBox::of([airline.from, airline.to]).grow(NARROW_RUN_MARGIN)
}

/// The part of the board a fine-grid corridor try of `airline` is routed in.
pub(crate) fn corridor_window(airline: &board::Airline) -> BBox {
    BBox::of([airline.from, airline.to]).grow(CORRIDOR_FINE_MARGIN)
}

/// How far apart a connection's ends may be for it to go narrowed the whole way.
const NARROW_RUN_REACH: Nm = 5_000_000;

/// The half width a connection of a net that carries current may run at, the whole way, where
/// its own width finds no way: the narrower of its two pins (pads), never under the net's own
/// minimum (the board's, or a custom rule's for the net), for ends no further apart than
/// [`NARROW_RUN_REACH`]. `None` for a signal, which
/// necks already, for ends that are not both pads, and where that is no narrower.
pub(crate) fn narrow_run(board: &Board, profile: &Profile, airline: &board::Airline) -> Option<Nm> {
    if profile.narrow_half_width < profile.half_width || airline.from.distance(airline.to) > NARROW_RUN_REACH as f64 {
        return None;
    }
    let pin = |id: ItemId| board.item(id).filter(|p| matches!(p.kind, Kind::Pad { .. })).map(crate::grid::pad_half_side);
    // Never under the net's own minimum: a custom rule's width for it holds the whole way, as
    // it does not over a forced neck's millimetre.
    let hw = pin(airline.from_item)?.min(pin(airline.to_item)?).max(profile.min_half_width);
    (hw < profile.half_width).then_some(hw)
}

/// How far around a connection's chosen anchor `terminals` walks joined copper to gather escape
/// vias, stubs, and bridged sibling pads; wider when crossing a kept block pour inside a package.
const CLUSTER_REACH: Nm = 2_500_000;
const POUR_REACH: Nm = 8_000_000;
const CLUSTER_LIMIT: usize = 32;

fn routing_layers(item: &Item, grid: &RouteGrid) -> Vec<usize> {
    grid.layers.iter().enumerate().filter(|(_, l)| item.on(**l)).map(|(r, _)| r).collect()
}

/// The point a leg into the item must reach: a pad's or via's centre, or the nearer end of a track.
pub(crate) fn anchor(item: &Item, near: Point) -> Point {
    match (&item.kind, &item.shape) {
        (_, Shape::Capsule { a, b, .. }) if a == b => *a,
        (Kind::Track, Shape::Capsule { a, b, .. }) => {
            if a.distance(near) <= b.distance(near) {
                *a
            } else {
                *b
            }
        }
        (_, Shape::Polygon(p)) => {
            let n = p.len().max(1) as Nm;
            Point::new(p.iter().map(|q| q.x).sum::<Nm>() / n, p.iter().map(|q| q.y).sum::<Nm>() / n)
        }
        (_, Shape::Capsule { a, .. }) => *a,
    }
}

/// Local non-refilled copper joined to `root`, so a pad's escape stub, via, inner straw, or
/// kept-pour bridge offers its free cells even when connectivity named the pad itself as the
/// airline's nearest item.
pub(crate) fn local_cluster(board: &Board, root: ItemId) -> Vec<ItemId> {
    let Some(root_item) = board.item(root) else { return Vec::new() };
    if !root_item.is_copper() || root_item.net == 0 {
        return vec![root];
    }
    let net = root_item.net;
    let rb = board.bbox(root).unwrap_or_else(|| root_item.shape.bbox());
    let origin = Point::new((rb.min.x + rb.max.x) / 2, (rb.min.y + rb.max.y) / 2);
    let mut seen = BTreeSet::from([root]);
    let mut queue = VecDeque::from([(root, matches!(root_item.kind, Kind::Pour { kept: true, .. }))]);
    let mut out = Vec::new();
    while let Some((cur, via_pour)) = queue.pop_front() {
        let Some(item) = board.item(cur) else { continue };
        if matches!(item.kind, Kind::Pad { .. } | Kind::Track | Kind::Via) {
            out.push(cur);
        }
        let cb = board.bbox(cur).unwrap_or_else(|| item.shape.bbox());
        for l in (0..board.layers.len()).filter(|l| item.on(*l)) {
            for other in board.near(l, cb) {
                if seen.len() >= CLUSTER_LIMIT || seen.contains(&other) {
                    continue;
                }
                let Some(o) = board.item(other) else { continue };
                if o.net != net || !o.is_copper() || o.refilled() {
                    continue;
                }
                let is_pour = matches!(o.kind, Kind::Pour { kept: true, .. });
                let reach = if via_pour || is_pour { POUR_REACH } else { CLUSTER_REACH };
                let ob = board.bbox(other).unwrap_or_else(|| o.shape.bbox());
                if !ob.overlaps(BBox::of([origin]).grow(reach)) || !joined(board, cur, other) {
                    continue;
                }
                seen.insert(other);
                queue.push_back((other, via_pour || is_pour));
            }
        }
    }
    if out.is_empty() {
        out.push(root);
    }
    out
}

/// How many of its cluster's pads, nearest first, a connection may also end at.
const NEAR_PADS: usize = 4;

/// The pads of each of `conn`'s clusters.
pub(crate) fn cluster_pads(board: &Board, conn: &board::Connectivity) -> HashMap<usize, Vec<ItemId>> {
    let mut pads: HashMap<usize, Vec<ItemId>> = HashMap::new();
    for (id, _) in board.items().filter(|(_, it)| matches!(it.kind, Kind::Pad { .. })) {
        if let Some(&c) = conn.cluster.get(&id) {
            pads.entry(c).or_default().push(id);
        }
    }
    pads
}

/// `ends` of `root`, and of the pads of its cluster nearest it, from `pads`. A connection the
/// plough cut ends at the cut, in a channel the cut may have lost; it can as well join its net
/// at a pad, and the stub at the cut is pruned after. Where two give the same cell, the one that
/// needs no leg is kept.
pub(crate) fn ends_with_pads(
    board: &mut Board,
    grid: &RouteGrid,
    root: ItemId,
    pads: &[ItemId],
    ends: &dyn Fn(&mut Board, &RouteGrid, ItemId) -> Vec<Terminal>,
) -> Vec<Terminal> {
    let _b = board::work::enter(board::work::Bucket::Terminals);
    let centre = |board: &Board, id: ItemId| {
        board.bbox(id).map(|b| Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2))
    };
    let Some(origin) = centre(board, root) else { return ends(board, grid, root) };
    let mut near: Vec<(f64, ItemId)> =
        pads.iter().filter(|&&id| id != root).filter_map(|&id| Some((centre(board, id)?.distance(origin), id))).collect();
    near.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut by_cell: BTreeMap<usize, Option<Leg>> = BTreeMap::new();
    for id in std::iter::once(root).chain(near.into_iter().take(NEAR_PADS).map(|(_, id)| id)) {
        for (cell, leg) in ends(board, grid, id) {
            let at = by_cell.entry(cell).or_insert(leg);
            if leg.is_none() {
                *at = None;
            }
        }
    }
    by_cell.into_iter().collect()
}

/// The free cells a path to or from the item may start at, for tools that look at the grid.
pub fn terminal_cells(board: &mut Board, grid: &RouteGrid, id: ItemId) -> Vec<usize> {
    terminals(board, grid, id).into_iter().map(|t| t.0).collect()
}

/// Merges terminals gathered from several items: a cell any of them holds without a leg keeps no
/// leg, else the first leg found to it.
fn merge_terminals(by_cell: &mut BTreeMap<usize, Option<Leg>>, found: Vec<Terminal>) {
    for (cell, leg) in found {
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

/// Terminals of `id` within the net's own width rules, by straight legs: at full width, then
/// further out, then necked as [`Profile::neck_into`] allows.
fn terminals_within_rules(board: &mut Board, grid: &RouteGrid, id: ItemId) -> Vec<Terminal> {
    let cluster = local_cluster(board, id);
    let gather = |board: &mut Board, hw: Nm, cells: Nm, tries: usize| -> Vec<Terminal> {
        let mut by_cell: BTreeMap<usize, Option<Leg>> = BTreeMap::new();
        for &member in &cluster {
            let found = item_terminals(board, grid, member, hw, cells, tries);
            merge_terminals(&mut by_cell, found);
        }
        by_cell.into_iter().collect()
    };
    let found = gather(board, grid.half_width, LEG_CELLS, LEG_CANDIDATES);
    if !found.is_empty() {
        return found;
    }
    // A fine-pitch pad whose neighbours' mask openings, with the grid's margins, take every cell
    // round it may still have room for an exactly checked leg out past them.
    let found = gather(board, grid.half_width, NECK_CELLS, 2 * LEG_CANDIDATES);
    if !found.is_empty() {
        return found;
    }
    let profile = Profile::for_net(board, grid.net);
    let narrow = profile.neck_into(board.item(id));
    if narrow < grid.half_width {
        return gather(board, narrow, NECK_CELLS, 2 * LEG_CANDIDATES);
    }
    Vec::new()
}

/// Terminals of an SMD pad by bent legs, for a pad no straight leg leaves: an off-grid pin of a
/// fine-pitch part whose neighbours' mask openings leave a few microns either side (0.635 mm
/// SSOP pads, 4 um), where every leg slanting to a cell centre clips one, but a leg straight
/// down the pin's own centre line clears them before it steps onto the grid. Only a pad with no
/// hole: a through-hole pin's neighbours are far enough apart for straight legs. At full width,
/// then necked as [`Profile::neck_into`] allows.
fn bent_terminals(board: &mut Board, grid: &RouteGrid, id: ItemId) -> Vec<Terminal> {
    let _b = board::work::enter(board::work::Bucket::Terminals);
    if !board.item(id).is_some_and(|it| matches!(it.kind, Kind::Pad { .. }) && it.hole.is_none()) {
        return Vec::new();
    }
    let found = item_bent_terminals(board, grid, id, grid.half_width, NECK_CELLS, 2 * LEG_CANDIDATES);
    if !found.is_empty() {
        return found;
    }
    let narrow = Profile::for_net(board, grid.net).neck_into(board.item(id));
    if narrow < grid.half_width {
        return item_bent_terminals(board, grid, id, narrow, NECK_CELLS, 2 * LEG_CANDIDATES);
    }
    Vec::new()
}

/// Free cells on the item's local copper cluster, and free cells a short straight leg from its
/// anchors reaches legally: how a path leaves a pad too small or too crowded to hold a free cell
/// of its own, or picks up a pre-laid escape via. Where no full-width leg fits, a leg necked down
/// to the narrowest track the net may have may reach further, as a designer would leave a
/// fine-pitch pad.
pub(crate) fn terminals(board: &mut Board, grid: &RouteGrid, id: ItemId) -> Vec<Terminal> {
    let _b = board::work::enter(board::work::Bucket::Terminals);
    let found = terminals_within_rules(board, grid, id);
    if !found.is_empty() {
        return found;
    }
    // A pin no leg within the rules can leave would stay open: a short leg narrower than the
    // rules allow reaches it instead, as a designer necks a wide net into a fine-pitch part,
    // and the run reports it (`crate::necks`).
    let cluster = local_cluster(board, id);
    let profile = Profile::for_net(board, grid.net);
    let reach = (FORCED_NECK_REACH + grid.h - 1) / grid.h.max(1);
    for hw in profile.forced_necks(board.item(id)).into_iter().filter(|&hw| hw < grid.half_width) {
        let mut by_cell: BTreeMap<usize, Option<Leg>> = BTreeMap::new();
        for &member in &cluster {
            let found = item_terminals(board, grid, member, hw, reach, 2 * LEG_CANDIDATES);
            merge_terminals(&mut by_cell, found);
        }
        if !by_cell.is_empty() {
            return by_cell.into_iter().collect();
        }
    }
    Vec::new()
}

pub(crate) fn item_terminals(board: &mut Board, grid: &RouteGrid, id: ItemId, leg_half_width: Nm, leg_cells: Nm, tries: usize) -> Vec<Terminal> {
    let _b = board::work::enter(board::work::Bucket::Terminals);
    let Some(item) = board.item(id).cloned() else { return Vec::new() };
    let mut out: Vec<Terminal> = Vec::new();
    let bb = item.shape.bbox();
    let anchors: Vec<Point> = match (&item.kind, &item.shape) {
        (Kind::Track, Shape::Capsule { a, b, .. }) if a != b => vec![*a, *b],
        _ => vec![anchor(&item, Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2))],
    };
    let per_anchor = (tries / anchors.len().max(1)).max(8);
    for layer in routing_layers(&item, grid) {
        let (Some(lo), Some(hi)) = (
            grid.cell_at(Point::new(bb.min.x.max(grid.x0), bb.min.y.max(grid.y0))),
            grid.cell_at(Point::new(bb.max.x.min(grid.x0 + grid.h * grid.nx as Nm - 1), bb.max.y.min(grid.y0 + grid.h * grid.ny as Nm - 1))),
        ) else {
            continue;
        };
        // A route ends in a pour deep enough that its end, or a via there, joins it
        // (`board::joined`), not at the pour's edge.
        // An island kept as laid, often a strip no wider than a via, is not refilled: a cell
        // inside it joins it.
        let deep = match item.kind {
            Kind::Pour { .. } if board.is_laid(id) => grid.h as f64 / 2.0,
            Kind::Pour { .. } => grid.half_width.max(grid.via.as_ref().map_or(0, |v| v.radius)) as f64 + grid.h as f64,
            _ => 0.0,
        };
        for j in lo.1..=hi.1 {
            for i in lo.0..=hi.0 {
                let index = grid.index(layer, i, j);
                let centre = grid.centre(i, j);
                if !grid.blocked[index] && item.shape.contains(centre) && (deep == 0.0 || item.shape.depth(centre) >= deep) {
                    out.push((index, None));
                }
            }
        }
        let extent = match &item.kind {
            Kind::Track => 0,
            _ => (bb.max.x - bb.min.x).max(bb.max.y - bb.min.y) / 2,
        };
        let reach = (extent + leg_cells * grid.h) as f64;
        for &from in &anchors {
            let Some(c) = grid.cell_at(from) else { continue };
            let span = (reach / grid.h as f64).ceil() as usize;
            let mut candidates: Vec<(f64, usize, Point)> = Vec::new();
            for j in c.1.saturating_sub(span)..=(c.1 + span).min(grid.ny - 1) {
                for i in c.0.saturating_sub(span)..=(c.0 + span).min(grid.nx - 1) {
                    let index = grid.index(layer, i, j);
                    let centre = grid.centre(i, j);
                    let d = centre.distance(from);
                    if !grid.blocked[index] && d <= reach && !item.shape.contains(centre) {
                        candidates.push((d, index, centre));
                    }
                }
            }
            candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (_, index, centre) in candidates.into_iter().take(per_anchor) {
                let leg = track(item.net, grid.layers[layer], from, centre, leg_half_width);
                if fits(board, grid, leg) {
                    out.push((index, Some(Leg { from, elbow: None, half_width: leg_half_width })));
                }
            }
        }
    }
    out
}

/// Two-piece axis-aligned legs out of `id`: straight along the pad's axis past neighbouring pads
/// or mask openings, then a short step onto a grid cell centre.
fn item_bent_terminals(board: &mut Board, grid: &RouteGrid, id: ItemId, leg_half_width: Nm, leg_cells: Nm, tries: usize) -> Vec<Terminal> {
    let Some(item) = board.item(id).cloned() else { return Vec::new() };
    let mut out: Vec<Terminal> = Vec::new();
    let bb = item.shape.bbox();
    let anchors: Vec<Point> = match (&item.kind, &item.shape) {
        (Kind::Track, Shape::Capsule { a, b, .. }) if a != b => vec![*a, *b],
        _ => vec![anchor(&item, Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2))],
    };
    let per_anchor = (tries / anchors.len().max(1)).max(16);
    let extent = match &item.kind {
        Kind::Track => 0,
        _ => (bb.max.x - bb.min.x).max(bb.max.y - bb.min.y) / 2,
    };
    let reach = (extent + leg_cells * grid.h) as f64;
    for layer in routing_layers(&item, grid) {
        for &from in &anchors {
            let Some(c) = grid.cell_at(from) else { continue };
            let span = (reach / grid.h as f64).ceil() as usize;
            let mut candidates: Vec<(f64, usize, Point)> = Vec::new();
            for j in c.1.saturating_sub(span)..=(c.1 + span).min(grid.ny - 1) {
                for i in c.0.saturating_sub(span)..=(c.0 + span).min(grid.nx - 1) {
                    let index = grid.index(layer, i, j);
                    let centre = grid.centre(i, j);
                    let d = centre.distance(from);
                    if !grid.blocked[index] && d <= reach && !item.shape.contains(centre) && centre.x != from.x && centre.y != from.y {
                        let off_axis = (centre.x - from.x).abs().min((centre.y - from.y).abs()) as f64;
                        candidates.push((off_axis * 4.0 + d, index, centre));
                    }
                }
            }
            candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (_, index, centre) in candidates.into_iter().take(per_anchor) {
                let elbows = if (centre.y - from.y).abs() >= (centre.x - from.x).abs() {
                    [Point::new(from.x, centre.y), Point::new(centre.x, from.y)]
                } else {
                    [Point::new(centre.x, from.y), Point::new(from.x, centre.y)]
                };
                for elbow in elbows {
                    let a = track(item.net, grid.layers[layer], from, elbow, leg_half_width);
                    let b = track(item.net, grid.layers[layer], elbow, centre, leg_half_width);
                    if fits_pair(board, grid, a, b) {
                        out.push((index, Some(Leg { from, elbow: Some(elbow), half_width: leg_half_width })));
                        break;
                    }
                }
            }
        }
    }
    out
}

pub(crate) fn track(net: u32, layer: usize, a: Point, b: Point, half_width: Nm) -> Item {
    Item {
        kind: Kind::Track,
        net,
        layers: 1 << layer,
        shape: Shape::Capsule { a, b, r: half_width },
        hole: None,
        locked: false,
        span: None,
    }
}

/// Whether the item could be laid without breaking a rule; the board is left as it was.
pub(crate) fn fits(board: &mut Board, grid: &RouteGrid, item: Item) -> bool {
    let id = board.insert(item);
    let ok = grid.violations(board, id).is_empty();
    board.discard(id);
    ok
}

fn fits_pair(board: &mut Board, grid: &RouteGrid, a: Item, b: Item) -> bool {
    let ia = board.insert(a);
    let ib = board.insert(b);
    let ok = grid.violations(board, ia).is_empty() && grid.violations(board, ib).is_empty();
    board.discard(ib);
    board.discard(ia);
    ok
}

/// Lays the path: straight runs merged, a via at each layer change, legs to the terminals'
/// anchors where needed. All or nothing: `None`, with the board as it was, if any piece breaks a
/// rule.
pub(crate) fn lay(board: &mut Board, grid: &RouteGrid, path: &GridPath, sources: &[Terminal], targets: &[Terminal]) -> Option<Vec<ItemId>> {
    lay_pieces(board, grid, pieces(grid, path, sources, targets)?)
}

fn lay_pieces(board: &mut Board, grid: &RouteGrid, pieces: Vec<Item>) -> Option<Vec<ItemId>> {
    let _b = board::work::enter(board::work::Bucket::Geometry);
    let mut laid = Vec::new();
    let holes = board.layers.len();
    for piece in pieces {
        // Two routes of one net that change layer at the same cell share the via already there
        // instead of drilling a second hole on top of the first.
        if piece.kind == Kind::Via {
            if let Shape::Capsule { a, r, .. } = piece.shape {
                let hole_r = piece.hole.map_or(0, |h| h.1);
                let reuse = board.near(holes, BBox::of([a])).into_iter().any(|id| {
                    board.item(id).is_some_and(|it| {
                        it.kind == Kind::Via
                            && it.net == piece.net
                            && (it.layers & piece.layers) == piece.layers
                            && matches!(it.shape, Shape::Capsule { a: p, b: q, r: vr } if p == a && q == a && vr == r)
                            && it.hole == Some((a, hole_r))
                    })
                });
                if reuse {
                    continue;
                }
            }
        }
        let id = board.insert(piece);
        laid.push(id);
        if !grid.violations(board, id).is_empty() {
            for id in laid.into_iter().rev() {
                board.discard(id);
            }
            return None;
        }
    }
    Some(laid)
}

/// The tracks and vias a path becomes: straight runs merged, a via at each layer change, legs to
/// the terminals' anchors where needed.
pub(crate) fn pieces(grid: &RouteGrid, path: &GridPath, sources: &[Terminal], targets: &[Terminal]) -> Option<Vec<Item>> {
    let _b = board::work::enter(board::work::Bucket::Geometry);
    let index = |&(l, i, j): &(usize, usize, usize)| grid.index(l, i, j);
    let leg = |terminals: &[Terminal], cell: usize| terminals.iter().find(|t| t.0 == cell && t.1.is_some()).and_then(|t| t.1);
    let first = path.first()?;
    let last = path.last()?;
    let start_leg = if sources.iter().any(|t| t.0 == index(first) && t.1.is_none()) { None } else { leg(sources, index(first)) };
    let end_leg = if targets.iter().any(|t| t.0 == index(last) && t.1.is_none()) { None } else { leg(targets, index(last)) };

    // Corners on each layer: where the direction turns or the layer changes. A full-width leg
    // joins the run; a necked-down one is a track of its own.
    let mut pieces: Vec<Item> = Vec::new();
    let mut run: Vec<Point> = Vec::new();
    let mut run_layer = first.0;
    match start_leg {
        Some(Leg { from: p, elbow: None, half_width: hw }) if hw == grid.half_width => run.push(p),
        Some(Leg { from: p, elbow: Some(e), half_width: hw }) if hw == grid.half_width => {
            run.push(p);
            run.push(e);
        }
        Some(Leg { from: p, elbow: None, half_width: hw }) => {
            pieces.push(track(grid.net, grid.layers[first.0], p, grid.centre(first.1, first.2), hw));
        }
        Some(Leg { from: p, elbow: Some(e), half_width: hw }) => {
            pieces.push(track(grid.net, grid.layers[first.0], p, e, hw));
            pieces.push(track(grid.net, grid.layers[first.0], e, grid.centre(first.1, first.2), hw));
        }
        None => {}
    }
    for (k, cell) in path.iter().enumerate() {
        let p = grid.centre(cell.1, cell.2);
        if cell.0 != run_layer {
            flush(&mut pieces, &run, grid, run_layer, grid.half_width);
            let via = grid.via.as_ref()?;
            pieces.push(Item {
                kind: Kind::Via,
                net: grid.net,
                layers: grid.layers.iter().fold(0, |m, l| m | 1 << l),
                shape: Shape::Capsule { a: p, b: p, r: via.radius },
                hole: Some((p, via.drill_radius)),
                locked: false,
                span: None,
            });
            run = vec![p];
            run_layer = cell.0;
            continue;
        }
        if (k > 0 || run.len() >= 2) && run.len() >= 2 {
            let (a, b) = (run[run.len() - 2], run[run.len() - 1]);
            if cross(a, b, p) == 0 && (b.x - a.x) * (p.x - b.x) + (b.y - a.y) * (p.y - b.y) > 0 {
                run.pop();
            }
        }
        run.push(p);
    }
    let mut neck: Vec<Item> = Vec::new();
    match end_leg {
        Some(Leg { from: p, elbow: None, half_width: hw }) if hw == grid.half_width => run.push(p),
        Some(Leg { from: p, elbow: Some(e), half_width: hw }) if hw == grid.half_width => {
            if run.len() >= 2 {
                let (a, b) = (run[run.len() - 2], run[run.len() - 1]);
                if cross(a, b, e) == 0 && (b.x - a.x) * (e.x - b.x) + (b.y - a.y) * (e.y - b.y) > 0 {
                    run.pop();
                }
            }
            run.push(e);
            run.push(p);
        }
        Some(Leg { from: p, elbow: None, half_width: hw }) => {
            neck.push(track(grid.net, grid.layers[last.0], grid.centre(last.1, last.2), p, hw));
        }
        Some(Leg { from: p, elbow: Some(e), half_width: hw }) => {
            neck.push(track(grid.net, grid.layers[last.0], grid.centre(last.1, last.2), e, hw));
            neck.push(track(grid.net, grid.layers[last.0], e, p, hw));
        }
        None => {}
    }
    flush(&mut pieces, &run, grid, run_layer, grid.half_width);
    pieces.extend(neck);
    // A one-cell path where start and end share a free cell without legs lays no copper and
    // leaves disconnected items disconnected; refusing it keeps callers from counting a no-op.
    (!pieces.is_empty()).then_some(pieces)
}

/// As [`pieces`], widening any step whose both ends pass `full_free` to `full_hw` while keeping
/// steps into or through narrow cells at `grid.half_width`.
pub(crate) fn pieces_corridor(
    grid: &RouteGrid,
    full_hw: Nm,
    full_free: &dyn Fn(usize) -> bool,
    path: &GridPath,
    sources: &[Terminal],
    targets: &[Terminal],
) -> Option<Vec<Item>> {
    let _b = board::work::enter(board::work::Bucket::Geometry);
    let index = |&(l, i, j): &(usize, usize, usize)| grid.index(l, i, j);
    let leg = |terminals: &[Terminal], cell: usize| terminals.iter().find(|t| t.0 == cell && t.1.is_some()).and_then(|t| t.1);
    let first = path.first()?;
    let last = path.last()?;
    let start_leg = if sources.iter().any(|t| t.0 == index(first) && t.1.is_none()) { None } else { leg(sources, index(first)) };
    let end_leg = if targets.iter().any(|t| t.0 == index(last) && t.1.is_none()) { None } else { leg(targets, index(last)) };

    let mut pieces: Vec<Item> = Vec::new();
    let mut run: Vec<Point> = Vec::new();
    let mut run_layer = first.0;
    let mut run_hw = grid.half_width;
    let push_seg = |pieces: &mut Vec<Item>, run: &mut Vec<Point>, run_hw: &mut Nm, layer: usize, a: Point, b: Point, hw: Nm| {
        if a == b {
            return;
        }
        if run.is_empty() {
            *run_hw = hw;
            run.push(a);
            run.push(b);
            return;
        }
        if *run_hw != hw || run.last() != Some(&a) {
            flush(pieces, run, grid, layer, *run_hw);
            run.clear();
            *run_hw = hw;
            run.push(a);
            run.push(b);
            return;
        }
        if run.len() >= 2 {
            let (p0, p1) = (run[run.len() - 2], run[run.len() - 1]);
            if cross(p0, p1, b) == 0 && (p1.x - p0.x) * (b.x - p1.x) + (p1.y - p0.y) * (b.y - p1.y) > 0 {
                run.pop();
            }
        }
        run.push(b);
    };

    let first_pt = grid.centre(first.1, first.2);
    if let Some(leg) = start_leg {
        match leg.elbow {
            Some(e) => {
                push_seg(&mut pieces, &mut run, &mut run_hw, run_layer, leg.from, e, leg.half_width);
                push_seg(&mut pieces, &mut run, &mut run_hw, run_layer, e, first_pt, leg.half_width);
            }
            None => push_seg(&mut pieces, &mut run, &mut run_hw, run_layer, leg.from, first_pt, leg.half_width),
        }
    }
    for w in path.windows(2) {
        let (a, b) = (w[0], w[1]);
        let pa = grid.centre(a.1, a.2);
        let pb = grid.centre(b.1, b.2);
        if a.0 != b.0 {
            flush(&mut pieces, &run, grid, run_layer, run_hw);
            run.clear();
            let via = grid.via.as_ref()?;
            pieces.push(Item {
                kind: Kind::Via,
                net: grid.net,
                layers: grid.layers.iter().fold(0, |m, l| m | 1 << l),
                shape: Shape::Capsule { a: pa, b: pa, r: via.radius },
                hole: Some((pa, via.drill_radius)),
                locked: false,
                span: None,
            });
            run_layer = b.0;
            continue;
        }
        let step_hw = if full_free(index(&a)) && full_free(index(&b)) { full_hw } else { grid.half_width };
        push_seg(&mut pieces, &mut run, &mut run_hw, run_layer, pa, pb, step_hw);
    }
    let last_pt = grid.centre(last.1, last.2);
    if let Some(leg) = end_leg {
        match leg.elbow {
            Some(e) => {
                push_seg(&mut pieces, &mut run, &mut run_hw, run_layer, last_pt, e, leg.half_width);
                push_seg(&mut pieces, &mut run, &mut run_hw, run_layer, e, leg.from, leg.half_width);
            }
            None => push_seg(&mut pieces, &mut run, &mut run_hw, run_layer, last_pt, leg.from, leg.half_width),
        }
    }
    flush(&mut pieces, &run, grid, run_layer, run_hw);
    (!pieces.is_empty()).then_some(pieces)
}

fn cross(a: Point, b: Point, c: Point) -> i128 {
    (b.x - a.x) as i128 * (c.y - a.y) as i128 - (b.y - a.y) as i128 * (c.x - a.x) as i128
}

fn flush(pieces: &mut Vec<Item>, run: &[Point], grid: &RouteGrid, layer: usize, half_width: Nm) {
    for w in run.windows(2) {
        if w[0] != w[1] {
            pieces.push(track(grid.net, grid.layers[layer], w[0], w[1], half_width));
        }
    }
}

/// The routed copper as edits for the board file.
/// Adds to `laid` the file's copper still on the board whose text is going (its span is in
/// `removed`), so that it is written back as the router's own: a KiCad arc is several chords
/// sharing one span, and when one chord was ripped or moved the arc's whole text goes, the other
/// chords with it unless written anew. Call before [`edits`].
pub fn keep_orphans(board: &Board, removed: &[kicad_pcb::Span], laid: &mut Vec<ItemId>) {
    let gone: std::collections::HashSet<(usize, usize)> = removed.iter().map(|s| (s.start, s.end)).collect();
    let mine: std::collections::HashSet<ItemId> = laid.iter().copied().collect();
    let orphans: Vec<ItemId> = board
        .items()
        .filter(|(id, it)| matches!(it.kind, Kind::Track | Kind::Via) && it.span.is_some_and(|s| gone.contains(&(s.start, s.end))) && !mine.contains(id))
        .map(|(id, _)| id)
        .collect();
    laid.extend(orphans);
}

/// The arcs [`crate::round_corners`] laid, as the writer takes them.
pub fn arc_edits(board: &Board, arcs: &[crate::RoundArc]) -> Vec<kicad_pcb::NewArc> {
    let metres = |p: Point| [p.x as f64 * 1e-9, p.y as f64 * 1e-9];
    arcs.iter()
        .map(|arc| kicad_pcb::NewArc {
            start: metres(arc.start),
            mid: metres(arc.mid),
            end: metres(arc.end),
            width: arc.width as f64 * 1e-9,
            layer: board.layers[arc.layers.trailing_zeros() as usize].clone(),
            net: arc.net,
            locked: false,
        })
        .collect()
}

pub fn edits(board: &Board, laid: &[ItemId]) -> (Vec<kicad_pcb::NewTrack>, Vec<kicad_pcb::NewVia>) {
    let metres = |p: Point| [p.x as f64 * 1e-9, p.y as f64 * 1e-9];
    let mut tracks = Vec::new();
    let mut vias = Vec::new();
    for item in laid.iter().filter_map(|id| board.item(*id)) {
        match (&item.kind, &item.shape) {
            (Kind::Track, Shape::Capsule { a, b, r }) => {
                let layer = (0..board.layers.len()).find(|l| item.on(*l)).map_or_else(String::new, |l| board.layers[l].clone());
                tracks.push(kicad_pcb::NewTrack { start: metres(*a), end: metres(*b), width: 2.0 * *r as f64 * 1e-9, layer, net: item.net, locked: item.locked });
            }
            (Kind::Via, Shape::Capsule { a, r, .. }) => {
                let on: Vec<usize> = (0..board.layers.len()).filter(|l| item.on(*l)).collect();
                let (Some(&first), Some(&last)) = (on.first(), on.last()) else { continue };
                // The writer has no blind or buried via, and nothing lays one: say so rather than
                // write a through via's tokens for it.
                assert!(first == 0 && last + 1 == board.layers.len(), "a via from {} to {} does not go through the board", board.layers[first], board.layers[last]);
                vias.push(kicad_pcb::NewVia {
                    at: metres(*a),
                    size: 2.0 * *r as f64 * 1e-9,
                    drill: 2.0 * item.hole.map_or(0, |h| h.1) as f64 * 1e-9,
                    layers: [board.layers[first].clone(), board.layers[last].clone()],
                    net: item.net,
                    locked: item.locked,
                });
            }
            _ => {}
        }
    }
    (tracks, vias)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_open_joins_the_pins_either_side_of_a_walled_pin() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"A\")
             (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1)))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let mm = |v: f64| (v * 1e6) as Nm;
        let pad = |x, y| Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(mm(x), mm(y)), b: Point::new(mm(x), mm(y)), r: mm(0.3) },
            hole: None,
            locked: true,
            span: None,
        };
        let left = board.insert(pad(2.0, 10.0));
        board.insert(pad(10.0, 10.0));
        let right = board.insert(pad(18.0, 10.0));
        // The middle pin is boxed in by locked copper: both airlines of the closest-pairs tree run
        // into it, while left and right can meet round the box.
        for (a, b) in [((8.5, 8.5), (11.5, 8.5)), ((11.5, 8.5), (11.5, 11.5)), ((11.5, 11.5), (8.5, 11.5)), ((8.5, 11.5), (8.5, 8.5))] {
            board.insert(Item {
                kind: Kind::Track,
                net: 0,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(mm(a.0), mm(a.1)), b: Point::new(mm(b.0), mm(b.1)), r: mm(0.2) },
                hole: None,
                locked: true,
                span: None,
            });
        }
        assert_eq!(connectivity_without_pours(&board).airlines.len(), 2);
        let stats = route_open(&mut board, &mut CpuSolver, &Options::default(), &mut |_| {});
        assert_eq!(stats.routed, 1);
        let conn = connectivity_without_pours(&board);
        assert_eq!(conn.airlines.len(), 1, "only the walled pin is left open");
        assert_eq!(conn.cluster[&left], conn.cluster[&right]);
        for id in stats.laid {
            assert!(board::check_item(&board, id).is_empty());
        }
    }

    /// Net A's two pads 12 mm apart either side of a wall of unnetted locked copper with an 0.80
    /// mm gap at y = 7.0: a 0.50 mm track and 2 * 0.25 mm clearance (1.00 mm) cannot pass, the
    /// 0.20 mm minimum track (0.70 mm) can. With `power`, A is of a class wider than the default.
    fn gap_board(power: bool, pad_r: f64) -> Board {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal))
             (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
             (gr_rect (start 0 0) (end 16 14) (layer \"Edge.Cuts\") (width 0.1)))",
        )
        .unwrap();
        let mut rules = kicad_pcb::Rules::parse("{}").unwrap();
        rules.min_track_width = 0.0002;
        rules.min_clearance = 0.00025;
        rules.min_copper_edge_clearance = 0.0003;
        rules.classes[0].track_width = if power { 0.0003 } else { 0.0005 };
        rules.classes[0].clearance = 0.00025;
        if power {
            let mut class = rules.classes[0].clone();
            class.name = "Power".into();
            class.track_width = 0.0005;
            rules.classes.push(class);
            rules.assignments.push(("A".into(), "Power".into()));
        }
        let mut board = Board::new(&pcb, &copper, rules);
        let mm = |v: f64| (v * 1e6) as Nm;
        for x in [2.0, 14.0] {
            board.insert(Item {
                kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
                net: 1,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(mm(x), mm(7.0)), b: Point::new(mm(x), mm(7.0)), r: mm(pad_r) },
                hole: None,
                locked: true,
                span: None,
            });
        }
        for (y0, y1) in [(0.4, 6.2), (7.8, 13.6)] {
            board.insert(Item {
                kind: Kind::Track,
                net: 0,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(mm(8.0), mm(y0)), b: Point::new(mm(8.0), mm(y1)), r: mm(0.4) },
                hole: None,
                locked: true,
                span: None,
            });
        }
        board
    }

    #[test]
    fn corridor_narrows_only_through_pin_gap_and_widens_outside() {
        let mm = |v: f64| (v * 1e6) as Nm;
        // Off, as every caller but the stages that ask has it, the gap stays shut.
        let mut board = gap_board(false, 0.4);
        let stats = route_open(&mut board, &mut CpuSolver, &Options::default(), &mut |_| {});
        assert_eq!((stats.routed, stats.corridor), (0, 0), "{stats:?}");
        let mut board = gap_board(false, 0.4);
        let stats = route_open(&mut board, &mut CpuSolver, &Options { corridor: true, ..Default::default() }, &mut |_| {});
        assert_eq!(stats.routed, 1, "{stats:?}");
        assert_eq!(stats.corridor, 1, "{stats:?}");
        assert!(connectivity_without_pours(&board).airlines.is_empty());
        assert!(crate::necks::necks(&board, &stats.laid).is_empty());
        // Every narrowed stretch is told, at the minimum track and no longer than the cap.
        assert!(!stats.corridors.is_empty(), "{stats:?}");
        for &(_, _, width, length) in &stats.corridors {
            assert_eq!(width, mm(0.2));
            assert!(length > 0 && length <= CORRIDOR_STRETCH, "{length}");
        }
        let mut saw_narrow = false;
        let mut saw_full = false;
        for &id in &stats.laid {
            assert!(board::check_item(&board, id).is_empty(), "{:?}", board.item(id));
            if let Some(Shape::Capsule { a, b, r }) = board.item(id).map(|it| &it.shape) {
                if a != b && *r == mm(0.1) {
                    saw_narrow = true;
                }
                if a != b && *r == mm(0.25) {
                    saw_full = true;
                }
            }
        }
        assert!(saw_narrow && saw_full, "expected narrow in gap and full width outside: {:?}", stats.laid);
    }

    #[test]
    fn corridor_keeps_a_current_net_to_its_pins_and_short_runs() {
        let mm = |v: f64| (v * 1e6) as Nm;
        let airline = |board: &Board| connectivity_without_pours(board).airlines.into_iter().next().unwrap();
        // A signal narrows to its minimum.
        let board = gap_board(false, 0.4);
        assert_eq!(corridor_widths(&board, &Profile::for_net(&board, 1), &airline(&board)), vec![mm(0.1)]);
        // A net of a class wider than the default, 12 mm between its pins, keeps its width: the
        // gap stays shut to it even with corridors on.
        let mut board = gap_board(true, 0.15);
        let profile = Profile::for_net(&board, 1);
        assert_eq!(profile.half_width, mm(0.25));
        assert!(corridor_widths(&board, &profile, &airline(&board)).is_empty());
        let stats = route_open(&mut board, &mut CpuSolver, &Options { corridor: true, ..Default::default() }, &mut |_| {});
        assert_eq!((stats.routed, stats.corridor), (0, 0), "{stats:?}");
        // Its pins 3 mm apart, it may narrow to their width and no thinner.
        let mut short = airline(&board);
        short.to = Point::new(short.from.x + mm(3.0), short.from.y);
        assert_eq!(corridor_widths(&board, &profile, &short), vec![mm(0.15)]);
        // Pins as wide as its track leave it nothing to narrow to.
        let board = gap_board(true, 0.3);
        let mut short = airline(&board);
        short.to = Point::new(short.from.x + mm(3.0), short.from.y);
        assert!(corridor_widths(&board, &Profile::for_net(&board, 1), &short).is_empty());
    }

    /// A solver that stops the run at its first search, as a deadline passing then would.
    struct Stopping(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl FieldSolver for Stopping {
        fn solve(&mut self, grid: &RouteGrid, sources: &[usize], via_cost: u32) -> Vec<u32> {
            CpuSolver.solve(grid, sources, via_cost)
        }

        fn route(&mut self, grid: &RouteGrid, sources: &[usize], targets: &[usize], via_cost: u32) -> Option<GridPath> {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
            CpuSolver.route(grid, sources, targets, via_cost)
        }
    }

    #[test]
    fn corridor_cut_short_by_the_deadline_counts_the_connection_failed() {
        // The deadline passes before the corridor's map is built: the corridor is given up, and
        // the connection is counted failed as any other, not dropped from the stats unsaid.
        let mut board = gap_board(false, 0.4);
        let deadline = crate::clock::Deadline::default();
        let mut solver = Stopping(deadline.cancel.clone());
        let stats = route_open(&mut board, &mut solver, &Options { corridor: true, deadline, ..Default::default() }, &mut |_| {});
        assert_eq!((stats.routed, stats.corridor, stats.failed), (0, 0, 1), "{stats:?}");
        assert_eq!(stats.failures.len(), 1, "{stats:?}");
        assert!(stats.laid.is_empty());
    }

    #[test]
    fn bent_leg_escapes_off_grid_pad_between_tight_masks() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6))
             (layers (0 \"F.Cu\" signal))
             (setup (pad_to_mask_clearance 0.254))
             (net 0 \"\") (net 1 \"A\") (net 2 \"B\") (net 3 \"C\")
             (gr_rect (start 0 0) (end 12 14) (layer \"Edge.Cuts\") (width 0.1))
             (footprint \"SSOP\" (layer \"F.Cu\") (at 5.0475 5.0)
               (pad \"1\" smd rect (at -0.635 0) (size 0.35 0.9) (layers \"F.Cu\" \"F.Mask\") (net 2 \"B\"))
               (pad \"2\" smd rect (at 0 0) (size 0.35 0.9) (layers \"F.Cu\" \"F.Mask\") (net 1 \"A\"))
               (pad \"3\" smd rect (at 0.635 0) (size 0.35 0.9) (layers \"F.Cu\" \"F.Mask\") (net 3 \"C\")))
             (footprint \"DST\" (layer \"F.Cu\") (at 5.05 10.5)
               (pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\" \"F.Mask\") (net 1 \"A\"))))",
        )
        .unwrap();
        let mut rules = kicad_pcb::Rules::parse("{}").unwrap();
        rules.min_track_width = 0.000374;
        rules.min_clearance = 0.0002;
        rules.classes[0].track_width = 0.000376;
        rules.classes[0].clearance = 0.0002;
        let mut board = Board::new(&pcb, &copper, rules);
        // The SSOP's middle pin, 22.5 um off the cells' columns, leaves by legs straight down its
        // own centre line before they step onto a free cell.
        let pin = board.items().find(|(_, it)| it.net == 1 && matches!(it.kind, Kind::Pad { .. }) && it.shape.bbox().max.y < 6_000_000).map(|(id, _)| id).unwrap();
        let item = board.item(pin).unwrap().clone();
        let bb = item.shape.bbox();
        let from = anchor(&item, Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2));
        let map = ObstacleMap::new(&board, Profile::for_net(&board, 1), 50_000, false);
        let grid = map.view(1);
        let bent = bent_terminals(&mut board, &grid, pin);
        assert!(!bent.is_empty());
        for (cell, leg) in bent {
            let leg = leg.expect("a bent terminal has a leg");
            let elbow = leg.elbow.expect("a bent leg has an elbow");
            assert!(!grid.blocked[cell]);
            assert_eq!(leg.from, from);
            assert!(elbow.x == from.x || elbow.y == from.y, "{elbow:?} off {from:?}'s axes");
        }
        // A corridor try, which alone takes bent legs, routes the board within the rules.
        let stats = route_open(&mut board, &mut CpuSolver, &Options { fine: false, corridor: true, ..Default::default() }, &mut |_| {});
        assert_eq!(stats.routed, 1, "{stats:?}");
        assert!(connectivity_without_pours(&board).airlines.is_empty());
        for id in stats.laid {
            assert!(board::check_item(&board, id).is_empty(), "{:?}", board.item(id));
        }
    }
}
