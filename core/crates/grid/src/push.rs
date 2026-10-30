//! A push: every part and every piece of copper free to move flows with one smooth displacement
//! field, given as a function from a point to its move. Only locked parts and locked copper stand
//! still. Nudging pushes one part at a time this way.
//!
//! Parts move rigidly, each by the field at its pads' centre, and take a smaller step (or none)
//! where a full one would put their pads or courtyard into something that cannot move. The copper
//! is carried by a warp field rigid on every moving part and still on everything that stays
//! (`morph::flow`), so it flows around the parts and joints hold.
//! Copper is ripped only where the flow has pushed it into something it may not touch; the
//! caller routes the connections that opens again. The field is applied in small steps, each
//! shown to `watch`, so the flow can be watched.

use std::collections::{HashMap, HashSet};

use board::{check_item, check_item_with_pours, nm, Board, Item, ItemId, Kind, Point, Shape};
use kicad_pcb::Span;

use crate::report::{tell, Log};

#[derive(Debug, Clone)]
pub struct PushOptions {
    /// Steps the field is applied in; each is shown.
    pub steps: usize,
    /// Footprints that may not move, by index, besides those locked on the board.
    pub locked: HashSet<usize>,
    /// When set, only these footprints may move.
    pub only: Option<HashSet<usize>>,
    pub pours_block: bool,
    /// Steps not begun by then are not taken.
    pub deadline: crate::clock::Deadline,
    pub log: Option<Log>,
}

impl Default for PushOptions {
    fn default() -> PushOptions {
        PushOptions { steps: 20, locked: HashSet::new(), only: None, pours_block: false, deadline: Default::default(), log: None }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PushStats {
    /// Each part moved and how far in all.
    pub moved: Vec<(usize, Point)>,
    /// Part steps cut short, or refused, by something that cannot move.
    pub held_back: usize,
    /// Copper items moved, counted once per step.
    pub flowed: usize,
    /// Copper ripped where the flow pushed it into something it may not touch.
    pub ripped: usize,
    /// Copper from the board file that moved or was ripped, for removing from the file; what
    /// moved is in `soft` and is written back from there.
    pub removed_spans: Vec<Span>,
}

fn rounded((x, y): (f64, f64)) -> Point {
    Point::new(x.round() as i64, y.round() as i64)
}

fn pads_of(board: &Board, footprint: usize) -> Vec<ItemId> {
    board.items().filter(|(_, it)| matches!(it.kind, Kind::Pad { footprint: f, .. } if f == footprint)).map(|(id, _)| id).collect()
}

fn centre_of(board: &Board, pads: &[ItemId]) -> Option<Point> {
    let (mut x, mut y, mut n) = (0i128, 0i128, 0i128);
    for &p in pads {
        let b = board.item(p)?.shape.bbox();
        x += ((b.min.x + b.max.x) / 2) as i128;
        y += ((b.min.y + b.max.y) / 2) as i128;
        n += 1;
    }
    (n > 0).then(|| Point::new((x / n) as i64, (y / n) as i64))
}

/// Whether the footprint, where it now stands, is clear of everything that will not flow out of
/// its way: pads, locked copper, keepouts, the edge, other courtyards on its side, and every
/// other part's holes. KiCad holds some unplated holes clear of other parts' pads and not
/// others, by a rule not yet pinned down, so a part that moves keeps clear of them all.
pub(crate) fn part_clear(board: &Board, footprint: usize, pads: &[ItemId], flowing: &HashSet<ItemId>, pours_block: bool) -> bool {
    let hole_clearance = nm(board.rules.min_hole_clearance) as f64;
    let holes = board.layers.len();
    for &p in pads {
        let found = if pours_block { check_item_with_pours(board, p) } else { check_item(board, p) };
        if found.iter().any(|v| v.b.is_none_or(|b| !flowing.contains(&b))) {
            return false;
        }
        let Some(pad) = board.item(p) else { continue };
        for other in board.near(holes, pad.shape.bbox().grow(hole_clearance as i64)) {
            let Some(o) = board.item(other) else { continue };
            let theirs = matches!(o.kind, Kind::Pad { footprint: f, .. } if f != footprint);
            if let (true, Some((c, r))) = (theirs, o.hole) {
                if pad.shape.gap(&Shape::Capsule { a: c, b: c, r }) < hole_clearance {
                    return false;
                }
            }
        }
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

/// One step of [`push_watch`], after parts and copper flowed and broken copper was ripped.
pub struct PushStep<'a> {
    pub step: usize,
    pub moves: &'a HashMap<usize, Point>,
    pub held_back: usize,
    pub flowed: usize,
    pub ripped: &'a [Item],
}

/// Pushes everything free to move along `field`, each point's move in nanometres. `soft` holds
/// the router's copper; copper from the board file that moves joins it, as it must now be written
/// back.
pub fn push(board: &mut Board, field: &dyn Fn(Point) -> (f64, f64), options: &PushOptions, soft: &mut Vec<ItemId>, watch: &mut dyn FnMut(&Board)) -> PushStats {
    push_watch(board, field, options, soft, &mut |b, _| watch(b))
}

/// As [`push`], reporting each step's part moves and ripped copper to `watch`.
pub fn push_watch(
    board: &mut Board,
    field: &dyn Fn(Point) -> (f64, f64),
    options: &PushOptions,
    soft: &mut Vec<ItemId>,
    watch: &mut dyn FnMut(&Board, PushStep),
) -> PushStats {
    let mut stats = PushStats::default();
    let steps = options.steps.max(1);
    let still = |board: &Board, f: usize| {
        board.footprints.get(f).is_none_or(|fp| fp.locked) || options.locked.contains(&f) || options.only.as_ref().is_some_and(|only| !only.contains(&f))
    };
    let mut total: HashMap<usize, Point> = HashMap::new();
    for step in 0..steps {
        if options.deadline.expired() {
            break;
        }
        let flowing: HashSet<ItemId> =
            board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked).map(|(id, _)| id).collect();

        // Each part's step, by the field at its centre, cut back while it would land on something
        // that cannot move. Parts stand where they would go while the rest are judged, and go
        // back after, for the flow to carry them with their copper.
        let mut moves: HashMap<usize, Point> = HashMap::new();
        let mut step_held_back = 0;
        for f in 0..board.footprints.len() {
            if still(board, f) {
                continue;
            }
            let pads = pads_of(board, f);
            let Some(c) = centre_of(board, &pads) else { continue };
            let (dx, dy) = field(c);
            let full = (dx / steps as f64, dy / steps as f64);
            if rounded(full) == Point::new(0, 0) {
                continue;
            }
            let mut kept = None;
            for scale in [1.0, 0.5, 0.25] {
                let by = rounded((full.0 * scale, full.1 * scale));
                board.shift_footprint(f, by);
                if part_clear(board, f, &pads, &flowing, options.pours_block) {
                    kept = Some(by);
                    break;
                }
                board.shift_footprint(f, Point::new(-by.x, -by.y));
            }
            if kept != Some(rounded(full)) {
                stats.held_back += 1;
                step_held_back += 1;
            }
            if let Some(by) = kept {
                moves.insert(f, by);
            }
        }
        for (&f, by) in &moves {
            board.shift_footprint(f, Point::new(-by.x, -by.y));
            let t = total.entry(f).or_insert(Point::new(0, 0));
            *t = Point::new(t.x + by.x, t.y + by.y);
        }

        // Then everything moves together, the copper carried by one warp field.
        let flowed = morph::flow(board, &moves, soft);
        stats.flowed += flowed.flowed;
        stats.removed_spans.extend(flowed.removed_spans);

        // Copper the flow pushed into something it may not touch is ripped, to be routed again.
        let (ripped_items, spans) = rip_broken_items(board, soft, options.pours_block);
        stats.ripped += ripped_items.len();
        stats.removed_spans.extend(spans);
        tell(&options.log, "pushing", || format!("step {} of {steps}: {} parts moved, {} ripped so far", step + 1, moves.len(), stats.ripped), None);
        watch(board, PushStep { step, moves: &moves, held_back: step_held_back, flowed: flowed.flowed, ripped: &ripped_items });
    }
    let mut moved: Vec<(usize, Point)> = total.into_iter().filter(|(_, p)| *p != Point::new(0, 0)).collect();
    moved.sort_unstable_by_key(|(f, _)| *f);
    stats.moved = moved;
    stats
}

/// Rips every unlocked track and via that breaks a rule, to be routed again: how many, and the
/// board file's spans among them.
pub fn rip_broken(board: &mut Board, soft: &mut Vec<ItemId>, pours_block: bool) -> (usize, Vec<Span>) {
    let (items, spans) = rip_broken_items(board, soft, pours_block);
    (items.len(), spans)
}

/// As [`rip_broken`], returning the ripped items themselves so callers can inspect or draw them.
pub fn rip_broken_items(board: &mut Board, soft: &mut Vec<ItemId>, pours_block: bool) -> (Vec<Item>, Vec<Span>) {
    let mut ids: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked).map(|(id, _)| id).collect();
    ids.sort_unstable();
    let (mut ripped, mut spans) = (Vec::new(), Vec::new());
    for id in ids {
        if board.item(id).is_none() {
            continue;
        }
        let broken = if pours_block { check_item_with_pours(board, id) } else { check_item(board, id) };
        if !broken.is_empty() {
            if let Some(mut it) = board.remove(id) {
                spans.extend(it.span.take());
                ripped.push(it);
            }
        }
    }
    soft.retain(|id| board.item(*id).is_some());
    (ripped, spans)
}
