//! Topological routing on the core board model. Each layer is a Delaunay triangulation of the
//! obstacles' outlines, sampled closely enough that no wire fits between two neighbouring samples
//! of one obstacle, so the outlines are walls. A connection searches the triangles for a channel
//! wide enough for it, changing layers where a via fits; its copper is the shortest path through
//! that channel that keeps the clearances (the funnel algorithm, each bend pushed out round the
//! corner it wraps), which is a rubber band pulled taut. Committed copper joins the
//! triangulations, so later wires go round it. A connection that finds no way may then cross
//! other connections' copper at a price that rises each time a connection is ripped; what it
//! crosses is ripped and rerouted, and the board that joins most connections is kept.
//!
//! Every piece is judged by the board model's own check before it stays, so the samples and
//! reaches only have to find good channels, not prove them legal.

pub mod funnel;
pub mod outline;
pub mod world;

use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};

use board::{check_item, nm, Board, Item, ItemId, Kind, Nm, Point, Shape};
use grid::{item_name, Activity, Focus, Log, Profile};

use funnel::{Bend, Gate, Pt};
use outline::{pt, round};
use world::{Conns, Face, Rule, World};

/// The longest the grid may take to route what one rip knocked loose, and to rip up for it, inside
/// the stage's own deadline.
const REPAIR_ROUTE_SECONDS: f64 = 3.0;
const REPAIR_RIP_SECONDS: f64 = 2.5;

#[derive(Debug, Clone)]
pub struct Options {
    /// Spacing of the samples along each obstacle's outline, in nanometres. It must stay below
    /// twice the narrowest reach, or wires slip between the samples of one obstacle.
    pub sample: Nm,
    /// What a via costs, as a length in nanometres.
    pub via_cost: Nm,
    /// When to stop, between connections and between orders.
    pub deadline: grid::clock::Deadline,
    /// Triangles one search may expand before it gives the connection up.
    pub max_expansions: usize,
    /// Orders to try the connections in; the board keeps the one that routes most.
    pub orders: usize,
    /// What crossing another connection's copper costs, as a length in nanometres, before that
    /// connection's history of being ripped multiplies it.
    pub rip_cost: Nm,
    /// Rounds of rip-up over the connections still open; 0 turns rip-up off.
    pub rip_passes: usize,
    /// Connections to route, each as `(from.min(to), from.max(to))`; every open connection when
    /// `None`.
    pub only: Option<HashSet<(ItemId, ItemId)>>,
    /// Nets whose open connections are routed; every net when `None`.
    pub nets: Option<HashSet<u32>>,
    /// Keep other nets out of poured copper instead of routing through it to be refilled.
    pub pours_block: bool,
    pub log: Option<Log>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            sample: 100_000,
            via_cost: 2_000_000,
            deadline: Default::default(),
            max_expansions: 400_000,
            orders: 1,
            rip_cost: 1_000_000,
            rip_passes: 6,
            only: None,
            nets: None,
            pours_block: false,
            log: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    pub connections: usize,
    pub routed: usize,
    /// No channel wide enough was found.
    pub no_channel: usize,
    /// A channel was found but its copper broke a rule or didn't join the two items.
    pub rejected: usize,
    /// Rip-ups kept: a connection routed through others' copper, theirs rerouted.
    pub rip_ups: usize,
    pub vias: usize,
    /// Every track and via laid, for writing back.
    pub laid: Vec<ItemId>,
}

fn tell(log: &Option<Log>, phase: &'static str, text: impl FnOnce() -> String, focus: Option<Focus>) {
    if let Some(Log(log)) = log {
        log(Activity { phase, text: text(), focus });
    }
}

fn filtered_gaps(board: &Board, options: &Options) -> Vec<(f64, ItemId, ItemId)> {
    let pair = |a: ItemId, b: ItemId| (a.min(b), a.max(b));
    let airlines = if options.pours_block { board::connectivity(board) } else { board::connectivity_without_pours(board) }.airlines;
    let mut gaps: Vec<(f64, ItemId, ItemId)> = airlines
        .iter()
        .filter(|a| options.nets.as_ref().is_none_or(|n| n.contains(&a.net)))
        .filter(|a| options.only.as_ref().is_none_or(|o| o.contains(&pair(a.from_item, a.to_item))))
        .map(|a| (a.from.distance(a.to), a.from_item, a.to_item))
        .collect();
    gaps.sort_by(|a, b| a.0.total_cmp(&b.0));
    gaps
}

/// Routes the board's open connections as rubber bands, then rips up what is still open. Each
/// order is tried on a copy of the board; the copy that joins most, with fewest vias, is kept.
/// `watch` sees the board being routed after every connection laid.
pub fn route_topological(board: &mut Board, options: &Options, watch: &mut dyn FnMut(&Board)) -> Stats {
    let started = grid::clock::Instant::now();
    let out_of_time = || options.deadline.expired();
    let gaps = filtered_gaps(board, options);
    if gaps.is_empty() {
        return Stats::default();
    }
    let mut best: Option<(Stats, Board)> = None;
    for order in 0..options.orders.max(1) {
        if order > 0 && out_of_time() {
            break;
        }
        let mut trial = board.clone();
        let stats = route_in_order(&mut trial, &reorder(&gaps, order), options, &out_of_time, watch);
        tell(
            &options.log,
            "topo",
            || {
                format!(
                    "order {order}: {} of {} connections routed ({} without a channel, {} rejected before rip-up; {} rip-ups), {} vias, {:.1} s",
                    stats.routed,
                    stats.connections,
                    stats.no_channel,
                    stats.rejected,
                    stats.rip_ups,
                    stats.vias,
                    started.elapsed().as_secs_f64()
                )
            },
            None,
        );
        if best.as_ref().is_none_or(|(b, _)| (stats.routed, std::cmp::Reverse(stats.vias)) > (b.routed, std::cmp::Reverse(b.vias))) {
            best = Some((stats, trial));
        }
    }
    let Some((stats, routed)) = best else { return Stats::default() };
    *board = routed;
    stats
}

/// Groups the caller's unlocked `soft` copper into touching same-net clusters, numbered from
/// `base` on, so topological rip-up can cross and rip a grid route as one unit.
fn adopt_soft(board: &Board, soft: &[ItemId], base: usize) -> (Conns, Vec<Vec<ItemId>>) {
    let soft_set: HashSet<ItemId> = soft
        .iter()
        .copied()
        .filter(|&id| board.item(id).is_some_and(|it| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked))
        .collect();
    let mut sorted: Vec<ItemId> = soft_set.iter().copied().collect();
    sorted.sort_unstable();
    let mut visited: HashSet<ItemId> = HashSet::new();
    let mut conns = Conns::new();
    let mut clusters = Vec::new();
    for start_id in sorted {
        if !visited.insert(start_id) {
            continue;
        }
        let Some(first) = board.item(start_id) else { continue };
        let net = first.net;
        let idx = base + clusters.len();
        let mut cluster = vec![start_id];
        let mut queue = vec![start_id];
        while let Some(id) = queue.pop() {
            let Some(it) = board.item(id) else { continue };
            let bb = it.shape.bbox();
            for layer in (0..board.layers.len()).filter(|l| it.on(*l)) {
                for other in board.near(layer, bb) {
                    if soft_set.contains(&other) && !visited.contains(&other) {
                        if let Some(o) = board.item(other) {
                            if o.net == net && o.on(layer) && it.shape.gap_within(&o.shape, 0.0) == 0.0 {
                                visited.insert(other);
                                cluster.push(other);
                                queue.push(other);
                            }
                        }
                    }
                }
            }
        }
        for &id in &cluster {
            conns.insert(id, idx as u32);
        }
        clusters.push(cluster);
    }
    (conns, clusters)
}

/// Routes the open connections `options` selects as rubber bands, treating `soft` tracks and vias
/// from earlier passes as rippable copper during rip-up. When a rubber band crosses an existing
/// cluster, that cluster is ripped and the connections it opened are routed again (as rubber
/// bands, then on the grid, and if needed with a short grid rip-up while the freed rubber band
/// holds still). Each rip stands only when fewer connections are left open across the whole board.
/// `soft` is updated in place with what survived and what was laid.
pub fn route_topological_with_soft(
    board: &mut Board,
    options: &Options,
    soft: &mut Vec<ItemId>,
    watch: &mut dyn FnMut(&Board),
) -> Stats {
    let out_of_time = || options.deadline.expired();
    let raw_gaps = filtered_gaps(board, options);
    if raw_gaps.is_empty() {
        return Stats::default();
    }
    let gaps = reorder(&raw_gaps, 0);
    let pours = options.pours_block;
    let all_open = |b: &Board| if pours { board::connectivity(b) } else { board::connectivity_without_pours(b) }.airlines;
    let pair = |a: ItemId, b: ItemId| (a.min(b), a.max(b));

    let initial_soft: HashSet<ItemId> = soft.iter().copied().collect();
    let (conns, clusters) = adopt_soft(board, soft, gaps.len());
    let mut items = vec![Vec::new(); gaps.len()];
    items.extend(clusters);
    let mut laid = Laid { conns, items };
    let mut world = World::build(board, options.sample as f64, pours, &laid.conns);
    let mut stats = Stats { connections: gaps.len(), ..Stats::default() };

    for (conn, &gap) in gaps.iter().enumerate() {
        if out_of_time() {
            break;
        }
        match attempt(board, &world, gap, options) {
            Attempt::Routed(added) => {
                laid.record(&mut world, board, conn, added);
                watch(board);
            }
            Attempt::Joined => {}
            Attempt::NoChannel => stats.no_channel += 1,
            Attempt::Rejected(_) => stats.rejected += 1,
        }
    }

    let mut history = vec![0.0; laid.items.len()];
    for _ in 0..options.rip_passes {
        if out_of_time() {
            break;
        }
        let open_conns: Vec<usize> = (0..gaps.len()).filter(|&c| !made(board, gaps[c], pours)).collect();
        let mut moved = false;
        for conn in open_conns {
            if out_of_time() {
                break;
            }
            let (from, to) = gaps[conn];
            if made(board, (from, to), pours) {
                continue;
            }
            let Some(net) = board.item(from).map(|it| it.net) else { continue };
            let name = board.net_name(net).to_string();
            let before_air = all_open(board);
            let before_len = before_air.len();
            let was_open: HashSet<(ItemId, ItemId)> = before_air.iter().map(|a| pair(a.from_item, a.to_item)).collect();

            if history.len() < laid.items.len() {
                history.resize(laid.items.len(), 0.0);
            }
            let wire = WireRules::new(board, net, options.sample as f64 * MARGINS[0]);
            let rip = Rip { cost: options.rip_cost as f64, history: &history };
            let route = search(board, &world, &wire, from, to, options, Some(&rip));
            let mut victims: BTreeSet<u32> = route.as_ref().map(|r| r.victims.clone()).unwrap_or_default();
            if victims.is_empty() {
                if let Attempt::Rejected(clashed) = attempt(board, &world, (from, to), options) {
                    victims.extend(clashed.iter().filter_map(|id| laid.conns.get(id).copied()));
                }
            }
            if victims.is_empty() {
                continue;
            }
            tell(&options.log, "topo-rip", || format!("{name}: ripping {} clusters for a rubber band", victims.len()), None);
            let saved = (board.clone(), world.clone(), laid.clone());
            for &v in &victims {
                laid.rip(board, v as usize);
                if (v as usize) < history.len() {
                    history[v as usize] += 1.0;
                }
            }
            world = World::build(board, options.sample as f64, pours, &laid.conns);
            let mut routed = route.and_then(|r| commit(board, &world, &wire, &r, from, to, pours).ok());
            if routed.is_none() {
                if let Attempt::Routed(added) = attempt(board, &world, (from, to), options) {
                    routed = Some(added);
                }
            }
            if let Some(added) = routed {
                laid.record(&mut world, board, conn, added.clone());
                let fresh = all_open(board);
                for a in fresh.iter().filter(|a| !was_open.contains(&pair(a.from_item, a.to_item))) {
                    if let Attempt::Routed(v_added) = attempt(board, &world, (a.from_item, a.to_item), options) {
                        let slot = laid.items.len();
                        laid.items.push(Vec::new());
                        history.push(0.0);
                        laid.record(&mut world, board, slot, v_added);
                    }
                }
                if !all_open(board).is_empty() && !out_of_time() {
                    let g = grid::route_open(
                        board,
                        &mut grid::CpuSolver,
                        &grid::Options { deadline: options.deadline.seconds(REPAIR_ROUTE_SECONDS), pours_block: pours, ..Default::default() },
                        &mut |_| {},
                    );
                    if !g.laid.is_empty() {
                        let slot = laid.items.len();
                        laid.items.push(Vec::new());
                        history.push(0.0);
                        laid.record(&mut world, board, slot, g.laid);
                    }
                }
                let mid_len = all_open(board).len();
                if mid_len >= before_len && mid_len <= before_len + 2 && !out_of_time() {
                    // Hold the newly laid rubber band still while grid rip-up tries to repair
                    // the one or two knocked-loose connections that could not find a plain path.
                    let mut held = Vec::new();
                    for &id in &added {
                        if let Some(mut it) = board.remove(id) {
                            it.locked = true;
                            board.restore(id, it);
                            held.push(id);
                        }
                    }
                    let mut repair_soft: Vec<ItemId> = laid.all().into_iter().filter(|id| !held.contains(id) && board.item(*id).is_some()).collect();
                    grid::rip_up(
                        board,
                        &mut grid::CpuSolver,
                        &grid::RipOptions { deadline: options.deadline.seconds(REPAIR_RIP_SECONDS), reroute_seconds: 1.0, pours_block: pours, ..Default::default() },
                        &mut repair_soft,
                        &mut |_| {},
                    );
                    for id in held {
                        if let Some(mut it) = board.remove(id) {
                            it.locked = false;
                            board.restore(id, it);
                        }
                    }
                    if all_open(board).len() < before_len {
                        let (new_conns, new_clusters) = adopt_soft(board, &repair_soft, gaps.len());
                        let mut new_items = vec![Vec::new(); gaps.len()];
                        for (c, old) in laid.items.iter().take(gaps.len()).enumerate() {
                            if c == conn {
                                new_items[c] = added.iter().copied().filter(|id| board.item(*id).is_some()).collect();
                            } else {
                                new_items[c] = old.iter().copied().filter(|id| board.item(*id).is_some()).collect();
                            }
                        }
                        let mut merged_conns = new_conns;
                        for (c, ids) in new_items.iter().enumerate() {
                            for &id in ids {
                                merged_conns.insert(id, c as u32);
                            }
                        }
                        new_items.extend(new_clusters);
                        laid = Laid { conns: merged_conns, items: new_items };
                        history.resize(laid.items.len(), 0.0);
                        world = World::build(board, options.sample as f64, pours, &laid.conns);
                    }
                }
            }
            let after_len = all_open(board).len();
            if !laid.items[conn].is_empty() && after_len < before_len {
                moved = true;
                stats.rip_ups += 1;
                tell(&options.log, "topo-rip", || format!("{name}: kept, {before_len} -> {after_len} open"), None);
                watch(board);
            } else {
                tell(&options.log, "topo-rip", || format!("{name}: no better ({before_len} -> {after_len} open), put back"), None);
                (*board, world, laid) = saved;
            }
        }
        if !moved {
            break;
        }
    }

    *soft = laid.all().into_iter().filter(|id| board.item(*id).is_some()).collect();
    stats.routed = gaps.iter().filter(|&&gap| made(board, gap, pours)).count();
    stats.laid = soft.iter().copied().filter(|id| !initial_soft.contains(id)).collect();
    stats.vias = stats.laid.iter().filter(|id| board.item(**id).is_some_and(|it| it.kind == Kind::Via)).count();
    stats
}

/// The connections in the order a trial routes them: shortest first, then longest first, then
/// shuffled with the trial's number as seed.
fn reorder(gaps: &[(f64, ItemId, ItemId)], order: usize) -> Vec<(ItemId, ItemId)> {
    let mut out: Vec<(ItemId, ItemId)> = gaps.iter().map(|&(_, a, b)| (a, b)).collect();
    match order {
        0 => {}
        1 => out.reverse(),
        _ => {
            let mut state = (order as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            for i in (1..out.len()).rev() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                out.swap(i, (state % (i as u64 + 1)) as usize);
            }
        }
    }
    out
}

/// The copper laid so far: which connection each item belongs to, and each connection's items.
#[derive(Clone)]
struct Laid {
    conns: Conns,
    items: Vec<Vec<ItemId>>,
}

impl Laid {
    fn record(&mut self, world: &mut World, board: &Board, conn: usize, added: Vec<ItemId>) {
        for &id in &added {
            self.conns.insert(id, conn as u32);
        }
        world.add_items(board, &added, &self.conns);
        self.items[conn].extend(added);
    }

    fn rip(&mut self, board: &mut Board, conn: usize) {
        for id in std::mem::take(&mut self.items[conn]) {
            self.conns.remove(&id);
            board.remove(id);
        }
    }

    fn all(&self) -> Vec<ItemId> {
        let mut ids: Vec<ItemId> = self.items.iter().flatten().copied().collect();
        ids.sort_unstable();
        ids
    }
}

enum Attempt {
    Joined,
    Routed(Vec<ItemId>),
    NoChannel,
    /// A channel was found; its copper clashed with these items.
    Rejected(Vec<ItemId>),
}

/// The copper joined to `from` through its own net, as the board model joins it. Pours join only
/// when they are kept, as otherwise they will be refilled and may join nothing.
fn joined_to(board: &Board, from: ItemId, pours: bool) -> HashSet<ItemId> {
    let mut seen = HashSet::from([from]);
    let Some(net) = board.item(from).map(|it| it.net) else { return seen };
    if net == 0 {
        return seen;
    }
    let mut stack = vec![from];
    while let Some(id) = stack.pop() {
        let Some(item) = board.item(id) else { continue };
        for layer in (0..board.layers.len()).filter(|l| item.on(*l)) {
            for other in board.near(layer, item.shape.bbox()) {
                if seen.contains(&other) {
                    continue;
                }
                let Some(o) = board.item(other) else { continue };
                if o.net == net && o.is_copper() && o.on(layer) && (pours || !o.refilled()) && board::joined(board, id, other) {
                    seen.insert(other);
                    stack.push(other);
                }
            }
        }
    }
    seen
}

fn made(board: &Board, (from, to): (ItemId, ItemId), pours: bool) -> bool {
    board.item(to).is_none() || joined_to(board, from, pours).contains(&to)
}

/// Routes one connection through the world as it is, widening the margin while its copper clashes.
fn attempt(board: &mut Board, world: &World, gap: (ItemId, ItemId), options: &Options) -> Attempt {
    let (from, to) = gap;
    if made(board, gap, options.pours_block) {
        return Attempt::Joined;
    }
    let Some(net) = board.item(from).map(|it| it.net) else { return Attempt::Joined };
    let mut clashes: Option<Vec<ItemId>> = None;
    for widen in MARGINS {
        let wire = WireRules::new(board, net, options.sample as f64 * widen);
        let Some(route) = search(board, world, &wire, from, to, options, None) else { break };
        match commit(board, world, &wire, &route, from, to, options.pours_block) {
            Ok(added) => return Attempt::Routed(added),
            Err(items) => clashes.get_or_insert_with(Vec::new).extend(items),
        }
    }
    clashes.map_or(Attempt::NoChannel, Attempt::Rejected)
}

fn route_in_order(
    board: &mut Board,
    gaps: &[(ItemId, ItemId)],
    options: &Options,
    out_of_time: &dyn Fn() -> bool,
    watch: &mut dyn FnMut(&Board),
) -> Stats {
    let mut laid = Laid { conns: Conns::new(), items: vec![Vec::new(); gaps.len()] };
    let mut world = World::build(board, options.sample as f64, options.pours_block, &laid.conns);
    let mut stats = Stats { connections: gaps.len(), ..Stats::default() };
    for (conn, &gap) in gaps.iter().enumerate() {
        if out_of_time() {
            break;
        }
        let net = board.item(gap.0).map_or(0, |it| it.net);
        let name = board.net_name(net).to_string();
        let anchor = |id: ItemId| board.item(id).and_then(|it| anchors(it).first().copied());
        if let (Some(from), Some(to)) = (anchor(gap.0), anchor(gap.1)) {
            tell(
                &options.log,
                "routing",
                || format!("{name}: {} to {} as a rubber band", item_name(board, gap.0), item_name(board, gap.1)),
                Some(Focus::Connection { net, from, to }),
            );
        }
        match attempt(board, &world, gap, options) {
            Attempt::Routed(added) => {
                tell(&options.log, "routed", || format!("{name}: laid {} pieces", added.len()), None);
                laid.record(&mut world, board, conn, added);
                watch(board);
            }
            Attempt::Joined => {}
            Attempt::NoChannel => {
                stats.no_channel += 1;
                tell(&options.log, "gave up", || format!("{name}: no channel wide enough"), None);
            }
            Attempt::Rejected(_) => {
                stats.rejected += 1;
                tell(&options.log, "gave up", || format!("{name}: its copper broke a rule"), None);
            }
        }
    }
    stats.rip_ups = rip_up(board, &mut world, &mut laid, gaps, options, out_of_time, watch);
    stats.routed = gaps.iter().filter(|&&gap| made(board, gap, options.pours_block)).count();
    stats.laid = laid.all();
    stats.vias = stats.laid.iter().filter(|id| board.item(**id).is_some_and(|it| it.kind == Kind::Via)).count();
    stats
}

/// Rounds over the open connections, each routed through other connections' copper where that is
/// cheapest, the copper it crosses ripped and its connections rerouted. A round's result stands
/// when it joins no fewer connections, so a connection can trade places with the one it ripped;
/// the board that joined most is the one left. Returns the rip-ups in that board.
fn rip_up(
    board: &mut Board,
    world: &mut World,
    laid: &mut Laid,
    gaps: &[(ItemId, ItemId)],
    options: &Options,
    out_of_time: &dyn Fn() -> bool,
    watch: &mut dyn FnMut(&Board),
) -> usize {
    let pours = options.pours_block;
    let count = |board: &Board| gaps.iter().filter(|&&gap| made(board, gap, pours)).count();
    let mut history = vec![0.0; gaps.len()];
    let mut kept = 0;
    let mut best: Option<(usize, usize, Board, Laid)> = None;
    for _ in 0..options.rip_passes {
        let open: Vec<usize> = (0..gaps.len()).filter(|&c| !made(board, gaps[c], pours)).collect();
        let mut moved = false;
        for conn in open {
            if out_of_time() {
                break;
            }
            let (from, to) = gaps[conn];
            if made(board, gaps[conn], pours) {
                continue;
            }
            let before = count(board);
            let Some(net) = board.item(from).map(|it| it.net) else { continue };
            let name = board.net_name(net).to_string();
            let wire = WireRules::new(board, net, options.sample as f64 * MARGINS[0]);
            let rip = Rip { cost: options.rip_cost as f64, history: &history };
            let route = search(board, world, &wire, from, to, options, Some(&rip));
            let mut victims: BTreeSet<u32> = route.as_ref().map(|r| r.victims.clone()).unwrap_or_default();
            if victims.is_empty() {
                // The cheapest way crosses nobody's copper yet clashes when laid: rip what it
                // clashes with.
                if let Attempt::Rejected(items) = attempt(board, world, gaps[conn], options) {
                    victims.extend(items.iter().filter_map(|id| laid.conns.get(id)));
                }
            }
            if victims.is_empty() {
                continue;
            }
            tell(&options.log, "ripping up", || format!("{name}: through the copper of {} connections", victims.len()), None);
            let saved = (board.clone(), world.clone(), laid.clone());
            for &victim in &victims {
                laid.rip(board, victim as usize);
                history[victim as usize] += 1.0;
            }
            *world = World::build(board, options.sample as f64, pours, &laid.conns);
            let mut routed = route.and_then(|r| commit(board, world, &wire, &r, from, to, pours).ok());
            if routed.is_none() {
                if let Attempt::Routed(added) = attempt(board, world, gaps[conn], options) {
                    routed = Some(added);
                }
            }
            if let Some(added) = routed {
                laid.record(world, board, conn, added);
                for &victim in &victims {
                    if let Attempt::Routed(added) = attempt(board, world, gaps[victim as usize], options) {
                        laid.record(world, board, victim as usize, added);
                    }
                }
            }
            let after = count(board);
            if !laid.items[conn].is_empty() && after >= before {
                moved = true;
                kept += 1;
                tell(&options.log, "ripping up", || format!("{name}: kept, {before} -> {after} joined"), None);
                watch(board);
                if best.as_ref().is_none_or(|b| after > b.0) {
                    best = Some((after, kept, board.clone(), laid.clone()));
                }
            } else {
                tell(&options.log, "ripping up", || format!("{name}: no better, put back"), None);
                (*board, *world, *laid) = saved;
            }
        }
        if !moved {
            break;
        }
    }
    match best {
        Some((joined_then, kept_then, b, l)) if joined_then > count(board) => {
            *board = b;
            *laid = l;
            kept_then
        }
        _ => kept,
    }
}

/// What one net's wire needs, in nanometres: its width, clearances and via.
#[derive(Clone)]
pub struct WireRules {
    net: u32,
    half_width: Nm,
    /// The narrowest half-width a leg at a pad may neck down to.
    min_half_width: Nm,
    /// Radius and drill radius.
    via: Option<(f64, f64)>,
    /// The clearance to each net's copper, by net number.
    clearance: Vec<f64>,
    hole_clearance: f64,
    hole_to_hole: f64,
    edge: f64,
    /// How far a sampled outline may bulge between its samples.
    margin: f64,
    /// The furthest any obstacle can keep a via's centre away.
    max_via_reach: f64,
}

impl WireRules {
    fn new(board: &Board, net: u32, margin: f64) -> WireRules {
        let profile = Profile::for_net(board, net);
        let rules = &board.rules;
        let clearance: Vec<f64> = (0..board.nets.len() as u32).map(|other| board.clearance(net, other) as f64).collect();
        let none = board.clearance(net, 0) as f64;
        let hole_clearance = nm(rules.min_hole_clearance) as f64;
        let hole_to_hole = nm(rules.min_hole_to_hole) as f64;
        let edge = nm(rules.min_copper_edge_clearance) as f64;
        let via = (profile.via_radius > 0).then_some((profile.via_radius as f64, profile.drill_radius as f64));
        let widest = clearance.iter().copied().fold(none, f64::max).max(hole_clearance).max(hole_to_hole).max(edge);
        WireRules {
            net,
            half_width: profile.half_width,
            // The net's own minimum, from a custom rule, where it is above the board's.
            min_half_width: profile.min_half_width,
            via,
            clearance,
            hole_clearance,
            hole_to_hole,
            edge,
            margin,
            max_via_reach: via.map_or(0.0, |(r, _)| r) + widest + margin,
        }
    }

    fn clearance_to(&self, net: u32) -> f64 {
        self.clearance.get(net as usize).copied().unwrap_or(self.clearance[0])
    }

    fn own(&self, net: u32) -> bool {
        net == self.net && net != 0
    }

    /// Narrower half-widths a leg at a pad may neck down to, never below the net's minimum.
    fn necked_widths(&self) -> Vec<Nm> {
        let (full, floor) = (self.half_width, self.min_half_width);
        let mut widths: Vec<Nm> = [full * 3 / 4, full / 2].into_iter().filter(|&w| w > floor).collect();
        if floor < full {
            widths.push(floor);
        }
        widths
    }

    /// Rules at a pad exit, where the first or last segment may neck down to the net's
    /// narrowest track so a wide net or wider margin can still leave a fine-pitch pad.
    pub fn necked(&self) -> WireRules {
        WireRules { half_width: self.min_half_width, margin: self.margin.min(25_000.0), ..self.clone() }
    }

    /// How near the wire's centre may come to an obstacle, `None` when it may cross it.
    pub fn trace_reach(&self, rule: Rule, net: u32) -> Option<f64> {
        let wire = self.half_width as f64 + self.margin;
        match rule {
            Rule::Copper if self.own(net) => None,
            Rule::Copper => Some(wire + self.clearance_to(net)),
            Rule::Hole { plated: true } if self.own(net) => None,
            Rule::Hole { .. } => Some(wire + self.hole_clearance),
            Rule::Edge => Some(wire + self.edge),
            Rule::Keepout { tracks: true, .. } => Some(wire),
            Rule::Keepout { .. } => None,
        }
    }

    /// How near a via's centre may come to an obstacle, `None` when it may stand on it.
    pub fn via_reach(&self, rule: Rule, net: u32) -> Option<f64> {
        let (radius, drill) = self.via?;
        let ring = radius + self.margin;
        let hole = drill + self.margin;
        match rule {
            Rule::Copper if self.own(net) => None,
            Rule::Copper => Some((ring + self.clearance_to(net)).max(hole + self.hole_clearance)),
            Rule::Hole { plated } => {
                let apart = hole + self.hole_to_hole;
                Some(if plated && self.own(net) { apart } else { apart.max(ring + self.hole_clearance) })
            }
            Rule::Edge => Some(ring + self.edge),
            Rule::Keepout { vias: true, .. } => Some(ring),
            Rule::Keepout { .. } => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct State {
    layer: usize,
    face: Face,
}

#[derive(Clone)]
enum Step {
    Start(Pt),
    Cross(Gate, Vec<u32>),
    Via(Pt, Vec<u32>),
}

/// What crossing other connections' copper costs a search that may.
struct Rip<'a> {
    cost: f64,
    history: &'a [f64],
}

impl Rip<'_> {
    fn price(&self, victims: &[u32]) -> f64 {
        victims.iter().map(|&c| self.cost * (1.0 + self.history[c as usize])).sum()
    }
}

struct Open {
    f: f64,
    g: f64,
    state: State,
    at: Pt,
}

impl PartialEq for Open {
    fn eq(&self, other: &Self) -> bool {
        self.f == other.f
    }
}
impl Eq for Open {}
impl PartialOrd for Open {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Open {
    fn cmp(&self, other: &Self) -> Ordering {
        other.f.total_cmp(&self.f)
    }
}

/// One layer's run of a route: where it starts, the gates it passes, where it ends.
struct Run {
    layer: usize,
    from: Pt,
    gates: Vec<Gate>,
    to: Pt,
}

struct Route {
    runs: Vec<Run>,
    /// The connections whose copper it crosses.
    victims: BTreeSet<u32>,
}

const HEURISTIC_WEIGHT: f64 = 1.2;
/// The margins a connection is tried with, in samples, widening each time its copper clashes.
const MARGINS: [f64; 3] = [0.5, 1.5, 3.0];

/// Where a wire may leave an item: a pad's or via's centre, or a track's ends.
fn anchors(item: &Item) -> Vec<Point> {
    match (&item.kind, &item.shape) {
        (Kind::Pour { .. } | Kind::Keepout { .. }, _) => Vec::new(),
        (_, Shape::Capsule { a, b, .. }) if a == b => vec![*a],
        (Kind::Track, Shape::Capsule { a, b, .. }) => vec![*a, *b],
        (_, Shape::Polygon(p)) if !p.is_empty() => {
            let n = p.len() as Nm;
            vec![Point::new(p.iter().map(|q| q.x).sum::<Nm>() / n, p.iter().map(|q| q.y).sum::<Nm>() / n)]
        }
        _ => Vec::new(),
    }
}

/// A* over the triangles of every layer, from any face holding copper joined to `from` to any
/// holding copper joined to `to`.
fn search(board: &Board, world: &World, wire: &WireRules, from: ItemId, to: ItemId, options: &Options, rip: Option<&Rip<'_>>) -> Option<Route> {
    let ends = |item: ItemId| -> Vec<(State, Pt)> {
        let mut out = Vec::new();
        let mut ids: Vec<ItemId> = joined_to(board, item, options.pours_block).into_iter().collect();
        ids.sort_unstable();
        for id in ids {
            let Some(it) = board.item(id) else { continue };
            for at in anchors(it).into_iter().map(pt) {
                for layer in (0..board.layers.len()).filter(|l| it.on(*l)) {
                    if let Some(face) = world.face_at(layer, at) {
                        out.push((State { layer, face }, at));
                    }
                }
            }
        }
        out
    };
    let sources = ends(from);
    let targets = ends(to);
    if sources.is_empty() || targets.is_empty() {
        return None;
    }
    let goal: HashMap<State, Pt> = targets.iter().copied().collect();
    let source_points: Vec<Pt> = sources.iter().map(|(_, p)| *p).collect();
    let goal_points: Vec<Pt> = targets.iter().map(|(_, p)| *p).collect();
    let necked = wire.necked();
    let near_pad = |p: Pt| {
        (wire.half_width > wire.min_half_width || wire.margin > 25_000.0)
            && source_points.iter().chain(&goal_points).any(|q| p.distance(*q) <= 500_000.0)
    };
    let h = |p: Pt| goal_points.iter().map(|q| p.distance(*q)).fold(f64::INFINITY, f64::min) * HEURISTIC_WEIGHT;

    let mut best: HashMap<State, f64> = HashMap::new();
    let mut parent: HashMap<State, (Option<State>, Step)> = HashMap::new();
    let mut open = BinaryHeap::new();
    for (state, at) in &sources {
        best.insert(*state, 0.0);
        parent.insert(*state, (None, Step::Start(*at)));
        open.push(Open { f: h(*at), g: 0.0, state: *state, at: *at });
    }
    let price = |victims: &[u32]| rip.map_or(0.0, |r| r.price(victims));
    let mut via_ok: HashMap<State, Option<(Pt, Vec<u32>)>> = HashMap::new();
    let mut expanded = 0;
    while let Some(Open { g, state, at, .. }) = open.pop() {
        if best.get(&state).is_some_and(|b| *b < g) {
            continue;
        }
        if let Some(end) = goal.get(&state) {
            return Some(unwind(&parent, state, *end));
        }
        expanded += 1;
        if expanded > options.max_expansions {
            return None;
        }
        let mut push = |next: State, g2: f64, at2: Pt, step: Step| {
            if best.get(&next).is_none_or(|b| g2 < *b) {
                best.insert(next, g2);
                parent.insert(next, (Some(state), step));
                open.push(Open { f: g2 + h(at2), g: g2, state: next, at: at2 });
            }
        };
        let gate_wire = if near_pad(at) { &necked } else { wire };
        for (face, gate, victims) in world.gates(gate_wire, state.layer, state.face, rip.is_some()) {
            let p = gate.nearest(at);
            let g2 = g + at.distance(p) + price(&victims);
            push(State { layer: state.layer, face }, g2, p, Step::Cross(gate, victims));
        }
        let via = via_ok.entry(state).or_insert_with(|| world.via_site(wire, state.layer, state.face, rip.is_some())).clone();
        if let Some((site, victims)) = via {
            for layer in (0..board.layers.len()).filter(|l| *l != state.layer) {
                if let Some(face) = world.face_at(layer, site) {
                    let g2 = g + at.distance(site) + options.via_cost as f64 + price(&victims);
                    push(State { layer, face }, g2, site, Step::Via(site, victims.clone()));
                }
            }
        }
    }
    None
}

fn unwind(parent: &HashMap<State, (Option<State>, Step)>, last: State, end: Pt) -> Route {
    let mut steps = Vec::new();
    let mut at = Some(last);
    while let Some(state) = at {
        let (prev, step) = parent[&state].clone();
        steps.push((state.layer, step));
        at = prev;
    }
    steps.reverse();
    let mut runs: Vec<Run> = Vec::new();
    let mut victims = BTreeSet::new();
    for (layer, step) in steps {
        match step {
            Step::Start(p) => runs.push(Run { layer, from: p, gates: Vec::new(), to: p }),
            Step::Cross(gate, crossed) => {
                runs.last_mut().expect("a route starts").gates.push(gate);
                victims.extend(crossed);
            }
            Step::Via(site, crossed) => {
                runs.last_mut().expect("a route starts").to = site;
                runs.push(Run { layer, from: site, gates: Vec::new(), to: site });
                victims.extend(crossed);
            }
        }
    }
    runs.last_mut().expect("a route starts").to = end;
    Route { runs, victims }
}

/// The items `id` breaks a rule against, `None` for the board edge. With `pours_block`, other
/// nets' pours count too: the board check leaves them out, as they are usually refilled.
fn faults(board: &Board, id: ItemId, pours_block: bool) -> Vec<Option<ItemId>> {
    let mut out: Vec<Option<ItemId>> = check_item(board, id).into_iter().map(|v| v.b).collect();
    let Some(item) = board.item(id) else { return out };
    if pours_block {
        let widest = board.rules.classes.iter().map(|c| nm(c.clearance)).chain([nm(board.rules.min_clearance)]).max().unwrap_or(0);
        for layer in (0..board.layers.len()).filter(|l| item.on(*l)) {
            for other in board.near(layer, item.shape.bbox().grow(widest)) {
                let Some(o) = board.item(other) else { continue };
                if !o.refilled() || !o.on(layer) || (o.net == item.net && item.net != 0) {
                    continue;
                }
                let required = board.clearance(item.net, o.net) as f64;
                if item.shape.gap_within(&o.shape, required) < required {
                    out.push(Some(other));
                }
            }
        }
    }
    out
}

fn track(net: u32, layer: usize, a: Point, b: Point, half_width: Nm) -> Item {
    Item { kind: Kind::Track, net, layers: 1 << layer, shape: Shape::Capsule { a, b, r: half_width }, hole: None, locked: false, span: None }
}

/// Lays the route's tracks and vias; returns the items added, or, leaving the board as it was,
/// the items its copper clashes with (none when a via clashes or the copper doesn't join).
fn commit(board: &mut Board, world: &World, wire: &WireRules, route: &Route, from: ItemId, to: ItemId, pours_block: bool) -> Result<Vec<ItemId>, Vec<ItemId>> {
    let mut added: Vec<ItemId> = Vec::new();
    let undo = |board: &mut Board, added: &[ItemId]| {
        for &id in added {
            board.remove(id);
        }
    };
    let necked = wire.necked();
    for (k, run) in route.runs.iter().enumerate() {
        let pulled: Vec<Pt> = funnel::pull(run.from, &run.gates, run.to).iter().flat_map(Bend::points).collect();
        let mut points: Vec<Point> = Vec::new();
        for q in world.clear_path(wire, &necked, run.layer, pulled, k == 0, k + 1 == route.runs.len()).into_iter().map(round) {
            if points.last() != Some(&q) {
                points.push(q);
            }
        }
        let last = points.len().saturating_sub(2);
        for (segment, pair) in points.windows(2).enumerate() {
            let at_pad = (k == 0 && segment == 0) || (k + 1 == route.runs.len() && segment == last);
            let narrower = if at_pad { wire.necked_widths() } else { Vec::new() };
            let mut kept = None;
            for width in std::iter::once(wire.half_width).chain(narrower) {
                let id = board.insert(track(wire.net, run.layer, pair[0], pair[1], width));
                if faults(board, id, pours_block).is_empty() {
                    kept = Some(id);
                    break;
                }
                board.remove(id);
            }
            let Some(id) = kept else {
                let id = board.insert(track(wire.net, run.layer, pair[0], pair[1], wire.half_width));
                let clashes: Vec<ItemId> = faults(board, id, pours_block).into_iter().flatten().filter(|c| !added.contains(c)).collect();
                board.remove(id);
                undo(board, &added);
                return Err(clashes);
            };
            added.push(id);
        }
        if k + 1 < route.runs.len() {
            let Some((radius, drill)) = wire.via else {
                undo(board, &added);
                return Err(Vec::new());
            };
            let at = round(run.to);
            let id = board.insert(Item {
                kind: Kind::Via,
                net: wire.net,
                layers: (1u64 << board.layers.len()) - 1,
                shape: Shape::Capsule { a: at, b: at, r: radius as Nm },
                hole: Some((at, drill as Nm)),
                locked: false,
                span: None,
            });
            added.push(id);
            if !faults(board, id, pours_block).is_empty() {
                undo(board, &added);
                return Err(Vec::new());
            }
        }
    }
    if !made(board, (from, to), pours_block) {
        undo(board, &added);
        return Err(Vec::new());
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_two_orders_are_shortest_and_longest_first() {
        let gaps = [(1.0, 1, 2), (2.0, 3, 4), (3.0, 5, 6)];
        assert_eq!(reorder(&gaps, 0), [(1, 2), (3, 4), (5, 6)]);
        assert_eq!(reorder(&gaps, 1), [(5, 6), (3, 4), (1, 2)]);
        let mut shuffled = reorder(&gaps, 2);
        shuffled.sort();
        assert_eq!(shuffled, [(1, 2), (3, 4), (5, 6)], "a shuffle keeps every connection");
    }

    #[test]
    fn a_rip_costs_more_for_a_connection_ripped_before() {
        let history = [0.0, 2.0];
        let rip = Rip { cost: 10.0, history: &history };
        assert_eq!(rip.price(&[0]), 10.0);
        assert_eq!(rip.price(&[1]), 30.0);
        assert_eq!(rip.price(&[0, 1]), 40.0);
    }
}
