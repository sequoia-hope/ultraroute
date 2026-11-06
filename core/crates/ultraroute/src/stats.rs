//! What a run did, as one record: each stage's time and work, the run's settings and provenance,
//! the connections given up, and the findings a designer acts on (connections left open, forced
//! necks, broken region rules and length limits, stages the clock cut short). `--stats FILE`
//! (or `--report FILE`) writes it as JSON; `--verbose` prints its table.

use serde::Serialize;
use serde_json::Value;

/// The router's version, and the commit it was built from when the build could ask git.
pub fn version() -> (&'static str, Option<&'static str>) {
    let git = option_env!("ULTRAROUTE_GIT").filter(|g| !g.is_empty());
    (env!("CARGO_PKG_VERSION"), git)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RunStats {
    pub router: Router,
    pub input: Input,
    pub board: BoardInfo,
    pub summary: Summary,
    pub findings: Vec<Finding>,
    pub deadline: DeadlineStats,
    pub stages: Vec<StageStats>,
    pub moves: MoveStats,
    pub progress: ProgressStats,
    /// Each connection a stage gave up on, and why.
    pub failures: Vec<Failure>,
    pub settings: Settings,
    /// Every attempt the run made when it routed again with another recipe, in order; empty
    /// when it routed once. The rest of the stats are the kept attempt's.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<Attempt>,
    pub memory: Memory,
    /// Process CPU seconds spent loading and parsing the input board before `route` began.
    pub load_cpu_seconds: f64,
    /// The run's own bookkeeping: what counting stages' open connections and copper took.
    pub stats_seconds: f64,
    /// Main-thread CPU seconds spent inside `Mark::take` while stage timing was paused.
    pub stats_cpu_seconds: f64,
}

/// One attempt of a run that routed again ([`crate::RETRIES`]).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Attempt {
    /// What the attempt changed from the run as asked; "as asked" for the first.
    pub change: String,
    pub opens: usize,
    pub vias: usize,
    /// Seconds on the deadlines' clock (the CPU's under `--cpu-clock`).
    pub seconds: f64,
    /// Whether the clock cut any of its stages short.
    pub time_limited: bool,
    pub kept: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct BoardInfo {
    pub copper_layers: usize,
    pub footprints: usize,
    pub fine_pitch_parts: usize,
    pub pads: usize,
    pub nets: usize,
    pub diff_pairs: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Router {
    pub version: String,
    /// The commit, `None` when built without git.
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Input {
    pub path: String,
    pub sha256: String,
    pub bytes: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Summary {
    /// Connections open after set-up (what the run had to make) and at the end.
    pub connections: usize,
    pub opens: usize,
    /// Connections the run made: `connections` less `opens`.
    pub routed: usize,
    /// Connections routing one at a time gave up on after negotiation (the `failed` of the
    /// `routed N more` line); later stages may still have made some of them.
    pub failed: usize,
    /// Vias on the board at the end.
    pub vias: usize,
    pub seconds: f64,
    /// Total routing CPU seconds across all stages (main thread plus any worker threads).
    pub cpu_seconds: f64,
}

/// Something about the board as routed for its designer: `severity` is `error` (the board is
/// not done), `warning` (done, but look) or `info`.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub severity: &'static str,
    /// `open`, `neck`, `region`, `length` (a net over a length or via-count limit, from
    /// `--regions` or a custom rule), `deadline`, `stopped` (why a run with connections open
    /// ended), `crash`, `rule` (a project or custom rule
    /// left out or not applied) or `board` (something in the board file, such as an outline that
    /// does not close).
    pub kind: &'static str,
    pub message: String,
    /// The parts it is about, by reference, and the nets, by name.
    pub refs: Vec<String>,
    pub nets: Vec<String>,
    /// An open connection's two ends, as `grid::item_name` names them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends: Option<[String; 2]>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DeadlineStats {
    /// The stages whose time ran out, in the order they ran.
    pub limited: Vec<String>,
    /// The stage running when the run's hard deadline passed, if it did (not by cancelling).
    pub hard_passed_in: Option<String>,
    /// How far past `--timeout` the run ended (negative: within it), when the hard deadline passed.
    pub overran_seconds: Option<f64>,
    pub cancelled: bool,
}

impl DeadlineStats {
    /// The `deadline:` lines the router prints, which the benchmark reads.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.limited.is_empty() {
            out.push(format!("deadline: time ran out in {}", self.limited.join(", ")));
        }
        if let Some(stage) = &self.hard_passed_in {
            let over = self.overran_seconds.unwrap_or(0.0);
            let by = if over > 0.0 { format!("the run overran --timeout by {over:.1} s") } else { "the run still ended within --timeout".to_string() };
            out.push(format!("deadline: the run's hard limit passed during {stage}; {by}"));
        }
        out
    }
}

/// One stage of the run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StageStats {
    pub name: &'static str,
    /// Seconds since the run began when it started, and how long it took on the wall clock.
    pub started: f64,
    pub seconds: f64,
    /// Thread CPU seconds spent in the stage (main thread plus any worker threads).
    pub cpu_seconds: f64,
    /// Exclusive CPU seconds per sub-operation within the stage, plus `accounted` and `other`.
    pub cpu: CpuBreakdown,
    /// CPU seconds split between kept connection-closing work, other kept work, wasted tries and setup/other.
    pub effort: Effort,
    /// Whether the clock stopped it short.
    pub cut: bool,
    pub open_before: usize,
    pub open_after: usize,
    /// Tracks and vias that came and went (a piece moved counts both ways), and the vias on the
    /// board after.
    pub laid: usize,
    pub ripped: usize,
    pub vias: usize,
    pub work: Work,
    /// The stage's own figures, as it reports them.
    pub detail: Value,
}

fn step_down(x: f64) -> f64 {
    if x <= 0.0 { 0.0 } else { f64::from_bits(x.to_bits() - 1) }
}

/// Exclusive CPU seconds per sub-operation (`board::work::Bucket`), their sum (`accounted`), and
/// the unattributed remainder (`other`).
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct CpuBreakdown {
    pub astar: f64,
    pub map_build: f64,
    pub map_patch: f64,
    pub zones: f64,
    pub checks: f64,
    pub connectivity: f64,
    pub terminals: f64,
    pub geometry: f64,
    pub accounted: f64,
    pub other: f64,
}

impl CpuBreakdown {
    /// Converts nanosecond counters into seconds while keeping `accounted + other <= stage_sec`
    /// exact under IEEE-754 rounding.
    pub fn from_ns(total_ns: u64, raw: &[u64; board::work::BUCKET_COUNT]) -> (f64, Self) {
        if total_ns == 0 {
            return (0.0, Self::default());
        }
        let raw_sum: u128 = raw.iter().map(|&x| x as u128).sum();
        let mut b = *raw;
        if raw_sum > total_ns as u128 && raw_sum > 0 {
            for x in &mut b {
                *x = ((*x as u128) * (total_ns as u128) / raw_sum) as u64;
            }
        }
        let sec = |ns: u64| ns as f64 * 1e-9;
        let astar = sec(b[board::work::Bucket::Astar as usize]);
        let map_build = sec(b[board::work::Bucket::MapBuild as usize]);
        let map_patch = sec(b[board::work::Bucket::MapPatch as usize]);
        let zones = sec(b[board::work::Bucket::Zones as usize]);
        let checks = sec(b[board::work::Bucket::Checks as usize]);
        let connectivity = sec(b[board::work::Bucket::Connectivity as usize]);
        let terminals = sec(b[board::work::Bucket::Terminals as usize]);
        let geometry = sec(b[board::work::Bucket::Geometry as usize]);
        let accounted = astar + map_build + map_patch + zones + checks + connectivity + terminals + geometry;
        let stage_sec = sec(total_ns).max(accounted);
        let mut other = (stage_sec - accounted).max(0.0);
        while accounted + other > stage_sec && other > 0.0 {
            other = step_down(other);
        }
        (
            stage_sec,
            Self { astar, map_build, map_patch, zones, checks, connectivity, terminals, geometry, accounted, other },
        )
    }
}

/// CPU seconds spent on kept work that closed a connection (`useful_seconds`), kept work for
/// non-connection changes (`kept_other_seconds`: fanout stubs, shove tie swaps, pulled-tight
/// chains, dropped vias), discarded or failed tries (`wasted_seconds`), their sum
/// (`accounted_seconds`), and fixed stage setup/cleanup (`other_seconds`).
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Effort {
    pub useful_seconds: f64,
    pub kept_other_seconds: f64,
    pub wasted_seconds: f64,
    pub accounted_seconds: f64,
    pub other_seconds: f64,
}

impl Effort {
    pub fn from_ns(stage_sec: f64, useful_ns: u64, kept_other_ns: u64, wasted_ns: u64) -> Self {
        if stage_sec <= 0.0 {
            return Self::default();
        }
        let mut u = useful_ns as f64 * 1e-9;
        let mut k = kept_other_ns as f64 * 1e-9;
        let mut w = wasted_ns as f64 * 1e-9;
        let sum = u + k + w;
        if sum > stage_sec && sum > 0.0 {
            let scale = stage_sec / sum;
            u *= scale;
            k *= scale;
            w *= scale;
            while u + k + w > stage_sec && (u > 0.0 || k > 0.0 || w > 0.0) {
                if u >= k && u >= w {
                    u = step_down(u);
                } else if k >= w {
                    k = step_down(k);
                } else {
                    w = step_down(w);
                }
            }
        }
        let accounted_seconds = u + k + w;
        let mut other_seconds = (stage_sec - accounted_seconds).max(0.0);
        while accounted_seconds + other_seconds > stage_sec && other_seconds > 0.0 {
            other_seconds = step_down(other_seconds);
        }
        Self { useful_seconds: u, kept_other_seconds: k, wasted_seconds: w, accounted_seconds, other_seconds }
    }
}

/// Per-move-kind breakdown of kept versus wasted work across the run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MoveStats {
    pub negotiation: NegMoveStats,
    pub single: SingleMoveStats,
    pub rip_up: RipMoveStats,
    pub shove: ShoveMoveStats,
    pub nudge: NudgeMoveStats,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NegRoundStat {
    pub pass: usize,
    pub round: usize,
    pub rerouted: usize,
    pub colliding: usize,
    pub improved: bool,
    pub cpu_seconds: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NegMoveStats {
    pub passes: usize,
    pub rounds: usize,
    pub converging_rounds: usize,
    pub converging_seconds: f64,
    pub stalled_rounds: usize,
    pub stalled_seconds: f64,
    pub setup_useful_seconds: f64,
    pub setup_wasted_seconds: f64,
    pub commit_useful_seconds: f64,
    pub commit_wasted_seconds: f64,
    pub rescues_tried: usize,
    pub rescues_kept: usize,
    pub rescue_useful_seconds: f64,
    pub rescues_wasted: usize,
    pub rescue_wasted_seconds: f64,
    pub useful_seconds: f64,
    pub wasted_seconds: f64,
    pub round_trace: Vec<NegRoundStat>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SingleMoveStats {
    pub attempts: usize,
    pub routed: usize,
    pub useful_seconds: f64,
    pub no_terminal: usize,
    pub no_terminal_seconds: f64,
    pub no_path: usize,
    pub no_path_seconds: f64,
    pub refused: usize,
    pub refused_seconds: f64,
    pub wasted_seconds: f64,
    pub setup_seconds: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RipMoveStats {
    pub attempts: usize,
    pub kept: usize,
    pub useful_seconds: f64,
    pub no_path: usize,
    pub no_path_seconds: f64,
    pub hard_conflict: usize,
    pub hard_conflict_seconds: f64,
    pub no_better: usize,
    pub no_better_seconds: f64,
    pub wasted_seconds: f64,
    pub setup_seconds: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ShoveMoveStats {
    pub passes: usize,
    pub attempts: usize,
    pub kept_closed: usize,
    pub useful_seconds: f64,
    pub kept_tie: usize,
    pub kept_tie_seconds: f64,
    pub walled_no_path: usize,
    pub walled_no_path_seconds: f64,
    pub reverted_routed: usize,
    pub reverted_routed_seconds: f64,
    pub reverted_unrouted: usize,
    pub reverted_unrouted_seconds: f64,
    pub wasted_seconds: f64,
    pub setup_seconds: f64,
    pub kept_plough_seconds: f64,
    pub kept_reroute_seconds: f64,
    pub kept_repair_seconds: f64,
    pub wasted_plough_seconds: f64,
    pub wasted_reroute_seconds: f64,
    pub wasted_repair_seconds: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NudgeMoveStats {
    pub tried: usize,
    pub kept: usize,
    pub guided_kept: usize,
    pub blind_kept: usize,
    pub blame_queries: usize,
    pub blame_useful_seconds: f64,
    pub blame_wasted_seconds: f64,
    pub kept_guided_seconds: f64,
    pub kept_blind_seconds: f64,
    pub useful_seconds: f64,
    pub illegal_guided: usize,
    pub illegal_guided_seconds: f64,
    pub illegal_blind: usize,
    pub illegal_blind_seconds: f64,
    pub no_better_guided: usize,
    pub no_better_guided_seconds: f64,
    pub no_better_blind: usize,
    pub no_better_blind_seconds: f64,
    pub wasted_seconds: f64,
}

impl MoveStats {
    pub fn record_negotiation(&mut self, n: &grid::NegotiationStats) -> (u64, u64) {
        let sec = |ns: u64| ns as f64 * 1e-9;
        let (useful_ns, wasted_ns) = if n.committed > 0 {
            (
                n.converging_ns.saturating_add(n.setup_useful_ns).saturating_add(n.commit_useful_ns).saturating_add(n.rescue_useful_ns),
                n.stalled_ns.saturating_add(n.setup_wasted_ns).saturating_add(n.commit_wasted_ns).saturating_add(n.rescue_wasted_ns),
            )
        } else {
            (
                n.rescue_useful_ns,
                n.converging_ns
                    .saturating_add(n.stalled_ns)
                    .saturating_add(n.setup_useful_ns)
                    .saturating_add(n.setup_wasted_ns)
                    .saturating_add(n.commit_useful_ns)
                    .saturating_add(n.commit_wasted_ns)
                    .saturating_add(n.rescue_wasted_ns),
            )
        };
        let dst = &mut self.negotiation;
        dst.passes += n.passes;
        dst.rounds += n.rounds_total;
        dst.converging_rounds += n.converging_rounds;
        dst.converging_seconds += sec(n.converging_ns);
        dst.stalled_rounds += n.stalled_rounds;
        dst.stalled_seconds += sec(n.stalled_ns);
        dst.setup_useful_seconds += sec(n.setup_useful_ns);
        dst.setup_wasted_seconds += sec(n.setup_wasted_ns);
        dst.commit_useful_seconds += sec(n.commit_useful_ns);
        dst.commit_wasted_seconds += sec(n.commit_wasted_ns);
        dst.rescues_tried += n.rescues_tried;
        dst.rescues_kept += n.rescues_kept;
        dst.rescue_useful_seconds += sec(n.rescue_useful_ns);
        dst.rescues_wasted += n.rescues_wasted;
        dst.rescue_wasted_seconds += sec(n.rescue_wasted_ns);
        dst.useful_seconds += sec(useful_ns);
        dst.wasted_seconds += sec(wasted_ns);
        dst.round_trace.extend(n.round_trace.iter().map(|r| NegRoundStat {
            pass: r.pass,
            round: r.round,
            rerouted: r.rerouted,
            colliding: r.colliding,
            improved: r.improved,
            cpu_seconds: sec(r.cpu_ns),
        }));
        (useful_ns, wasted_ns)
    }

    pub fn record_single(&mut self, s: &grid::Stats) -> (u64, u64) {
        let sec = |ns: u64| ns as f64 * 1e-9;
        let wasted_ns = s.no_terminal_ns.saturating_add(s.no_path_ns).saturating_add(s.refused_ns);
        let dst = &mut self.single;
        dst.attempts += s.routed + s.failed;
        dst.routed += s.routed;
        dst.useful_seconds += sec(s.routed_ns);
        dst.no_terminal += s.no_terminal;
        dst.no_terminal_seconds += sec(s.no_terminal_ns);
        dst.no_path += s.no_path;
        dst.no_path_seconds += sec(s.no_path_ns);
        dst.refused += s.refused;
        dst.refused_seconds += sec(s.refused_ns);
        dst.wasted_seconds += sec(wasted_ns);
        dst.setup_seconds += sec(s.setup_ns);
        (s.routed_ns, wasted_ns)
    }

    pub fn record_rip(&mut self, r: &grid::RipStats) -> (u64, u64) {
        let sec = |ns: u64| ns as f64 * 1e-9;
        let wasted_ns = r.no_path_ns.saturating_add(r.hard_conflict_ns).saturating_add(r.no_better_ns);
        let dst = &mut self.rip_up;
        dst.attempts += r.attempts;
        dst.kept += r.kept;
        dst.useful_seconds += sec(r.kept_ns);
        dst.no_path += r.no_path;
        dst.no_path_seconds += sec(r.no_path_ns);
        dst.hard_conflict += r.hard_conflict;
        dst.hard_conflict_seconds += sec(r.hard_conflict_ns);
        dst.no_better += r.no_better;
        dst.no_better_seconds += sec(r.no_better_ns);
        dst.wasted_seconds += sec(wasted_ns);
        dst.setup_seconds += sec(r.setup_ns);
        (r.kept_ns, wasted_ns)
    }

    pub fn record_shove(&mut self, s: &grid::ShoveStats) -> (u64, u64, u64) {
        let sec = |ns: u64| ns as f64 * 1e-9;
        let dst = &mut self.shove;
        dst.passes += 1;
        dst.attempts += s.tries.len();
        dst.setup_seconds += sec(s.setup_ns);
        let (mut useful_ns, mut tie_ns, mut wasted_ns) = (0u64, 0u64, 0u64);
        for t in &s.tries {
            if t.kept {
                dst.kept_plough_seconds += sec(t.plough_ns);
                dst.kept_reroute_seconds += sec(t.reroute_ns);
                dst.kept_repair_seconds += sec(t.repair_ns);
                if t.open_after < t.open_before {
                    dst.kept_closed += 1;
                    dst.useful_seconds += sec(t.cpu_ns);
                    useful_ns = useful_ns.saturating_add(t.cpu_ns);
                } else {
                    dst.kept_tie += 1;
                    dst.kept_tie_seconds += sec(t.cpu_ns);
                    tie_ns = tie_ns.saturating_add(t.cpu_ns);
                }
            } else {
                dst.wasted_plough_seconds += sec(t.plough_ns);
                dst.wasted_reroute_seconds += sec(t.reroute_ns);
                dst.wasted_repair_seconds += sec(t.repair_ns);
                wasted_ns = wasted_ns.saturating_add(t.cpu_ns);
                if t.rounds.iter().all(|r| r.path.is_empty()) {
                    dst.walled_no_path += 1;
                    dst.walled_no_path_seconds += sec(t.cpu_ns);
                } else if t.routed {
                    dst.reverted_routed += 1;
                    dst.reverted_routed_seconds += sec(t.cpu_ns);
                } else {
                    dst.reverted_unrouted += 1;
                    dst.reverted_unrouted_seconds += sec(t.cpu_ns);
                }
            }
        }
        dst.wasted_seconds += sec(wasted_ns);
        (useful_ns, tie_ns, wasted_ns)
    }

    pub fn record_nudge(&mut self, n: &grid::NudgeStats) -> (u64, u64) {
        let sec = |ns: u64| ns as f64 * 1e-9;
        let useful_ns = n.kept_guided_ns.saturating_add(n.kept_blind_ns).saturating_add(n.blame_useful_ns);
        let wasted_ns = n
            .illegal_guided_ns
            .saturating_add(n.illegal_blind_ns)
            .saturating_add(n.no_better_guided_ns)
            .saturating_add(n.no_better_blind_ns)
            .saturating_add(n.blame_wasted_ns);
        let dst = &mut self.nudge;
        dst.tried += n.tried;
        dst.kept += n.kept;
        dst.guided_kept += n.guided;
        dst.blind_kept += n.kept.saturating_sub(n.guided);
        dst.blame_queries += n.blame_queries;
        dst.blame_useful_seconds += sec(n.blame_useful_ns);
        dst.blame_wasted_seconds += sec(n.blame_wasted_ns);
        dst.kept_guided_seconds += sec(n.kept_guided_ns);
        dst.kept_blind_seconds += sec(n.kept_blind_ns);
        dst.useful_seconds += sec(useful_ns);
        dst.illegal_guided += n.illegal_guided;
        dst.illegal_guided_seconds += sec(n.illegal_guided_ns);
        dst.illegal_blind += n.illegal_blind;
        dst.illegal_blind_seconds += sec(n.illegal_blind_ns);
        dst.no_better_guided += n.no_better_guided;
        dst.no_better_guided_seconds += sec(n.no_better_guided_ns);
        dst.no_better_blind += n.no_better_blind;
        dst.no_better_blind_seconds += sec(n.no_better_blind_ns);
        dst.wasted_seconds += sec(wasted_ns);
        (useful_ns, wasted_ns)
    }
}

/// Open-count progress over the run's CPU timeline, plus what the router spent the second half
/// of its CPU time doing (`last_half`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProgressStats {
    pub initial_open: usize,
    pub final_open: usize,
    pub min_open: usize,
    pub total_cpu_seconds: f64,
    pub routing_cpu_seconds: f64,
    /// CPU timestamp at which the open connection count last reached a new record low.
    pub last_open_drop_cpu_seconds: f64,
    /// `last_open_drop_cpu_seconds / total_cpu_seconds` (`0.0` when `total_cpu_seconds == 0.0`).
    pub last_open_drop_fraction: f64,
    /// Stage in which the open connection count last dropped (`None` if it never dropped).
    pub last_open_drop_stage: Option<String>,
    /// CPU seconds spent in open-connection routing stages after `last_open_drop_cpu_seconds`:
    /// the search time an ideal stop-routing rule would save while still polishing and writing.
    pub no_progress_routing_cpu_seconds: f64,
    /// Total run CPU seconds after `last_open_drop_cpu_seconds` (including polish and write).
    pub no_progress_total_cpu_seconds: f64,
    /// Step changes in the open connection count along the run's CPU timeline.
    pub open_curve: Vec<OpenPoint>,
    /// Breakdown of the run's second half `[0.5 * total_cpu_seconds, total_cpu_seconds]`.
    pub last_half: HalfSlice,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenPoint {
    pub cpu_seconds: f64,
    pub open: usize,
    pub min_open: usize,
    pub stage: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct HalfSlice {
    pub start_cpu_seconds: f64,
    pub end_cpu_seconds: f64,
    pub cpu_seconds: f64,
    pub cpu: CpuBreakdown,
    pub effort: Effort,
    pub stages: Vec<StageSlice>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StageSlice {
    pub name: String,
    pub cpu_seconds: f64,
    pub useful_seconds: f64,
    pub kept_other_seconds: f64,
    pub wasted_seconds: f64,
    pub other_seconds: f64,
}

/// The work counted while a stage ran (`grid::work`): the process's, so a process routing two
/// boards at once counts both.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Work {
    pub astar: u64,
    pub astar_expanded: u64,
    pub fields: u64,
    pub field_settled: u64,
    pub maps: u64,
    pub checks: u64,
    pub connectivity: u64,
}

impl From<grid::work::Work> for Work {
    fn from(w: grid::work::Work) -> Work {
        Work {
            astar: w.astar,
            astar_expanded: w.astar_expanded,
            fields: w.fields,
            field_settled: w.field_settled,
            maps: w.maps,
            checks: w.checks,
            connectivity: w.connectivity,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub stage: &'static str,
    pub net: String,
    pub from: String,
    pub to: String,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Settings {
    /// The command line as the router read it, the recipe's flags put in.
    pub flags: Vec<String>,
    pub seed: u64,
    pub uuid_seed: Option<u64>,
    pub threads: usize,
    pub cell_mm: f64,
    pub timeout: Option<f64>,
    /// Grid nodes at that cell over every copper layer.
    pub nodes: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Memory {
    pub estimate_mb: u64,
    /// The most the process held, where the system says.
    pub peak_mb: Option<u64>,
}

impl RunStats {
    /// The table `--verbose` prints, one stage a line.
    pub fn table(&self) -> Vec<String> {
        let mut out = vec![format!(
            "stats: {:<16} {:>8} {:>6} {:>6} {:>6} {:>6} {:>5} {:>8} {:>11} {:>8} {:>5} {:>4}",
            "stage", "seconds", "open<", "open>", "laid", "ripped", "vias", "searches", "expanded", "checks", "maps", "cut"
        )];
        for s in &self.stages {
            out.push(format!(
                "stats: {:<16} {:>8.2} {:>6} {:>6} {:>6} {:>6} {:>5} {:>8} {:>11} {:>8} {:>5} {:>4}",
                s.name,
                s.seconds,
                s.open_before,
                s.open_after,
                s.laid,
                s.ripped,
                s.vias,
                s.work.astar + s.work.fields,
                s.work.astar_expanded + s.work.field_settled,
                s.work.checks,
                s.work.maps,
                if s.cut { "yes" } else { "" }
            ));
        }
        let m = &self.summary;
        out.push(format!(
            "stats: {} of {} connections routed, {} open, {} vias, {} findings, {:.1} s ({:.2} s counting)",
            m.routed,
            m.connections,
            m.opens,
            m.vias,
            self.findings.len(),
            m.seconds,
            self.stats_seconds
        ));
        out
    }
}

/// The SHA-256 of `data`, in hex.
pub fn sha256(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
        0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            (hh, g, f, e, d, c, b, a) = (g, f, e, d.wrapping_add(t1), c, b, a, t1.wrapping_add(t2));
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(sha256(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(sha256(&[b'a'; 1000]), "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3");
    }

    #[test]
    fn the_deadline_lines_read_as_the_benchmark_expects() {
        let d = DeadlineStats { limited: vec!["negotiation".into(), "rip-up".into()], hard_passed_in: Some("shove".into()), overran_seconds: Some(1.26), cancelled: false };
        assert_eq!(d.lines(), ["deadline: time ran out in negotiation, rip-up", "deadline: the run's hard limit passed during shove; the run overran --timeout by 1.3 s"]);
        let within = DeadlineStats { hard_passed_in: Some("vias".into()), overran_seconds: Some(-0.5), ..Default::default() };
        assert_eq!(within.lines(), ["deadline: the run's hard limit passed during vias; the run still ended within --timeout"]);
        assert!(DeadlineStats::default().lines().is_empty());
    }
}
