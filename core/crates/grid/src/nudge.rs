//! Moving parts aside for the connections nothing else could route.
//!
//! For each open connection, the unlocked parts near its airline (and the parts at its ends) are
//! tried a small step in each direction. The copper around the part flows aside with it (its own
//! traces dragged along), and only copper pushed into something that cannot move is ripped. A
//! step is legal when the part's pads keep every rule and its courtyard meets no other on its
//! side of the board; what was ripped is routed again, and the step is kept only if fewer
//! connections are left open.

use std::collections::HashSet;

use board::{check_item, connectivity, connectivity_without_pours, point_segment, Board, ItemId, Kind, Nm, Point, Shape};
use kicad_pcb::Span;

use crate::push::{push, Field, PushOptions};
use crate::report::{tell, Focus, Log};
use crate::route::{route_open, FieldSolver, Options};

#[derive(Debug, Clone)]
pub struct NudgeOptions {
    pub cell: Nm,
    pub via_cost: u32,
    pub max_seconds: Option<f64>,
    /// Step lengths tried, in nanometres, shortest first.
    pub steps: Vec<Nm>,
    /// How near an airline a part's courtyard must come to be tried.
    pub reach: Nm,
    /// Parts tried per connection, nearest first.
    pub parts_per_connection: usize,
    /// Time allowed for rerouting after one step.
    pub reroute_seconds: f64,
    /// Footprints that may not move, by index.
    pub locked: HashSet<usize>,
    pub pours_block: bool,
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub log: Option<Log>,
}

impl Default for NudgeOptions {
    fn default() -> NudgeOptions {
        NudgeOptions {
            cell: 50_000,
            via_cost: 80,
            max_seconds: None,
            steps: vec![250_000, 500_000, 1_000_000],
            reach: 2_000_000,
            parts_per_connection: 6,
            reroute_seconds: 5.0,
            locked: HashSet::new(),
            pours_block: false,
            cancel: None,
            log: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NudgeStats {
    pub tried: usize,
    pub illegal: usize,
    pub kept: usize,
    /// Footprints moved, by index and how far in all.
    pub moved: Vec<(usize, Point)>,
    /// Copper from the board file that moving parts tore off, for removing from the file.
    pub ripped_spans: Vec<Span>,
}

const DIRECTIONS: [(i64, i64); 8] = [(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)];

fn open_count(board: &Board, pours_block: bool) -> usize {
    if pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines.len()
}

/// The footprint an item belongs to, if a pad.
fn owner(board: &Board, id: ItemId) -> Option<usize> {
    match board.item(id)?.kind {
        Kind::Pad { footprint, .. } => Some(footprint),
        _ => None,
    }
}

/// Whether the footprint, where it now stands, breaks no rule: its pads keep their clearances
/// and its courtyard overlaps no other on the same side.
fn legal(board: &Board, footprint: usize) -> bool {
    let pads: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Pad { footprint: f, .. } if f == footprint)).map(|(id, _)| id).collect();
    if pads.iter().any(|&p| !check_item(board, p).is_empty()) {
        return false;
    }
    let me = &board.footprints[footprint];
    let Some(yard) = &me.courtyard else { return true };
    let mine = Shape::Polygon(yard.clone());
    let bb = mine.bbox();
    board.footprints.iter().enumerate().all(|(i, other)| {
        if i == footprint || other.back != me.back {
            return true;
        }
        let Some(theirs) = &other.courtyard else { return true };
        let theirs = Shape::Polygon(theirs.clone());
        !theirs.bbox().overlaps(bb) || theirs.gap(&mine) > 0.0
    })
}

/// Where the part's pads centre.
fn centre(board: &Board, footprint: usize) -> Point {
    let (mut x, mut y, mut n) = (0i128, 0i128, 0i128);
    for (_, it) in board.items().filter(|(_, it)| matches!(it.kind, Kind::Pad { footprint: f, .. } if f == footprint)) {
        let b = it.shape.bbox();
        x += ((b.min.x + b.max.x) / 2) as i128;
        y += ((b.min.y + b.max.y) / 2) as i128;
        n += 1;
    }
    if n == 0 {
        return board.footprints[footprint].at;
    }
    Point::new((x / n) as i64, (y / n) as i64)
}

/// How far around a part the copper flows with it: its courtyard's half-diagonal and a margin.
fn reach_of(board: &Board, footprint: usize) -> f64 {
    let half = board.footprints[footprint].courtyard.as_ref().map_or(500_000.0, |yard| {
        let b = Shape::Polygon(yard.clone()).bbox();
        ((b.max.x - b.min.x) as f64).hypot((b.max.y - b.min.y) as f64) / 2.0
    });
    half + 500_000.0
}

/// Unlocked footprints whose courtyard comes within `reach` of the segment, nearest first, and
/// the footprints at its two ends.
fn near_airline(board: &Board, a: Point, b: Point, ends: [Option<usize>; 2], options: &NudgeOptions) -> Vec<usize> {
    let mut found: Vec<(f64, usize)> = board
        .footprints
        .iter()
        .enumerate()
        .filter(|(i, f)| !f.locked && !options.locked.contains(i))
        .filter_map(|(i, f)| {
            let yard = f.courtyard.as_ref()?;
            let d = yard.iter().map(|p| point_segment(*p, a, b)).fold(f64::INFINITY, f64::min);
            (d <= options.reach as f64).then_some((d, i))
        })
        .collect();
    for end in ends.into_iter().flatten() {
        if !board.footprints[end].locked && !options.locked.contains(&end) {
            found.push((-1.0, end));
        }
    }
    found.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut out: Vec<usize> = Vec::new();
    for (_, i) in found {
        if !out.contains(&i) {
            out.push(i);
        }
    }
    out.truncate(options.parts_per_connection);
    out
}

/// Nudges parts until a round frees nothing more. `soft` holds the copper the router laid.
pub fn nudge(board: &mut Board, solver: &mut dyn FieldSolver, options: &NudgeOptions, soft: &mut Vec<ItemId>, watch: &mut dyn FnMut(&Board)) -> NudgeStats {
    let started = std::time::Instant::now();
    let out_of_time = || {
        options.max_seconds.is_some_and(|limit| started.elapsed().as_secs_f64() > limit)
            || options.cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    };
    let mut stats = NudgeStats::default();
    loop {
        let open = if options.pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines;
        if open.is_empty() || out_of_time() {
            break;
        }
        let kept_before = stats.kept;
        'connections: for airline in open {
            if board.item(airline.from_item).is_none() || board.item(airline.to_item).is_none() {
                continue;
            }
            let ends = [owner(board, airline.from_item), owner(board, airline.to_item)];
            for footprint in near_airline(board, airline.from, airline.to, ends, options) {
                for &step in &options.steps {
                    for (dx, dy) in DIRECTIONS {
                        if out_of_time() {
                            break 'connections;
                        }
                        stats.tried += 1;
                        let before = open_count(board, options.pours_block);
                        let snapshot = board.clone();
                        let soft_before = soft.clone();
                        let by = Point::new(dx * step, dy * step);
                        let reference = board.footprints[footprint].reference.clone();
                        tell(
                            &options.log,
                            "nudging",
                            || format!("{reference}: trying {:+.2}, {:+.2} mm for {}", (dx * step) as f64 * 1e-6, -(dy * step) as f64 * 1e-6, board.net_name(airline.net)),
                            Some(Focus::Part(footprint)),
                        );
                        // The part steps and the copper around it flows aside with it, its own
                        // traces dragged along; copper is ripped only where it would be pushed into
                        // something that cannot move.
                        let field = Field { center: centre(board, footprint), outward: 0.0, vector: (by.x as f64, by.y as f64), radius: reach_of(board, footprint), decay: 2.0 };
                        let only = Some(HashSet::from([footprint]));
                        let pushed = push(board, &field, &PushOptions { steps: 1, locked: options.locked.clone(), only, pours_block: options.pours_block, log: None }, soft, &mut |_| {});
                        let Some(&(_, by)) = pushed.moved.first() else {
                            stats.illegal += 1;
                            *board = snapshot;
                            *soft = soft_before;
                            continue;
                        };
                        let ripped = pushed.removed_spans;
                        if !legal(board, footprint) {
                            stats.illegal += 1;
                            *board = snapshot;
                            *soft = soft_before;
                            continue;
                        }
                        let reroute = Options {
                            cell: options.cell,
                            via_cost: options.via_cost,
                            max_seconds: Some(options.reroute_seconds),
                            pours_block: options.pours_block,
                            cancel: options.cancel.clone(),
                            log: None,
                        };
                        let again = route_open(board, solver, &reroute, &mut |_| {});
                        if open_count(board, options.pours_block) < before {
                            soft.retain(|id| board.item(*id).is_some());
                            soft.extend(again.laid);
                            stats.kept += 1;
                            tell(&options.log, "nudging", || format!("{reference}: moved, {} connections open", open_count(board, options.pours_block)), None);
                            stats.ripped_spans.extend(ripped);
                            match stats.moved.iter_mut().find(|(f, _)| *f == footprint) {
                                Some((_, total)) => *total = Point::new(total.x + by.x, total.y + by.y),
                                None => stats.moved.push((footprint, by)),
                            }
                            watch(board);
                            continue 'connections;
                        }
                        *board = snapshot;
                        *soft = soft_before;
                    }
                }
            }
        }
        if stats.kept == kept_before {
            break;
        }
    }
    soft.retain(|id| board.item(*id).is_some());
    stats
}
