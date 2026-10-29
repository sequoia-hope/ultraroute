//! Chained rip-up: a blocked connection is laid through the router's own copper, and whatever it
//! ripped is laid again the same way, ripping in turn, until nothing new is open. Copper laid
//! earlier in the chain is never ripped by a later link, so the chain cannot undo itself. The
//! board is kept only if, once the chain settles, fewer connections are open than before.
//!
//! Plain rip-up rips once: when the copper it ripped finds no way back without ripping something
//! else, the attempt is lost. On a crowded escape that is the usual case.

use std::collections::{HashMap, HashSet};

use board::{check_item, connectivity, connectivity_without_pours, Airline, BBox, Board, ItemId, Kind, Point, Shape};

use crate::grid::{ObstacleMap, Profile};
use crate::report::{item_name, tell, Focus, Log};
use crate::route::{pieces, terminals, FieldSolver};

#[derive(Debug, Clone)]
pub struct ChaseOptions {
    pub cell: board::Nm,
    pub via_cost: u32,
    /// Price of entering a cell that rippable copper of another net claims.
    pub rip_cost: u32,
    /// Price of entering a cell claimed by copper an earlier link of the chain laid. The blocked
    /// connection's own copper is never ripped.
    pub chain_cost: u32,
    /// Most connections laid in one chain.
    pub links: usize,
    pub deadline: crate::clock::Deadline,
    pub pours_block: bool,
    pub log: Option<Log>,
}

impl Default for ChaseOptions {
    fn default() -> ChaseOptions {
        ChaseOptions { cell: 50_000, via_cost: 80, rip_cost: 400, chain_cost: 3_000, links: 24, deadline: Default::default(), pours_block: false, log: None }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChaseStats {
    pub attempts: usize,
    /// Chains kept because they left fewer connections open.
    pub kept: usize,
    /// Connections laid by kept chains, the blocked ones included.
    pub links: usize,
    /// Chains lost by cause: a link with no way through, a link that met copper it may not rip,
    /// a chain that grew past `links`, and one that settled no better than before.
    pub no_path: usize,
    pub hard_conflict: usize,
    pub too_long: usize,
    pub no_better: usize,
}

enum Lost {
    NoPath,
    Hard,
}

fn open(board: &Board, pours_block: bool) -> Vec<Airline> {
    if pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines
}

/// Tries a chain from every open connection, shortest first, again from the top after any chain
/// is kept. `soft` holds the copper the router laid; it is updated as copper is ripped and laid.
/// `watch` sees the board after every link, and again when a chain is put back.
pub fn chase(board: &mut Board, solver: &mut dyn FieldSolver, options: &ChaseOptions, soft: &mut Vec<ItemId>, watch: &mut dyn FnMut(&Board)) -> ChaseStats {
    let out_of_time = || options.deadline.expired();
    let mut stats = ChaseStats::default();
    let mut tried: HashSet<(ItemId, ItemId)> = HashSet::new();
    'rounds: loop {
        let mut todo = open(board, options.pours_block);
        todo.sort_by(|a, b| a.from.distance(a.to).total_cmp(&b.from.distance(b.to)));
        todo.retain(|a| !tried.contains(&(a.from_item, a.to_item)));
        for airline in todo {
            if out_of_time() {
                break 'rounds;
            }
            tried.insert((airline.from_item, airline.to_item));
            stats.attempts += 1;
            if let Some(links) = chain(board, solver, options, soft, &airline, &mut stats, watch) {
                stats.kept += 1;
                stats.links += links;
                tried.clear();
                continue 'rounds;
            }
        }
        break;
    }
    soft.retain(|id| board.item(*id).is_some());
    stats
}

/// One chain from `airline`. Keeps the board and returns how many links it laid if the chain
/// left fewer connections open; otherwise puts everything back.
fn chain(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &ChaseOptions,
    soft: &mut Vec<ItemId>,
    airline: &Airline,
    stats: &mut ChaseStats,
    watch: &mut dyn FnMut(&Board),
) -> Option<usize> {
    let net_name = board.net_name(airline.net).to_string();
    tell(
        &options.log,
        "chasing",
        || format!("{net_name}: laying it through the router's copper, and what that rips after it"),
        Some(Focus::Connection { net: airline.net, from: airline.from, to: airline.to }),
    );
    let before = open(board, options.pours_block);
    // Either way round: a connection may come back from connectivity with its ends swapped.
    let key = |a: &Airline| (a.from_item.min(a.to_item), a.from_item.max(a.to_item));
    let was_open: HashSet<(ItemId, ItemId)> = before.iter().map(key).collect();
    let snapshot = (board.clone(), soft.clone());
    // The blocked connection's copper, never ripped, and what later links laid, ripped at a price.
    // A net laid twice in one chain is frozen too, so two nets cannot rip each other forever.
    let (mut root, mut chained): (HashSet<ItemId>, HashSet<ItemId>) = Default::default();
    let mut laid_for: HashMap<u32, usize> = HashMap::new();
    let mut first = Some(airline.clone());
    let mut links = 0;
    let lost = loop {
        // The blocked connection first, then whatever the chain has left open that was not open
        // before, shortest first, judged afresh after every link.
        let next = match first.take() {
            Some(a) => a,
            None => {
                let now = open(board, options.pours_block);
                let fresh = now.iter().filter(|a| !was_open.contains(&key(a))).min_by(|a, b| a.from.distance(a.to).total_cmp(&b.from.distance(b.to)));
                match fresh {
                    Some(a) => a.clone(),
                    None if now.len() < before.len() => {
                        tell(&options.log, "chasing", || format!("{net_name}: kept after {links} links, {} open", now.len()), None);
                        return Some(links);
                    }
                    None => {
                        stats.no_better += 1;
                        break "no better";
                    }
                }
            }
        };
        if links >= options.links {
            stats.too_long += 1;
            break "too long";
        }
        match link(board, solver, options, soft, &root, &chained, &next) {
            Ok((laid, ripped)) => {
                links += 1;
                tell(
                    &options.log,
                    "chasing",
                    || format!("  link {links}: {} {} to {}, {} pieces, ripped {}", board.net_name(next.net), item_name(board, next.from_item), item_name(board, next.to_item), laid.len(), tally(&ripped)),
                    Some(Focus::Connection { net: next.net, from: next.from, to: next.to }),
                );
                watch(board);
                let times = laid_for.entry(next.net).or_default();
                *times += 1;
                if links == 1 || *times > 1 { &mut root } else { &mut chained }.extend(laid.iter().copied());
                soft.extend(laid);
            }
            Err(Lost::NoPath) => {
                tell(&options.log, "chasing", || format!("  stuck on {} {} to {}", board.net_name(next.net), item_name(board, next.from_item), item_name(board, next.to_item)), None);
                stats.no_path += 1;
                break "no way through";
            }
            Err(Lost::Hard) => {
                stats.hard_conflict += 1;
                break "blocked by copper it may not rip";
            }
        }
    };
    tell(&options.log, "chasing", || format!("{net_name}: {lost} after {links} links, put back"), None);
    (*board, *soft) = snapshot;
    watch(board);
    None
}

/// Ripped pieces by net, as "VSYS ×3, GND".
fn tally(nets: &[String]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for n in nets {
        match counts.iter_mut().find(|(m, _)| m == n) {
            Some(c) => c.1 += 1,
            None => counts.push((n, 1)),
        }
    }
    if counts.is_empty() {
        return "nothing".to_string();
    }
    counts.iter().map(|(n, c)| if *c > 1 { format!("{n} ×{c}") } else { n.to_string() }).collect::<Vec<_>>().join(", ")
}

/// Lays one connection, crossing rippable copper at a price: the router's copper, dearer where
/// an earlier link of this chain laid it, and never the blocked connection's own. Rips what the
/// new copper breaks a rule against and returns what it laid.
fn link(
    board: &mut Board,
    solver: &mut dyn FieldSolver,
    options: &ChaseOptions,
    soft: &[ItemId],
    root: &HashSet<ItemId>,
    chained: &HashSet<ItemId>,
    airline: &Airline,
) -> Result<(Vec<ItemId>, Vec<String>), Lost> {
    let rippable: HashSet<ItemId> = soft.iter().copied().filter(|id| !root.contains(id) && board.item(*id).is_some()).collect();
    let profile = Profile::for_net(board, airline.net);
    let hard = ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| rippable.contains(&id));
    let mut grid = hard.view(airline.net);
    grid.deadline = options.deadline.clone();
    let cheap = ObstacleMap::without(board, profile.clone(), options.cell, options.pours_block, &|id| !rippable.contains(&id) || chained.contains(&id));
    let dear = ObstacleMap::without(board, profile, options.cell, options.pours_block, &|id| !chained.contains(&id));
    let (cheap, dear) = (cheap.view(airline.net), dear.view(airline.net));
    grid.penalty = cheap.blocked.iter().zip(&dear.blocked).map(|(&c, &d)| if d { options.chain_cost } else if c { options.rip_cost } else { 0 }).collect();
    let sources = terminals(board, &grid, airline.from_item);
    let targets = terminals(board, &grid, airline.to_item);
    if sources.is_empty() || targets.is_empty() {
        tell(&options.log, "chasing", || format!("  {}: no terminals at {}", board.net_name(airline.net), if sources.is_empty() { item_name(board, airline.from_item) } else { item_name(board, airline.to_item) }), None);
        return Err(Lost::NoPath);
    }
    let source_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
    let target_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
    let path = solver.route(&grid, &source_cells, &target_cells, options.via_cost).ok_or(Lost::NoPath)?;
    let new = pieces(&grid, &path, &sources, &targets).ok_or(Lost::NoPath)?;
    let mut laid = Vec::new();
    let mut ripped = Vec::new();
    let mut broken: HashSet<u32> = HashSet::new();
    for piece in new {
        let id = board.insert(piece);
        laid.push(id);
        for violation in check_item(board, id) {
            let Some(other) = violation.b else { return Err(Lost::Hard) };
            if !rippable.contains(&other) || laid.contains(&other) {
                return Err(Lost::Hard);
            }
            if let Some(it) = board.remove(other) {
                ripped.push(board.net_name(it.net).to_string());
                broken.insert(it.net);
            }
        }
        if !check_item(board, id).is_empty() {
            return Err(Lost::Hard);
        }
    }
    prune(board, soft, &broken);
    Ok((laid, ripped))
}

/// Takes away the router's copper of `nets` that leads nowhere: a track with an end touching no
/// other copper of its net, a via touching at most one piece. What is left of a ripped net is
/// then only what still joins its pads, and the net is routed again from there, free to go
/// another way instead of reaching for the stubs where it was cut.
fn prune(board: &mut Board, soft: &[ItemId], nets: &HashSet<u32>) {
    if nets.is_empty() {
        return;
    }
    let touching = |board: &Board, id: ItemId, p: Point, layers: u64| -> usize {
        let mut seen: HashSet<ItemId> = HashSet::new();
        let net = board.item(id).map_or(0, |it| it.net);
        for layer in (0..board.layers.len()).filter(|l| layers & (1 << l) != 0) {
            for other in board.near(layer, BBox::of([p])) {
                if other != id && board.item(other).is_some_and(|o| o.net == net && o.on(layer) && o.shape.contains(p)) {
                    seen.insert(other);
                }
            }
        }
        seen.len()
    };
    loop {
        let dead: Vec<ItemId> = soft
            .iter()
            .copied()
            .filter(|&id| {
                let Some(it) = board.item(id) else { return false };
                let Shape::Capsule { a, b, .. } = it.shape else { return false };
                if !nets.contains(&it.net) {
                    return false;
                }
                match it.kind {
                    Kind::Track => touching(board, id, a, it.layers) == 0 || touching(board, id, b, it.layers) == 0,
                    Kind::Via => touching(board, id, a, it.layers) <= 1,
                    _ => false,
                }
            })
            .collect();
        if dead.is_empty() {
            return;
        }
        for id in dead {
            board.remove(id);
        }
    }
}
