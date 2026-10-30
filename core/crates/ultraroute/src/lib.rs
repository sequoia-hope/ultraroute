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
//!
//! A run is the stages of [`STAGES`] in turn ([`stages`]), each with the share of `--timeout` the
//! table gives it. With `--stats FILE` (or `--report FILE`) the run's stats and findings are
//! written as JSON ([`stats`]); a caller of [`route`] can watch each stage as it goes ([`watch`]).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use board::{connectivity, connectivity_without_pours, Board, Item, ItemId, Kind, Nm, Point, Shape};
use kicad_pcb::{apply, mm, Edit};
use serde_json::{json, Value};

mod regions;
pub mod stages;
pub mod stats;
pub mod watch;

pub use stages::{Ledger, Stage, StageCtx};
pub use stats::{Finding, RunStats, StageStats};
pub use watch::{Cleaned, Flow, StageEvent, Unwatched, Watch};

pub const USAGE: &str = "usage: ultraroute route IN.kicad_pcb -o OUT.kicad_pcb [--strip] [--strip-all] [--planes LAYER,...] [--auto-planes] [--negotiate-rounds N] [--negotiate-patience N] [--negotiate-growth G] [--negotiate-repasses N] [--negotiate-jacobi] [--negotiate-reroute-all] [--diff-pairs] [--prefanout] [--prefanout-mode stub|dogbone|grid|inner|same] [--prefanout-order round|inner|trapped] [--prefanout-ring MM] [--negotiate] [--rip-up] [--nudge] [--tighten] [--via-cost MM[,OPEN_MM]] [--gpu] \
[--timeout SECONDS] [--cell MM] [--shove] [--shove-plough rip|region] [--shove-reach MM] [--shove-repair S] [--shove-passes N] [--shove-ties] [--shove-cascade N] [--shove-patience N] [--rip-region X,Y,R]... [--via-avoid X0,Y0,X1,Y1]... [--via-avoid-cost N] [--regions FILE] [--pour-cost W] [--seed N] [--uuid-seed N] [--adopt] [--verbose] [--place REF=X,Y]... [--lock REF,...] [--pours-block] [--no-route] [--threads N] [--max-memory MB] [--estimate] \
[--recipe default|bench|minimal] [--live PATH|-] [--live-fps N] [--live-step MS] [--blame] [--stats FILE]
--place takes millimetres with y pointing up: give KiCad's y negated (KiCad y 127.5 is -127.5).";

pub struct Args {
    pub input: String,
    pub output: String,
    pub strip: bool,
    pub strip_all: bool,
    /// Route differential pairs (USB D+ and D-) coupled, before anything else.
    pub diff_pairs: bool,
    /// Layers given whole to a plane (`In1.Cu,In2.Cu`): no tracks on them, vias through them, and
    /// their pours join their nets.
    pub planes: Vec<String>,
    /// Negotiation's most rounds, rounds without progress before it stops, how fast its
    /// congestion price grows, and passes after the first; the router's defaults when unset.
    pub negotiate_rounds: Option<usize>,
    pub negotiate_patience: Option<usize>,
    pub negotiate_growth: Option<f64>,
    pub negotiate_repasses: Option<usize>,
    /// Negotiation routes each round against the last on `--threads` threads
    /// (`NegotiationOptions::jacobi`), instead of one connection after another.
    pub negotiate_jacobi: bool,
    /// Every negotiation round reroutes every connection, not only those that collided.
    pub negotiate_reroute_all: bool,
    /// Escape every fine-pitch part's pads before routing: how, in what order, and how far out
    /// the escapes end.
    pub prefanout: bool,
    pub prefanout_mode: Option<grid::EscapeMode>,
    pub prefanout_order: Option<grid::EscapeOrder>,
    pub prefanout_ring: Option<Nm>,
    pub negotiate: bool,
    pub rip_up: bool,
    pub tighten: bool,
    /// Via prices in millimetres of straight track: negotiation's, and that of routing one
    /// connection at a time and of rip-up (1.2 and 0.8 times negotiation's unless given).
    pub via_cost: Option<(f64, Option<f64>)>,
    pub nudge: bool,
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
    /// Where to write the run's stats and findings as JSON (`--stats`, or `--report` as Studio
    /// names it).
    pub stats: Option<String>,
    /// The command line as read, the recipe's flags put in.
    pub line: Vec<String>,
}

/// The stages and settings the router runs with unless told otherwise: the benchmark's recipe
/// (`docs/pcbench.md`), measured as the best of what the router has.
pub const RECIPE: &str = "--negotiate --rip-up --tighten --prefanout --diff-pairs --shove-plough region --shove-reach 0.5 --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8";

/// Flags that name a stage to run. A run naming none gets [`RECIPE`]; one naming its own stages
/// gets those alone, as before there was a default.
const STAGE_FLAGS: &[&str] = &[
    "--negotiate", "--rip-up", "--tighten", "--prefanout", "--diff-pairs", "--shove", "--shove-plough", "--nudge",
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
        planes: Vec::new(),
        negotiate_rounds: None,
        negotiate_patience: None,
        negotiate_growth: None,
        negotiate_repasses: None,
        negotiate_jacobi: false,
        negotiate_reroute_all: false,
        prefanout: false,
        prefanout_mode: None,
        prefanout_order: None,
        prefanout_ring: None,
        negotiate: false,
        rip_up: false,
        tighten: false,
        via_cost: None,
        nudge: false,
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
        stats: None,
        line: args.to_vec(),
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
            "--negotiate-jacobi" => a.negotiate_jacobi = true,
            "--negotiate-reroute-all" => a.negotiate_reroute_all = true,
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
            "--timeout" => a.timeout = Some(value()?.parse().map_err(|e| format!("--timeout: {e}"))?),
            "--live" => a.live = Some(value()?),
            "--live-fps" => a.fps = value()?.parse().map_err(|e| format!("--live-fps: {e}"))?,
            "--live-step" => a.live_step = value()?.parse().map_err(|e| format!("--live-step: {e}"))?,
            "--lock" => a.lock.extend(value()?.split(',').filter(|r| !r.is_empty()).map(str::to_string)),
            "--nudge" => a.nudge = true,
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
            "--stats" | "--report" => a.stats = Some(value()?),
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
        self.send_frame(board, true, Vec::new());
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
    fn send_trials(&mut self, _round: usize, trials: &[grid::Trial]) {
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

    fn send_frame(&mut self, board: &Board, force: bool, events: Vec<Value>) {
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

fn moved_parts(board: &Board, original_at: &[Point], text: &str) -> Vec<(kicad_pcb::Span, String)> {
    let moved: Vec<(usize, Point)> = board
        .footprints
        .iter()
        .zip(original_at)
        .enumerate()
        .filter(|(_, (f, at))| f.at != **at)
        .map(|(k, (f, at))| (k, Point::new(f.at.x - at.x, f.at.y - at.y)))
        .collect();
    let mut out: Vec<(kicad_pcb::Span, String)> = moved
        .iter()
        .filter_map(|&(k, _)| {
            let f = &board.footprints[k];
            let angle = if f.rotation_deg == 0.0 { String::new() } else { format!(" {}", f.rotation_deg) };
            Some((f.at_span?, format!("(at {} {}{angle})", mm(f.at.x as f64 * 1e-9), mm(f.at.y as f64 * 1e-9))))
        })
        .collect();
    // A footprint's zones and rule areas are written in the board's frame, not the part's: each
    // point moves with the part. The board keeps no spans of them, so they are read again.
    if !moved.is_empty() {
        if let Ok((_, copper)) = kicad_pcb::parse_board(text) {
            for zone in &copper.zones {
                let Some(&(_, by)) = zone.footprint.and_then(|fp| moved.iter().find(|(k, _)| *k == fp)) else { continue };
                let (start, end) = (zone.span.start, zone.span.end);
                out.push((zone.span, shift_points(&text[start..end], by)));
            }
        }
    }
    out
}

/// `text` with every `(xy X Y)` in it moved by `by`.
fn shift_points(text: &str, by: Point) -> String {
    let (dx, dy) = (by.x as f64 * 1e-6, by.y as f64 * 1e-6);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("(xy ") {
        out.push_str(&rest[..i + 4]);
        rest = &rest[i + 4..];
        let close = rest.find(')').unwrap_or(rest.len());
        let nums: Vec<f64> = rest[..close].split_whitespace().filter_map(|v| v.parse().ok()).collect();
        if let [x, y] = nums[..] {
            out.push_str(&format!("{} {}", kicad_pcb::write::mm((x + dx) * 1e-3), kicad_pcb::write::mm((y + dy) * 1e-3)));
        } else {
            out.push_str(&rest[..close]);
        }
        rest = &rest[close..];
    }
    out.push_str(rest);
    out
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

/// The lines above, printed, and what each says stands in the way, one per open connection in
/// the order connectivity gives them.
fn print_blame(board: &mut Board, pours_block: bool) -> Vec<String> {
    let mut said = Vec::new();
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
        said.push(what.join("; "));
    }
    said
}

/// Via prices for each stage, in grid steps (ten to a cell of straight track).
#[derive(Default)]
struct ViaCosts {
    negotiate: u32,
    open: u32,
    rip: u32,
}

/// What a via costs negotiation unless `--via-cost` says, in millimetres of straight track:
/// measured on the benchmark at 1.0, 2.5 and 5.0 mm (docs/pcbench.md), 5.0 laid the fewest vias,
/// 7% fewer than 2.5, for 0.5% more track.
pub const VIA_COST_MM: f64 = 5.0;

/// The prices `--via-cost` gives, or by default [`VIA_COST_MM`], in each stage's grid steps:
/// routing one connection at a time pays 1.2 times negotiation's price and rip-up 0.8 times
/// (below its price for ripping copper, so a route takes the other layer before ripping a track
/// there). With planes inside, signals run on the outer layers among the planes' vias, and dear
/// vias drive routes the long way round dense parts: 1.0, 0.4 and 0.4 mm, as the router had them
/// before (the benchmark's recipe names no planes, so this is not measured there).
fn via_costs(a: &Args, board: &Board) -> ViaCosts {
    let cell = a.cell.unwrap_or(grid::Options::default().cell);
    let at = |mm: f64| grid::via_cost_mm(cell, mm);
    let (negotiate, open, rip) = match a.via_cost {
        Some((neg, open)) => (neg, open.unwrap_or(neg * 1.2), open.unwrap_or(neg * 0.8)),
        None if board.planes != 0 => (1.0, 0.4, 0.4),
        None => (VIA_COST_MM, VIA_COST_MM * 1.2, VIA_COST_MM * 0.8),
    };
    ViaCosts { negotiate: at(negotiate), open: at(open), rip: at(rip) }
}

/// The share of the board one net's pour must cover on an inner layer for the layer to be taken
/// as that net's plane.
const PLANE_SHARE: f64 = 0.6;

/// What the run keeps back at the end of `--timeout` to clean up and write the board: this
/// share of it, and never less than `RESERVE_SECONDS`.
const RESERVE_SHARE: f64 = 0.02;
const RESERVE_SECONDS: f64 = 2.0;

/// How a stage's deadline comes from the run's `--timeout`; with no timeout, every stage has the
/// run's own, which is no limit.
#[derive(Debug, Clone, Copy)]
pub enum Budget {
    /// No deadline: the stage runs whatever the time.
    Untimed,
    /// The run's own deadline.
    Run,
    /// This share of what is left of the run when it starts.
    Left(f64),
    /// Until this share of `--timeout` has gone since the run's clock started.
    Until(f64),
    /// This share of `--timeout` from when it starts.
    Timeout(f64),
    /// The deadline of the routing that rescues what it leaves open: the run's, less what the
    /// stages after it keep back ([`Budget::KeepBack`]).
    Rescue,
    /// This share of what is left of `Rescue` when it starts.
    RescueLeft(f64),
    /// The run's own deadline, and this share of `--timeout` kept back from `Rescue` for it.
    KeepBack(f64),
    /// One budget in a run with length-limited nets routed first, another without.
    First { alone: &'static Budget, after_first: &'static Budget },
}

/// The run's clock: its deadline, the routing's, and when it started.
pub(crate) struct RunClock {
    pub run: grid::Deadline,
    pub rescue: grid::Deadline,
    pub started: Instant,
    pub timeout: Option<f64>,
}

impl Budget {
    /// The deadline this budget gives a stage starting now.
    pub(crate) fn deadline(&self, clock: &RunClock, first: bool) -> grid::Deadline {
        let (run, t) = (&clock.run, clock.timeout);
        match *self {
            Budget::Untimed => grid::Deadline::unlimited(run.cancel.clone()),
            Budget::Run | Budget::KeepBack(_) => run.clone(),
            Budget::Left(share) => run.share(share),
            Budget::Until(share) => match t {
                Some(t) => run.until_at(clock.started + Duration::from_secs_f64(t * share)),
                None => run.clone(),
            },
            Budget::Timeout(share) => match t {
                Some(t) => run.seconds(t * share),
                None => run.clone(),
            },
            Budget::Rescue => clock.rescue.clone(),
            Budget::RescueLeft(share) => clock.rescue.share(share),
            Budget::First { alone, after_first } => if first { after_first } else { alone }.deadline(clock, first),
        }
    }
}

/// How a stage keeps the run's time: whether it starts only while the run's hard deadline has not
/// passed, starts whatever the time, or is a step of the stage before it that the clock does not
/// tell apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clock {
    Timed,
    Always,
    Untold,
}

/// A row of the stage table.
pub struct Entry {
    pub stage: &'static dyn Stage,
    pub clock: Clock,
    pub budget: Budget,
    /// The stage it is part of: it runs only if that one did.
    pub within: Option<&'static str>,
}

const fn row(stage: &'static dyn Stage, clock: Clock, budget: Budget, within: Option<&'static str>) -> Entry {
    Entry { stage, clock, budget, within }
}

/// Every stage of a run, in order, with its budget: the one place both live. Pre-fanout and
/// joining plane pads take shares of what is left; coupling pairs runs until a quarter of the
/// run has gone; negotiation takes half the run (half of what the length-limited nets leave);
/// the routing and rescue stages stop in time for the via pass's tenth of the run; the clean-up
/// and the write run whatever the time.
pub const STAGES: &[Entry] = &[
    row(&stages::SetUp, Clock::Untold, Budget::Untimed, None),
    row(&stages::Prefanout, Clock::Timed, Budget::Left(0.15), None),
    row(&stages::Planes, Clock::Timed, Budget::Left(0.05), None),
    row(&stages::Pairs, Clock::Timed, Budget::Until(0.25), None),
    row(&stages::PairEscapes, Clock::Untold, Budget::Left(0.15), Some("pairs")),
    row(&stages::Routing, Clock::Timed, Budget::Untimed, None),
    row(&stages::First, Clock::Timed, Budget::RescueLeft(0.25), Some("routing")),
    row(&stages::Negotiation, Clock::Timed, Budget::First { alone: &Budget::Timeout(0.5), after_first: &Budget::RescueLeft(0.5) }, Some("routing")),
    row(&stages::Finishing, Clock::Timed, Budget::Rescue, Some("routing")),
    row(&stages::PruneEscapes, Clock::Untold, Budget::Untimed, Some("routing")),
    row(&stages::RipUp, Clock::Timed, Budget::Rescue, Some("routing")),
    row(&stages::MergeEscapes, Clock::Untold, Budget::Untimed, Some("routing")),
    row(&stages::Shove, Clock::Timed, Budget::Rescue, None),
    row(&stages::Nudge, Clock::Timed, Budget::Rescue, None),
    row(&stages::Tightening, Clock::Timed, Budget::Run, None),
    row(&stages::Vias, Clock::Timed, Budget::KeepBack(0.1), None),
    row(&stages::Cleaning, Clock::Always, Budget::Untimed, None),
    row(&stages::LengthMatching, Clock::Timed, Budget::Run, None),
    row(&stages::Outcome, Clock::Untold, Budget::Untimed, None),
    row(&stages::Write, Clock::Untold, Budget::Untimed, None),
];

/// The share of `--timeout` the stages keep back from the routing before them.
fn kept_back() -> f64 {
    STAGES.iter().map(|e| if let Budget::KeepBack(share) = e.budget { share } else { 0.0 }).sum()
}

/// The open connections, poured copper joining nets or not as the run asks.
fn open_airlines(a: &Args, board: &Board) -> Vec<board::Airline> {
    if a.pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines
}

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

/// Routes `board`, read from `text`, as `a` asks, and returns the routed board's text. `live`
/// receives the run as frames; setting `cancel` stops it at the next connection.
pub fn run(a: &Args, board: Board, text: &str, live: Option<Live>, cancel: Arc<AtomicBool>) -> String {
    run_with_solver(a, board, text, live, cancel, None)
}

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

/// A routed board's text, and what the run did.
pub struct Routed {
    pub text: String,
    pub stats: RunStats,
}

/// Routes `board` using `custom_solver` when provided, or the default CPU/GPU solver otherwise.
pub fn run_with_solver(
    a: &Args,
    board: Board,
    text: &str,
    live: Option<Live>,
    cancel: Arc<AtomicBool>,
    custom_solver: Option<Box<dyn grid::FieldSolver>>,
) -> String {
    route(a, board, text, live, cancel, custom_solver, None).text
}

/// [`run_with_solver`], showing `watch` the run as it goes, and returning the run's stats with the
/// board: the stages of [`STAGES`] in turn, each with the deadline its budget gives it.
pub fn route(
    a: &Args,
    mut board: Board,
    text: &str,
    live: Option<Live>,
    cancel: Arc<AtomicBool>,
    custom_solver: Option<Box<dyn grid::FieldSolver>>,
    watch: Option<&mut dyn Watch>,
) -> Routed {
    let begun = Instant::now();
    let mut stats = begin_stats(a, &board, text);
    let mut ledger = Ledger::default();
    prepare(a, &mut board, &mut ledger);
    let live = Arc::new(Mutex::new(live));
    if let Some(l) = lock(&live).as_mut() {
        l.header(&board);
    }
    if a.no_route {
        return unrouted(a, board, text, ledger, &live, stats, begun);
    }
    let started = Instant::now();
    // One deadline for the whole run: every stage's is a share of it, none past its hard end.
    let run = run_deadline(started, a.timeout, cancel.clone());
    // Routing, and the stages that rescue what it left open, stop in time for the via pass.
    let rescue = sooner(&run, a.timeout.map_or(0.0, |t| t * kept_back()));
    let clock = RunClock { run, rescue, started, timeout: a.timeout };
    let mut timeline = Timeline { stage: "set-up", limited: Vec::new(), overran_in: None };
    let mut solver = solver_for(a, custom_solver);
    let log = run_log(a, &live, started, watch.as_ref().and_then(|w| w.log()));
    let mut hooks = watch::Hooks { live: live.clone(), user: watch };
    let threads = a.threads.unwrap_or_else(default_threads);
    let mut mark = Mark::take(a, &board, &mut stats);
    let (mut ran, mut stopped): (Vec<&str>, bool) = (Vec::new(), false);
    for entry in STAGES {
        let name = entry.stage.name();
        if entry.within.is_some_and(|w| !ran.contains(&w)) || !entry.stage.enabled(a, &board, &ledger) {
            continue;
        }
        // A timed stage starts only while the run's hard deadline has not passed, or a watcher
        // has not stopped the run; the clean-up starts whatever the time.
        let go = match entry.clock {
            Clock::Timed => timeline.enter(&clock.run, name) && !stopped,
            Clock::Always => {
                timeline.enter(&clock.run, name);
                true
            }
            Clock::Untold => true,
        };
        if !go {
            continue;
        }
        ran.push(name);
        stopped |= hooks.stage(name, &board, &ledger, StageEvent::Start) == Flow::Stop;
        let from = if ran.len() == 1 { begun } else { Instant::now() };
        let mut ctx = StageCtx {
            args: a,
            board: &mut board,
            ledger: &mut ledger,
            solver: solver.as_mut(),
            log: &log,
            watch: &mut hooks,
            stats: &mut stats,
            threads,
            clock: &clock,
            budget: entry.budget,
            text,
        };
        let detail = entry.stage.run(&mut ctx);
        let done = mark.close(a, &board, name, (begun, from), detail, &mut stats);
        stopped |= hooks.stage(name, &board, &ledger, StageEvent::End(&done)) == Flow::Stop;
        stats.stages.push(done);
    }
    timeline.enter(&clock.run, "done");
    finish(a, &board, &mut ledger, &live, stats, &timeline, (begun, started), &cancel)
}

/// The run's live frames, which its log writes to as well.
fn lock(live: &Arc<Mutex<Option<Live>>>) -> std::sync::MutexGuard<'_, Option<Live>> {
    live.lock().unwrap_or_else(|e| e.into_inner())
}

/// The run's settings and provenance, before it starts.
fn begin_stats(a: &Args, board: &Board, text: &str) -> RunStats {
    let (version, commit) = stats::version();
    let threads = a.threads.unwrap_or_else(default_threads);
    let cell = a.cell.unwrap_or_else(default_cell);
    let estimate = grid::memory_estimate(board, cell, if a.negotiate_jacobi { threads } else { 1 });
    RunStats {
        router: stats::Router { version: version.to_string(), commit: commit.map(String::from) },
        input: stats::Input { path: a.input.clone(), sha256: stats::sha256(text.as_bytes()), bytes: text.len() },
        settings: stats::Settings { flags: a.line.clone(), seed: a.seed, uuid_seed: a.uuid_seed, threads, cell_mm: cell as f64 / 1e6, timeout: a.timeout, nodes: estimate.nodes },
        memory: stats::Memory { estimate_mb: estimate.mb(), peak_mb: None },
        ..RunStats::default()
    }
}

/// The board as the run is asked to take it, before its clock starts: parts locked and placed,
/// copper stripped, planes taken.
fn prepare(a: &Args, board: &mut Board, ledger: &mut Ledger) {
    ledger.original_at = board.footprints.iter().map(|f| f.at).collect();
    for f in board.footprints.iter_mut().filter(|f| a.lock.contains(&f.reference)) {
        f.locked = true;
    }
    for name in a.lock.iter().filter(|n| !board.footprints.iter().any(|f| &f.reference == *n)) {
        eprintln!("warning: --lock {name}: no such part on the board");
    }
    for (name, x, y) in &a.place {
        let Some(fp) = board.footprints.iter().position(|f| &f.reference == name) else {
            eprintln!("warning: --place {name}: no such part on the board");
            continue;
        };
        let target = Point::new((x * 1e6).round() as i64, (-y * 1e6).round() as i64);
        let at = board.footprints[fp].at;
        let ripped = board.move_footprint(fp, Point::new(target.x - at.x, target.y - at.y));
        ledger.removed.extend(ripped.iter().filter_map(|it| it.span));
    }
    if a.strip || a.strip_all {
        let loose: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && (a.strip_all || !it.locked)).map(|(id, _)| id).collect();
        for id in loose {
            if let Some(it) = board.remove(id) {
                ledger.removed.extend(it.span);
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
        let said = board.warnings.len();
        board.set_planes(mask);
        for w in &board.warnings[said..] {
            eprintln!("warning: {w}");
        }
    }
}

/// A run that only loads the board (`--no-route`): it is written back with the parts moved as
/// asked, and what is open is said.
fn unrouted(a: &Args, mut board: Board, text: &str, mut ledger: Ledger, live: &Arc<Mutex<Option<Live>>>, mut stats: RunStats, begun: Instant) -> Routed {
    let open = open_airlines(a, &board);
    let blamed = if a.blame { print_blame(&mut board, a.pours_block) } else { Vec::new() };
    let replace = moved_parts(&board, &ledger.original_at, text);
    ledger.removed.extend(stale_fills(&board, &[], text, !replace.is_empty()));
    let out = apply(text, &Edit { remove: std::mem::take(&mut ledger.removed), replace, ..Edit::default() });
    if let Some(l) = lock(live).as_mut() {
        l.fills(&out, &board.rules);
        l.write(&json!({"t": "done", "open": open.len(), "passes": 0, "stopped": null}));
    }
    stats.findings = stages::open_findings(&board, &open, &blamed);
    stats.summary = stats::Summary { connections: open.len(), opens: open.len(), vias: vias_on(&board), seconds: begun.elapsed().as_secs_f64(), ..Default::default() };
    stats.memory.peak_mb = peak_memory_mb();
    Routed { text: out, stats }
}

/// The run's end: what the clock cut short said, the last frame sent, the summary made.
#[allow(clippy::too_many_arguments)]
fn finish(a: &Args, board: &Board, ledger: &mut Ledger, live: &Arc<Mutex<Option<Live>>>, mut stats: RunStats, timeline: &Timeline, (begun, started): (Instant, Instant), cancel: &AtomicBool) -> Routed {
    let over = a.timeout.map_or(0.0, |t| started.elapsed().as_secs_f64() - t);
    stats.deadline = stats::DeadlineStats {
        limited: timeline.limited.iter().map(|s| s.to_string()).collect(),
        hard_passed_in: timeline.overran_in.map(String::from),
        overran_seconds: timeline.overran_in.map(|_| over),
        cancelled: cancel.load(Ordering::Relaxed),
    };
    // A stage the clock stopped may stop elsewhere next run: the result is not the same every
    // time, which the benchmark's comparisons need to know.
    for line in stats.deadline.lines() {
        eprintln!("{line}");
        stats.findings.push(Finding { severity: "warning", kind: "deadline", message: line, refs: Vec::new(), nets: Vec::new(), ends: None });
    }
    for s in &mut stats.stages {
        s.cut = timeline.limited.contains(&s.name);
    }
    let routed = ledger.output.take().expect("the write stage runs whatever the time");
    if let Some(l) = lock(live).as_mut() {
        l.send_frame(board, true, Vec::new());
        l.fills(&routed, &board.rules);
        let stopped = if cancel.load(Ordering::Relaxed) {
            Some("Cancelled")
        } else if a.timeout.is_some_and(|t| started.elapsed().as_secs_f64() >= t) {
            Some("Deadline")
        } else {
            None
        };
        l.write(&json!({"t": "done", "open": ledger.open, "passes": 0, "stopped": stopped}));
    }
    let connections = stats.stages.first().map_or(ledger.open, |s| s.open_after);
    stats.summary = stats::Summary {
        connections,
        opens: ledger.open,
        routed: connections.saturating_sub(ledger.open),
        failed: ledger.routed.failed,
        vias: vias_on(board),
        seconds: begun.elapsed().as_secs_f64(),
    };
    stats.memory.peak_mb = peak_memory_mb();
    if a.verbose {
        for line in stats.table() {
            eprintln!("{line}");
        }
    }
    Routed { text: routed, stats }
}

fn vias_on(board: &Board) -> usize {
    board.items().filter(|(_, it)| it.kind == Kind::Via).count()
}

/// The field solver: the caller's, or the CPU's, or with `--gpu` the GPU's where there is one.
#[cfg_attr(not(feature = "gpu"), allow(unused_variables))]
fn solver_for(a: &Args, custom_solver: Option<Box<dyn grid::FieldSolver>>) -> Box<dyn grid::FieldSolver> {
    match custom_solver {
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
    }
}

/// The run's log, when anyone listens: `--verbose` on standard error, the live stream, and the
/// watcher's own.
fn run_log(a: &Args, live: &Arc<Mutex<Option<Live>>>, started: Instant, extra: Option<grid::Log>) -> Option<grid::Log> {
    let streaming = lock(live).is_some();
    let verbose = a.verbose;
    let shared = live.clone();
    (streaming || verbose || extra.is_some()).then(|| {
        grid::Log(Arc::new(move |act| {
            if verbose {
                eprintln!("  [{:8.3} s] {}: {}", started.elapsed().as_secs_f64(), act.phase, act.text);
            }
            if let Some(grid::Log(extra)) = &extra {
                extra(act.clone());
            }
            if let Some(l) = shared.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                l.note(act);
            }
        }))
    })
}

/// The board between two stages, to tell what the stage after did: connections open, the
/// router's copper by identity and shape, vias, and the work counts.
struct Mark {
    open: usize,
    copper: HashSet<(ItemId, u64, [Nm; 5])>,
    vias: usize,
    work: grid::work::Work,
}

impl Mark {
    fn take(a: &Args, board: &Board, stats: &mut RunStats) -> Mark {
        let t = Instant::now();
        let (open, copper, vias) = (open_airlines(a, board).len(), stages::copper_keys(board), vias_on(board));
        stats.stats_seconds += t.elapsed().as_secs_f64();
        // Last, so the counting itself is not counted as the next stage's work.
        Mark { open, copper, vias, work: grid::work::Work::now() }
    }

    /// The stage `name` that ran from `from` is done: its stats, and this mark moved past it.
    fn close(&mut self, a: &Args, board: &Board, name: &'static str, (begun, from): (Instant, Instant), detail: Value, stats: &mut RunStats) -> StageStats {
        let work = grid::work::Work::now().since(&self.work);
        let seconds = from.elapsed().as_secs_f64();
        let next = Mark::take(a, board, stats);
        let done = StageStats {
            name,
            started: from.duration_since(begun).as_secs_f64(),
            seconds,
            cut: false,
            open_before: self.open,
            open_after: next.open,
            laid: next.copper.difference(&self.copper).count(),
            ripped: self.copper.difference(&next.copper).count(),
            vias: next.vias,
            work: work.into(),
            detail,
        };
        *self = next;
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn vec_args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn prefanout_flags_parse_and_validate() {
        let a = parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--prefanout"])).unwrap();
        assert!(a.prefanout);

        let b = parse(&vec_args(&[
            "route",
            "in.kicad_pcb",
            "-o",
            "out.kicad_pcb",
            "--prefanout-mode=dogbone",
            "--prefanout-order=trapped",
            "--prefanout-ring",
            "1.25",
        ]))
        .unwrap();
        assert!(b.prefanout);
        assert_eq!(b.prefanout_mode, Some(grid::EscapeMode::Dogbone));
        assert_eq!(b.prefanout_order, Some(grid::EscapeOrder::TrappedFirst));
        assert_eq!(b.prefanout_ring, Some(1_250_000));

        assert!(parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--prefanout-mode", "bogus"])).is_err());
        assert!(parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--prefanout-ring", "9.0"])).is_err());
    }

    #[test]
    fn negotiate_flags_parse_and_validate() {
        let def = parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--negotiate"])).unwrap();
        assert_eq!(def.negotiate_repasses, None);

        let custom = parse(&vec_args(&[
            "route",
            "in.kicad_pcb",
            "-o",
            "out.kicad_pcb",
            "--negotiate-repasses=3",
            "--negotiate-jacobi",
        ]))
        .unwrap();
        assert_eq!(custom.negotiate_repasses, Some(3));
        assert!(custom.negotiate_jacobi);
        assert!(parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--negotiate-repasses", "nope"])).is_err());

        let dp = parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--diff-pairs"])).unwrap();
        assert!(dp.diff_pairs);
        assert!(parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--negotiate-legacy"])).is_err());
    }

    // Pins today's via prices on purpose, to document them. Stage 7 of REMEDIATION.md replaces
    // `via_costs` with one price per board and retires this test with it.
    #[test]
    fn via_prices_follow_the_planes_and_scale_with_the_cell() {
        let (pcb, copper) = kicad_pcb::parse_board("(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal)) (net 0 \"\"))").unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let plain = parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--cell", "0.035"])).unwrap();
        // Millimetres of track at a 0.035 mm cell: ten steps to a cell.
        let at = |mm: f64| grid::via_cost_mm(35_000, mm);
        let c = via_costs(&plain, &board);
        assert_eq!((c.negotiate, c.open, c.rip), (at(VIA_COST_MM), at(VIA_COST_MM * 1.2), at(VIA_COST_MM * 0.8)));
        assert_eq!(c.negotiate, 1429);
        board.planes = 1;
        let c = via_costs(&plain, &board);
        assert_eq!((c.negotiate, c.open, c.rip), (at(1.0), at(0.4), at(0.4)));
        let given = parse(&vec_args(&["route", "in.kicad_pcb", "-o", "out.kicad_pcb", "--cell", "0.035", "--via-cost", "0.7,0.28"])).unwrap();
        let c = via_costs(&given, &board);
        assert_eq!((c.negotiate, c.open, c.rip), (200, 80, 80));
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
