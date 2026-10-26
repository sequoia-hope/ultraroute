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
/// cover each node as each class sees them (classes in name order), and the present price per
/// net. `generation` changes from one round to the next.
pub struct RoundPrices<'a> {
    pub generation: u64,
    pub history: &'a [u32],
    pub counts: Vec<&'a [u16]>,
    pub present: f64,
}

/// One connection of a round to price and route: its grid without prices, the class whose
/// counts it sees, its weight, the nodes its own net covers, and its ends.
pub struct PricedJob<'a> {
    pub grid: &'a RouteGrid,
    pub class: usize,
    pub weight: f64,
    pub own: Vec<u32>,
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
    pub max_seconds: Option<f64>,
    /// Nets whose open connections are routed; every net when `None`.
    pub nets: Option<HashSet<u32>>,
    /// Keep other nets out of poured copper instead of routing through it to be refilled.
    pub pours_block: bool,
    /// Reorders connections of nearly equal length, for a search over many runs; 0 keeps the
    /// shortest-first order exact.
    pub seed: u64,
    /// Set from elsewhere to stop at the next connection.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub log: Option<Log>,
}

impl Default for Options {
    fn default() -> Options {
        Options { cell: 50_000, via_cost: 80, max_seconds: None, nets: None, pours_block: false, seed: 0, cancel: None, log: None }
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
}

/// A place a path may start or end: a free cell, and, when the cell's centre is not on the
/// terminal's copper, the point a straight leg must reach from it and the leg's half-width.
pub(crate) type Terminal = (usize, Option<(Point, Nm)>);

/// Routes the board's open connections, shortest first, until a round joins nothing more.
/// `watch` sees the board after every connection laid.
pub fn route_open(board: &mut Board, solver: &mut dyn FieldSolver, options: &Options, watch: &mut dyn FnMut(&Board)) -> Stats {
    let started = crate::clock::Instant::now();
    let out_of_time = || {
        options.max_seconds.is_some_and(|limit| started.elapsed().as_secs_f64() > limit)
            || options.cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    };
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
        pours_block: options.pours_block,
    };
    let mut given_up: BTreeSet<(ItemId, ItemId)> = BTreeSet::new();
    loop {
        // Unless pours are kept as planes, they will be refilled around what is routed, and may
        // no longer join what they join now; so every connection is made in tracks.
        let mut open = if options.pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines;
        if let Some(only) = &options.nets {
            open.retain(|a| only.contains(&a.net));
        }
        open.sort_by(|a, b| a.from.distance(a.to).total_cmp(&b.from.distance(b.to)));
        if options.seed != 0 {
            // Shortest first still, roughly: shuffled within runs of eight.
            for (k, chunk) in open.chunks_mut(8).enumerate() {
                crate::jitter::shuffle(chunk, options.seed.wrapping_add(k as u64));
            }
        }
        open.retain(|a| !given_up.contains(&(a.from_item, a.to_item)));
        if open.is_empty() {
            break;
        }
        let routed_before = stats.routed;
        for airline in open {
            if out_of_time() {
                return stats;
            }
            let net_name = board.net_name(airline.net).to_string();
            tell(
                &options.log,
                "routing",
                || format!("{net_name}: {} to {}", item_name(board, airline.from_item), item_name(board, airline.to_item)),
                Some(Focus::Connection { net: airline.net, from: airline.from, to: airline.to }),
            );
            let profile = Profile::for_net(board, airline.net);
            let map = maps.entry(profile.class.clone()).or_insert_with(|| ObstacleMap::new(board, profile, options.cell, options.pours_block));
            map.view_into(airline.net, &mut grid);
            let sources = terminals(board, &grid, airline.from_item);
            let targets = terminals(board, &grid, airline.to_item);
            let laid = if sources.is_empty() || targets.is_empty() {
                stats.no_terminal += 1;
                stats.failures.push((airline.from_item, airline.to_item, if sources.is_empty() { "no terminal at the start" } else { "no terminal at the end" }));
                None
            } else {
                let source_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
                let target_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
                match solver.route(&grid, &source_cells, &target_cells, options.via_cost) {
                    None => {
                        stats.no_path += 1;
                        stats.failures.push((airline.from_item, airline.to_item, "no path"));
                        None
                    }
                    Some(path) => {
                        let laid = lay(board, &grid, &path, &sources, &targets);
                        stats.refused += laid.is_none() as usize;
                        laid
                    }
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
                    stats.vias += items.iter().filter(|id| board.item(**id).is_some_and(|it| it.kind == Kind::Via)).count();
                    for map in maps.values_mut() {
                        for &id in &items {
                            map.stamp(board, id);
                        }
                    }
                    stats.laid.extend(items);
                    watch(board);
                }
                None => {
                    stats.failed += 1;
                    given_up.insert((airline.from_item, airline.to_item));
                }
            }
        }
        if stats.routed == routed_before {
            break;
        }
    }
    stats
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

/// The free cells a path to or from the item may start at, for tools that look at the grid.
pub fn terminal_cells(board: &mut Board, grid: &RouteGrid, id: ItemId) -> Vec<usize> {
    terminals(board, grid, id).into_iter().map(|t| t.0).collect()
}

/// Free cells on the item's local copper cluster, and free cells a short straight leg from its
/// anchors reaches legally: how a path leaves a pad too small or too crowded to hold a free cell
/// of its own, or picks up a pre-laid escape via. Where no full-width leg fits, a leg necked down
/// to the board's narrowest track may reach further, as a designer would leave a fine-pitch pad.
pub(crate) fn terminals(board: &mut Board, grid: &RouteGrid, id: ItemId) -> Vec<Terminal> {
    let cluster = local_cluster(board, id);
    let gather = |board: &mut Board, hw: Nm, cells: Nm, tries: usize| -> Vec<Terminal> {
        let mut by_cell: BTreeMap<usize, Option<(Point, Nm)>> = BTreeMap::new();
        for &member in &cluster {
            for (cell, leg) in item_terminals(board, grid, member, hw, cells, tries) {
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
    let found = gather(board, grid.half_width, LEG_CELLS, LEG_CANDIDATES);
    if !found.is_empty() {
        return found;
    }
    let narrow = (board::nm(board.rules.min_track_width) / 2).max(1);
    if narrow >= grid.half_width {
        return found;
    }
    gather(board, narrow, NECK_CELLS, 2 * LEG_CANDIDATES)
}

pub(crate) fn item_terminals(board: &mut Board, grid: &RouteGrid, id: ItemId, leg_half_width: Nm, leg_cells: Nm, tries: usize) -> Vec<Terminal> {
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
        for j in lo.1..=hi.1 {
            for i in lo.0..=hi.0 {
                let index = grid.index(layer, i, j);
                if !grid.blocked[index] && item.shape.contains(grid.centre(i, j)) {
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
                    out.push((index, Some((from, leg_half_width))));
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

/// Lays the path: straight runs merged, a via at each layer change, legs to the terminals'
/// anchors where needed. All or nothing: `None`, with the board as it was, if any piece breaks a
/// rule.
pub(crate) fn lay(board: &mut Board, grid: &RouteGrid, path: &GridPath, sources: &[Terminal], targets: &[Terminal]) -> Option<Vec<ItemId>> {
    let pieces = pieces(grid, path, sources, targets)?;
    let mut laid = Vec::new();
    for piece in pieces {
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
        Some((p, hw)) if hw == grid.half_width => run.push(p),
        Some((p, hw)) => pieces.push(track(grid.net, grid.layers[first.0], p, grid.centre(first.1, first.2), hw)),
        None => {}
    }
    for (k, cell) in path.iter().enumerate() {
        let p = grid.centre(cell.1, cell.2);
        if cell.0 != run_layer {
            flush(&mut pieces, &run, grid, run_layer);
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
        if k > 0 && run.len() >= 2 {
            let (a, b) = (run[run.len() - 2], run[run.len() - 1]);
            if cross(a, b, p) == 0 && (b.x - a.x) * (p.x - b.x) + (b.y - a.y) * (p.y - b.y) > 0 {
                run.pop();
            }
        }
        run.push(p);
    }
    let mut neck = None;
    match end_leg {
        Some((p, hw)) if hw == grid.half_width => run.push(p),
        Some((p, hw)) => neck = Some(track(grid.net, grid.layers[last.0], grid.centre(last.1, last.2), p, hw)),
        None => {}
    }
    flush(&mut pieces, &run, grid, run_layer);
    pieces.extend(neck);
    Some(pieces)
}

fn cross(a: Point, b: Point, c: Point) -> i128 {
    (b.x - a.x) as i128 * (c.y - a.y) as i128 - (b.y - a.y) as i128 * (c.x - a.x) as i128
}

fn flush(pieces: &mut Vec<Item>, run: &[Point], grid: &RouteGrid, layer: usize) {
    for w in run.windows(2) {
        if w[0] != w[1] {
            pieces.push(track(grid.net, grid.layers[layer], w[0], w[1], grid.half_width));
        }
    }
}

/// The routed copper as edits for the board file.
pub fn edits(board: &Board, laid: &[ItemId]) -> (Vec<kicad_pcb::NewTrack>, Vec<kicad_pcb::NewVia>) {
    let metres = |p: Point| [p.x as f64 * 1e-9, p.y as f64 * 1e-9];
    let mut tracks = Vec::new();
    let mut vias = Vec::new();
    for item in laid.iter().filter_map(|id| board.item(*id)) {
        match (&item.kind, &item.shape) {
            (Kind::Track, Shape::Capsule { a, b, r }) => {
                let layer = (0..board.layers.len()).find(|l| item.on(*l)).map_or_else(String::new, |l| board.layers[l].clone());
                tracks.push(kicad_pcb::NewTrack { start: metres(*a), end: metres(*b), width: 2.0 * *r as f64 * 1e-9, layer, net: item.net });
            }
            (Kind::Via, Shape::Capsule { a, r, .. }) => {
                let on: Vec<usize> = (0..board.layers.len()).filter(|l| item.on(*l)).collect();
                let (Some(&first), Some(&last)) = (on.first(), on.last()) else { continue };
                vias.push(kicad_pcb::NewVia {
                    at: metres(*a),
                    size: 2.0 * *r as f64 * 1e-9,
                    drill: 2.0 * item.hole.map_or(0, |h| h.1) as f64 * 1e-9,
                    layers: [board.layers[first].clone(), board.layers[last].clone()],
                    net: item.net,
                });
            }
            _ => {}
        }
    }
    (tracks, vias)
}
