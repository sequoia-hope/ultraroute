//! What a run shows as it goes, besides its log: the board after each step, each stage's own
//! events, and each stage starting and ending. The studio's frames ([`Live`]) and the lab's films
//! watch a run this way; the run's progress reaches anyone else through its log ([`grid::Log`])
//! and its stats ([`crate::RunStats`]).

use std::sync::{Arc, Mutex};

use board::{Board, Item, ItemId};

use crate::stages::Ledger;
use crate::stats::StageStats;
use crate::Live;

/// A stage starting, or ending with what it did.
pub enum StageEvent<'a> {
    Start,
    End(&'a StageStats),
}

/// Whether the run goes on after a stage; a run told to stop skips the stages the clock would
/// skip after its hard deadline, and writes the board as it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Go,
    Stop,
}

/// What clean-up took out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleaned {
    /// A track doubled by another the same.
    Duplicate,
    /// Copper leading nowhere.
    Dangling,
}

/// A watcher of a run. Every method has a default: the board-only ones do nothing, and each
/// stage's events become a frame where the stage itself would show one (a rip-up kept, a pair's
/// run laid), so a watcher that only draws boards need only take `frame` and `step`.
pub trait Watch {
    /// The board after a step of the work.
    fn frame(&mut self, _board: &Board) {}
    /// The board after a step worth seeing on its own, however fast the steps come.
    fn step(&mut self, board: &Board) {
        self.frame(board);
    }
    /// Negotiation's trial routes for a round.
    fn trials(&mut self, _round: usize, _trials: &[grid::Trial]) {}
    /// Whether to be shown each negotiation round's congestion ([`Watch::round`]).
    fn wants_rounds(&self) -> bool {
        false
    }
    fn round(&mut self, _view: grid::RoundView) {}
    /// A rip-up attempt, kept or not.
    fn rip(&mut self, board: &Board, attempt: &grid::RipAttempt) {
        if attempt.outcome == "kept" {
            self.frame(board);
        }
    }
    /// A nudge's blame or step, kept or not.
    fn nudge(&mut self, board: &Board, event: &grid::NudgeEvent) {
        if matches!(event, grid::NudgeEvent::Step { outcome: "kept", .. }) {
            self.frame(board);
        }
    }
    /// A differential pair's run, laid (`layer` set) or not.
    fn pair_run(&mut self, board: &Board, run: &grid::PairRun) {
        if run.layer.is_some() {
            self.frame(board);
        }
    }
    /// A plough try starting, a round of it, or its end.
    fn shove(&mut self, board: &Board, event: &grid::ShoveEvent) {
        if !matches!(event, grid::ShoveEvent::Start(_)) {
            self.step(board);
        }
    }
    /// A chain of `net` pulled tight: the tracks it replaced and the new ones.
    fn tight(&mut self, _board: &Board, _net: u32, _old: &[Item], _new: &[ItemId]) {}
    /// A piece of copper clean-up took out.
    fn cleaned(&mut self, _board: &Board, _item: &Item, _how: Cleaned) {}
    /// A bump kept in tuning a pair's lengths.
    fn bump(&mut self, _board: &Board, _bump: &grid::TuneBump) {}
    /// A log to hear the run's activity on, besides the run's own.
    fn log(&self) -> Option<grid::Log> {
        None
    }
    /// A stage starting or ending, with the copper the router holds as its own then.
    fn stage(&mut self, _name: &'static str, _board: &Board, _ledger: &Ledger, _event: StageEvent) -> Flow {
        Flow::Go
    }
}

/// Watches nothing.
pub struct Unwatched;

impl Watch for Unwatched {}

impl Watch for Live {
    fn frame(&mut self, board: &Board) {
        self.send_frame(board, false, Vec::new());
    }

    fn step(&mut self, board: &Board) {
        self.send_frame(board, true, Vec::new());
    }

    fn trials(&mut self, round: usize, trials: &[grid::Trial]) {
        self.send_trials(round, trials);
    }
}

/// The studio's frames, which the run's log also writes to and so are shared, and the caller's
/// watcher, both shown everything.
pub(crate) struct Hooks<'w> {
    pub live: Arc<Mutex<Option<Live>>>,
    pub user: Option<&'w mut dyn Watch>,
}

impl Hooks<'_> {
    fn live(&self, f: impl FnOnce(&mut Live)) {
        if let Some(l) = self.live.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            f(l);
        }
    }
}

impl Watch for Hooks<'_> {
    fn frame(&mut self, board: &Board) {
        self.live(|l| l.frame(board));
        if let Some(u) = self.user.as_mut() {
            u.frame(board);
        }
    }

    fn step(&mut self, board: &Board) {
        self.live(|l| l.step(board));
        if let Some(u) = self.user.as_mut() {
            u.step(board);
        }
    }

    fn trials(&mut self, round: usize, trials: &[grid::Trial]) {
        self.live(|l| l.trials(round, trials));
        if let Some(u) = self.user.as_mut() {
            u.trials(round, trials);
        }
    }

    fn wants_rounds(&self) -> bool {
        self.user.as_ref().is_some_and(|u| u.wants_rounds())
    }

    fn round(&mut self, view: grid::RoundView) {
        if let Some(u) = self.user.as_mut() {
            u.round(view);
        }
    }

    fn rip(&mut self, board: &Board, attempt: &grid::RipAttempt) {
        self.live(|l| l.rip(board, attempt));
        if let Some(u) = self.user.as_mut() {
            u.rip(board, attempt);
        }
    }

    fn nudge(&mut self, board: &Board, event: &grid::NudgeEvent) {
        self.live(|l| l.nudge(board, event));
        if let Some(u) = self.user.as_mut() {
            u.nudge(board, event);
        }
    }

    fn pair_run(&mut self, board: &Board, run: &grid::PairRun) {
        self.live(|l| l.pair_run(board, run));
        if let Some(u) = self.user.as_mut() {
            u.pair_run(board, run);
        }
    }

    fn shove(&mut self, board: &Board, event: &grid::ShoveEvent) {
        self.live(|l| l.shove(board, event));
        if let Some(u) = self.user.as_mut() {
            u.shove(board, event);
        }
    }

    fn tight(&mut self, board: &Board, net: u32, old: &[Item], new: &[ItemId]) {
        if let Some(u) = self.user.as_mut() {
            u.tight(board, net, old, new);
        }
    }

    fn cleaned(&mut self, board: &Board, item: &Item, how: Cleaned) {
        if let Some(u) = self.user.as_mut() {
            u.cleaned(board, item, how);
        }
    }

    fn bump(&mut self, board: &Board, bump: &grid::TuneBump) {
        if let Some(u) = self.user.as_mut() {
            u.bump(board, bump);
        }
    }

    fn stage(&mut self, name: &'static str, board: &Board, ledger: &Ledger, event: StageEvent) -> Flow {
        match self.user.as_mut() {
            Some(u) => u.stage(name, board, ledger, event),
            None => Flow::Go,
        }
    }
}
