//! The router's stages, one per step of a run, in the order `lib.rs`'s [`crate::STAGES`] table
//! runs them with the budgets it gives them. Each takes the board and the copper the run holds
//! as its own ([`Ledger`]) from the stage before, prints its line as it always has, and returns
//! its own figures for the run's stats.

use std::collections::{HashMap, HashSet};

use board::{Board, Item, ItemId, Kind, Nm, Point, Shape};
use kicad_pcb::{apply, Edit, Span};
use serde_json::{json, Value};

use crate::stats::{Failure, Finding, RunStats};
use crate::watch::{Cleaned, Watch};
use crate::{open_airlines, regions, Args, Budget, RunClock, ViaCosts};

/// The copper the run holds as its own, and what passes from one stage to the next.
#[derive(Default)]
pub struct Ledger {
    /// The router's own copper: free to rip, move and rewrite, and written out at the end.
    pub laid: Vec<ItemId>,
    /// Escapes laid before routing, held apart until the grid has had its pass, so the ones no
    /// route used can be taken out again.
    pub fanout: Vec<ItemId>,
    /// The differential pairs' copper, locked through the single-ended stages.
    pub pairs: Vec<ItemId>,
    /// The board file's spans of what was taken out.
    pub removed: Vec<Span>,
    /// Where each part stood before the run.
    pub(crate) original_at: Vec<Point>,
    /// Nets with a length limit, and those nets, routed first.
    pub(crate) lengths: Vec<regions::MaxLength>,
    pub(crate) first: HashSet<u32>,
    pub(crate) costs: ViaCosts,
    /// What routing one connection at a time after negotiation did.
    pub(crate) routed: grid::Stats,
    /// Tightening's figures, told once the via pass has pulled tight again.
    pub(crate) tightened: Option<grid::CleanStats>,
    /// Connections open at the end, and the board's text as written.
    pub(crate) open: usize,
    pub(crate) output: Option<String>,
}

/// What a stage works with.
pub struct StageCtx<'a> {
    pub args: &'a Args,
    pub board: &'a mut Board,
    pub ledger: &'a mut Ledger,
    pub solver: &'a mut dyn grid::FieldSolver,
    pub log: &'a Option<grid::Log>,
    pub watch: &'a mut dyn Watch,
    pub stats: &'a mut RunStats,
    pub threads: usize,
    pub(crate) clock: &'a RunClock,
    pub(crate) budget: Budget,
    pub(crate) text: &'a str,
}

impl StageCtx<'_> {
    /// The stage's deadline from its budget, as of now.
    pub fn deadline(&self) -> grid::Deadline {
        self.budget.deadline(self.clock, !self.ledger.first.is_empty())
    }

    /// Seconds since the run's clock started, as the stages' lines print it.
    pub fn elapsed(&self) -> f64 {
        self.clock.started.elapsed().as_secs_f64()
    }

    /// Tells the log, if anyone listens.
    pub fn say(&self, phase: &'static str, text: String) {
        if let Some(grid::Log(log)) = self.log {
            log(grid::Activity { phase, text, focus: None });
        }
    }

    fn open(&self) -> Vec<board::Airline> {
        open_airlines(self.args, self.board)
    }

    /// Each connection a route left given up, for the stats.
    fn failed(&mut self, stage: &'static str, failures: &[(ItemId, ItemId, &'static str)]) {
        for &(from, to, reason) in failures {
            let net = self.board.item(from).map_or("", |it| self.board.net_name(it.net)).to_string();
            let (from, to) = (grid::item_name(self.board, from), grid::item_name(self.board, to));
            self.stats.failures.push(Failure { stage, net, from, to, reason });
        }
    }
}

/// One stage of a run.
pub trait Stage: Sync {
    fn name(&self) -> &'static str;
    /// Whether the run's settings and the board ask for it.
    fn enabled(&self, _a: &Args, _board: &Board, _ledger: &Ledger) -> bool {
        true
    }
    /// Runs it, returning its own figures for the stats.
    fn run(&self, ctx: &mut StageCtx) -> Value;
}

/// Before routing: the router's own copper adopted, the planner's regions and the via and pour
/// prices taken, the discs to rebuild ripped.
pub struct SetUp;

impl Stage for SetUp {
    fn name(&self) -> &'static str {
        "set-up"
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let (a, board, ledger) = (ctx.args, &mut *ctx.board, &mut *ctx.ledger);
        if a.adopt {
            let mine: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked).map(|(id, _)| id).collect();
            for id in mine {
                if let Some(mut it) = board.remove(id) {
                    ledger.removed.extend(it.span.take());
                    board.restore(id, it);
                    ledger.laid.push(id);
                }
            }
        }
        if let Some(c) = a.via_avoid_cost {
            board.via_avoid_cost = c;
        }
        if let Some(w) = a.pour_cost {
            board.pour_cost = w;
        }
        ledger.lengths = match &a.regions {
            Some(path) => regions::load(path, board).unwrap_or_else(|e| panic!("{e}")),
            None => Vec::new(),
        };
        // Nets with a length limit route first, while their shortest ways are still free.
        ledger.first = ledger.lengths.iter().flat_map(|m| m.nets.iter().copied()).collect();
        for &[x0, y0, x1, y1] in &a.via_avoid {
            let p = |x: f64, y: f64| Point::new((x * 1e6).round() as i64, (y * 1e6).round() as i64);
            board.via_avoid.push(Shape::Polygon(vec![p(x0, y0), p(x1, y0), p(x1, y1), p(x0, y1)]));
        }
        for &(x, y, r) in &a.rip_regions {
            let disc = Shape::Capsule { a: Point::new((x * 1e6).round() as i64, (y * 1e6).round() as i64), b: Point::new((x * 1e6).round() as i64, (y * 1e6).round() as i64), r: (r * 1e6).round() as i64 };
            let inside: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked && it.shape.gap(&disc) == 0.0).map(|(id, _)| id).collect();
            for id in inside {
                if let Some(it) = board.remove(id) {
                    ledger.removed.extend(it.span);
                }
            }
            ledger.laid.retain(|id| board.item(*id).is_some());
        }
        ledger.costs = crate::via_costs(a, board);
        let connections = board::connectivity_without_pours(board).airlines.len();
        ctx.say("start", format!("{connections} connections to make"));
        json!({"connections": connections, "length_limited_nets": ctx.ledger.first.len()})
    }
}

/// Escapes every fine-pitch part's pads before routing.
pub struct Prefanout;

impl Stage for Prefanout {
    fn name(&self) -> &'static str {
        "prefanout"
    }

    fn enabled(&self, a: &Args, _: &Board, _: &Ledger) -> bool {
        a.prefanout
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let a = ctx.args;
        let parts = morph::fine_pitch(ctx.board, 500_000);
        let defaults = grid::PrefanoutOptions::default();
        // Open differential pairs are held back from pre-fanout so `route_pairs` can fan out and
        // couple their pads together; any pair pad still open afterwards is escaped after.
        let skip_nets: HashSet<u32> = if a.diff_pairs {
            grid::diff_pairs(ctx.board)
                .iter()
                .filter(|p| grid::shares_layer(ctx.board, p) && grid::pair_has_open_runs(ctx.board, p, a.pours_block))
                .flat_map(|p| [p.p, p.n])
                .collect()
        } else {
            HashSet::new()
        };
        let options = grid::PrefanoutOptions {
            global_cell: a.cell.unwrap_or(defaults.global_cell),
            ring: a.prefanout_ring.unwrap_or(defaults.ring),
            mode: a.prefanout_mode.unwrap_or(defaults.mode),
            order: a.prefanout_order.unwrap_or(defaults.order),
            inward: true,
            parts,
            skip_nets,
            pours_block: a.pours_block,
            deadline: ctx.deadline(),
            threads: ctx.threads,
            log: ctx.log.clone(),
            ..defaults
        };
        let watch = &mut *ctx.watch;
        let f = grid::prefanout(ctx.board, &options, &mut |b| watch.frame(b));
        ctx.say("prefanout", format!("{} parts, {} inward bridges, {} of {} pads escaped ({} vias)", f.parts, f.bridged_inward, f.escaped, f.candidates, f.vias));
        eprintln!(
            "prefanout: {} parts, {} inward, {} of {} escaped ({} failed), {} vias, skipped {} no-net {} pth {} exposed {} escaped, {:.1} s",
            f.parts,
            f.bridged_inward,
            f.escaped,
            f.candidates,
            f.failed,
            f.vias,
            f.skipped_no_net,
            f.skipped_pth,
            f.skipped_exposed,
            f.skipped_escaped,
            ctx.elapsed()
        );
        ctx.watch.step(ctx.board);
        ctx.ledger.fanout = f.laid;
        json!({"parts": f.parts, "inward": f.bridged_inward, "escaped": f.escaped, "candidates": f.candidates, "failed": f.failed, "vias": f.vias})
    }
}

/// Plane pads not on their plane get a via down to it beside them, as the router's own copper
/// a rip-up may take up again.
pub struct Planes;

impl Stage for Planes {
    fn name(&self) -> &'static str {
        "planes"
    }

    fn enabled(&self, _: &Args, board: &Board, _: &Ledger) -> bool {
        board.planes != 0
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let p = grid::join_planes(ctx.board, ctx.log, &ctx.deadline());
        ctx.say("planes", format!("{} of {} pads joined to their planes by a via", p.joined, p.pads));
        eprintln!("planes: {} of {} pads off their planes joined, {:.1} s", p.joined, p.pads, ctx.elapsed());
        ctx.watch.step(ctx.board);
        ctx.ledger.laid.extend(p.laid);
        json!({"pads": p.pads, "joined": p.joined})
    }
}

/// Differential pairs next, round the escapes, and held apart from the router's own copper so
/// nothing rips or straightens them apart; the legs to their pads are routed as usual.
pub struct Pairs;

impl Stage for Pairs {
    fn name(&self) -> &'static str {
        "pairs"
    }

    fn enabled(&self, a: &Args, _: &Board, _: &Ledger) -> bool {
        a.diff_pairs
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let a = ctx.args;
        // Coupling end to end may run until a quarter of the time is gone; later runs are laid
        // the legacy way, as the pairs are a small part of a board, the rest of it still to route.
        let options = grid::PairOptions { cell: a.cell.unwrap_or(grid::PairOptions::default().cell), pours_block: a.pours_block, deadline: ctx.deadline(), log: ctx.log.clone() };
        let watch = &mut *ctx.watch;
        let p = grid::route_pairs_watch(ctx.board, &options, &mut |b, run| watch.pair_run(b, run));
        eprintln!("pairs: {} pairs, {} of {} runs coupled, {:.1} mm, {:.1} s", p.pairs, p.coupled, p.runs, p.coupled_mm, ctx.elapsed());
        ctx.watch.step(ctx.board);
        ctx.ledger.pairs.extend(p.laid);
        let board = &*ctx.board;
        ctx.ledger.fanout.retain(|&id| board.item(id).is_some_and(|it| !it.locked));
        json!({"pairs": p.pairs, "runs": p.runs, "coupled": p.coupled, "coupled_mm": p.coupled_mm})
    }
}

/// The pads of pairs left open at fine-pitch parts, escaped now that the pairs are laid.
pub struct PairEscapes;

impl Stage for PairEscapes {
    fn name(&self) -> &'static str {
        "pair-escapes"
    }

    fn enabled(&self, a: &Args, _: &Board, _: &Ledger) -> bool {
        a.prefanout
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let a = ctx.args;
        let board = &*ctx.board;
        let fine: HashSet<usize> = morph::fine_pitch(board, 500_000).into_iter().collect();
        let pair_nets: HashSet<u32> = grid::diff_pairs(board).iter().flat_map(|pr| [pr.p, pr.n]).collect();
        let open = ctx.open();
        let open_fine_pair_nets: HashSet<u32> = open
            .iter()
            .filter(|air| pair_nets.contains(&air.net))
            .filter(|air| {
                [air.from_item, air.to_item].iter().any(|&id| board.item(id).is_some_and(|it| matches!(it.kind, Kind::Pad { footprint, .. } if fine.contains(&footprint))))
            })
            .map(|air| air.net)
            .collect();
        if open_fine_pair_nets.is_empty() {
            return json!({"nets": 0});
        }
        let skip_others: HashSet<u32> = (1..board.nets.len() as u32).filter(|n| !open_fine_pair_nets.contains(n)).collect();
        let defaults = grid::PrefanoutOptions::default();
        let options = grid::PrefanoutOptions {
            global_cell: a.cell.unwrap_or(defaults.global_cell),
            ring: a.prefanout_ring.unwrap_or(defaults.ring),
            mode: a.prefanout_mode.unwrap_or(defaults.mode),
            order: a.prefanout_order.unwrap_or(defaults.order),
            inward: false,
            parts: fine.into_iter().collect(),
            skip_nets: skip_others,
            pours_block: a.pours_block,
            deadline: ctx.deadline(),
            threads: ctx.threads,
            log: ctx.log.clone(),
            ..defaults
        };
        let watch = &mut *ctx.watch;
        let f = grid::prefanout(ctx.board, &options, &mut |b| watch.frame(b));
        ctx.ledger.fanout.extend(f.laid);
        json!({"nets": open_fine_pair_nets.len(), "escaped": f.escaped, "vias": f.vias})
    }
}

/// Routing and the stages that rescue what it leaves open: the stages within it run only if the
/// run's time has not gone when it starts.
pub struct Routing;

impl Stage for Routing {
    fn name(&self) -> &'static str {
        "routing"
    }

    fn run(&self, _: &mut StageCtx) -> Value {
        Value::Null
    }
}

/// Nets with a length limit, routed one at a time before negotiation.
pub struct First;

impl Stage for First {
    fn name(&self) -> &'static str {
        "first"
    }

    fn enabled(&self, _: &Args, _: &Board, ledger: &Ledger) -> bool {
        !ledger.first.is_empty()
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let a = ctx.args;
        let first = ctx.ledger.first.clone();
        ctx.say("first", format!("routing {} length-limited nets one at a time before negotiation", first.len()));
        let options = grid::Options {
            cell: a.cell.unwrap_or(grid::Options::default().cell),
            via_cost: ctx.ledger.costs.open,
            directional: true,
            seed: a.seed,
            deadline: ctx.deadline(),
            nets: Some(first.clone()),
            pours_block: a.pours_block,
            log: ctx.log.clone(),
        };
        let watch = &mut *ctx.watch;
        let s = grid::route_open(ctx.board, ctx.solver, &options, &mut |b| watch.frame(b));
        ctx.ledger.laid.extend(s.laid.iter().copied());
        eprintln!("first: {} nets, {} routed, {} failed, {} vias, {:.1} s", first.len(), s.routed, s.failed, s.vias, ctx.elapsed());
        ctx.failed("first", &s.failures);
        json!({"nets": first.len(), "routed": s.routed, "failed": s.failed, "vias": s.vias})
    }
}

/// Negotiated congestion: every connection routed at once, round after round, until none share.
pub struct Negotiation;

impl Stage for Negotiation {
    fn name(&self) -> &'static str {
        "negotiation"
    }

    fn enabled(&self, a: &Args, _: &Board, _: &Ledger) -> bool {
        a.negotiate
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let a = ctx.args;
        let defaults = grid::NegotiationOptions::default();
        let options = grid::NegotiationOptions {
            // With length-limited nets routed first, half of what is left; else half the run.
            deadline: ctx.deadline(),
            threads: ctx.threads,
            cell: a.cell.unwrap_or(defaults.cell),
            via_cost: ctx.ledger.costs.negotiate,
            pours_block: a.pours_block,
            seed: a.seed,
            log: ctx.log.clone(),
            max_rounds: a.negotiate_rounds.unwrap_or(defaults.max_rounds),
            patience: a.negotiate_patience.unwrap_or(defaults.patience),
            present_growth: a.negotiate_growth.unwrap_or(defaults.present_growth),
            repasses: a.negotiate_repasses.unwrap_or(defaults.repasses),
            jacobi: a.negotiate_jacobi,
            reroute_all: a.negotiate_reroute_all,
            ..Default::default()
        };
        // The board, the trial routes and the rounds go to one watcher, never at once.
        let watch = std::cell::RefCell::new(&mut *ctx.watch);
        let wants_rounds = watch.borrow().wants_rounds();
        let mut round = |view: grid::RoundView| watch.borrow_mut().round(view);
        let n = grid::route_negotiated_rounds(
            ctx.board,
            ctx.solver,
            &options,
            &mut |b| watch.borrow_mut().frame(b),
            &mut |r, trials| watch.borrow_mut().trials(r, trials),
            if wants_rounds { Some(&mut round) } else { None },
        );
        eprintln!("negotiated {} of {} connections in {} rounds, {} vias, {:.1} s", n.committed, n.connections, n.rounds, n.vias, ctx.elapsed());
        ctx.ledger.laid.extend(n.laid);
        json!({"connections": n.connections, "committed": n.committed, "rounds": n.rounds, "conflicted": n.conflicted, "unroutable": n.unroutable, "vias": n.vias})
    }
}

/// What negotiation left, routed one connection at a time.
pub struct Finishing;

impl Stage for Finishing {
    fn name(&self) -> &'static str {
        "finishing"
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let a = ctx.args;
        ctx.say("finishing", "routing what negotiation left, one connection at a time".to_string());
        let options = grid::Options {
            cell: a.cell.unwrap_or(grid::Options::default().cell),
            via_cost: ctx.ledger.costs.open,
            seed: a.seed,
            deadline: ctx.deadline(),
            pours_block: a.pours_block,
            log: ctx.log.clone(),
            ..Default::default()
        };
        let watch = &mut *ctx.watch;
        let s = grid::route_open(ctx.board, ctx.solver, &options, &mut |b| watch.frame(b));
        ctx.ledger.laid.extend(s.laid.iter().copied());
        ctx.failed("finishing", &s.failures);
        let detail = json!({"routed": s.routed, "failed": s.failed, "no_terminal": s.no_terminal, "no_path": s.no_path, "refused": s.refused, "vias": s.vias});
        ctx.ledger.routed = s;
        detail
    }
}

/// The escapes no route used, taken out again.
pub struct PruneEscapes;

impl Stage for PruneEscapes {
    fn name(&self) -> &'static str {
        "prune-escapes"
    }

    fn enabled(&self, _: &Args, _: &Board, ledger: &Ledger) -> bool {
        !ledger.fanout.is_empty()
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let (pruned, spans) = grid::prune_closed_escapes(ctx.board, &mut ctx.ledger.fanout, ctx.args.pours_block);
        ctx.ledger.removed.extend(spans);
        json!({"pruned": pruned})
    }
}

/// Rip-up and reroute: an open connection routed through the router's own copper at a price,
/// the copper in its way rerouted.
pub struct RipUp;

impl Stage for RipUp {
    fn name(&self) -> &'static str {
        "rip-up"
    }

    fn enabled(&self, a: &Args, _: &Board, _: &Ledger) -> bool {
        a.rip_up
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let a = ctx.args;
        let options = grid::RipOptions {
            cell: a.cell.unwrap_or(grid::RipOptions::default().cell),
            via_cost: ctx.ledger.costs.rip,
            deadline: ctx.deadline(),
            pours_block: a.pours_block,
            log: ctx.log.clone(),
            ..Default::default()
        };
        let watch = &mut *ctx.watch;
        let r = grid::rip_up_watch(ctx.board, ctx.solver, &options, &mut ctx.ledger.laid, &mut |b, attempt| watch.rip(b, &attempt));
        eprintln!(
            "rip-up: {} tries, {} kept, {} ripped; lost {} with no way through, {} to copper it may not rip, {} no better; {:.1} s",
            r.attempts,
            r.kept,
            r.ripped,
            r.no_path,
            r.hard_conflict,
            r.no_better,
            ctx.elapsed()
        );
        json!({"tries": r.attempts, "kept": r.kept, "ripped": r.ripped, "no_path": r.no_path, "hard_conflict": r.hard_conflict, "no_better": r.no_better})
    }
}

/// The escapes join the router's own copper, for the stages after to move and clean.
pub struct MergeEscapes;

impl Stage for MergeEscapes {
    fn name(&self) -> &'static str {
        "merge-escapes"
    }

    fn enabled(&self, _: &Args, _: &Board, ledger: &Ledger) -> bool {
        !ledger.fanout.is_empty()
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let n = ctx.ledger.fanout.len();
        ctx.ledger.laid.append(&mut ctx.ledger.fanout);
        json!({"escapes": n})
    }
}

/// The plough, for connections still open: then, if pair copper walls one in, the pair copper
/// near it is let go and the plough tries again.
pub struct Shove;

impl Stage for Shove {
    fn name(&self) -> &'static str {
        "shove"
    }

    fn enabled(&self, a: &Args, _: &Board, _: &Ledger) -> bool {
        a.shove
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let a = ctx.args;
        let deadline = ctx.deadline();
        let started = ctx.clock.started;
        let (mut passes, mut unlocked) = (shove_pass(a, ctx.board, ctx.solver, &mut ctx.ledger.laid, started, &deadline, ctx.log, ctx.watch, &mut ctx.ledger.removed), 0);
        if a.diff_pairs && !ctx.ledger.pairs.is_empty() {
            let open = ctx.open();
            unlocked = unlock_pairs_near_open(ctx.board, &ctx.ledger.pairs, &open);
            if unlocked > 0 {
                passes += shove_pass(a, ctx.board, ctx.solver, &mut ctx.ledger.laid, started, &deadline, ctx.log, ctx.watch, &mut ctx.ledger.removed);
                relock_pairs(ctx.board, &ctx.ledger.pairs);
            }
        }
        json!({"passes": passes, "pair_pieces_unlocked": unlocked})
    }
}

/// Parts moved a step at a time, as blame asks, where that frees a connection.
pub struct Nudge;

impl Stage for Nudge {
    fn name(&self) -> &'static str {
        "nudge"
    }

    fn enabled(&self, a: &Args, _: &Board, _: &Ledger) -> bool {
        a.nudge
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let a = ctx.args;
        let locked = locked_footprints(ctx.board, &ctx.ledger.pairs);
        let options = grid::NudgeOptions {
            cell: a.cell.unwrap_or(grid::NudgeOptions::default().cell),
            deadline: ctx.deadline(),
            locked,
            pours_block: a.pours_block,
            log: ctx.log.clone(),
            ..Default::default()
        };
        let watch = &mut *ctx.watch;
        let n = grid::nudge_watch(ctx.board, ctx.solver, &options, &mut ctx.ledger.laid, &mut |b, event| watch.nudge(b, &event));
        ctx.ledger.removed.extend(n.ripped_spans.iter().copied());
        let names: Vec<&str> = n.moved.iter().map(|(f, _)| ctx.board.footprints[*f].reference.as_str()).collect();
        eprintln!("nudge: {} steps tried, {} illegal, {} kept ({} as blame asked), moved {names:?}, {:.1} s", n.tried, n.illegal, n.kept, n.guided, ctx.elapsed());
        json!({"tried": n.tried, "illegal": n.illegal, "kept": n.kept, "guided": n.guided, "moved": names})
    }
}

/// The routed copper pulled tight; told after the via pass, which may pull it tight again.
pub struct Tightening;

impl Stage for Tightening {
    fn name(&self) -> &'static str {
        "tightening"
    }

    fn enabled(&self, a: &Args, _: &Board, _: &Ledger) -> bool {
        a.tighten
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        ctx.say("tightening", "straightening the routed copper".to_string());
        let deadline = ctx.deadline();
        let watch = &mut *ctx.watch;
        let c = grid::pull_tight_watch(ctx.board, &mut ctx.ledger.laid, &deadline, &mut |b, net, old, new| watch.tight(b, net, old, new));
        let detail = tight_json(&c);
        ctx.ledger.tightened = Some(c);
        detail
    }
}

fn tight_json(c: &grid::CleanStats) -> Value {
    json!({"chains": c.chains, "segments_before": c.segments_before, "segments_after": c.segments_after, "mm_before": c.length_before, "mm_after": c.length_after})
}

/// Copper pulled tight can leave room for a route on one layer where it took two: vias such a
/// route can do without are taken out.
pub struct Vias;

impl Stage for Vias {
    fn name(&self) -> &'static str {
        "vias"
    }

    fn enabled(&self, _: &Args, _: &Board, ledger: &Ledger) -> bool {
        !ledger.laid.is_empty() || !ledger.fanout.is_empty()
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let a = ctx.args;
        ctx.say("vias", "taking out vias a route on one layer can do without".to_string());
        let deadline = ctx.deadline();
        let options = grid::ViaOptions { cell: a.cell.unwrap_or(grid::ViaOptions::default().cell), pours_block: a.pours_block, deadline: deadline.clone(), log: ctx.log.clone(), ..Default::default() };
        let ledger = &mut *ctx.ledger;
        let watch = &mut *ctx.watch;
        let v = grid::reduce_vias_all(ctx.board, &options, &mut ledger.laid, &mut ledger.fanout, &mut |b| watch.frame(b));
        ledger.removed.extend(v.removed_spans);
        if v.dropped > 0 {
            eprintln!("vias: {} of {} dropped, {} of {} tries kept, {:.1} s", v.dropped, v.vias_before, v.kept, v.tried, ctx.elapsed());
            ctx.watch.step(ctx.board);
            if let Some(c) = &mut ctx.ledger.tightened {
                let watch = &mut *ctx.watch;
                let again = grid::pull_tight_watch(ctx.board, &mut ctx.ledger.laid, &deadline, &mut |b, net, old, new| watch.tight(b, net, old, new));
                (c.segments_after, c.length_after) = (again.segments_after, again.length_after);
                // Tightening's figures are those it tells after this, as its line does.
                if let Some(t) = ctx.stats.stages.iter_mut().find(|s| s.name == "tightening") {
                    t.detail = tight_json(c);
                }
            }
        }
        json!({"before": v.vias_before, "after": v.vias_after, "dropped": v.dropped, "tried": v.tried, "kept": v.kept})
    }
}

/// Doubled tracks and copper leading nowhere taken out, whatever the time: what it leaves out
/// would be written as it stands. Tightening's line comes first, as it waited for the via pass.
pub struct Cleaning;

impl Stage for Cleaning {
    fn name(&self) -> &'static str {
        "cleaning"
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        if let Some(c) = ctx.ledger.tightened.take() {
            ctx.say("tightening", format!("{} segments became {}, {:.0} mm became {:.0} mm", c.segments_before, c.segments_after, c.length_before, c.length_after));
            eprintln!("pulled tight: {} -> {} segments, {:.0} -> {:.0} mm, {:.1} s", c.segments_before, c.segments_after, c.length_before, c.length_after, ctx.elapsed());
            ctx.ledger.tightened = Some(c);
        }
        let open_before = ctx.open().len();
        let (board, ledger) = (&mut *ctx.board, &mut *ctx.ledger);
        // The pairs' copper was locked only against the single-ended stages; it is the router's own
        // copper again for the cleanup, so a track another of its net ends on is split like any other.
        for &id in &ledger.pairs {
            if let Some(mut it) = board.remove(id) {
                it.locked = false;
                board.restore(id, it);
            }
        }
        // The pairs' copper is cleaned with the rest: a run whose legs never came is taken out.
        ledger.laid.extend(ledger.pairs.iter().copied().filter(|id| board.item(*id).is_some()));
        let watch = &mut *ctx.watch;
        let (doubled, spans) = grid::drop_duplicates_watch(board, &mut ledger.laid, &mut |b, it| watch.cleaned(b, it, Cleaned::Duplicate));
        ledger.removed.extend(spans);
        let (pruned, spans) = grid::prune_dangling_watch(board, &mut ledger.laid, &mut |b, it| watch.cleaned(b, it, Cleaned::Dangling));
        ledger.removed.extend(spans);
        if doubled + pruned > 0 {
            ctx.say("cleaning", format!("{doubled} doubled tracks and {pruned} pieces of copper leading nowhere taken out"));
            eprintln!("dropped {doubled} doubled, pruned {pruned} dangling, open {open_before} -> {}, {:.1} s", ctx.open().len(), ctx.elapsed());
        }
        json!({"doubled": doubled, "pruned": pruned})
    }
}

/// Last, once the copper is settled: the shorter net of each pair is given bumps.
pub struct LengthMatching;

impl Stage for LengthMatching {
    fn name(&self) -> &'static str {
        "length matching"
    }

    fn enabled(&self, a: &Args, _: &Board, _: &Ledger) -> bool {
        a.diff_pairs
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let pairs = grid::diff_pairs(ctx.board);
        let deadline = ctx.deadline();
        let watch = &mut *ctx.watch;
        let t = grid::match_lengths_watch(ctx.board, &pairs, &mut ctx.ledger.laid, &deadline, &mut |b, bump| watch.bump(b, bump));
        for (pair, before, after) in &t.skews {
            ctx.say("pairs", format!("{pair}: {before:.2} mm apart in length, {after:.2} mm after tuning"));
            eprintln!("tune: {pair} skew {before:.2} -> {after:.2} mm");
        }
        ctx.watch.step(ctx.board);
        json!({"pairs": t.pairs, "bumps": t.bumps, "skews": t.skews.iter().map(|(p, b, a)| json!({"pair": p, "before_mm": b, "after_mm": a})).collect::<Vec<_>>()})
    }
}

/// What the run leaves open, said: the summary line, and with `--blame` what stands in the way
/// of each open connection. Each is a finding.
pub struct Outcome;

impl Stage for Outcome {
    fn name(&self) -> &'static str {
        "result"
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let open = ctx.open();
        let s = &ctx.ledger.routed;
        eprintln!("routed {} more, {} failed, {} open, {:.1} s", s.routed, s.failed, open.len(), ctx.elapsed());
        let detail = json!({"open": open.len(), "routed": s.routed, "failed": s.failed});
        let blamed = if ctx.args.blame { crate::print_blame(ctx.board, ctx.args.pours_block) } else { Vec::new() };
        ctx.stats.findings.extend(open_findings(ctx.board, &open, &blamed));
        ctx.ledger.open = open.len();
        detail
    }
}

/// A finding per open connection: its net, its two ends and their parts, and what blame said
/// stands in its way, when asked.
pub(crate) fn open_findings(board: &Board, open: &[board::Airline], blamed: &[String]) -> Vec<Finding> {
    let mut out = Vec::new();
    for (k, air) in open.iter().enumerate() {
        let ends = [grid::item_name(board, air.from_item), grid::item_name(board, air.to_item)];
        let mut refs: Vec<String> = Vec::new();
        for id in [air.from_item, air.to_item] {
            if let Some(Kind::Pad { footprint, .. }) = board.item(id).map(|it| &it.kind) {
                let r = &board.footprints[*footprint].reference;
                if !refs.contains(r) {
                    refs.push(r.clone());
                }
            }
        }
        let net = board.net_name(air.net).to_string();
        let mut message = format!("open {net}: {} to {}", ends[0], ends[1]);
        if let Some(what) = blamed.get(k) {
            message = format!("{message}: {what}");
        }
        out.push(Finding { severity: "error", kind: "open", message, refs, nets: vec![net], ends: Some(ends) });
    }
    out
}

/// A finding per warning from loading the board: of kind `rule` for what its project and custom
/// rules leave out or the router does not apply, `board` for the board's own file and outline,
/// so a designer sees a dropped rule wherever the report goes, not only on stderr.
pub(crate) fn load_findings(board: &Board) -> Vec<Finding> {
    let rules = board.rules.warnings.iter().map(|w| ("rule", w));
    let own = board.warnings.iter().map(|w| ("board", w));
    rules.chain(own).map(|(kind, w)| Finding { severity: "warning", kind, message: w.clone(), refs: Vec::new(), nets: Vec::new(), ends: None }).collect()
}

/// The board written: the router's copper as tracks and vias, what it took out removed, moved
/// parts at their new places, stale pour fills dropped; with the regions' and the necks' reports.
pub struct Write;

impl Stage for Write {
    fn name(&self) -> &'static str {
        "write"
    }

    fn run(&self, ctx: &mut StageCtx) -> Value {
        let (a, board, ledger) = (ctx.args, &mut *ctx.board, &mut *ctx.ledger);
        let mut laid = std::mem::take(&mut ledger.laid);
        laid.append(&mut ledger.fanout);
        if a.prefanout {
            // Escapes may have joined `laid` twice over, or been ripped since.
            let mut seen_laid = HashSet::new();
            laid.retain(|id| board.item(*id).is_some() && seen_laid.insert(*id));
        }
        let (split, spans) = grid::split_at_joins(board, &mut laid);
        ledger.removed.extend(spans);
        if split > 0 {
            eprintln!("split {split} tracks where others end on them");
        }
        for (line, nets) in regions::report(board, &ledger.lengths, &laid) {
            eprintln!("regions: {line}");
            let kind = if line.starts_with("length:") { "length" } else { "region" };
            ctx.stats.findings.push(Finding { severity: "warning", kind, message: line.clone(), refs: Vec::new(), nets, ends: None });
            ctx.say("regions", line);
        }
        for (line, net) in ctx.board.rule_limits() {
            eprintln!("rules: {line}");
            ctx.stats.findings.push(Finding { severity: "warning", kind: "length", message: line.clone(), refs: Vec::new(), nets: vec![net], ends: None });
            ctx.say("rules", line);
        }
        let board = &mut *ctx.board;
        for neck in grid::necks::necks(board, &laid) {
            eprintln!("neck: {}", neck.line);
            ctx.stats.findings.push(Finding { severity: "warning", kind: "neck", message: neck.line.clone(), refs: neck.part.into_iter().collect(), nets: vec![neck.net], ends: None });
            ctx.say("necks", neck.line);
        }
        let (board, ledger) = (&mut *ctx.board, &mut *ctx.ledger);
        let replace = crate::moved_parts(board, &ledger.original_at, ctx.text);
        grid::keep_orphans(board, &ledger.removed, &mut laid);
        let (tracks, vias) = grid::edits(board, &laid);
        let stale = crate::stale_fills(board, &laid, ctx.text, !replace.is_empty());
        if !stale.is_empty() {
            eprintln!("dropped {} pour fills new copper cuts; KiCad refills them", stale.len());
        }
        let detail = json!({"tracks": tracks.len(), "vias": vias.len(), "split": split, "stale_fills": stale.len(), "moved_parts": replace.len()});
        let mut removed = std::mem::take(&mut ledger.removed);
        removed.extend(stale);
        ledger.output = Some(apply(ctx.text, &Edit { remove: removed, replace, tracks, vias, uuid_seed: ctx.args.uuid_seed }));
        ledger.laid = laid;
        detail
    }
}

/// The widest region the region plough rips round a trapped connection's path.
const REGION_WIDEST: Nm = 4_000_000;

/// The plough for connections trapped at fine-pitch parts, up to `--shove-passes` times, each in
/// another order, stopping at `deadline`; the board file's spans of copper it moved or ripped go
/// to `removed`. How many passes it made.
#[allow(clippy::too_many_arguments)]
fn shove_pass(
    a: &Args,
    board: &mut Board,
    solver: &mut dyn grid::FieldSolver,
    laid: &mut Vec<ItemId>,
    started: grid::clock::Instant,
    deadline: &grid::Deadline,
    log: &Option<grid::Log>,
    watch: &mut dyn Watch,
    removed: &mut Vec<Span>,
) -> usize {
    let defaults = grid::ShoveOptions::default();
    let plough = a.shove_plough.unwrap_or(defaults.plough);
    let region = plough == grid::Plough::Region;
    // On a board with planes, the region plough counts signal connections first: signal
    // connections open, then all, are what a pass must leave fewer of.
    let plane_nets: HashSet<u32> = if region && !a.pours_block && board.planes != 0 {
        board.items().filter(|(_, it)| board.is_plane(it) && it.net != 0).map(|(_, it)| it.net).collect()
    } else {
        HashSet::new()
    };
    let open_now = |b: &Board| {
        let open = open_airlines(a, b);
        (open.iter().filter(|x| !plane_nets.contains(&x.net)).count(), open.len())
    };
    let mut stalled = 0;
    let mut passes = 0;
    let reach = a.shove_reach.map_or(defaults.reach, |mm| (mm * 1e6).round() as Nm);
    let mut reaches: HashMap<ItemId, Nm> = HashMap::new();
    let mut net_reaches: HashMap<u32, Nm> = HashMap::new();
    // The reaches a net has been widened to since the last pass that freed anything.
    let mut widened: HashSet<(u32, Nm)> = HashSet::new();
    for pass in 0..a.shove_passes.max(1) {
        let before = open_now(board);
        if before.1 == 0 {
            break;
        }
        passes += 1;
        let fine: HashSet<usize> = morph::fine_pitch(board, 500_000).into_iter().collect();
        let s = grid::shove_escape(
            board,
            solver,
            &grid::ShoveOptions {
                cell: a.cell.unwrap_or(defaults.cell),
                // Ripping and renegotiating round a path frees any connection, not only an escape.
                parts: (!region).then_some(fine),
                plough,
                reach,
                reaches: reaches.clone(),
                net_reaches: net_reaches.clone(),
                repair_seconds: a.shove_repair.unwrap_or(defaults.repair_seconds),
                accept_ties: a.shove_ties,
                negotiate_repasses: a.negotiate_repasses.map_or(defaults.negotiate_repasses, |r| r.min(1)),
                cascade: a.shove_cascade.unwrap_or(defaults.cascade),
                seed: a.seed.wrapping_mul(1000).wrapping_add(pass as u64 + 1),
                deadline: deadline.clone(),
                pours_block: a.pours_block,
                log: log.clone(),
                ..defaults
            },
            laid,
            &mut |b: &Board, event| watch.shove(b, &event),
        );
        removed.extend(s.removed_spans.iter().copied());
        let routed = s.tries.iter().filter(|t| t.routed).count();
        let kept = s.tries.iter().filter(|t| t.kept).count();
        let after = open_now(board);
        eprintln!("shove pass {}: {} trapped tried, {routed} routed out, {kept} kept, {} -> {} open, {:.1} s", pass + 1, s.tries.len(), before.1, after.1, started.elapsed().as_secs_f64());
        // Passes go on, each in another order, until `--shove-patience` of them in a row free
        // nothing more, or nothing is left open.
        // The region plough tries a connection it could not free twice as far round its path
        // next time, up to the widest region, then from the narrowest again; a pass that widened
        // a net to a reach it had not had is no stall. A tie kept in a pass that freed nothing
        // only swapped two nets in a corridor, and widens too; a connection walled in by what
        // cannot move does not. The net widens with the pad: a tie lays the net's copper again
        // under new ids, and a trapped end that is a track would come back next pass under a new
        // id at the narrowest reach. A pass that frees anything starts every reach afresh.
        let mut grew = false;
        if region && after < before {
            reaches.clear();
            net_reaches.clear();
            widened.clear();
        } else if region {
            for t in &s.tries {
                if t.kept && t.open_after < t.open_before {
                    reaches.remove(&t.pad);
                    net_reaches.remove(&t.net);
                } else if t.rounds.iter().any(|r| !r.path.is_empty()) {
                    let next = if t.reach < REGION_WIDEST { (t.reach * 2).min(REGION_WIDEST) } else { reach };
                    reaches.insert(t.pad, next);
                    net_reaches.insert(t.net, next);
                    grew |= widened.insert((t.net, next));
                }
            }
        }
        if after.1 == 0 {
            break;
        }
        stalled = if after < before || grew { 0 } else { stalled + 1 };
        if stalled >= a.shove_patience.max(1) || deadline.expired() {
            break;
        }
    }
    passes
}

/// The pairs' copper is locked through the single-ended stages, and nothing may rip it. A
/// connection the shove leaves open may be walled in by it: the pair copper within a millimetre
/// of the straight line of an open connection is unlocked, for the shove to try again, a board
/// that routes coming before coupling. How many pieces were unlocked.
fn unlock_pairs_near_open(board: &mut Board, pair_laid: &[ItemId], open: &[board::Airline]) -> usize {
    let corridors: Vec<Shape> = open.iter().map(|c| Shape::Capsule { a: c.from, b: c.to, r: 1_000_000 }).collect();
    let mut freed = 0;
    for &id in pair_laid {
        if !board.item(id).is_some_and(|it| it.locked && corridors.iter().any(|c| it.shape.gap(c) == 0.0)) {
            continue;
        }
        if let Some(mut it) = board.remove(id) {
            it.locked = false;
            board.restore(id, it);
            freed += 1;
        }
    }
    if freed > 0 {
        eprintln!("pairs: {freed} pieces of pair copper unlocked round {} open connections", open.len());
    }
    freed
}

/// Locks again what is left of the pairs' copper after the shove that was let at it, so the
/// stages after (tightening, dropping vias) leave the pairs as they are.
fn relock_pairs(board: &mut Board, pair_laid: &[ItemId]) {
    // An id the shove freed may have been given to new copper; only the pairs' nets are locked.
    let nets: HashSet<u32> = grid::diff_pairs(board).iter().flat_map(|p| [p.p, p.n]).collect();
    for &id in pair_laid {
        if board.item(id).is_some_and(|it| nets.contains(&it.net) && matches!(it.kind, Kind::Track | Kind::Via)) {
            if let Some(mut it) = board.remove(id) {
                it.locked = true;
                board.restore(id, it);
            }
        }
    }
}

/// Footprints that may not be nudged or pushed: the board's own locked parts, plus any part
/// joined to locked differential-pair copper so moving a part never tears its pair legs loose.
fn locked_footprints(board: &Board, pair_laid: &[ItemId]) -> HashSet<usize> {
    let mut out: HashSet<usize> = board.footprints.iter().enumerate().filter(|(_, f)| f.locked).map(|(i, _)| i).collect();
    if pair_laid.is_empty() {
        return out;
    }
    let pair_set: HashSet<ItemId> = pair_laid.iter().copied().filter(|&id| board.item(id).is_some()).collect();
    for (id, it) in board.items() {
        let Kind::Pad { footprint, .. } = it.kind else { continue };
        if out.contains(&footprint) || it.net == 0 {
            continue;
        }
        let bb = board.bbox(id).unwrap_or_else(|| it.shape.bbox());
        let touches_pair = (0..board.layers.len()).filter(|&l| it.on(l)).any(|l| board.near(l, bb).into_iter().any(|o| pair_set.contains(&o) && board::joined(board, id, o)));
        if touches_pair {
            out.insert(footprint);
        }
    }
    out
}

/// A track's or via's identity and shape, to tell what a stage laid and ripped.
pub(crate) fn copper_keys(board: &Board) -> HashSet<(ItemId, u64, [Nm; 5])> {
    board
        .items()
        .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via))
        .map(|(id, it): (ItemId, &Item)| {
            let shape = match &it.shape {
                Shape::Capsule { a, b, r } => [a.x, a.y, b.x, b.y, *r],
                Shape::Polygon(p) => [p.len() as Nm, p.first().map_or(0, |q| q.x), p.first().map_or(0, |q| q.y), 0, 0],
            };
            (id, it.layers, shape)
        })
        .collect()
}
