//! The router on the MIT core as a library: the `ultraroute` command's options, its whole
//! pipeline from a board's text to the routed board's text, and the studio's live frames. The
//! command and the in-browser router (`web`) both drive it.
//!
//! `ultraroute route in.kicad_pcb -o out.kicad_pcb`: routes a KiCad board on the MIT core.
//!
//! The project's `.kicad_pro` beside the input supplies the rules. Pours are left as they are
//! for KiCad to refill. With `--live -` the board streams to standard output as the studio's
//! newline-delimited JSON frames, and a line `stop` on standard input stops the run. With
//! `--blame`, every connection still open at the end is listed with what stands in its way.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use board::{connectivity, connectivity_without_pours, Board, Item, ItemId, Kind, Nm, Point, Shape};
use kicad_pcb::{apply, mm, Edit};
use serde_json::{json, Value};

mod regions;

pub const USAGE: &str = "usage: ultraroute route IN.kicad_pcb -o OUT.kicad_pcb [--strip] [--strip-all] [--planes LAYER,...] [--auto-planes] [--negotiate-rounds N] [--negotiate-patience N] [--negotiate-growth G] [--negotiate-repasses N] [--no-negotiate-vias] [--no-negotiate-rip] [--no-negotiate-tree] [--negotiate-jacobi] [--negotiate-reroute-all] [--negotiate-legacy] [--diff-pairs] [--diff-pairs-legacy] [--prefanout] [--prefanout-mode stub|dogbone|grid|inner|same] [--prefanout-order round|inner|trapped] [--prefanout-ring MM] [--prefanout-soft] [--prefanout-keep-unused] [--no-prefanout-inward] [--topo] [--negotiate] [--rip-up] [--nudge] [--nudge-guided] [--tighten] [--via-cost MM[,OPEN_MM]] [--no-directional] [--no-reduce-vias] [--gpu] \
[--timeout SECONDS] [--cell MM] [--first NET,...] [--first-negotiate] [--first-weight W] [--restart N] [--topo-first REF,...|auto] [--topo-trapped] [--topo-trapped-rip] [--topo-sample MM] \
[--shove] [--shove-plough rip|region] [--shove-reach MM] [--shove-repair S] [--shove-passes N] [--shove-ties] [--shove-cascade N] [--shove-patience N] [--rip-region X,Y,R]... [--via-avoid X0,Y0,X1,Y1]... [--via-avoid-cost N] [--regions FILE] [--pour-cost W] [--seed N] [--uuid-seed N] [--adopt] [--verbose] [--place REF=X,Y]... [--lock REF,...] [--pours-block] [--no-route] [--threads N] [--max-memory MB] [--estimate] \
[--recipe default|bench|minimal] [--live PATH|-] [--live-fps N] [--live-step MS] [--blame]
--place takes millimetres with y pointing up: give KiCad's y negated (KiCad y 127.5 is -127.5).";

pub struct Args {
    pub input: String,
    pub output: String,
    pub strip: bool,
    pub strip_all: bool,
    /// Escape every fine-pitch part's pads before routing: how, in what order, how far out the
    /// escapes end, whether the router may rip them, whether unused ones stay, and whether power
    /// pads are first bridged inward to their pours.
    /// Route differential pairs (USB D+ and D-) coupled, before anything else.
    pub diff_pairs: bool,
    /// Use the legacy single-layer, legless differential pair stage, for comparison.
    pub diff_pairs_legacy: bool,
    /// Layers given whole to a plane (`In1.Cu,In2.Cu`): no tracks on them, vias through them, and
    /// their pours join their nets.
    pub planes: Vec<String>,
    /// Negotiation's most rounds, rounds without progress before it stops, how fast its
    /// congestion price grows, and passes after the first; the router's defaults when unset.
    pub negotiate_rounds: Option<usize>,
    pub negotiate_patience: Option<usize>,
    pub negotiate_growth: Option<f64>,
    pub negotiate_repasses: Option<usize>,
    /// Whether negotiation prices vias, rescues connections its own routes wall off, and spans
    /// trees past pocketed pins (`grid::NegotiationOptions`); `--negotiate-legacy` turns these
    /// and the passes after the first off, for comparing against.
    pub negotiate_vias: bool,
    pub negotiate_rip: bool,
    pub negotiate_tree: bool,
    /// Negotiation routes each round against the last on `--threads` threads
    /// (`NegotiationOptions::jacobi`), instead of one connection after another.
    pub negotiate_jacobi: bool,
    /// Every negotiation round reroutes every connection, not only those that collided.
    pub negotiate_reroute_all: bool,
    pub prefanout: bool,
    pub prefanout_mode: Option<grid::EscapeMode>,
    pub prefanout_order: Option<grid::EscapeOrder>,
    pub prefanout_ring: Option<Nm>,
    pub prefanout_soft: bool,
    pub prefanout_keep_unused: bool,
    pub prefanout_inward: bool,
    pub topo: bool,
    pub negotiate: bool,
    pub rip_up: bool,
    pub tighten: bool,
    /// Via prices in millimetres of straight track: negotiation's, and that of routing one
    /// connection at a time and of rip-up (1.2 and 0.8 times negotiation's unless given).
    pub via_cost: Option<(f64, Option<f64>)>,
    /// Two-layer routes without a preferred direction per layer.
    pub no_directional: bool,
    /// No pass after routing that takes out vias a route on one layer can do without.
    pub no_reduce_vias: bool,
    pub nudge: bool,
    /// Nudge only by blame's moves, without trying small steps in every direction.
    pub nudge_guided: bool,
    pub lock: Vec<String>,
    pub gpu: bool,
    pub pours_block: bool,
    pub timeout: Option<f64>,
    pub place: Vec<(String, f64, f64)>,
    pub live: Option<String>,
    pub fps: f64,
    pub no_route: bool,
    pub threads: Option<usize>,
    /// The most memory the run may take, in megabytes; `None` for the machine's own limit
    /// ([`memory_cap`]).
    pub max_memory: Option<u64>,
    /// Print what the run's grids will take, and the limit, then stop.
    pub estimate: bool,
    /// Grid cell for every stage, in nanometres.
    pub cell: Option<Nm>,
    /// Nets to favour first (names, `@REF` for a part's signal nets, or `@auto` for fine-pitch parts).
    pub first: Vec<String>,
    /// Negotiate priority nets among themselves before the main negotiation pass.
    pub first_negotiate: bool,
    /// Scale congestion and history penalties for priority nets in negotiation instead of pre-routing them.
    pub first_weight: Option<f64>,
    /// Extra passes from the starting board, promoting nets left open to priority and keeping the best board.
    pub restart: usize,
    /// Parts (or `auto`, `REF:signals`, `REF:pads`, or net names) whose connections route first as rubber bands.
    pub topo_first: Vec<String>,
    /// Try rubber-band routing on the connections still open after the grid stages, without ripping grid copper.
    pub topo_trapped: bool,
    /// Try rubber-band routing with rip-up of soft grid copper on the connections still open after the grid stages.
    pub topo_trapped_rip: bool,
    /// Outline sample spacing for topological passes, in nanometres.
    pub topo_sample: Option<Nm>,
    /// Plough a way out for each open connection trapped at a fine-pitch part's pads.
    pub shove: bool,
    /// How the plough clears the copper in the way, its reach, the rip-up repair after each try,
    /// how many passes, and whether a try that frees its connection without losing ground is kept.
    pub shove_plough: Option<grid::Plough>,
    pub shove_reach: Option<f64>,
    pub shove_repair: Option<f64>,
    pub shove_passes: usize,
    pub shove_ties: bool,
    /// Reorders the connections negotiation and the grid router take, for a search over runs.
    pub seed: u64,
    /// The new copper's uuids drawn from this seed, so that a run's output is the same every time;
    /// random when `None`.
    pub uuid_seed: Option<u64>,
    /// Connections a plough try knocked loose that are ploughed out in turn.
    pub shove_cascade: Option<usize>,
    /// Plough passes in a row that free nothing before the plough stops.
    pub shove_patience: usize,
    /// Discs, in mm as in the board file (y down), in which every unlocked track and via is ripped
    /// before routing: a large move for a search, rebuilding one region from scratch.
    pub rip_regions: Vec<(f64, f64, f64)>,
    /// Boxes, in millimetres, where vias cost dearly but are allowed.
    pub via_avoid: Vec<[f64; 4]>,
    pub via_avoid_cost: Option<u32>,
    /// A planner's routing regions and length limits (JSON), see `regions`.
    pub regions: Option<String>,
    /// What crossing another net's pour costs, as a multiple of the length inside.
    pub pour_cost: Option<f64>,
    /// Take inner layers that one net's pour all but fills as planes, as --planes would.
    pub auto_planes: bool,
    /// Treat the board's unlocked tracks and vias as the router's own: free to rip, move and
    /// rewrite, as if this run had laid them.
    pub adopt: bool,
    /// Everything the router says it is doing, on standard error.
    pub verbose: bool,
    /// Least time between the live stream's step frames, in its clock: see [`Live::with_step`].
    pub live_step: u64,
    pub blame: bool,
}

/// The stages and settings the router runs with unless told otherwise: the benchmark's recipe
/// (`docs/pcbench.md`), measured as the best of what the router has.
pub const RECIPE: &str = "--negotiate --rip-up --tighten --prefanout --diff-pairs --shove-plough region --shove-reach 0.5 --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8";

/// Flags that name a stage to run. A run naming none gets [`RECIPE`]; one naming its own stages
/// gets those alone, as before there was a default.
const STAGE_FLAGS: &[&str] = &[
    "--negotiate", "--rip-up", "--tighten", "--prefanout", "--diff-pairs", "--shove", "--shove-plough", "--nudge", "--topo", "--topo-first",
    "--topo-trapped", "--topo-trapped-rip", "--restart", "--first",
    // Loading a board to look at it names no stage to run, and wants none.
    "--no-route",
];

/// Reads a command line. `--recipe default` (or `bench`, the same) puts [`RECIPE`]'s flags before
/// the others, which then override its settings; `--recipe minimal` puts none, for library users
/// and tests; with no `--recipe`, a line naming no stage gets `default`.
pub fn parse(args: &[String]) -> Result<Args, String> {
    if args.first().map(String::as_str) != Some("route") {
        return Err(USAGE.to_string());
    }
    let mut args = args.to_vec();
    let named = match args.iter().position(|a| a == "--recipe") {
        Some(i) if i + 1 < args.len() => {
            let name = args.remove(i + 1);
            args.remove(i);
            Some(name)
        }
        Some(_) => return Err("--recipe: default, bench or minimal".to_string()),
        None => None,
    };
    let recipe = match named.as_deref() {
        Some("default" | "bench") => RECIPE,
        Some("minimal") => "",
        Some(other) => return Err(format!("--recipe {other}: default, bench or minimal")),
        None if args.iter().any(|a| STAGE_FLAGS.contains(&a.as_str())) => "",
        None => RECIPE,
    };
    let mut all = vec![args[0].clone()];
    all.extend(recipe.split_whitespace().map(String::from));
    all.extend(args[1..].iter().cloned());
    parse_flags(&all)
}

fn parse_flags(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        input: String::new(),
        output: String::new(),
        strip: false,
        strip_all: false,
        diff_pairs: false,
        diff_pairs_legacy: false,
        planes: Vec::new(),
        negotiate_rounds: None,
        negotiate_patience: None,
        negotiate_growth: None,
        negotiate_repasses: None,
        negotiate_vias: true,
        negotiate_rip: true,
        negotiate_tree: true,
        negotiate_jacobi: false,
        negotiate_reroute_all: false,
        prefanout: false,
        prefanout_mode: None,
        prefanout_order: None,
        prefanout_ring: None,
        prefanout_soft: false,
        prefanout_keep_unused: false,
        prefanout_inward: true,
        topo: false,
        negotiate: false,
        rip_up: false,
        tighten: false,
        via_cost: None,
        no_directional: false,
        no_reduce_vias: false,
        nudge: false,
        nudge_guided: false,
        lock: Vec::new(),
        gpu: false,
        pours_block: false,
        timeout: None,
        place: Vec::new(),
        live: None,
        fps: 30.0,
        no_route: false,
        threads: None,
        max_memory: None,
        estimate: false,
        cell: None,
        first: Vec::new(),
        first_negotiate: false,
        first_weight: None,
        restart: 0,
        topo_first: Vec::new(),
        topo_trapped: false,
        topo_trapped_rip: false,
        topo_sample: None,
        shove: false,
        shove_plough: None,
        shove_reach: None,
        shove_repair: None,
        shove_passes: 1,
        shove_ties: false,
        seed: 0,
        uuid_seed: None,
        shove_cascade: None,
        shove_patience: 1,
        rip_regions: Vec::new(),
        via_avoid: Vec::new(),
        via_avoid_cost: None,
        regions: None,
        pour_cost: None,
        auto_planes: false,
        adopt: false,
        verbose: false,
        live_step: 0,
        blame: false,
    };
    let mut it = args[1..].iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        let mut value = || inline.clone().or_else(|| it.next().cloned()).ok_or(format!("{flag} needs a value"));
        match flag {
            "-o" | "--output" => a.output = value()?,
            "--strip" => a.strip = true,
            "--strip-all" => a.strip_all = true,
            "--diff-pairs" => a.diff_pairs = true,
            "--diff-pairs-legacy" => {
                a.diff_pairs = true;
                a.diff_pairs_legacy = true;
            }
            "--negotiate-rounds" => a.negotiate_rounds = Some(value()?.parse().map_err(|e| format!("--negotiate-rounds: {e}"))?),
            "--negotiate-patience" => a.negotiate_patience = Some(value()?.parse().map_err(|e| format!("--negotiate-patience: {e}"))?),
            "--negotiate-growth" => {
                let g: f64 = value()?.parse().map_err(|e| format!("--negotiate-growth: {e}"))?;
                if !g.is_finite() || g < 1.0 {
                    return Err(format!("--negotiate-growth: {g} must be 1 or more"));
                }
                a.negotiate_growth = Some(g);
            }
            "--negotiate-repasses" => a.negotiate_repasses = Some(value()?.parse().map_err(|e| format!("--negotiate-repasses: {e}"))?),
            "--no-negotiate-vias" => a.negotiate_vias = false,
            "--no-negotiate-rip" => a.negotiate_rip = false,
            "--no-negotiate-tree" => a.negotiate_tree = false,
            "--negotiate-jacobi" => a.negotiate_jacobi = true,
            "--negotiate-reroute-all" => a.negotiate_reroute_all = true,
            "--negotiate-legacy" => {
                a.negotiate_repasses = Some(0);
                a.negotiate_vias = false;
                a.negotiate_rip = false;
                a.negotiate_tree = false;
            }
            "--planes" => a.planes.extend(value()?.split(',').filter(|s| !s.is_empty()).map(String::from)),
            "--prefanout" => a.prefanout = true,
            "--prefanout-mode" => {
                a.prefanout = true;
                let v = value()?;
                a.prefanout_mode = Some(match v.as_str() {
                    "stub" => grid::EscapeMode::Stub,
                    "dogbone" => grid::EscapeMode::Dogbone,
                    "grid" => grid::EscapeMode::GridRing,
                    "inner" => grid::EscapeMode::GridInner,
                    "same" => grid::EscapeMode::GridSame,
                    _ => return Err(format!("--prefanout-mode: {v} is not stub, dogbone, grid, inner or same")),
                });
            }
            "--prefanout-order" => {
                a.prefanout = true;
                let v = value()?;
                a.prefanout_order = Some(match v.as_str() {
                    "round" => grid::EscapeOrder::Round,
                    "inner" => grid::EscapeOrder::InnerFirst,
                    "trapped" => grid::EscapeOrder::TrappedFirst,
                    _ => return Err(format!("--prefanout-order: {v} is not round, inner or trapped")),
                });
            }
            "--prefanout-ring" => {
                a.prefanout = true;
                let mm: f64 = value()?.parse().map_err(|e| format!("--prefanout-ring: {e}"))?;
                if !(0.2..=5.0).contains(&mm) {
                    return Err(format!("--prefanout-ring: {mm} mm is outside 0.2 to 5 mm"));
                }
                a.prefanout_ring = Some((mm * 1e6).round() as Nm);
            }
            "--prefanout-soft" => {
                a.prefanout = true;
                a.prefanout_soft = true;
            }
            "--prefanout-keep-unused" => {
                a.prefanout = true;
                a.prefanout_keep_unused = true;
            }
            "--no-prefanout-inward" => {
                a.prefanout = true;
                a.prefanout_inward = false;
            }
            "--topo" => a.topo = true,
            "--negotiate" => a.negotiate = true,
            "--rip-up" => a.rip_up = true,
            "--tighten" => a.tighten = true,
            "--via-cost" => {
                let v = value()?;
                let mm: Vec<f64> = v.split(',').map(|x| x.trim().parse::<f64>().map_err(|e| format!("--via-cost {v}: {e}"))).collect::<Result<_, _>>()?;
                if mm.iter().any(|x| !(0.05..=50.0).contains(x)) {
                    return Err(format!("--via-cost {v}: from 0.05 to 50 mm"));
                }
                a.via_cost = match mm[..] {
                    [neg] => Some((neg, None)),
                    [neg, open] => Some((neg, Some(open))),
                    _ => return Err(format!("--via-cost {v}: MM or NEGOTIATE_MM,OPEN_MM")),
                };
            }
            "--no-directional" => a.no_directional = true,
            "--no-reduce-vias" => a.no_reduce_vias = true,
            "--gpu" => a.gpu = true,
            "--pours-block" => a.pours_block = true,
            "--no-route" => a.no_route = true,
            "--threads" => a.threads = Some(value()?.parse().map_err(|e| format!("--threads: {e}"))?),
            "--max-memory" => a.max_memory = Some(value()?.parse().map_err(|e| format!("--max-memory: {e}"))?),
            "--estimate" => a.estimate = true,
            "--cell" => {
                let mm: f64 = value()?.parse().map_err(|e| format!("--cell: {e}"))?;
                if !(0.005..=1.0).contains(&mm) {
                    return Err(format!("--cell: {mm} mm is outside 0.005 to 1 mm"));
                }
                a.cell = Some((mm * 1e6).round() as Nm);
            }
            "--first" => a.first.extend(value()?.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)),
            "--first-negotiate" => a.first_negotiate = true,
            "--first-weight" => {
                let w: f64 = value()?.parse().map_err(|e| format!("--first-weight: {e}"))?;
                if !w.is_finite() || w < 0.0 {
                    return Err(format!("--first-weight: {w} must be 0 or greater"));
                }
                a.first_weight = Some(w);
            }
            "--restart" => a.restart = value()?.parse().map_err(|e| format!("--restart: {e}"))?,
            "--topo-first" => a.topo_first.extend(value()?.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)),
            "--topo-trapped" => a.topo_trapped = true,
            "--topo-trapped-rip" => a.topo_trapped_rip = true,
            "--topo-sample" => {
                let mm: f64 = value()?.parse().map_err(|e| format!("--topo-sample: {e}"))?;
                if !(0.01..=1.0).contains(&mm) {
                    return Err(format!("--topo-sample: {mm} mm is outside 0.01 to 1 mm"));
                }
                a.topo_sample = Some((mm * 1e6).round() as Nm);
            }
            "--timeout" => a.timeout = Some(value()?.parse().map_err(|e| format!("--timeout: {e}"))?),
            "--live" => a.live = Some(value()?),
            "--live-fps" => a.fps = value()?.parse().map_err(|e| format!("--live-fps: {e}"))?,
            "--live-step" => a.live_step = value()?.parse().map_err(|e| format!("--live-step: {e}"))?,
            "--lock" => a.lock.extend(value()?.split(',').filter(|r| !r.is_empty()).map(str::to_string)),
            "--nudge" => a.nudge = true,
            "--nudge-guided" => {
                a.nudge = true;
                a.nudge_guided = true;
            }
            "--shove" => a.shove = true,
            "--shove-plough" => {
                a.shove = true;
                a.shove_plough = Some(match value()?.as_str() {
                    "rip" => grid::Plough::Rip,
                    "region" => grid::Plough::Region,
                    other => return Err(format!("--shove-plough {other}: rip or region")),
                });
            }
            "--shove-reach" => a.shove_reach = Some(value()?.parse().map_err(|e| format!("--shove-reach: {e}"))?),
            "--shove-repair" => a.shove_repair = Some(value()?.parse().map_err(|e| format!("--shove-repair: {e}"))?),
            "--shove-passes" => a.shove_passes = value()?.parse().map_err(|e| format!("--shove-passes: {e}"))?,
            "--shove-ties" => a.shove_ties = true,
            "--shove-cascade" => a.shove_cascade = Some(value()?.parse().map_err(|e| format!("--shove-cascade: {e}"))?),
            "--shove-patience" => a.shove_patience = value()?.parse().map_err(|e| format!("--shove-patience: {e}"))?,
            "--seed" => a.seed = value()?.parse().map_err(|e| format!("--seed: {e}"))?,
            "--uuid-seed" => a.uuid_seed = Some(value()?.parse().map_err(|e| format!("--uuid-seed: {e}"))?),
            "--adopt" => a.adopt = true,
            "--verbose" | "-v" => a.verbose = true,
            "--blame" => a.blame = true,
            "--via-avoid" => {
                let v = value()?;
                let n: Vec<f64> = v.split(',').map(|x| x.trim().parse::<f64>()).collect::<Result<_, _>>().map_err(|e| format!("--via-avoid {v}: {e}"))?;
                let [x0, y0, x1, y1] = n[..] else { return Err(format!("--via-avoid {v}: X0,Y0,X1,Y1 in mm")) };
                a.via_avoid.push([x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)]);
            }
            "--auto-planes" => a.auto_planes = true,
            "--regions" => a.regions = Some(value()?),
            "--pour-cost" => a.pour_cost = Some(value()?.parse().map_err(|e| format!("--pour-cost: {e}"))?),
            "--via-avoid-cost" => a.via_avoid_cost = Some(value()?.parse().map_err(|e| format!("--via-avoid-cost: {e}"))?),
            "--rip-region" => {
                let v = value()?;
                let n: Vec<f64> = v.split(',').map(|x| x.trim().parse::<f64>()).collect::<Result<_, _>>().map_err(|e| format!("--rip-region {v}: {e}"))?;
                let [x, y, r] = n[..] else { return Err(format!("--rip-region {v}: X,Y,RADIUS in mm")) };
                a.rip_regions.push((x, y, r));
            }
            "--place" => {
                let v = value()?;
                let (name, at) = v.split_once('=').ok_or("--place REF=X,Y")?;
                let (x, y) = at.split_once(',').ok_or("--place REF=X,Y")?;
                let n = |s: &str| s.trim().parse::<f64>().map_err(|e| format!("--place {v}: {e}"));
                a.place.push((name.to_string(), n(x)?, n(y)?));
            }
            f if f.starts_with('-') => return Err(format!("unknown option {f}\n{USAGE}")),
            path => a.input = path.to_string(),
        }
    }
    if a.input.is_empty() || a.output.is_empty() {
        return Err(USAGE.to_string());
    }
    Ok(a)
}

/// The studio draws Y up; a KiCad board runs Y down.
fn mm_xy(p: Point) -> [f64; 2] {
    [round(p.x as f64 * 1e-6), round(-p.y as f64 * 1e-6)]
}

fn round(v: f64) -> f64 {
    (v * 1e4).round() / 1e4
}

fn flat(points: &[Point]) -> Vec<f64> {
    points.iter().flat_map(|p| mm_xy(*p)).collect()
}

fn shape_json(shape: &Shape) -> Value {
    match shape {
        Shape::Capsule { a, b, r } if a == b => {
            let c = mm_xy(*a);
            json!({"c": [c[0], c[1], round(*r as f64 * 1e-6)]})
        }
        Shape::Capsule { a, b, r } => {
            // A capsule pad as its outline polygon, for the studio's fills.
            let base = ((b.y - a.y) as f64).atan2((b.x - a.x) as f64);
            let mut pts = Vec::new();
            for (c, from) in [(*b, base - std::f64::consts::FRAC_PI_2), (*a, base + std::f64::consts::FRAC_PI_2)] {
                for k in 0..=8 {
                    let t = from + std::f64::consts::PI * k as f64 / 8.0;
                    pts.push(Point::new(c.x + (*r as f64 * t.cos()) as i64, c.y + (*r as f64 * t.sin()) as i64));
                }
            }
            json!({"g": flat(&pts)})
        }
        Shape::Polygon(p) => json!({"g": flat(p)}),
    }
}

fn layers_of(board: &Board, item: &Item) -> Vec<usize> {
    (0..board.layers.len()).filter(|l| item.on(*l)).collect()
}

fn item_json(board: &Board, id: ItemId, item: &Item) -> Value {
    let layers = layers_of(board, item);
    match &item.kind {
        Kind::Track => {
            let Shape::Capsule { a, b, r } = &item.shape else { return Value::Null };
            json!({"id": id, "k": "trace", "net": item.net, "fixed": item.locked, "layer": layers.first(),
                   "w": round(2.0 * *r as f64 * 1e-6), "p": flat(&[*a, *b])})
        }
        Kind::Via => {
            let Shape::Capsule { a, .. } = &item.shape else { return Value::Null };
            json!({"id": id, "k": "via", "net": item.net, "fixed": item.locked, "at": mm_xy(*a),
                   "layers": [layers.first(), layers.last()], "shape": shape_json(&item.shape),
                   "drill": item.hole.map(|h| round(h.1 as f64 * 1e-6))})
        }
        Kind::Pad { footprint, .. } => {
            let b = item.shape.bbox();
            let shape = shape_json(&item.shape);
            json!({"id": id, "k": "pin", "net": item.net, "part": footprint + 1,
                   "at": mm_xy(Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2)),
                   "pads": layers.iter().map(|l| json!({"layer": l, "shape": shape})).collect::<Vec<_>>(),
                   "drill": item.hole.map(|h| round(h.1 as f64 * 1e-6))})
        }
        Kind::Pour { .. } | Kind::Keepout { .. } => {
            let Shape::Polygon(p) = &item.shape else { return Value::Null };
            let kind = if matches!(item.kind, Kind::Pour { .. }) { "pour" } else { "keepout" };
            let part = match &item.kind {
                Kind::Keepout { footprint: Some(f), .. } => f + 1,
                _ => 0,
            };
            json!({"id": id, "k": kind, "net": item.net, "part": part,
                   "tiles": layers.iter().map(|l| json!({"layer": l, "g": flat(p)})).collect::<Vec<_>>()})
        }
    }
}

/// The board as the studio's frames: everything once, then what was added or removed.
pub struct Live {
    out: Box<dyn Write + Send>,
    /// Activity since the last frame, and where the router last looked.
    activity: Vec<Value>,
    focus: Value,
    /// What the viewer has, by id, as it was sent: an item moved in place is sent again.
    sent: HashMap<ItemId, Shape>,
    seq: u64,
    started: Instant,
    last: Option<Instant>,
    interval: Duration,
    pours_join: bool,
    /// Least time between forced frames in the stream's clock, and how far that clock has been
    /// pushed ahead of the wall's to keep them apart.
    hold: u64,
    ahead: u64,
    last_ms: u64,
}

impl Live {
    /// Frames written to `out`, at most `fps` a second.
    pub fn new(out: Box<dyn Write + Send>, fps: f64, pours_join: bool) -> Live {
        Live { out, activity: Vec::new(), focus: Value::Null, sent: HashMap::new(), seq: 0, started: Instant::now(), last: None, interval: Duration::from_secs_f64(1.0 / fps.max(1.0)), pours_join, hold: 0, ahead: 0, last_ms: 0 }
    }

    /// Spaces the stream's forced frames, each a step of the work, at least `ms` apart in its
    /// clock, so a recording shows every step for a moment however fast it ran.
    pub fn with_step(mut self, ms: u64) -> Live {
        self.hold = ms;
        self
    }

    /// The stream's clock for the next frame; a forced frame is held apart from the last.
    fn stamp(&mut self, force: bool) -> u64 {
        let mut ms = self.started.elapsed().as_millis() as u64 + self.ahead;
        if force && self.hold > 0 && self.seq > 0 && ms < self.last_ms + self.hold {
            self.ahead += self.last_ms + self.hold - ms;
            ms = self.last_ms + self.hold;
        }
        self.last_ms = self.last_ms.max(ms);
        ms
    }

    /// Takes the router's activity; when the board has not changed for a frame, sends it alone.
    fn note(&mut self, act: grid::Activity) {
        self.activity.push(json!(format!("{}: {}", act.phase, act.text)));
        match act.focus {
            Some(grid::Focus::Connection { net, from, to }) => {
                self.focus = json!({"phase": act.phase, "net": net, "a": mm_xy(from), "b": mm_xy(to)});
            }
            Some(grid::Focus::Part(f)) => self.focus = json!({"phase": act.phase, "part": f + 1}),
            // Board Y runs down, the studio's up.
            Some(grid::Focus::Trial { part, by }) => {
                self.focus = json!({"phase": act.phase, "part": part + 1, "ghost": [round(by.x as f64 * 1e-6), round(-by.y as f64 * 1e-6)]});
            }
            None => {
                if let Some(focus) = self.focus.as_object_mut() {
                    focus.insert("phase".into(), json!(act.phase));
                }
            }
        }
        if self.last.is_some_and(|t| t.elapsed() < self.interval) {
            return;
        }
        self.last = Some(Instant::now());
        self.seq += 1;
        let ms = self.stamp(false);
        let v = json!({
            "t": "frame", "seq": self.seq, "ms": ms,
            "add": [], "del": [], "parts": [], "probes": [],
            "events": std::mem::take(&mut self.activity), "focus": self.focus,
        });
        self.write(&v);
    }

    fn write(&mut self, v: &Value) {
        let _ = serde_json::to_writer(&mut self.out, v);
        let _ = self.out.write_all(b"\n");
        let _ = self.out.flush();
    }

    fn header(&mut self, board: &Board) {
        let all: Vec<Point> = board.outline.iter().flatten().copied().collect();
        let bb = board::BBox::of(all.iter().copied());
        let nets: serde_json::Map<String, Value> =
            board.nets.iter().enumerate().filter(|(_, n)| !n.is_empty()).map(|(i, n)| (i.to_string(), json!(n))).collect();
        let v = json!({
            "t": "board",
            "layers": board.layers.iter().map(|l| json!({"name": l, "signal": true})).collect::<Vec<_>>(),
            "nets": nets,
            "bbox": [round(bb.min.x as f64 * 1e-6), round(-bb.max.y as f64 * 1e-6), round(bb.max.x as f64 * 1e-6), round(-bb.min.y as f64 * 1e-6)],
        });
        self.write(&v);
        self.frame(board, true, Vec::new());
    }

    /// The zones of the board in `text` poured as KiCad would refill them, for drawing.
    fn fills(&mut self, text: &str, rules: &kicad_pcb::Rules) {
        let Ok((pcb, copper)) = kicad_pcb::parse_board(text) else { return };
        let board = Board::new(&pcb, &copper, rules.clone());
        let mm = |v: Nm| round(v as f64 * 1e-6);
        let fills: Vec<Value> = board::fill_zones(&board, &copper)
            .iter()
            .map(|f| {
                let rects: Vec<f64> = f.rects.iter().flat_map(|r| [mm(r[0]), mm(-r[3]), mm(r[2]), mm(-r[1])]).collect();
                json!({"zone": f.zone, "net": f.net, "layer": f.layer, "r": rects})
            })
            .collect();
        self.write(&json!({"t": "fill", "fills": fills}));
    }

    /// Negotiation's current trial routes, drawn over the board until the next ones.
    fn trials(&mut self, _round: usize, trials: &[grid::Trial]) {
        if self.last.is_some_and(|t| t.elapsed() < self.interval) {
            return;
        }
        self.last = Some(Instant::now());
        let mut runs: Vec<Value> = Vec::new();
        let mut vias: Vec<Value> = Vec::new();
        for (net, corners, collides) in trials {
            let mut k = 0;
            while k < corners.len() {
                let layer = corners[k].0;
                let mut points = vec![corners[k].1];
                k += 1;
                while k < corners.len() && corners[k].0 == layer {
                    points.push(corners[k].1);
                    k += 1;
                }
                if points.len() >= 2 {
                    runs.push(json!({"net": net, "layer": layer, "p": flat(&points), "collides": collides}));
                }
                // The route changes layer where this run ends: a via, if it is laid.
                if k < corners.len() {
                    vias.push(json!(mm_xy(corners[k - 1].1)));
                }
            }
        }
        self.seq += 1;
        let ms = self.stamp(false);
        let v = json!({
            "t": "frame", "seq": self.seq, "ms": ms,
            "add": [], "del": [], "parts": [], "probes": [],
            "events": std::mem::take(&mut self.activity), "focus": self.focus,
            "trial": runs, "trial_vias": vias,
        });
        self.write(&v);
    }

    fn frame(&mut self, board: &Board, force: bool, events: Vec<Value>) {
        if !force && self.last.is_some_and(|t| t.elapsed() < self.interval) {
            return;
        }
        self.last = Some(Instant::now());
        let now: HashMap<ItemId, Shape> = board.items().map(|(id, it)| (id, it.shape.clone())).collect();
        let mut add: Vec<Value> =
            board.items().filter(|(id, it)| self.sent.get(id) != Some(&it.shape)).map(|(id, it)| item_json(board, id, it)).collect();
        let mut del: Vec<ItemId> = self.sent.keys().filter(|id| !now.contains_key(id)).copied().collect();
        del.sort_unstable();
        if self.seq == 0 {
            let polys: Vec<Vec<f64>> = board.outline.iter().map(|p| flat(p)).collect();
            add.push(json!({"id": u32::MAX, "k": "outline", "polys": polys}));
        }
        self.sent = now;
        let ms = self.stamp(force);
        self.seq += 1;
        let parts: Vec<Value> = board
            .footprints
            .iter()
            .enumerate()
            .map(|(i, f)| json!({"id": i + 1, "name": f.reference, "at": mm_xy(f.at), "fixed": f.locked}))
            .collect();
        let open = if self.pours_join { connectivity(board) } else { connectivity_without_pours(board) }.airlines;
        let air: Vec<Value> = open
            .iter()
            .map(|a| {
                let (f, t) = (mm_xy(a.from), mm_xy(a.to));
                json!([f[0], f[1], t[0], t[1], a.net])
            })
            .collect();
        let v = json!({
            "t": "frame", "seq": self.seq, "ms": ms,
            "add": add, "del": del, "parts": parts, "probes": [],
            "events": events.into_iter().chain(std::mem::take(&mut self.activity)).collect::<Vec<_>>(), "focus": self.focus,
            "open": air.len(), "air": air, "trial": [], "trial_vias": [],
        });
        self.write(&v);
    }
}

/// Every part that moved, by hand or by the nudger, as the `(at …)` it now has.
/// The fills of refillable pours, as last poured, that new copper of another net now comes
/// within clearance of, or every such fill once a part has moved: written out they would short
/// the new copper until KiCad refilled them, so they go, and KiCad shows those zones unfilled.
/// Kept pours, which the router steers clear of, keep theirs.
fn stale_fills(board: &Board, laid: &[ItemId], text: &str, moved: bool) -> Vec<kicad_pcb::Span> {
    let Ok((_, copper)) = kicad_pcb::parse_board(text) else { return Vec::new() };
    let new: Vec<&Item> = laid.iter().filter_map(|&id| board.item(id)).filter(|it| matches!(it.kind, Kind::Track | Kind::Via)).collect();
    let mut stale: BTreeSet<(usize, String)> = BTreeSet::new();
    for (_, pour) in board.items() {
        let Kind::Pour { zone, kept: false } = pour.kind else { continue };
        let Some(z) = copper.zones.get(zone) else { continue };
        let layer = board.layers.get(pour.layers.trailing_zeros() as usize).cloned().unwrap_or_default();
        if stale.contains(&(zone, layer.clone())) {
            continue;
        }
        let cuts = |it: &&Item| {
            let c = (board.clearance(pour.net, it.net).max(board::nm(z.clearance)) + 1) as f64;
            it.net != pour.net && it.layers & pour.layers != 0 && pour.shape.bbox().grow(c as Nm).overlaps(it.shape.bbox()) && pour.shape.gap_within(&it.shape, c) < c
        };
        if moved || new.iter().any(cuts) {
            stale.insert((zone, layer));
        }
    }
    stale
        .iter()
        .flat_map(|(zone, layer)| {
            let z = &copper.zones[*zone];
            z.filled.iter().zip(&z.fill_spans).filter(move |((l, _), _)| l == layer).map(|(_, span)| *span)
        })
        .collect()
}

fn moved_parts(board: &Board, original_at: &[Point]) -> Vec<(kicad_pcb::Span, String)> {
    board
        .footprints
        .iter()
        .zip(original_at)
        .filter(|(f, at)| f.at != **at)
        .filter_map(|(f, _)| {
            let angle = if f.rotation_deg == 0.0 { String::new() } else { format!(" {}", f.rotation_deg) };
            Some((f.at_span?, format!("(at {} {}{angle})", mm(f.at.x as f64 * 1e-9), mm(f.at.y as f64 * 1e-9))))
        })
        .collect()
}

/// Every connection left open and what is in the way of it, one line each, on standard error so
/// a live stream on standard output stays clean. Each is asked twice: once with routed copper
/// passable at a price, which names the nets in the way, and once with it a wall, which names
/// the unlocked parts whose move would open a channel of its own.
/// The routing regions whose never or must rules hold for `net` and keep the connection from a
/// path: without each alone, a path exists.
fn regions_blocking(board: &Board, net: u32, from: ItemId, to: ItemId, pours_block: bool) -> Vec<String> {
    let routes = |b: &mut Board| {
        let grid = grid::ObstacleMap::new(b, grid::Profile::for_net(b, net), grid::Options::default().cell, pours_block).view(net);
        let (src, tgt) = (grid::terminal_cells(b, &grid, from), grid::terminal_cells(b, &grid, to));
        !src.is_empty() && !tgt.is_empty() && grid::search::route_astar(&grid, &src, &tgt, grid::Options::default().via_cost).is_some()
    };
    let hard = |r: &board::RegionRule| r.holds_for(net) && matches!(r.kind, board::RegionKind::Never | board::RegionKind::Must);
    let mut out = Vec::new();
    for k in (0..board.regions.len()).filter(|&k| board.regions[k].rules.iter().any(hard)) {
        let mut without = board.clone();
        let region = without.regions.remove(k);
        if routes(&mut without) {
            out.push(region.name);
        }
    }
    out
}

fn print_blame(board: &mut Board, pours_block: bool) {
    let open = if pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines;
    let priced = grid::BlameOptions { pours_block, ..Default::default() };
    let walled = grid::BlameOptions { routed_copper: None, ..priced.clone() };
    let locked: Vec<bool> = board.footprints.iter().map(|f| f.locked).collect();
    for airline in open {
        let r = grid::blame(board, airline.from_item, airline.to_item, &priced, &|f| !locked[f]);
        let w = grid::blame(board, airline.from_item, airline.to_item, &walled, &|f| !locked[f]);
        let reference = |f: &usize| board.footprints[*f].reference.clone();
        let mut what: Vec<String> = Vec::new();
        if r.walled_off {
            what.push(if r.end_cells.0 == 0 || r.end_cells.1 == 0 { "an end is buried".into() } else { "walled off".into() });
        }
        if !r.pinned_parts.is_empty() {
            what.push(format!("fixed or end parts {}", r.pinned_parts.iter().map(reference).collect::<Vec<_>>().join(", ")));
        }
        if !r.routed_nets.is_empty() {
            let nets: Vec<&str> = r.routed_nets.iter().map(|n| board.net_name(*n)).collect();
            what.push(format!("routed copper of {} ({} cells)", nets.join(", "), r.routed_cells));
        }
        // Parts in the way from either asking, each once; a move is in millimetres, board Y down.
        let mut parts: Vec<usize> = Vec::new();
        for b in r.blamed.iter().chain(&w.blamed) {
            if !parts.contains(&b.footprint) {
                parts.push(b.footprint);
                what.push(format!("{} (move {:+.3}, {:+.3} mm)", reference(&b.footprint), b.dx * 1e-6, b.dy * 1e-6));
            }
        }
        for f in r.unmovable.iter().chain(&w.unmovable) {
            if !parts.contains(f) {
                parts.push(*f);
                what.push(format!("{} (no move clears it)", reference(f)));
            }
        }
        // A planner's region whose hard rule holds for the net, and without which it would route.
        for name in regions_blocking(board, airline.net, airline.from_item, airline.to_item, pours_block) {
            what.push(format!("region {name}"));
        }
        if what.is_empty() {
            what.push("nothing on the grid: a clear path exists".into());
        }
        eprintln!(
            "open {}: {} to {}: {}",
            board.net_name(airline.net),
            grid::item_name(board, airline.from_item),
            grid::item_name(board, airline.to_item),
            what.join("; ")
        );
    }
}

/// Resolves `--first` entries into net numbers: a plain name matches a net, `@auto` selects
/// signal nets on fine-pitch parts, and `@REF` (or `@REF:all` including poured nets) selects nets
/// with pads on footprint `REF`.
fn resolve_priority_nets(board: &Board, specs: &[String]) -> HashSet<u32> {
    let poured: HashSet<u32> = board
        .items()
        .filter(|(_, it)| matches!(it.kind, Kind::Pour { .. }))
        .map(|(_, it)| it.net)
        .collect();
    let mut out = HashSet::new();
    for spec in specs {
        if spec == "@auto" {
            let fine: HashSet<usize> = morph::fine_pitch(board, 500_000).into_iter().collect();
            for (_, it) in board.items() {
                if let Kind::Pad { footprint, .. } = it.kind {
                    if fine.contains(&footprint) && it.net != 0 && !poured.contains(&it.net) {
                        out.insert(it.net);
                    }
                }
            }
        } else if let Some(rest) = spec.strip_prefix('@') {
            let (ref_name, include_pours) = rest.strip_suffix(":all").map_or((rest, false), |r| (r, true));
            let Some(fp) = board.footprints.iter().position(|f| f.reference == ref_name) else {
                eprintln!("warning: --first {spec}: no such part on the board");
                continue;
            };
            for (_, it) in board.items() {
                if matches!(it.kind, Kind::Pad { footprint, .. } if footprint == fp)
                    && it.net != 0
                    && (include_pours || !poured.contains(&it.net))
                {
                    out.insert(it.net);
                }
            }
        } else if let Some(net) = (1..board.nets.len() as u32).find(|&n| board.net_name(n) == spec) {
            out.insert(net);
        } else {
            eprintln!("warning: --first {spec}: no such net on the board");
        }
    }
    out
}

/// Groups priority nets by descending failure count so nets that stayed open across more restart
/// passes route ahead of ones that failed fewer times.
fn priority_tiers(fail_counts: &HashMap<u32, usize>) -> Vec<BTreeSet<u32>> {
    let mut by_count: BTreeMap<std::cmp::Reverse<usize>, BTreeSet<u32>> = BTreeMap::new();
    for (&net, &count) in fail_counts {
        if count > 0 {
            by_count.entry(std::cmp::Reverse(count)).or_default().insert(net);
        }
    }
    by_count.into_values().collect()
}

/// Resolves `--topo-first` targets into open endpoint pairs `(from.min(to), from.max(to))`:
/// `auto` selects fine-pitch parts, `REF` selects a part's connections (plus all connections of
/// its non-poured nets), `REF:signals` skips poured nets, `REF:pads` restricts to airlines with
/// an end directly on `REF`, and a net name selects that net's open connections.
fn resolve_topo_only(board: &Board, specs: &[String], pours_block: bool) -> HashSet<(ItemId, ItemId)> {
    let airlines = if pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines;
    let poured: HashSet<u32> = board
        .items()
        .filter(|(_, it)| matches!(it.kind, Kind::Pour { .. }))
        .map(|(_, it)| it.net)
        .collect();
    let pair = |a: ItemId, b: ItemId| (a.min(b), a.max(b));
    let mut out = HashSet::new();
    for spec in specs {
        let (name, signals_only, pads_only) = if let Some(r) = spec.strip_suffix(":signals") {
            (r, true, false)
        } else if let Some(r) = spec.strip_suffix(":pads") {
            (r, false, true)
        } else {
            (spec.as_str(), false, false)
        };
        let parts: Vec<usize> = if name == "auto" {
            morph::fine_pitch(board, 500_000)
        } else if let Some(fp) = board.footprints.iter().position(|f| f.reference == name) {
            vec![fp]
        } else if let Some(net) = (1..board.nets.len() as u32).find(|&n| board.net_name(n) == spec) {
            for a in airlines.iter().filter(|a| a.net == net) {
                out.insert(pair(a.from_item, a.to_item));
            }
            continue;
        } else {
            eprintln!("warning: --topo-first {spec}: no such part or net on the board");
            continue;
        };
        let fp_set: HashSet<usize> = parts.into_iter().collect();
        let mut pads = HashSet::new();
        let mut signal_nets = HashSet::new();
        for (id, it) in board.items() {
            if let Kind::Pad { footprint, .. } = it.kind {
                if fp_set.contains(&footprint) {
                    pads.insert(id);
                    if it.net != 0 && !poured.contains(&it.net) {
                        signal_nets.insert(it.net);
                    }
                }
            }
        }
        for a in &airlines {
            if signals_only && poured.contains(&a.net) {
                continue;
            }
            let touches_pad = pads.contains(&a.from_item) || pads.contains(&a.to_item);
            if touches_pad || (!pads_only && signal_nets.contains(&a.net)) {
                out.insert(pair(a.from_item, a.to_item));
            }
        }
    }
    out
}

/// A restart round's board, the copper it laid, the file's spans it took out, and the last
/// grid pass's stats, kept while it is the best so far.
type Restart = (Board, Vec<ItemId>, Vec<kicad_pcb::Span>, grid::Stats);

/// Via prices for each stage, in grid steps (ten to a cell of straight track).
struct ViaCosts {
    negotiate: u32,
    open: u32,
    rip: u32,
}

/// The prices `--via-cost` gives, or by default: with every layer free for signals, a via costs
/// negotiation 500, routing one connection at a time 600 and rip-up 320 (below its 400 for
/// ripping copper, so a route takes the other layer before ripping a track there): a via pair
/// to hop over a crossing must cost more than the congestion it dodges, or two-layer boards
/// fill with them. With planes inside, signals run on the outer layers among the planes' vias,
/// and dear vias drive routes the long way round dense parts: 200, 80 and 80, as before.
fn via_costs(a: &Args, board: &Board) -> ViaCosts {
    let cell = a.cell.unwrap_or(grid::Options::default().cell);
    match a.via_cost {
        Some((neg, open)) => {
            let at = |mm: f64| grid::via_cost_mm(cell, mm);
            ViaCosts { negotiate: at(neg), open: at(open.unwrap_or(neg * 1.2)), rip: at(open.unwrap_or(neg * 0.8)) }
        }
        None if board.planes != 0 => ViaCosts { negotiate: 200, open: 80, rip: 80 },
        None => ViaCosts { negotiate: 500, open: 600, rip: 320 },
    }
}

/// The widest region the region plough rips round a trapped connection's path.
const REGION_WIDEST: Nm = 4_000_000;

/// The share of the board one net's pour must cover on an inner layer for the layer to be taken
/// as that net's plane.
const PLANE_SHARE: f64 = 0.6;

// The stages' shares of `--timeout`. Stage 8 of REMEDIATION.md moves them into the stage table.
/// Pre-fanout: of what is left when it starts.
const PREFANOUT_SHARE: f64 = 0.15;
/// Joining plane pads to their planes: of what is left when it starts.
const PLANES_SHARE: f64 = 0.05;
/// Coupling differential pairs end to end, until this share of the whole run has gone.
const PAIRS_UNTIL: f64 = 0.25;
/// Negotiation in a plain run (no restarts, nothing routed first): of the whole run.
const NEGOTIATE_SHARE: f64 = 0.5;
/// Dropping vias: this share of `--timeout`, kept back from the stages before it.
const VIAS_SHARE: f64 = 0.1;
/// What the run keeps back at the end of `--timeout` to clean up and write the board: this
/// share of it, and never less than `RESERVE_SECONDS`.
const RESERVE_SHARE: f64 = 0.02;
const RESERVE_SECONDS: f64 = 2.0;

/// The deadline of a run limited to `timeout` seconds from `started`: its hard end comes early
/// by the reserve, which the clean-up and the write after it may use.
fn run_deadline(started: Instant, timeout: Option<f64>, cancel: Arc<AtomicBool>) -> grid::Deadline {
    match timeout {
        Some(t) => {
            let reserve = (t * RESERVE_SHARE).max(RESERVE_SECONDS).min(t);
            grid::Deadline::until(started + Duration::from_secs_f64((t - reserve).max(0.0)), cancel)
        }
        None => grid::Deadline::unlimited(cancel),
    }
}

/// `deadline` ending `seconds` before it would, or itself when it has no end.
fn sooner(deadline: &grid::Deadline, seconds: f64) -> grid::Deadline {
    match deadline.at {
        Some(at) => deadline.until_at(at.checked_sub(Duration::from_secs_f64(seconds.max(0.0))).unwrap_or_else(Instant::now)),
        None => deadline.clone(),
    }
}

/// Which stage is running, the stages the clock stopped short, and the one that was running when
/// the run's hard deadline passed.
struct Timeline {
    stage: &'static str,
    limited: Vec<&'static str>,
    overran_in: Option<&'static str>,
}

impl Timeline {
    /// `stage` starts. Whether it may run: not once the hard deadline has passed.
    fn enter(&mut self, run: &grid::Deadline, stage: &'static str) -> bool {
        if run.take_fired() && !self.limited.contains(&self.stage) {
            self.limited.push(self.stage);
        }
        let passed = run.hard_passed();
        if passed && self.overran_in.is_none() && !run.cancelled() {
            self.overran_in = Some(self.stage);
        }
        self.stage = stage;
        !passed
    }
}

/// The plough for connections trapped at fine-pitch parts, up to `--shove-passes` times, each in
/// another order, stopping at `deadline`: the board file's spans of copper it moved or ripped.
#[allow(clippy::too_many_arguments)]
fn shove_pass(
    a: &Args,
    board: &mut Board,
    solver: &mut dyn grid::FieldSolver,
    laid: &mut Vec<ItemId>,
    started: Instant,
    deadline: &grid::Deadline,
    log: &Option<grid::Log>,
    step: &dyn Fn(&Board),
) -> Vec<kicad_pcb::Span> {
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
        let open = if a.pours_block { connectivity(b) } else { connectivity_without_pours(b) }.airlines;
        (open.iter().filter(|x| !plane_nets.contains(&x.net)).count(), open.len())
    };
    let mut removed = Vec::new();
    let mut stalled = 0;
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
                negotiate_vias: a.negotiate_vias,
                negotiate_repasses: a.negotiate_repasses.map_or(defaults.negotiate_repasses, |r| r.min(1)),
                negotiate_tree: a.negotiate_tree,
                cascade: a.shove_cascade.unwrap_or(defaults.cascade),
                seed: a.seed.wrapping_mul(1000).wrapping_add(pass as u64 + 1),
                deadline: deadline.clone(),
                pours_block: a.pours_block,
                log: log.clone(),
                ..defaults
            },
            laid,
            &mut |b: &Board, event| {
                if !matches!(event, grid::ShoveEvent::Start(_)) {
                    step(b);
                }
            },
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
    removed
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
fn locked_footprints(board: &Board, pair_laid: &[ItemId], legacy_pairs: bool) -> HashSet<usize> {
    let mut out: HashSet<usize> = board.footprints.iter().enumerate().filter(|(_, f)| f.locked).map(|(i, _)| i).collect();
    if legacy_pairs || pair_laid.is_empty() {
        return out;
    }
    let pair_set: HashSet<ItemId> = pair_laid.iter().copied().filter(|&id| board.item(id).is_some()).collect();
    for (id, it) in board.items() {
        let Kind::Pad { footprint, .. } = it.kind else { continue };
        if out.contains(&footprint) || it.net == 0 {
            continue;
        }
        let bb = board.bbox(id).unwrap_or_else(|| it.shape.bbox());
        let touches_pair = (0..board.layers.len()).filter(|&l| it.on(l)).any(|l| {
            board.near(l, bb).into_iter().any(|o| pair_set.contains(&o) && board::joined(board, id, o))
        });
        if touches_pair {
            out.insert(footprint);
        }
    }
    out
}

/// Routes `board`, read from `text`, as `a` asks, and returns the routed board's text. `live`
/// receives the run as frames; setting `cancel` stops it at the next connection.
pub fn run(a: &Args, board: Board, text: &str, live: Option<Live>, cancel: Arc<AtomicBool>) -> String {
    run_with_solver(a, board, text, live, cancel, None)
}

/// Routes `board` using `custom_solver` when provided, or the default CPU/GPU solver otherwise.
/// Threads for a run that names none: half the machine's.
pub fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get() / 2).max(1)
}

/// The grid cell a run that names none routes on.
fn default_cell() -> Nm {
    grid::Options::default().cell
}

/// The coarsest cell [`fit_memory`] goes to, and its step there, in nanometres.
const COARSEST_CELL: Nm = 100_000;
const CELL_STEP: Nm = 10_000;

/// The most memory a run may take, in bytes, and where that figure comes from: `--max-memory`,
/// else the tightest `memory.max` of the cgroups the process runs in, else 60% of the memory
/// the kernel reports available, else 4 GB.
pub fn memory_cap(a: &Args) -> (u64, String) {
    if let Some(mb) = a.max_memory {
        return (mb << 20, "--max-memory".into());
    }
    if let Some(limit) = cgroup_limit() {
        return (limit, "its cgroup's memory.max".into());
    }
    let available = std::fs::read_to_string("/proc/meminfo").ok().and_then(|m| {
        let line = m.lines().find(|l| l.starts_with("MemAvailable:"))?;
        line.split_whitespace().nth(1)?.parse::<u64>().ok()
    });
    match available {
        Some(kb) => (kb * 1024 / 10 * 6, "60% of available memory".into()),
        None => (4 << 30, "4 GB, the memory available unknown".into()),
    }
}

/// The tightest `memory.max` from the process's cgroup (v2) up to the root.
fn cgroup_limit() -> Option<u64> {
    let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = own.lines().find_map(|l| l.strip_prefix("0::"))?.trim().to_string();
    let mut dir = std::path::PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    let mut tightest: Option<u64> = None;
    loop {
        if let Ok(max) = std::fs::read_to_string(dir.join("memory.max")) {
            if let Ok(v) = max.trim().parse::<u64>() {
                tightest = Some(tightest.map_or(v, |t| t.min(v)));
            }
        }
        if dir == std::path::Path::new("/sys/fs/cgroup") || !dir.pop() {
            return tightest;
        }
    }
}

/// Fits the run to the memory it may take: what its grids will take is worked out before any is
/// allocated, and when that is over the cap the run first gives up threads, then routes on a
/// coarser cell, a step at a time up to 0.1 mm, and says what it chose. A board still over
/// the cap is refused (`Err`), with both figures. `Ok` carries the lines to print.
pub fn fit_memory(a: &mut Args, board: &Board) -> Result<Vec<String>, String> {
    let (cap, from) = memory_cap(a);
    let mb = |b: u64| b.div_ceil(1 << 20);
    let (asked_threads, asked_cell) = (a.threads.unwrap_or_else(default_threads), a.cell.unwrap_or_else(default_cell));
    let (mut threads, mut cell) = (asked_threads, asked_cell);
    // Only Jacobi negotiation searches the whole board on every thread at once; otherwise the
    // threads work on windows (pre-fanout), and one search holds the whole board.
    let jacobi = a.negotiate_jacobi;
    let searching = |threads: usize| if jacobi { threads } else { 1 };
    let first = grid::memory_estimate(board, cell, searching(threads));
    let mut lines = vec![format!(
        "memory: {} MB estimated ({} nodes at {} mm, {} classes, {} searching the whole board), {} MB allowed by {from}",
        first.mb(),
        first.nodes,
        cell as f64 / 1e6,
        first.classes,
        first.threads,
        mb(cap)
    )];
    let mut e = first.clone();
    while e.bytes > cap && jacobi && threads > 1 {
        threads -= 1;
        e = grid::memory_estimate(board, cell, searching(threads));
    }
    while e.bytes > cap && cell < COARSEST_CELL {
        cell = (cell + CELL_STEP).min(COARSEST_CELL);
        e = grid::memory_estimate(board, cell, searching(threads));
    }
    if e.bytes > cap {
        return Err(format!(
            "memory: the board needs {} MB even on one thread at {} mm, over the {} MB allowed by {from}; give --max-memory or a coarser --cell",
            e.mb(),
            cell as f64 / 1e6,
            mb(cap)
        ));
    }
    if e != first {
        if threads != asked_threads {
            a.threads = Some(threads);
        }
        if cell != asked_cell {
            a.cell = Some(cell);
        }
        lines.push(format!("memory: {} threads at {} mm to fit, {} MB estimated", threads, cell as f64 / 1e6, e.mb()));
    }
    Ok(lines)
}

/// The most memory the process has held, in megabytes, where the system says (Linux).
pub fn peak_memory_mb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    line.split_whitespace().nth(1)?.parse::<u64>().ok().map(|kb| kb.div_ceil(1024))
}

pub fn run_with_solver(
    a: &Args,
    mut board: Board,
    text: &str,
    mut live: Option<Live>,
    cancel: Arc<AtomicBool>,
    custom_solver: Option<Box<dyn grid::FieldSolver>>,
) -> String {
    let mut removed = Vec::new();
    let original_at: Vec<Point> = board.footprints.iter().map(|f| f.at).collect();
    for f in board.footprints.iter_mut().filter(|f| a.lock.contains(&f.reference)) {
        f.locked = true;
    }
    for (name, x, y) in &a.place {
        let Some(fp) = board.footprints.iter().position(|f| &f.reference == name) else {
            eprintln!("warning: --place {name}: no such part on the board");
            continue;
        };
        let target = Point::new((x * 1e6).round() as i64, (-y * 1e6).round() as i64);
        let at = board.footprints[fp].at;
        let ripped = board.move_footprint(fp, Point::new(target.x - at.x, target.y - at.y));
        removed.extend(ripped.iter().filter_map(|it| it.span));
    }
    if a.strip || a.strip_all {
        let loose: Vec<ItemId> = board
            .items()
            .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && (a.strip_all || !it.locked))
            .map(|(id, _)| id)
            .collect();
        for id in loose {
            if let Some(it) = board.remove(id) {
                removed.extend(it.span);
            }
        }
    }
    // An inner layer one net's pour all but fills is a plane in all but name: signals routed
    // across it cut its return paths. With --auto-planes it is taken as one; otherwise said.
    // (Reserving every such layer costs some boards connections: bluebuzz has three.)
    let mut a_planes_found: Vec<String> = Vec::new();
    if a.planes.is_empty() {
        for (l, net) in board.plane_layers(PLANE_SHARE) {
            if a.auto_planes {
                eprintln!("planes: {} taken as {}'s plane, its pour covering most of the board", board.layers[l], board.net_name(net));
                a_planes_found.push(board.layers[l].clone());
            } else {
                eprintln!("planes: {} looks like {}'s plane (its pour covers most of the board); --planes or --auto-planes keeps signals off it", board.layers[l], board.net_name(net));
            }
        }
    }
    let planes: Vec<String> = if a.planes.is_empty() { a_planes_found } else { a.planes.clone() };
    if !planes.is_empty() {
        let mut mask = 0u64;
        for name in &planes {
            match board.layers.iter().position(|l| l == name) {
                Some(l) => mask |= 1 << l,
                None => panic!("--planes: the board has no copper layer {name} (it has {})", board.layers.join(", ")),
            }
        }
        board.set_planes(mask);
    }
    if let Some(l) = live.as_mut() {
        l.header(&board);
    }

    if a.no_route {
        let open = if a.pours_block { connectivity(&board) } else { connectivity_without_pours(&board) }.airlines.len();
        if a.blame {
            print_blame(&mut board, a.pours_block);
        }
        let replace = moved_parts(&board, &original_at);
        removed.extend(stale_fills(&board, &[], text, !replace.is_empty()));
        let out = apply(text, &Edit { remove: removed, replace, ..Edit::default() });
        if let Some(l) = live.as_mut() {
            l.fills(&out, &board.rules);
            l.write(&json!({"t": "done", "open": open, "passes": 0, "stopped": null}));
        }
        return out;
    }
    let started = Instant::now();
    // One deadline for the whole run: every stage's is a share of it, none past its hard end.
    let run = run_deadline(started, a.timeout, cancel.clone());
    // Routing, and the stages that rescue what it left open, stop in time for the via pass.
    let rescue = if a.no_reduce_vias { run.clone() } else { sooner(&run, a.timeout.map_or(0.0, |t| t * VIAS_SHARE)) };
    let mut timeline = Timeline { stage: "set-up", limited: Vec::new(), overran_in: None };
    let mut solver: Box<dyn grid::FieldSolver> = match custom_solver {
        Some(s) => s,
        None => {
            #[allow(unused_mut)]
            let mut s: Box<dyn grid::FieldSolver> = Box::new(grid::CpuSolver);
            #[cfg(feature = "gpu")]
            if a.gpu {
                match grid::GpuSolver::new() {
                    Some(gpu) => {
                        eprintln!("field solver on {}", gpu.adapter_name);
                        s = Box::new(gpu);
                    }
                    None => eprintln!("no GPU; solving on the CPU"),
                }
            }
            s
        }
    };
    let mut laid = Vec::new();
    if a.adopt {
        let mine: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked).map(|(id, _)| id).collect();
        for id in mine {
            if let Some(mut it) = board.remove(id) {
                removed.extend(it.span.take());
                board.restore(id, it);
                laid.push(id);
            }
        }
    }
    if let Some(c) = a.via_avoid_cost {
        board.via_avoid_cost = c;
    }
    if let Some(w) = a.pour_cost {
        board.pour_cost = w;
    }
    let lengths = match &a.regions {
        Some(path) => regions::load(path, &mut board).unwrap_or_else(|e| panic!("{e}")),
        None => Vec::new(),
    };
    for &[x0, y0, x1, y1] in &a.via_avoid {
        let p = |x: f64, y: f64| Point::new((x * 1e6).round() as i64, (y * 1e6).round() as i64);
        board.via_avoid.push(Shape::Polygon(vec![p(x0, y0), p(x1, y0), p(x1, y1), p(x0, y1)]));
    }
    for &(x, y, r) in &a.rip_regions {
        let disc = Shape::Capsule { a: Point::new((x * 1e6).round() as i64, (y * 1e6).round() as i64), b: Point::new((x * 1e6).round() as i64, (y * 1e6).round() as i64), r: (r * 1e6).round() as i64 };
        let inside: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked && it.shape.gap(&disc) == 0.0).map(|(id, _)| id).collect();
        for id in inside {
            if let Some(it) = board.remove(id) {
                removed.extend(it.span);
            }
        }
        laid.retain(|id| board.item(*id).is_some());
    }
    let streaming = live.is_some();
    let live = Arc::new(std::sync::Mutex::new(live));
    let shared = live.clone();
    let verbose = a.verbose;
    let log = (streaming || verbose).then(|| {
        grid::Log(Arc::new(move |act| {
            if verbose {
                eprintln!("  [{:8.3} s] {}: {}", started.elapsed().as_secs_f64(), act.phase, act.text);
            }
            if let Some(l) = shared.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                l.note(act);
            }
        }))
    });
    let say = |phase: &'static str, text: String| {
        if let Some(grid::Log(log)) = &log {
            log(grid::Activity { phase, text, focus: None });
        }
    };
    let mut watch = |b: &Board| {
        if let Some(l) = live.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            l.frame(b, false, Vec::new());
        }
    };
    // A step of the work worth seeing on its own, sent whatever the frame rate.
    let step = |b: &Board| {
        if let Some(l) = live.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            l.frame(b, true, Vec::new());
        }
    };
    say("start", format!("{} connections to make", connectivity_without_pours(&board).airlines.len()));
    let mut pair_laid = Vec::new();
    // Escapes laid before routing, held apart from the router's own copper until the grid has
    // had its pass, so the ones no route used can be taken out again.
    let mut fanout_laid = Vec::new();
    let threads = a.threads.unwrap_or_else(default_threads);
    if a.prefanout && timeline.enter(&run, "prefanout") {
        let parts = morph::fine_pitch(&board, 500_000);
        let defaults = grid::PrefanoutOptions::default();
        // Open differential pairs are held back from pre-fanout so `route_pairs` can fan out and
        // couple their pads together; any pair pad still open afterwards is escaped below.
        let skip_nets: HashSet<u32> = if a.diff_pairs && a.diff_pairs_legacy {
            grid::diff_pairs(&board).iter().filter(|p| grid::shares_layer(&board, p)).flat_map(|p| [p.p, p.n]).collect()
        } else if a.diff_pairs {
            grid::diff_pairs(&board)
                .iter()
                .filter(|p| grid::shares_layer(&board, p) && grid::pair_has_open_runs(&board, p, a.pours_block))
                .flat_map(|p| [p.p, p.n])
                .collect()
        } else {
            HashSet::new()
        };
        let f = grid::prefanout(
            &mut board,
            &grid::PrefanoutOptions {
                global_cell: a.cell.unwrap_or(defaults.global_cell),
                ring: a.prefanout_ring.unwrap_or(defaults.ring),
                mode: a.prefanout_mode.unwrap_or(defaults.mode),
                order: a.prefanout_order.unwrap_or(defaults.order),
                inward: a.prefanout_inward,
                parts,
                skip_nets,
                pours_block: a.pours_block,
                deadline: run.share(PREFANOUT_SHARE),
                threads,
                log: log.clone(),
                ..defaults
            },
            &mut watch,
        );
        say(
            "prefanout",
            format!(
                "{} parts, {} inward bridges, {} of {} pads escaped ({} vias)",
                f.parts, f.bridged_inward, f.escaped, f.candidates, f.vias
            ),
        );
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
            started.elapsed().as_secs_f64()
        );
        step(&board);
        if a.prefanout_soft {
            laid.extend(f.laid);
        } else {
            fanout_laid = f.laid;
        }
    }
    // Plane pads not on their plane get a via down to it beside them, as the router's own copper
    // a rip-up may take up again.
    if board.planes != 0 && timeline.enter(&run, "planes") {
        let p = grid::join_planes(&mut board, &log, &run.share(PLANES_SHARE));
        say("planes", format!("{} of {} pads joined to their planes by a via", p.joined, p.pads));
        eprintln!("planes: {} of {} pads off their planes joined, {:.1} s", p.joined, p.pads, started.elapsed().as_secs_f64());
        step(&board);
        laid.extend(p.laid);
    }
    // Differential pairs next, round the escapes, and held apart from the router's own copper so
    // nothing rips or straightens them apart; the legs to their pads are routed as usual.
    if a.diff_pairs && timeline.enter(&run, "pairs") {
        let p = grid::route_pairs(
            &mut board,
            &grid::PairOptions {
                cell: a.cell.unwrap_or(grid::PairOptions::default().cell),
                pours_block: a.pours_block,
                legacy: a.diff_pairs_legacy,
                // Coupling end to end may run until a quarter of the time is gone; later runs are
                // laid the legacy way, as the pairs are a small part of a board, the rest of it
                // still to route.
                deadline: match a.timeout {
                    Some(t) => run.until_at(started + Duration::from_secs_f64(t * PAIRS_UNTIL)),
                    None => run.clone(),
                },
                log: log.clone(),
            },
            &mut watch,
        );
        eprintln!("pairs: {} pairs, {} of {} runs coupled, {:.1} mm, {:.1} s", p.pairs, p.coupled, p.runs, p.coupled_mm, started.elapsed().as_secs_f64());
        step(&board);
        pair_laid.extend(p.laid);
        fanout_laid.retain(|&id| board.item(id).is_some_and(|it| !it.locked));
        if a.prefanout && !a.diff_pairs_legacy {
            let fine: HashSet<usize> = morph::fine_pitch(&board, 500_000).into_iter().collect();
            let pair_nets: HashSet<u32> = grid::diff_pairs(&board).iter().flat_map(|pr| [pr.p, pr.n]).collect();
            let open = if a.pours_block { connectivity(&board) } else { connectivity_without_pours(&board) }.airlines;
            let open_fine_pair_nets: HashSet<u32> = open
                .iter()
                .filter(|air| pair_nets.contains(&air.net))
                .filter(|air| {
                    [air.from_item, air.to_item].iter().any(|&id| {
                        board.item(id).is_some_and(|it| matches!(it.kind, Kind::Pad { footprint, .. } if fine.contains(&footprint)))
                    })
                })
                .map(|air| air.net)
                .collect();
            if !open_fine_pair_nets.is_empty() {
                let skip_others: HashSet<u32> = (1..board.nets.len() as u32).filter(|n| !open_fine_pair_nets.contains(n)).collect();
                let defaults = grid::PrefanoutOptions::default();
                let f = grid::prefanout(
                    &mut board,
                    &grid::PrefanoutOptions {
                        global_cell: a.cell.unwrap_or(defaults.global_cell),
                        ring: a.prefanout_ring.unwrap_or(defaults.ring),
                        mode: a.prefanout_mode.unwrap_or(defaults.mode),
                        order: a.prefanout_order.unwrap_or(defaults.order),
                        inward: false,
                        parts: fine.into_iter().collect(),
                        skip_nets: skip_others,
                        pours_block: a.pours_block,
                        deadline: run.share(PREFANOUT_SHARE),
                        threads,
                        log: log.clone(),
                        ..defaults
                    },
                    &mut watch,
                );
                if a.prefanout_soft {
                    laid.extend(f.laid);
                } else {
                    fanout_laid.extend(f.laid);
                }
            }
        }
    }
    let topo_sample = a.topo_sample.unwrap_or(topo::Options::default().sample);
    let open_airlines = |b: &Board| if a.pours_block { connectivity(b) } else { connectivity_without_pours(b) }.airlines;
    let costs = via_costs(a, &board);
    let directional = !a.no_directional;

    // With `--restart`, the routing stages run again from the starting board, the nets left open
    // promoted to priority each time, and the best board is kept. Without it they run once, timed
    // exactly as a single pass always was.
    let total_rounds = a.restart + 1;
    let poured_nets: HashSet<u32> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Pour { .. })).map(|(_, it)| it.net).collect();
    let start_state = (a.restart > 0).then(|| (board.clone(), laid.clone(), removed.clone(), fanout_laid.clone()));
    let mut fail_counts: HashMap<u32, usize> = resolve_priority_nets(&board, &a.first).into_iter().map(|n| (n, 1)).collect();
    // Nets with a length limit route first, while their shortest ways are still free.
    for m in &lengths {
        for &n in &m.nets {
            fail_counts.entry(n).or_insert(1);
        }
    }
    let mut best_run: Option<((usize, usize), Restart)> = None;
    let mut stats = grid::Stats::default();
    // Restarts leave the rescue stages after them a share of the time.
    let post_rescue = a.shove || a.topo_trapped_rip;
    let routing_timeout = match (a.timeout, a.restart > 0) {
        (Some(t), true) if post_rescue => Some(t * 0.7),
        (Some(t), true) if a.nudge => Some(t * 0.8),
        (t, _) => t,
    };
    let routing = match routing_timeout {
        Some(t) => rescue.until_at(started + Duration::from_secs_f64(t)),
        None => rescue.clone(),
    };
    // A stage's share of the whole `--timeout`, from when it starts.
    let of_timeout = |share: f64| match a.timeout {
        Some(t) => run.seconds(t * share),
        None => run.clone(),
    };
    // A pass that routes nothing first and does not restart keeps the time limits it always had.
    let plain = a.restart == 0 && a.first.is_empty() && a.topo_first.is_empty() && lengths.is_empty();

    for round in 0..total_rounds {
        if (round > 0 && routing.expired()) || !timeline.enter(&run, "routing") {
            break;
        }
        if round > 0 {
            if let Some((b0, l0, r0, f0)) = &start_state {
                board = b0.clone();
                laid = l0.clone();
                removed = r0.clone();
                fanout_laid = f0.clone();
            }
            say("restart", format!("round {} of {}: {} priority nets", round + 1, total_rounds, fail_counts.len()));
            step(&board);
        }
        // This round's even share of the routing time left.
        let rounds_left = (total_rounds - round) as f64;
        let round_deadline = routing.share(1.0 / rounds_left);

        if a.topo && timeline.enter(&run, "topo") {
            // Rubber bands first, through a triangulation of the copper; the grid finishes what they
            // leave open.
            let t = topo::route_topological(
                &mut board,
                &topo::Options {
                    sample: topo_sample,
                    deadline: if a.restart > 0 { round_deadline.share(0.4) } else { of_timeout(0.4) },
                    pours_block: a.pours_block,
                    log: log.clone(),
                    ..Default::default()
                },
                &mut watch,
            );
            eprintln!(
                "topo: {} of {} connections, {} rip-ups, {} vias, {:.1} s",
                t.routed,
                t.connections,
                t.rip_ups,
                t.vias,
                started.elapsed().as_secs_f64()
            );
            laid.extend(t.laid);
        }
        if !a.topo_first.is_empty() && timeline.enter(&run, "topo-first") {
            let only = resolve_topo_only(&board, &a.topo_first, a.pours_block);
            if !only.is_empty() {
                say("topo-first", format!("routing {} target connections as rubber bands before negotiation", only.len()));
                let t = topo::route_topological(
                    &mut board,
                    &topo::Options {
                        sample: topo_sample,
                        deadline: round_deadline.share(0.25),
                        only: Some(only),
                        pours_block: a.pours_block,
                        log: log.clone(),
                        ..Default::default()
                    },
                    &mut watch,
                );
                eprintln!(
                    "topo-first: {} of {} connections, {} rip-ups, {} vias, {:.1} s",
                    t.routed,
                    t.connections,
                    t.rip_ups,
                    t.vias,
                    started.elapsed().as_secs_f64()
                );
                laid.extend(t.laid);
            }
        }
        if !fail_counts.is_empty() && a.first_weight.is_none() && timeline.enter(&run, "first") {
            if a.first_negotiate {
                let prio_nets: HashSet<u32> = fail_counts.keys().copied().collect();
                let weights: HashMap<u32, f64> = fail_counts.iter().map(|(&net, &k)| (net, 0.5_f64.powi(k.saturating_sub(1) as i32))).collect();
                say("first", format!("negotiating {} priority nets among themselves first", prio_nets.len()));
                let n = grid::route_negotiated(
                    &mut board,
                    solver.as_mut(),
                    &grid::NegotiationOptions {
                        deadline: round_deadline.share(0.25),
                        threads,
                        cell: a.cell.unwrap_or(grid::NegotiationOptions::default().cell),
                        via_cost: costs.negotiate,
                        directional,
                        nets: Some(prio_nets.clone()),
                        net_weight: weights,
                        pours_block: a.pours_block,
                        via_zones: a.negotiate_vias,
                        repasses: a.negotiate_repasses.unwrap_or(grid::NegotiationOptions::default().repasses),
                        rip_blocked: a.negotiate_rip,
                        reachable_tree: a.negotiate_tree,
                        jacobi: a.negotiate_jacobi,
                        reroute_all: a.negotiate_reroute_all,
                        seed: a.seed,
                        log: log.clone(),
                        ..Default::default()
                    },
                    &mut watch,
                    &mut |r, trials| {
                        if let Some(l) = live.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                            l.trials(r, trials);
                        }
                    },
                );
                laid.extend(n.laid);
                let finish = grid::route_open(
                    &mut board,
                    solver.as_mut(),
                    &grid::Options {
                        cell: a.cell.unwrap_or(grid::Options::default().cell),
                        via_cost: costs.open,
                        directional,
                        seed: a.seed,
                        deadline: round_deadline.share(0.15),
                        nets: Some(prio_nets.clone()),
                        pours_block: a.pours_block,
                        log: log.clone(),
                    },
                    &mut watch,
                );
                laid.extend(finish.laid);
                eprintln!(
                    "first-negotiate: {} nets, {} negotiated ({} rounds) + {} finished, {} failed, {:.1} s",
                    prio_nets.len(),
                    n.committed,
                    n.rounds,
                    finish.routed,
                    finish.failed,
                    started.elapsed().as_secs_f64()
                );
            } else {
                let tiers = priority_tiers(&fail_counts);
                let total_prio: usize = tiers.iter().map(BTreeSet::len).sum();
                say("first", format!("routing {} priority nets one at a time before negotiation", total_prio));
                let (mut routed, mut failed, mut vias) = (0, 0, 0);
                for tier in tiers {
                    let net_set: HashSet<u32> = tier.into_iter().collect();
                    let s = grid::route_open(
                        &mut board,
                        solver.as_mut(),
                        &grid::Options {
                            cell: a.cell.unwrap_or(grid::Options::default().cell),
                            via_cost: costs.open,
                            directional,
                            seed: a.seed,
                            deadline: round_deadline.share(0.25),
                            nets: Some(net_set),
                            pours_block: a.pours_block,
                            log: log.clone(),
                        },
                        &mut watch,
                    );
                    routed += s.routed;
                    failed += s.failed;
                    vias += s.vias;
                    laid.extend(s.laid);
                }
                eprintln!("first: {total_prio} nets, {routed} routed, {failed} failed, {vias} vias, {:.1} s", started.elapsed().as_secs_f64());
            }
        }
        if a.negotiate && timeline.enter(&run, "negotiation") {
            // Nets that failed more often route at a lower price, and so first.
            let net_weight: HashMap<u32, f64> = match a.first_weight {
                Some(w) => fail_counts.iter().map(|(&net, &k)| (net, w.powi(k as i32))).collect(),
                None => HashMap::new(),
            };
            let n = grid::route_negotiated(
                &mut board,
                solver.as_mut(),
                &grid::NegotiationOptions {
                    deadline: if plain { of_timeout(NEGOTIATE_SHARE) } else { round_deadline.share(if a.restart > 0 { 0.6 } else { 0.5 }) },
                    threads,
                    cell: a.cell.unwrap_or(grid::NegotiationOptions::default().cell),
                    via_cost: costs.negotiate,
                    directional,
                    net_weight,
                    pours_block: a.pours_block,
                    seed: a.seed,
                    log: log.clone(),
                    max_rounds: a.negotiate_rounds.unwrap_or(grid::NegotiationOptions::default().max_rounds),
                    patience: a.negotiate_patience.unwrap_or(grid::NegotiationOptions::default().patience),
                    present_growth: a.negotiate_growth.unwrap_or(grid::NegotiationOptions::default().present_growth),
                    via_zones: a.negotiate_vias,
                    repasses: a.negotiate_repasses.unwrap_or(grid::NegotiationOptions::default().repasses),
                    rip_blocked: a.negotiate_rip,
                    reachable_tree: a.negotiate_tree,
                    jacobi: a.negotiate_jacobi,
                    reroute_all: a.negotiate_reroute_all,
                    ..Default::default()
                },
                &mut watch,
                &mut |round, trials| {
                    if let Some(l) = live.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                        l.trials(round, trials);
                    }
                },
            );
            eprintln!(
                "negotiated {} of {} connections in {} rounds, {} vias, {:.1} s",
                n.committed,
                n.connections,
                n.rounds,
                n.vias,
                started.elapsed().as_secs_f64()
            );
            laid.extend(n.laid);
        }
        let options = grid::Options {
            cell: a.cell.unwrap_or(grid::Options::default().cell),
            via_cost: costs.open,
            directional,
            seed: a.seed,
            deadline: round_deadline.clone(),
            pours_block: a.pours_block,
            log: log.clone(),
            ..Default::default()
        };
        if timeline.enter(&run, "finishing") {
            say("finishing", "routing what negotiation left, one connection at a time".to_string());
            stats = grid::route_open(&mut board, solver.as_mut(), &options, &mut watch);
            laid.extend(stats.laid.iter().copied());
        }
        if !fanout_laid.is_empty() && !a.prefanout_keep_unused {
            let (_, spans) = grid::prune_closed_escapes(&mut board, &mut fanout_laid, a.pours_block);
            removed.extend(spans);
        }
        if a.rip_up && timeline.enter(&run, "rip-up") {
            let r = grid::rip_up(
                &mut board,
                solver.as_mut(),
                &grid::RipOptions {
                    cell: a.cell.unwrap_or(grid::RipOptions::default().cell),
                    via_cost: costs.rip,
                    deadline: round_deadline.clone(),
                    pours_block: a.pours_block,
                    log: log.clone(),
                    ..Default::default()
                },
                &mut laid,
                &mut watch,
            );
            eprintln!(
                "rip-up: {} tries, {} kept, {} ripped; lost {} with no way through, {} to copper it may not rip, {} no better; {:.1} s",
                r.attempts,
                r.kept,
                r.ripped,
                r.no_path,
                r.hard_conflict,
                r.no_better,
                started.elapsed().as_secs_f64()
            );
        }
        if !fanout_laid.is_empty() && !a.prefanout_keep_unused {
            laid.append(&mut fanout_laid);
        }
        if a.restart > 0 {
            let left = open_airlines(&board);
            let total_open = left.len();
            let signal_open = if a.pours_block { total_open } else { left.iter().filter(|c| !poured_nets.contains(&c.net)).count() };
            let score = (signal_open, total_open);
            eprintln!("restart round {}/{}: {signal_open} signal open ({total_open} total), {:.1} s", round + 1, total_rounds, started.elapsed().as_secs_f64());
            if best_run.as_ref().is_none_or(|b| score < b.0) {
                best_run = Some((score, (board.clone(), laid.clone(), removed.clone(), stats.clone())));
            }
            if signal_open == 0 || round == a.restart {
                break;
            }
            let prev_tiers = priority_tiers(&fail_counts);
            // When pours will be refilled by KiCad, plane nets (`GND`, `3V3`) join through their
            // pours; promoting a 100-pad plane net to pre-route or high criticality floods the
            // board with plane tracks and chokes signal escapes.
            let open_nets: BTreeSet<u32> = left.iter().map(|c| c.net).filter(|n| a.pours_block || !poured_nets.contains(n)).collect();
            for net in open_nets {
                *fail_counts.entry(net).or_insert(0) += 1;
            }
            let next_tiers = priority_tiers(&fail_counts);
            if a.first_weight.is_none_or(|w| (w - 1.0).abs() < 1e-12) && next_tiers == prev_tiers {
                eprintln!("restart: priority tiers unchanged after round {}, stopping early", round + 1);
                break;
            }
        }
    }
    if let Some((best_score, (best_board, best_laid, best_removed, best_stats))) = best_run {
        let left = open_airlines(&board);
        let cur_signal = if a.pours_block { left.len() } else { left.iter().filter(|c| !poured_nets.contains(&c.net)).count() };
        let current_score = (cur_signal, left.len());
        if best_score <= current_score {
            eprintln!("restart: keeping best board with {} signal open ({} total; last round had {} signal open)", best_score.0, best_score.1, current_score.0);
            board = best_board;
            laid = best_laid;
            removed = best_removed;
            stats = best_stats;
        }
    }
    if a.shove && timeline.enter(&run, "shove") {
        // Rubber bands to come after keep a third of what is left.
        let until = if a.topo_trapped_rip { rescue.share(0.65) } else { rescue.clone() };
        let spans = shove_pass(a, &mut board, solver.as_mut(), &mut laid, started, &until, &log, &step);
        removed.extend(spans);
        if a.diff_pairs && !a.diff_pairs_legacy && !pair_laid.is_empty() {
            let open = open_airlines(&board);
            if unlock_pairs_near_open(&mut board, &pair_laid, &open) > 0 {
                let spans = shove_pass(a, &mut board, solver.as_mut(), &mut laid, started, &until, &log, &step);
                removed.extend(spans);
                relock_pairs(&mut board, &pair_laid);
            }
        }
    }
    if (a.topo_trapped || a.topo_trapped_rip) && timeline.enter(&run, "topo-trapped") {
        let trapped: HashSet<(ItemId, ItemId)> = open_airlines(&board).iter().map(|c| (c.from_item.min(c.to_item), c.from_item.max(c.to_item))).collect();
        if !trapped.is_empty() {
            let mut topo_opts = topo::Options {
                sample: topo_sample,
                deadline: rescue.clone(),
                only: Some(trapped),
                pours_block: a.pours_block,
                log: log.clone(),
                ..Default::default()
            };
            if a.topo_trapped_rip {
                say("topo-trapped", format!("routing {} trapped connections as rubber bands with rip-up", topo_opts.only.as_ref().map_or(0, HashSet::len)));
                let t = topo::route_topological_with_soft(&mut board, &topo_opts, &mut laid, &mut watch);
                if t.routed > 0 || t.rip_ups > 0 {
                    let after_topo = grid::route_open(
                        &mut board,
                        solver.as_mut(),
                        &grid::Options {
                            cell: a.cell.unwrap_or(grid::Options::default().cell),
                            via_cost: costs.open,
                            directional,
                            seed: a.seed,
                            deadline: rescue.clone(),
                            pours_block: a.pours_block,
                            log: log.clone(),
                            ..Default::default()
                        },
                        &mut watch,
                    );
                    laid.extend(after_topo.laid);
                }
                eprintln!(
                    "topo-trapped-rip: {} of {} trapped, {} rip-ups, {} vias, {} open, {:.1} s",
                    t.routed,
                    t.connections,
                    t.rip_ups,
                    t.vias,
                    open_airlines(&board).len(),
                    started.elapsed().as_secs_f64()
                );
            } else {
                topo_opts.rip_passes = 0;
                say("topo-trapped", format!("routing {} trapped connections as rubber bands", topo_opts.only.as_ref().map_or(0, HashSet::len)));
                let t = topo::route_topological(&mut board, &topo_opts, &mut watch);
                laid.extend(t.laid);
                eprintln!(
                    "topo-trapped: {} of {} trapped ({} no channel, {} rejected), {} vias, {} open, {:.1} s",
                    t.routed,
                    t.connections,
                    t.no_channel,
                    t.rejected,
                    t.vias,
                    open_airlines(&board).len(),
                    started.elapsed().as_secs_f64()
                );
            }
        }
    }
    if a.nudge && timeline.enter(&run, "nudge") {
        let locked = locked_footprints(&board, &pair_laid, a.diff_pairs_legacy);
        let n = grid::nudge(
            &mut board,
            solver.as_mut(),
            &grid::NudgeOptions {
                cell: a.cell.unwrap_or(grid::NudgeOptions::default().cell),
                blind: !a.nudge_guided,
                deadline: rescue.clone(),
                locked,
                pours_block: a.pours_block,
                log: log.clone(),
                ..Default::default()
            },
            &mut laid,
            &mut watch,
        );
        removed.extend(n.ripped_spans.iter().copied());
        let names: Vec<&str> = n.moved.iter().map(|(f, _)| board.footprints[*f].reference.as_str()).collect();
        eprintln!("nudge: {} steps tried, {} illegal, {} kept ({} as blame asked), moved {names:?}, {:.1} s", n.tried, n.illegal, n.kept, n.guided, started.elapsed().as_secs_f64());
    }
    let mut tightened = (a.tighten && timeline.enter(&run, "tightening")).then(|| {
        say("tightening", "straightening the routed copper".to_string());
        grid::pull_tight(&mut board, &mut laid, &run)
    });
    // Copper pulled tight can leave room for a route on one layer where it took two.
    if !a.no_reduce_vias && (!laid.is_empty() || !fanout_laid.is_empty()) && timeline.enter(&run, "vias") {
        say("vias", "taking out vias a route on one layer can do without".to_string());
        let v = grid::reduce_vias_all(
            &mut board,
            &grid::ViaOptions { cell: a.cell.unwrap_or(grid::ViaOptions::default().cell), pours_block: a.pours_block, deadline: run.clone(), log: log.clone(), ..Default::default() },
            &mut laid,
            &mut fanout_laid,
            &mut watch,
        );
        removed.extend(v.removed_spans);
        if v.dropped > 0 {
            eprintln!("vias: {} of {} dropped, {} of {} tries kept, {:.1} s", v.dropped, v.vias_before, v.kept, v.tried, started.elapsed().as_secs_f64());
            step(&board);
            if let Some(c) = &mut tightened {
                let again = grid::pull_tight(&mut board, &mut laid, &run);
                (c.segments_after, c.length_after) = (again.segments_after, again.length_after);
            }
        }
    }
    if let Some(c) = tightened {
        say("tightening", format!("{} segments became {}, {:.0} mm became {:.0} mm", c.segments_before, c.segments_after, c.length_before, c.length_after));
        eprintln!(
            "pulled tight: {} -> {} segments, {:.0} -> {:.0} mm, {:.1} s",
            c.segments_before,
            c.segments_after,
            c.length_before,
            c.length_after,
            started.elapsed().as_secs_f64()
        );
    }
    // The clean-up runs whatever the time: what it leaves out would be written as it stands.
    timeline.enter(&run, "cleaning");
    let open_now = |board: &Board| if a.pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines.len();
    let open_before = open_now(&board);
    // The pairs' copper was locked only against the single-ended stages; it is the router's own
    // copper again for the cleanup, so a track another of its net ends on is split like any other.
    for &id in &pair_laid {
        if let Some(mut it) = board.remove(id) {
            it.locked = false;
            board.restore(id, it);
        }
    }
    // The pairs' copper is cleaned with the rest: a run whose legs never came is taken out.
    laid.extend(pair_laid.iter().copied().filter(|id| board.item(*id).is_some()));
    let (doubled, spans) = grid::drop_duplicates(&mut board, &mut laid);
    removed.extend(spans);
    let (pruned, spans) = grid::prune_dangling(&mut board, &mut laid);
    removed.extend(spans);
    if doubled + pruned > 0 {
        say("cleaning", format!("{doubled} doubled tracks and {pruned} pieces of copper leading nowhere taken out"));
        eprintln!("dropped {doubled} doubled, pruned {pruned} dangling, open {open_before} -> {}, {:.1} s", open_now(&board), started.elapsed().as_secs_f64());
    }
    if a.diff_pairs && timeline.enter(&run, "length matching") {
        // Last, once the copper is settled: the shorter net of each pair is given bumps.
        let pairs = grid::diff_pairs(&board);
        let t = grid::match_lengths(&mut board, &pairs, &mut laid, &run);
        for (pair, before, after) in &t.skews {
            say("pairs", format!("{pair}: {before:.2} mm apart in length, {after:.2} mm after tuning"));
            eprintln!("tune: {pair} skew {before:.2} -> {after:.2} mm");
        }
        step(&board);
    }
    let open = if a.pours_block { connectivity(&board) } else { connectivity_without_pours(&board) }.airlines.len();
    eprintln!("routed {} more, {} failed, {} open, {:.1} s", stats.routed, stats.failed, open, started.elapsed().as_secs_f64());
    if a.blame {
        print_blame(&mut board, a.pours_block);
    }

    if !fanout_laid.is_empty() {
        laid.extend(fanout_laid);
    }
    if a.prefanout {
        // Escapes may have joined `laid` twice over, or been ripped since.
        let mut seen_laid = HashSet::new();
        laid.retain(|id| board.item(*id).is_some() && seen_laid.insert(*id));
    }
    let (split, spans) = grid::split_at_joins(&mut board, &mut laid);
    removed.extend(spans);
    if split > 0 {
        eprintln!("split {split} tracks where others end on them");
    }
    for line in regions::report(&board, &lengths, &laid) {
        eprintln!("regions: {line}");
        say("regions", line);
    }
    for line in grid::necks::report(&board, &laid) {
        eprintln!("neck: {line}");
        say("necks", line);
    }
    let replace = moved_parts(&board, &original_at);
    grid::keep_orphans(&board, &removed, &mut laid);
    let (tracks, vias) = grid::edits(&board, &laid);
    let stale = stale_fills(&board, &laid, text, !replace.is_empty());
    if !stale.is_empty() {
        eprintln!("dropped {} pour fills new copper cuts; KiCad refills them", stale.len());
    }
    removed.extend(stale);
    let routed = apply(text, &Edit { remove: removed, replace, tracks, vias, uuid_seed: a.uuid_seed });
    timeline.enter(&run, "done");
    // A stage the clock stopped may stop elsewhere next run: the result is not the same every
    // time, which the benchmark's comparisons need to know.
    if !timeline.limited.is_empty() {
        eprintln!("deadline: time ran out in {}", timeline.limited.join(", "));
    }
    if let Some(stage) = timeline.overran_in {
        let over = a.timeout.map_or(0.0, |t| started.elapsed().as_secs_f64() - t);
        let by = if over > 0.0 { format!("the run overran --timeout by {over:.1} s") } else { "the run still ended within --timeout".to_string() };
        eprintln!("deadline: the run's hard limit passed during {stage}; {by}");
    }
    let mut live = live.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(l) = live.as_mut() {
        l.frame(&board, true, Vec::new());
        l.fills(&routed, &board.rules);
        let stopped = if cancel.load(Ordering::Relaxed) {
            Some("Cancelled")
        } else if a.timeout.is_some_and(|t| started.elapsed().as_secs_f64() >= t) {
            Some("Deadline")
        } else {
            None
        };
        l.write(&json!({"t": "done", "open": open, "passes": 0, "stopped": stopped}));
    }
    routed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn a_line_naming_no_stage_runs_the_recipe_and_one_naming_stages_runs_those() {
        let recipe = parse(&words(&format!("route in.kicad_pcb -o out.kicad_pcb {RECIPE}"))).unwrap();
        let same = |a: &Args, b: &Args| {
            (a.negotiate, a.rip_up, a.tighten, a.prefanout, a.diff_pairs, a.shove_passes, a.shove_patience) == (b.negotiate, b.rip_up, b.tighten, b.prefanout, b.diff_pairs, b.shove_passes, b.shove_patience)
        };
        assert!(same(&parse(&words("route in.kicad_pcb -o out.kicad_pcb")).unwrap(), &recipe));
        assert!(same(&parse(&words("route in.kicad_pcb -o out.kicad_pcb --recipe default")).unwrap(), &recipe));
        assert!(same(&parse(&words("route --recipe bench in.kicad_pcb -o out.kicad_pcb")).unwrap(), &recipe));
        let minimal = parse(&words("route in.kicad_pcb -o out.kicad_pcb --recipe minimal")).unwrap();
        assert!(!minimal.negotiate && !minimal.prefanout && !minimal.tighten);
        let named = parse(&words("route in.kicad_pcb -o out.kicad_pcb --negotiate")).unwrap();
        assert!(named.negotiate && !named.prefanout && !named.rip_up);
        let load = parse(&words("route in.kicad_pcb -o out.kicad_pcb --no-route")).unwrap();
        assert!(load.no_route && !load.prefanout && !load.negotiate);
        // The recipe's settings come first, so a flag given after them wins.
        let fewer = parse(&words("route in.kicad_pcb -o out.kicad_pcb --recipe default --shove-passes 5")).unwrap();
        assert!(fewer.prefanout && fewer.shove_passes == 5);
        assert!(parse(&words("route in.kicad_pcb -o out.kicad_pcb --recipe fast")).is_err());
    }

    fn earring() -> Board {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pcb");
        Board::load(&path).unwrap().0
    }

    #[test]
    fn a_fill_new_copper_cuts_is_dropped_and_others_kept() {
        let text = "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal)) (net 0 \"\") (net 1 \"GND\") (net 2 \"SIG\")
\t(zone (net 1) (net_name \"GND\") (layer \"F.Cu\") (uuid \"z1\") (name \"gnd\") (connect_pads (clearance 0.2)) (min_thickness 0.2) (polygon (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10))) (filled_polygon (layer \"F.Cu\") (pts (xy 0.2 0.2) (xy 9.8 0.2) (xy 9.8 9.8) (xy 0.2 9.8))))
\t(zone (net 1) (net_name \"GND\") (layer \"F.Cu\") (uuid \"z2\") (name \"far\") (connect_pads (clearance 0.2)) (min_thickness 0.2) (polygon (pts (xy 50 0) (xy 60 0) (xy 60 10) (xy 50 10))) (filled_polygon (layer \"F.Cu\") (pts (xy 50.2 0.2) (xy 59.8 0.2) (xy 59.8 9.8) (xy 50.2 9.8))))
)";
        let (pcb, copper) = kicad_pcb::parse_board(text).unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let track = |net: u32, layers: u64| Item {
            kind: Kind::Track,
            net,
            layers,
            shape: Shape::Capsule { a: Point::new(-5_000_000, 5_000_000), b: Point::new(15_000_000, 5_000_000), r: 100_000 },
            hole: None,
            locked: false,
            span: None,
        };
        let (sig, gnd, back) = (board.insert(track(2, 1)), board.insert(track(1, 1)), board.insert(track(2, 2)));
        let gone = stale_fills(&board, &[sig], text, false);
        assert_eq!(gone, vec![copper.zones[0].fill_spans[0]], "only the fill the other net's track crosses goes");
        assert!(text[gone[0].start..gone[0].end].starts_with("(filled_polygon"));
        assert!(stale_fills(&board, &[gnd, back], text, false).is_empty(), "its own net, or another layer, cuts nothing");
        assert_eq!(stale_fills(&board, &[], text, true).len(), 2, "a moved part leaves every fill stale");
        let out = apply(text, &Edit { remove: gone, ..Edit::default() });
        assert_eq!(out.matches("filled_polygon").count(), 1);
        assert!(kicad_pcb::parse_board(&out).is_ok());
    }

    #[test]
    fn parses_priority_and_topo_flags() {
        let a = parse(&words(
            "route in.kicad_pcb -o out.kicad_pcb --first CHRG,SWDIO --first @U1 --first-negotiate --first-weight 0.25 --restart 2 --topo-first U1,auto --topo-trapped --topo-trapped-rip --topo-sample 0.05",
        ))
        .unwrap();
        assert_eq!(a.first, vec!["CHRG", "SWDIO", "@U1"]);
        assert!(a.first_negotiate);
        assert_eq!(a.first_weight, Some(0.25));
        assert_eq!(a.restart, 2);
        assert_eq!(a.topo_first, vec!["U1", "auto"]);
        assert!(a.topo_trapped);
        assert!(a.topo_trapped_rip);
        assert_eq!(a.topo_sample, Some(50_000));

        assert!(parse(&words("route in.kicad_pcb -o out.kicad_pcb --first-weight -0.1")).is_err());
        assert!(parse(&words("route in.kicad_pcb -o out.kicad_pcb --topo-sample 0.001")).is_err());
    }

    #[test]
    fn priority_tiers_sort_higher_failure_counts_first() {
        let counts = HashMap::from([(10, 1), (20, 3), (30, 1), (40, 2)]);
        assert_eq!(
            priority_tiers(&counts),
            vec![BTreeSet::from([20]), BTreeSet::from([40]), BTreeSet::from([10, 30])]
        );
    }

    #[test]
    fn resolves_part_signal_nets_and_topo_pairs_on_earring() {
        let b = earring();
        let u1_signals = resolve_priority_nets(&b, &[String::from("@U1")]);
        let gnd = (1..b.nets.len() as u32).find(|&n| b.net_name(n) == "GND").unwrap();
        let swdio = (1..b.nets.len() as u32).find(|&n| b.net_name(n) == "SWDIO").unwrap();
        assert!(u1_signals.contains(&swdio), "@U1 includes U1 signal nets");
        assert!(!u1_signals.contains(&gnd), "@U1 skips poured planes unless :all is asked");
        let u1_all = resolve_priority_nets(&b, &[String::from("@U1:all")]);
        assert!(u1_all.contains(&gnd));

        let sig_pairs = resolve_topo_only(&b, &[String::from("U1:signals")], false);
        let pad_pairs = resolve_topo_only(&b, &[String::from("U1:pads")], false);
        assert!(!sig_pairs.is_empty());
        assert!(!pad_pairs.is_empty());
    }

    fn vec_args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn prefanout_flags_parse_and_validate() {
        let a = parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--prefanout"])).unwrap();
        assert!(a.prefanout);
        assert!(a.prefanout_inward);
        assert!(!a.prefanout_soft);

        let b = parse(&vec_args(&[
            "route",
            "in.kicad_pcb",
            "-o",
            "out.kicad_pcb",
            "--prefanout-mode=dogbone",
            "--prefanout-order=trapped",
            "--prefanout-ring",
            "1.25",
            "--prefanout-soft",
            "--prefanout-keep-unused",
            "--no-prefanout-inward",
        ]))
        .unwrap();
        assert!(b.prefanout);
        assert_eq!(b.prefanout_mode, Some(grid::EscapeMode::Dogbone));
        assert_eq!(b.prefanout_order, Some(grid::EscapeOrder::TrappedFirst));
        assert_eq!(b.prefanout_ring, Some(1_250_000));
        assert!(b.prefanout_soft);
        assert!(b.prefanout_keep_unused);
        assert!(!b.prefanout_inward);

        assert!(parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--prefanout-mode", "bogus"])).is_err());
        assert!(parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--prefanout-ring", "9.0"])).is_err());
    }

    #[test]
    fn negotiate_flags_parse_and_validate() {
        let def = parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--negotiate"])).unwrap();
        assert!(def.negotiate_vias);
        assert!(def.negotiate_rip);
        assert!(def.negotiate_tree);
        assert_eq!(def.negotiate_repasses, None);

        let leg = parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--negotiate", "--negotiate-legacy"])).unwrap();
        assert!(!leg.negotiate_vias);
        assert!(!leg.negotiate_rip);
        assert!(!leg.negotiate_tree);
        assert_eq!(leg.negotiate_repasses, Some(0));

        let custom = parse(&vec_args(&[
            "route",
            "in.kicad_pcb",
            "-o",
            "out.kicad_pcb",
            "--negotiate-repasses=3",
            "--no-negotiate-vias",
            "--no-negotiate-rip",
            "--no-negotiate-tree",
        ]))
        .unwrap();
        assert_eq!(custom.negotiate_repasses, Some(3));
        assert!(!custom.negotiate_vias);
        assert!(!custom.negotiate_rip);
        assert!(!custom.negotiate_tree);
        assert!(parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--negotiate-repasses", "nope"])).is_err());

        let dp = parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--diff-pairs"])).unwrap();
        assert!(dp.diff_pairs && !dp.diff_pairs_legacy);
        let dpl = parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--diff-pairs-legacy"])).unwrap();
        assert!(dpl.diff_pairs && dpl.diff_pairs_legacy);
    }

    // Pins today's via prices on purpose, to document them. Stage 7 of REMEDIATION.md replaces
    // `via_costs` with one price per board and retires this test with it.
    #[test]
    fn via_prices_follow_the_planes_and_scale_with_the_cell() {
        let (pcb, copper) = kicad_pcb::parse_board("(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal)) (net 0 \"\"))").unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let plain = parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--cell", "0.035"])).unwrap();
        let c = via_costs(&plain, &board);
        assert_eq!((c.negotiate, c.open, c.rip), (500, 600, 320));
        board.planes = 1;
        let c = via_costs(&plain, &board);
        assert_eq!((c.negotiate, c.open, c.rip), (200, 80, 80));
        let given = parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--cell", "0.035", "--via-cost", "0.7,0.28", "--no-directional", "--no-reduce-vias"])).unwrap();
        let c = via_costs(&given, &board);
        assert_eq!((c.negotiate, c.open, c.rip), (200, 80, 80));
        assert!(given.no_directional && given.no_reduce_vias);
        assert!(parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--via-cost", "0"])).is_err());
        assert!(parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--via-cost", "1,2,3"])).is_err());
    }
}

#[cfg(test)]
mod region_blame_tests {
    use super::*;

    #[test]
    fn a_region_that_keeps_a_net_off_the_board_is_blamed() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pcb");
        let (mut board, _) = Board::load(&path).unwrap();
        let mine: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via)).map(|(id, _)| id).collect();
        for id in mine {
            board.remove(id);
        }
        let open = connectivity_without_pours(&board).airlines;
        let airline = open.iter().find(|a| board.net_name(a.net) == "SWDIO").expect("SWDIO open");
        let p = |x: i64, y: i64| Point::new(x * 1_000_000, y * 1_000_000);
        board.regions = vec![board::Region {
            name: "nowhere".into(),
            shape: Shape::Polygon(vec![p(0, 0), p(300, 0), p(300, 300), p(0, 300)]),
            layers: (1u64 << board.layers.len()) - 1,
            rules: vec![board::RegionRule { nets: Some([airline.net].into()), kind: board::RegionKind::Never, vias: false }],
        }];
        let grid = grid::ObstacleMap::new(&board, grid::Profile::for_net(&board, airline.net), 100_000, false).view(airline.net);
        assert!(grid.blocked.iter().all(|b| *b), "the net may run nowhere");
        assert_eq!(regions_blocking(&board, airline.net, airline.from_item, airline.to_item, false), vec!["nowhere".to_string()]);
    }
}
