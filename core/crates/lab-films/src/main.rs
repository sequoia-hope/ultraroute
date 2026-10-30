//! `feature_film <feature> BOARD OUT.json [film flags] [-- router flags]`: films one stage of the
//! router as it runs, step by step, as a scrubbable JSON film for `lab/features.html`.
//!
//! A stage's film is recorded from the router itself: `ultraroute::route` runs its recipe on the
//! board (the router flags after `--`; `--strip-all` and the default recipe when none are given),
//! and the film watches the stage through the run's hooks (`ultraroute::Watch`), from its start
//! to its end, then stops the run. The board the stage gets is the one the router gives it. Two
//! films are of things that are not stages of a run, a part moved by the copper flow and the zone
//! fill; `--prepare STAGE` takes the board through the router up to and including STAGE first.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use board::{BBox, Board, Item, ItemId, Kind, Point};
use serde_json::{json, Value};
use ultraroute::{Cleaned, Flow, Ledger, StageEvent, Watch};

mod film;
mod standalone;

use film::*;

const USAGE: &str = "\
usage:
  feature_film prefanout|pairs|negotiate|route|rip-up|shove|nudge|tighten|cleanup|tune BOARD OUT.json [film flags] [-- router flags]
  feature_film flow  BOARD OUT.json REF=DX,DY [--steps N] [--rip] [--reroute S] [--prepare STAGE] [-- router flags]
  feature_film fill  BOARD OUT.json [--prepare STAGE] [-- router flags]
  feature_film index FILMS_DIR
film flags: [--max-frames N] [--title TEXT] [--subtitle TEXT] [--crop X0,Y0,X1,Y1]
router flags: as `ultraroute route` takes them (default --strip-all and the default recipe; the
  nudge film adds --nudge); --pours-block, --seed, --threads, --timeout, --negotiate-rounds,
  --shove-plough and the rest set the filmed stage as they set the router's.";

/// The stage of a run each film watches.
fn stage_of(feature: &str) -> Option<&'static str> {
    Some(match feature {
        "prefanout" => "prefanout",
        "pairs" => "pairs",
        "negotiate" => "negotiation",
        "route" => "finishing",
        "rip-up" => "rip-up",
        "shove" => "shove",
        "nudge" => "nudge",
        "tighten" => "tightening",
        "cleanup" => "cleaning",
        "tune" => "length matching",
        _ => return None,
    })
}

pub(crate) struct Cli {
    pub(crate) args: Vec<String>,
    /// What came after `--`, for the router.
    pub(crate) router: Vec<String>,
}

impl Cli {
    pub(crate) fn flag(&self, name: &str) -> Option<String> {
        for (i, a) in self.args.iter().enumerate() {
            if a == name {
                return self.args.get(i + 1).cloned();
            }
            if let Some(rest) = a.strip_prefix(&format!("{name}=")) {
                return Some(rest.to_string());
            }
        }
        None
    }

    pub(crate) fn has(&self, name: &str) -> bool {
        self.args.iter().any(|a| a == name)
    }

    pub(crate) fn positional(&self) -> Vec<String> {
        let takes_val = ["--steps", "--reroute", "--max-frames", "--title", "--subtitle", "--crop", "--prepare"];
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.args.len() {
            let a = &self.args[i];
            if a.starts_with("--") {
                i += if !a.contains('=') && takes_val.contains(&a.as_str()) { 2 } else { 1 };
                continue;
            }
            out.push(a.clone());
            i += 1;
        }
        out
    }

    pub(crate) fn crop_override(&self) -> Option<BBox> {
        let s = self.flag("--crop")?;
        let ns: Vec<f64> = s.split(',').map(|x| x.trim().parse().expect("--crop X0,Y0,X1,Y1")).collect();
        assert_eq!(ns.len(), 4, "--crop X0,Y0,X1,Y1");
        Some(BBox::of([Point::new(nm_mm(ns[0]), nm_mm(ns[1])), Point::new(nm_mm(ns[2]), nm_mm(ns[3]))]))
    }

    /// Whether poured copper blocks other nets, as the router is told.
    pub(crate) fn pours_block(&self) -> bool {
        self.has("--pours-block") || self.router.iter().any(|f| f == "--pours-block")
    }

    pub(crate) fn max_frames(&self, default: usize) -> usize {
        self.flag("--max-frames").map_or(default, |s| s.parse().expect("--max-frames"))
    }

    /// The router's settings for `board_path`: the flags after `--`, or `--strip-all` and the
    /// recipe, with `extra` added.
    pub(crate) fn router_args(&self, board_path: &str, extra: &[&str]) -> ultraroute::Args {
        let mut line: Vec<String> = vec!["route".into(), board_path.into(), "-o".into(), "unused.kicad_pcb".into()];
        if self.router.is_empty() {
            line.push("--strip-all".into());
        } else {
            line.extend(self.router.iter().cloned());
        }
        line.extend(extra.iter().map(|s| s.to_string()));
        ultraroute::parse(&line).unwrap_or_else(|e| panic!("{e}"))
    }
}

/// Runs the router on the board at `board_path` as `a` says, showing `watch` the run.
fn route_with(cli: &Cli, a: &ultraroute::Args, board_path: &str, watch: &mut dyn Watch) {
    let (board, text) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    let flags = if cli.router.is_empty() { "--strip-all".to_string() } else { cli.router.join(" ") };
    let _ = PREPARED.set(format!("through the router ({flags})"));
    ultraroute::route(a, board, &text, None, Arc::new(AtomicBool::new(false)), None, Some(watch));
}

/// Takes the board through the router up to and including the stage, and keeps it with the
/// copper the router holds as its own then.
struct Prepare {
    upto: String,
    got: Option<(Board, Vec<ItemId>)>,
}

impl Watch for Prepare {
    fn stage(&mut self, name: &'static str, board: &Board, ledger: &Ledger, event: StageEvent) -> Flow {
        if !matches!(event, StageEvent::End(_)) || name != self.upto {
            return Flow::Go;
        }
        let mut seen = HashSet::new();
        let soft: Vec<ItemId> = ledger.laid.iter().chain(&ledger.fanout).chain(&ledger.pairs).copied().filter(|id| board.item(*id).is_some() && seen.insert(*id)).collect();
        self.got = Some((board.clone(), soft));
        Flow::Stop
    }
}

/// With `--prepare STAGE`, the board taken through the router to the end of STAGE and the
/// copper the router holds as its own; without it, nothing is done.
pub(crate) fn prepared(cli: &Cli, board: &mut Board, board_path: &str) -> Option<Vec<ItemId>> {
    let upto = cli.flag("--prepare")?;
    let a = cli.router_args(board_path, &[]);
    let mut p = Prepare { upto: upto.clone(), got: None };
    route_with(cli, &a, board_path, &mut p);
    let stages: Vec<&str> = ultraroute::STAGES.iter().map(|e| e.stage.name()).collect();
    let (b, soft) = p.got.unwrap_or_else(|| panic!("--prepare {upto}: the run did not reach it (stages: {})", stages.join(", ")));
    *board = b;
    Some(soft)
}

/// What an activity says and where on the board it looks.
type Focused = (String, Option<grid::Focus>);

/// What a film has seen so far, for its captions and its last frame.
#[derive(Default)]
struct Seen {
    count: usize,
    second: usize,
    open0: usize,
    init: Vec<u32>,
    prev: Vec<u32>,
    paths: Vec<Value>,
    heat: Value,
    tried: usize,
    routed: usize,
    kept: usize,
    base_fp: Vec<Point>,
}

/// A film of one stage, recorded as the router runs it.
struct StageFilm<'c> {
    cli: &'c Cli,
    feature: String,
    stage: &'static str,
    board_path: String,
    out: PathBuf,
    pours_block: bool,
    fb: Option<FilmBuilder>,
    /// The board as the stage got it: negotiation's trial routes are drawn over it.
    base: Option<Board>,
    seen: Seen,
    /// The last activity the router told, and where it looked.
    latest: Arc<Mutex<Option<Focused>>>,
    gave_up: Arc<Mutex<Vec<Focused>>>,
    written: bool,
}

fn centre(b: BBox) -> Point {
    Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2)
}

impl StageFilm<'_> {
    fn active(&mut self) -> Option<&mut FilmBuilder> {
        self.fb.as_mut()
    }

    /// The window and the default title and subtitle of the film, from the board the stage gets.
    fn frame_of(&self, board: &Board) -> (BBox, bool, String, String) {
        let fine = || morph::fine_pitch(board, 500_000);
        let around = |parts: Vec<usize>, by: i64| parts.iter().filter_map(|&f| pad_box_of(board, f)).reduce(BBox::union).map(|b| b.grow(by));
        let (window, cropped) = match self.feature.as_str() {
            "prefanout" => (around(fine(), 2_500_000), true),
            "shove" => (around(fine(), 5_000_000), true),
            "rip-up" => {
                let open = open_airlines(board, self.pours_block);
                (open.iter().map(|a| BBox::of([a.from, a.to])).reduce(BBox::union).map(|b| b.grow(4_000_000)), true)
            }
            "tune" => {
                let nets: HashSet<u32> = grid::diff_pairs(board).iter().flat_map(|p| [p.p, p.n]).collect();
                let b = board.items().filter(|(_, it)| nets.contains(&it.net) && matches!(it.kind, Kind::Track | Kind::Via | Kind::Pad { .. })).map(|(_, it)| it.shape.bbox()).reduce(BBox::union);
                (b.map(|b| b.grow(1_500_000)), true)
            }
            _ => (None, false),
        };
        let (window, cropped) = match self.cli.crop_override() {
            Some(c) => (c, true),
            None => (window.unwrap_or_else(|| board_bounds(board)), cropped && window.is_some()),
        };
        let (title, subtitle) = match self.feature.as_str() {
            "prefanout" => ("Pre-fanout: escape fine-pitch pins to vias", "pad by pad before any routing"),
            "pairs" => ("Differential pairs routed coupled", "one centreline per run between pin pairs, a track laid each side"),
            "negotiate" => ("PathFinder negotiated congestion", "congestion heat map and trial routes, round by round"),
            "route" => ("Grid route_open finishing after negotiation", "one connection per frame on the obstacle grid"),
            "rip-up" => ("Rip-up and reroute", "route through soft copper at a price, rip conflicts, reroute victims"),
            "shove" => ("Plough escape", "trapped pads ploughed out, the copper in the way moved or ripped and rerouted"),
            "nudge" => ("Nudge parts", "step unlocked parts aside with flowing copper and reroute"),
            "tighten" => ("Pull routed tracks tight", "replace stair-stepped grid chains with longest legal straight tracks"),
            "cleanup" => ("Cleanup: drop duplicates and prune dangling copper", "remove doubled tracks and dead-end antennas without breaking connectivity"),
            _ => ("Length matching: bumps on the shorter net", "the shorter net of each pair given bumps"),
        };
        (window, cropped, self.cli.flag("--title").unwrap_or_else(|| title.into()), self.cli.flag("--subtitle").unwrap_or_else(|| subtitle.into()))
    }

    fn start(&mut self, board: &Board) {
        let (window, cropped, title, subtitle) = self.frame_of(board);
        let max_frames = self.cli.max_frames(if self.feature == "nudge" { 140 } else { 200 });
        let spec = FilmSpec { feature: &self.feature, out_path: &self.out, board_path: &self.board_path, cropped, max_frames, title, subtitle };
        let mut fb = FilmBuilder::new(spec, board, window, self.pours_block);
        let init = fb.snapshot_copper(board);
        let open0 = open_airlines(board, self.pours_block).len();
        let caption = match self.feature.as_str() {
            "prefanout" => format!("Bare pads: {open0} open connections; fine-pitch parts to escape"),
            "pairs" => {
                let names: Vec<String> = grid::diff_pairs(board).iter().map(|p| format!("{}/{}", board.net_name(p.p), board.net_name(p.n))).collect();
                format!("{} pairs found ({}); {open0} open in all", names.len(), names.join(", "))
            }
            "negotiate" => format!("Unrouted board: {open0} open connections ready for PathFinder negotiation"),
            "route" => format!("Start of route_open: {open0} open connections to try shortest-first"),
            "tighten" => format!("Before pull_tight: {} track segments", board.items().filter(|(_, it)| it.kind == Kind::Track && !it.locked).count()),
            "tune" => format!("Settled copper, pairs to match in length; {open0} open"),
            other => format!("Before {other}: {open0} open connections"),
        };
        fb.push_frame(board, caption, FrameOverlays { initial: true, important: true, stats: vec![("open".into(), json!(open0))], ..Default::default() });
        self.seen = Seen { open0, prev: init.clone(), init, base_fp: board.footprints.iter().map(|f| f.at).collect(), ..Default::default() };
        if self.feature == "negotiate" {
            self.base = Some(board.clone());
        }
        self.fb = Some(fb);
    }

    fn end(&mut self, board: &Board, detail: &Value) {
        let pours_block = self.pours_block;
        let feature = self.feature.clone();
        let Some(mut fb) = self.fb.take() else { return };
        let s = &self.seen;
        if feature == "route" {
            for (text, focus) in self.gave_up.lock().unwrap().iter().take(16) {
                let f = match focus {
                    Some(grid::Focus::Connection { net, from, to }) => json!({"net": *net, "from": xy(*from), "to": xy(*to), "failed": true}),
                    _ => Value::Null,
                };
                let open_now = open_airlines(board, pours_block).len();
                fb.push_frame(board, format!("Gave up on {text}; {open_now} open"), FrameOverlays { important: true, focus: f, stats: vec![("open".into(), json!(open_now))], ..Default::default() });
            }
        }
        let end = fb.snapshot_copper(board);
        let (gone, came) = diff_pools(&s.init, &end);
        let open_end = open_airlines(board, pours_block).len();
        let open0 = s.open0;
        let d = |k: &str| detail[k].clone();
        let (caption, mut overlays) = match feature.as_str() {
            "prefanout" => (
                format!("Pre-fanout done: {} of {} pads escaped ({} failed), {} inward bridges, {} vias; {open0} -> {open_end} open", d("escaped"), d("candidates"), d("failed"), d("inward"), d("vias")),
                FrameOverlays { laid: came, stats: vec![("escaped".into(), d("escaped")), ("failed".into(), d("failed")), ("vias".into(), d("vias")), ("open".into(), json!(open_end))], ..Default::default() },
            ),
            "pairs" => (
                format!("Pairs done: {} of {} runs coupled, {:.1} mm along their centrelines; the legs to the pads are left to the router ({open_end} open)", d("coupled"), d("runs"), d("coupled_mm").as_f64().unwrap_or(0.0)),
                FrameOverlays { laid: came, paths: s.paths.clone(), stats: vec![("pairs".into(), d("pairs")), ("coupled".into(), json!(format!("{} / {}", d("coupled"), d("runs")))), ("open".into(), json!(open_end))], ..Default::default() },
            ),
            "negotiate" => (
                format!("Committed {} of {} conflict-free routes ({} vias) after {} rounds; {open_end} left open", d("committed"), d("connections"), d("vias"), d("rounds")),
                FrameOverlays { laid: came, heat: s.heat.clone(), stats: vec![("rounds".into(), d("rounds")), ("committed".into(), d("committed")), ("vias".into(), d("vias")), ("open left".into(), json!(open_end))], ..Default::default() },
            ),
            "route" => (
                format!("route_open complete: {} routed ({} vias), {} failed; {open0} -> {open_end} open", d("routed"), d("vias"), d("failed")),
                FrameOverlays { laid: came, stats: vec![("routed".into(), d("routed")), ("vias".into(), d("vias")), ("failed".into(), d("failed")), ("open".into(), json!(open_end))], ..Default::default() },
            ),
            "rip-up" => (
                format!("Rip-up finished: {} attempts, {} kept ({} pieces ripped); open {open0} -> {open_end}", d("tries"), d("kept"), d("ripped")),
                FrameOverlays { ripped: gone, laid: came, stats: vec![("attempts".into(), d("tries")), ("kept".into(), d("kept")), ("no better".into(), d("no_better")), ("open".into(), json!(open_end))], ..Default::default() },
            ),
            "shove" => (
                format!("Plough complete: {} tried, {} routed out, {} kept in {} passes; open {open0} -> {open_end}", s.tried, s.routed, s.kept, d("passes")),
                FrameOverlays { moved_from: gone, laid: came, stats: vec![("tried".into(), json!(s.tried)), ("routed out".into(), json!(s.routed)), ("kept".into(), json!(s.kept)), ("open".into(), json!(open_end))], ..Default::default() },
            ),
            "nudge" => {
                let moved_parts: Vec<Value> = board
                    .footprints
                    .iter()
                    .enumerate()
                    .filter_map(|(i, f)| {
                        let dd = Point::new(f.at.x - fb.initial_fp_at[i].x, f.at.y - fb.initial_fp_at[i].y);
                        (dd != Point::new(0, 0)).then(|| json!({"fp": i, "ref": f.reference, "from": [0.0, 0.0], "to": [mm(dd.x as f64), mm(dd.y as f64)], "status": "kept"}))
                    })
                    .collect();
                (
                    format!("Nudge finished: {} tried, {} illegal, {} kept ({} guided); open {open0} -> {open_end}", d("tried"), d("illegal"), d("kept"), d("guided")),
                    FrameOverlays { moved_from: gone, laid: came, moved_parts, stats: vec![("tried".into(), d("tried")), ("illegal".into(), d("illegal")), ("kept".into(), d("kept")), ("guided".into(), d("guided")), ("open".into(), json!(open_end))], ..Default::default() },
                )
            }
            "tighten" => {
                let (b, a) = (d("mm_before").as_f64().unwrap_or(0.0), d("mm_after").as_f64().unwrap_or(0.0));
                (
                    format!("Tighten finished: {} -> {} segments, {b:.1} -> {a:.1} mm ({} chains shortened)", d("segments_before"), d("segments_after"), s.count),
                    FrameOverlays { moved_from: gone, laid: came, stats: vec![("segs before".into(), d("segments_before")), ("segs after".into(), d("segments_after")), ("mm before".into(), json!((b * 10.0).round() / 10.0)), ("mm after".into(), json!((a * 10.0).round() / 10.0))], ..Default::default() },
                )
            }
            "cleanup" => (
                format!("Cleanup complete: dropped {} duplicates, pruned {} dangling pieces; open {open0} -> {open_end}", d("doubled"), d("pruned")),
                FrameOverlays { ripped: gone, stats: vec![("doubled".into(), d("doubled")), ("pruned".into(), d("pruned")), ("open before".into(), json!(open0)), ("open after".into(), json!(open_end))], ..Default::default() },
            ),
            _ => {
                let skews = detail["skews"].as_array().cloned().unwrap_or_default();
                let said: Vec<String> = skews.iter().map(|k| format!("{} {:.2} -> {:.2} mm", k["pair"].as_str().unwrap_or(""), k["before_mm"].as_f64().unwrap_or(0.0), k["after_mm"].as_f64().unwrap_or(0.0))).collect();
                let mut stats: Vec<(String, Value)> = skews.iter().map(|k| (k["pair"].as_str().unwrap_or("").to_string(), json!(format!("{:.2} mm", k["after_mm"].as_f64().unwrap_or(0.0))))).collect();
                stats.extend([("bumps".to_string(), d("bumps")), ("open".to_string(), json!(open_end))]);
                (format!("Tuned with {} bumps: {}; {open0} -> {open_end} open", d("bumps"), said.join(", ")), FrameOverlays { ripped: gone, laid: came, stats, ..Default::default() })
            }
        };
        overlays.important = true;
        overlays.summary = true;
        fb.push_frame(board, caption, overlays);
        fb.write(&self.out);
        self.written = true;
    }

    /// A frame of the copper laid and ripped since the last one.
    fn since_last(&mut self, board: &Board) -> (Vec<u32>, Vec<u32>) {
        let fb = self.fb.as_mut().expect("filming");
        let cur = fb.snapshot_copper(board);
        let prev = std::mem::replace(&mut self.seen.prev, cur.clone());
        diff_pools(&prev, &cur)
    }
}

impl Watch for StageFilm<'_> {
    fn stage(&mut self, name: &'static str, board: &Board, _ledger: &Ledger, event: StageEvent) -> Flow {
        if name != self.stage {
            return Flow::Go;
        }
        match event {
            StageEvent::Start => {
                self.start(board);
                Flow::Go
            }
            StageEvent::End(stats) => {
                self.end(board, &stats.detail);
                Flow::Stop
            }
        }
    }

    fn log(&self) -> Option<grid::Log> {
        let (latest, gave_up) = (self.latest.clone(), self.gave_up.clone());
        Some(grid::Log(Arc::new(move |act| match act.phase {
            "prefanout" | "routing" => *latest.lock().unwrap() = Some((act.text, act.focus)),
            "gave up" => {
                let focus = latest.lock().unwrap().as_ref().and_then(|(_, f)| f.clone());
                gave_up.lock().unwrap().push((act.text, focus));
            }
            _ => {}
        })))
    }

    fn step(&mut self, _board: &Board) {}

    fn frame(&mut self, board: &Board) {
        if self.fb.is_none() || !matches!(self.feature.as_str(), "prefanout" | "route") {
            return;
        }
        let pours_block = self.pours_block;
        let (gone, laid) = self.since_last(board);
        self.seen.count += 1;
        let latest = self.latest.lock().unwrap().clone();
        let open_now = open_airlines(board, pours_block).len();
        let n = self.seen.count;
        let fb = self.fb.as_mut().unwrap();
        if self.feature == "route" {
            let desc = latest.as_ref().map_or_else(|| "connection".to_string(), |(t, _)| t.clone());
            let focus = match latest.and_then(|(_, f)| f) {
                Some(grid::Focus::Connection { net, from, to }) => json!({"net": net, "from": xy(from), "to": xy(to)}),
                _ => Value::Null,
            };
            let pieces = laid.len();
            fb.push_frame(board, format!("Connection #{n}: routed {desc} ({pieces} pieces); {open_now} open"), FrameOverlays { laid, focus, stats: vec![("routed".into(), json!(n)), ("open".into(), json!(open_now))], ..Default::default() });
            return;
        }
        let (text, focus) = latest.unwrap_or_default();
        let bridged = text.ends_with("preparing escapes");
        if !bridged {
            self.seen.second += 1;
        }
        let caption = if bridged {
            format!("{}: pads bridged inward to their pours and neighbours ({} pieces)", text.trim_end_matches(": preparing escapes"), laid.len())
        } else {
            format!("Escape #{}: {text} ({} pieces)", self.seen.second, laid.len())
        };
        let vias = laid.iter().filter(|&&i| fb.pool[i as usize]["via"].as_bool() == Some(true)).count();
        let focus = standalone::focus_json(board, focus.as_ref(), false);
        let stats = vec![("escaped".into(), json!(self.seen.second)), ("vias now".into(), json!(vias)), ("open".into(), json!(open_now))];
        fb.push_frame(board, format!("{caption}; {open_now} open"), FrameOverlays { important: bridged, ripped: gone, laid, focus, stats, ..Default::default() });
    }

    fn wants_rounds(&self) -> bool {
        self.feature == "negotiate"
    }

    fn round(&mut self, rv: grid::RoundView) {
        let (Some(fb), Some(base)) = (self.fb.as_mut(), self.base.as_ref()) else { return };
        let window = fb.window;
        let heat = standalone::build_heatmap(&rv, window);
        self.seen.heat = heat.clone();
        let paths: Vec<Value> = rv
            .trials
            .iter()
            .filter(|(_, pts, _)| pts.iter().any(|(_, p)| window.contains(*p)))
            .map(|(net, pts, collides)| json!({"net": net, "collides": collides, "points": pts.iter().map(|(l, p)| json!([l, xy(*p)[0], xy(*p)[1]])).collect::<Vec<_>>()}))
            .collect();
        let hot = heat["cells"].as_array().map_or(0, Vec::len);
        fb.push_frame(
            base,
            format!("Round {}: {} of {} trial routes collide (present price {:.1}, {hot} congested tiles)", rv.round + 1, rv.colliding, rv.trials.len(), rv.present),
            FrameOverlays {
                important: true,
                paths,
                heat,
                stats: vec![("round".into(), json!(rv.round + 1)), ("colliding".into(), json!(rv.colliding)), ("trials".into(), json!(rv.trials.len())), ("present".into(), json!((rv.present * 10.0).round() / 10.0))],
                ..Default::default()
            },
        );
    }

    fn rip(&mut self, now: &Board, att: &grid::RipAttempt) {
        let Some(fb) = self.active() else { return };
        let window = fb.window;
        let ripped = fb.intern_items(now, &att.ripped);
        let laid: Vec<u32> = att.laid.iter().filter_map(|&id| now.item(id).filter(|it| it.shape.bbox().overlaps(window.grow(500_000))).map(|it| fb.intern(now, id, it))).collect();
        let paths = if att.path.is_empty() {
            Vec::new()
        } else {
            vec![json!({"net": att.airline.net, "collides": att.outcome == "hard conflict", "points": att.path.iter().map(|(l, p)| json!([l, xy(*p)[0], xy(*p)[1]])).collect::<Vec<_>>()})]
        };
        self.seen.count += 1;
        let n = self.seen.count;
        let fb = self.fb.as_mut().unwrap();
        fb.push_frame(
            now,
            format!(
                "Attempt #{n} {} ({} to {}): ripped {}, laid {} -> {} (open {} -> {})",
                now.net_name(att.airline.net),
                grid::item_name(now, att.airline.from_item),
                grid::item_name(now, att.airline.to_item),
                att.ripped.len(),
                att.laid.len(),
                att.outcome,
                att.open_before,
                att.open_after
            ),
            FrameOverlays {
                important: true,
                ripped,
                laid,
                paths,
                focus: json!({"net": att.airline.net, "from": xy(att.airline.from), "to": xy(att.airline.to), "failed": att.outcome != "kept"}),
                stats: vec![("attempt".into(), json!(n)), ("ripped".into(), json!(att.ripped.len())), ("open".into(), json!(att.open_after)), ("outcome".into(), json!(att.outcome))],
                ..Default::default()
            },
        );
    }

    fn nudge(&mut self, now: &Board, ev: &grid::NudgeEvent) {
        let Some(fb) = self.fb.as_mut() else { return };
        match *ev {
            grid::NudgeEvent::Blame { airline, blamed } => {
                if blamed.is_empty() {
                    return;
                }
                let arrows: Vec<Value> = blamed.iter().map(|b| {
                    let c = part_centre(now, b.footprint);
                    json!([mm(c.x as f64), mm(c.y as f64), mm(b.dx), mm(b.dy)])
                }).collect();
                let names: Vec<String> = blamed.iter().map(|b| format!("{} ({:+.2},{:+.2} mm)", now.footprints[b.footprint].reference, mm(b.dx), mm(b.dy))).collect();
                fb.push_frame(
                    now,
                    format!("Blame for open {}: asks to move {}", now.net_name(airline.net), names.join(", ")),
                    FrameOverlays {
                        important: true,
                        arrows,
                        focus: json!({"net": airline.net, "from": xy(airline.from), "to": xy(airline.to)}),
                        stats: vec![("net".into(), json!(now.net_name(airline.net))), ("blamed parts".into(), json!(blamed.len()))],
                        ..Default::default()
                    },
                );
            }
            grid::NudgeEvent::Step { airline, footprint, requested, moved, guided, ripped, open_before, open_after, outcome } => {
                self.seen.count += 1;
                let n = self.seen.count;
                let cur = fb.snapshot_copper(now);
                let (gone, came) = diff_pools(&self.seen.prev, &cur);
                let base = self.seen.base_fp[footprint];
                let d0 = Point::new(base.x - fb.initial_fp_at[footprint].x, base.y - fb.initial_fp_at[footprint].y);
                let shown_to = if moved == Point::new(0, 0) {
                    Point::new(d0.x + requested.x, d0.y + requested.y)
                } else {
                    Point::new(now.footprints[footprint].at.x - fb.initial_fp_at[footprint].x, now.footprints[footprint].at.y - fb.initial_fp_at[footprint].y)
                };
                let c = part_centre(now, footprint);
                let ref_name = now.footprints[footprint].reference.clone();
                let kind = if guided { "blame" } else { "blind" };
                fb.push_frame(
                    now,
                    format!("Trial #{n} ({kind}): {ref_name} ({:+.2}, {:+.2}) mm for {} -> {outcome} (open {open_before} -> {open_after})", mm(requested.x as f64), mm(requested.y as f64), now.net_name(airline.net)),
                    FrameOverlays {
                        important: outcome == "kept" || guided,
                        moved_from: gone,
                        moved_to: came,
                        moved_parts: vec![json!({"fp": footprint, "ref": ref_name, "from": [mm(d0.x as f64), mm(d0.y as f64)], "to": [mm(shown_to.x as f64), mm(shown_to.y as f64)], "status": outcome})],
                        arrows: vec![json!([mm(c.x as f64), mm(c.y as f64), mm(requested.x as f64), mm(requested.y as f64)])],
                        focus: json!({"net": airline.net, "from": xy(airline.from), "to": xy(airline.to)}),
                        stats: vec![("trial".into(), json!(n)), ("part".into(), json!(ref_name)), ("ripped".into(), json!(ripped)), ("outcome".into(), json!(outcome))],
                        ..Default::default()
                    },
                );
                if outcome == "kept" {
                    self.seen.prev = cur;
                    self.seen.base_fp = now.footprints.iter().map(|f| f.at).collect();
                }
            }
        }
    }

    fn pair_run(&mut self, now: &Board, run: &grid::PairRun) {
        if self.fb.is_none() {
            return;
        }
        let pours_block = self.pours_block;
        let (_, laid) = self.since_last(now);
        self.seen.count += 1;
        let n = self.seen.count;
        let name = format!("{}/{}", now.net_name(run.pair.p), now.net_name(run.pair.n));
        let length: f64 = run.centreline.windows(2).map(|w| w[0].distance(w[1])).sum();
        if let Some(layer) = run.layer {
            self.seen.paths.push(json!({"net": run.pair.p, "kind": "centreline", "points": run.centreline.iter().map(|p| json!([layer, xy(*p)[0], xy(*p)[1]])).collect::<Vec<_>>()}));
        }
        let caption = match run.layer {
            Some(layer) => format!("Run #{n} {name}: coupled on {} for {:.2} mm, {:.2} mm pitch ({} pieces)", now.layers[layer], length * 1e-6, run.pair.pitch() as f64 * 1e-6, run.laid.len()),
            None => format!("Run #{n} {name}: no coupled run lays cleanly between these pin pairs; left to the router"),
        };
        let pair_nets: HashSet<u32> = grid::diff_pairs(now).iter().flat_map(|p| [p.p, p.n]).collect();
        let pair_open = open_airlines(now, pours_block).iter().filter(|a| pair_nets.contains(&a.net)).count();
        let paths = self.seen.paths.clone();
        self.fb.as_mut().unwrap().push_frame(
            now,
            format!("{caption}; {pair_open} pair connections open"),
            FrameOverlays {
                important: true,
                laid,
                paths,
                focus: json!({"net": run.pair.p, "from": xy(run.from), "to": xy(run.to), "failed": run.layer.is_none()}),
                stats: vec![("pair".into(), json!(name)), ("run mm".into(), json!((length * 1e-4).round() / 100.0)), ("pair open".into(), json!(pair_open))],
                ..Default::default()
            },
        );
    }

    fn shove(&mut self, now: &Board, event: &grid::ShoveEvent) {
        if self.fb.is_none() {
            return;
        }
        let pours_block = self.pours_block;
        let at_of = |t: &grid::ShoveTry| now.item(t.pad).map_or(t.to, |it| centre(it.shape.bbox()));
        match *event {
            grid::ShoveEvent::Start(t) => {
                let fb = self.fb.as_mut().unwrap();
                self.seen.prev = fb.snapshot_copper(now);
                let (pad, net) = (grid::item_name(now, t.pad), now.net_name(t.net).to_string());
                fb.push_frame(
                    now,
                    format!("Trapped {pad} ({net}): searching for cheapest path out ({} open)", t.open_before),
                    FrameOverlays {
                        focus: json!({"pad": t.pad, "net": t.net, "at": xy(at_of(t)), "to": xy(t.to)}),
                        stats: vec![("pad".into(), json!(pad)), ("net".into(), json!(net)), ("open".into(), json!(t.open_before))],
                        ..Default::default()
                    },
                );
            }
            grid::ShoveEvent::Round(t, r) => {
                let (gone, came) = self.since_last(now);
                let fb = self.fb.as_mut().unwrap();
                let ripped = fb.intern_items(now, &r.ripped_items);
                let ripped_set: HashSet<u32> = ripped.iter().copied().collect();
                let moved_from: Vec<u32> = gone.into_iter().filter(|id| !ripped_set.contains(id)).collect();
                let (laid, moved_to): (Vec<u32>, Vec<u32>) = came.into_iter().partition(|&i| fb.pool[i as usize]["net"].as_u64() == Some(u64::from(r.net)));
                let paths = if r.path.is_empty() { Vec::new() } else { vec![json!({"net": r.net, "collides": false, "points": r.path.iter().map(|(l, p)| json!([l, xy(*p)[0], xy(*p)[1]])).collect::<Vec<_>>()})] };
                let open_now = open_airlines(now, pours_block).len();
                fb.push_frame(
                    now,
                    format!(
                        "{} ({}) round {}: {} moved, {} crossed, {} ripped -> {}; {open_now} open",
                        grid::item_name(now, t.pad),
                        now.net_name(r.net),
                        t.rounds.len(),
                        r.moved,
                        r.crossed,
                        r.ripped,
                        if r.routed { "routed out" } else { "still blocked" }
                    ),
                    FrameOverlays {
                        important: r.routed,
                        moved_from,
                        moved_to,
                        ripped,
                        laid,
                        paths,
                        focus: json!({"pad": t.pad, "net": t.net, "at": xy(at_of(t)), "to": xy(t.to)}),
                        stats: vec![("round".into(), json!(t.rounds.len())), ("moved".into(), json!(r.moved)), ("ripped".into(), json!(r.ripped + r.crossed)), ("open".into(), json!(open_now))],
                        ..Default::default()
                    },
                );
            }
            grid::ShoveEvent::Done(t) => {
                let (gone, came) = self.since_last(now);
                self.seen.tried += 1;
                self.seen.routed += usize::from(t.routed);
                self.seen.kept += usize::from(t.kept);
                let verdict = if t.kept { "kept" } else { "undone" };
                let pad = grid::item_name(now, t.pad);
                self.fb.as_mut().unwrap().push_frame(
                    now,
                    format!("{pad} ({}) repair done: open {} -> {} ({verdict})", now.net_name(t.net), t.open_before, t.open_after),
                    FrameOverlays {
                        important: true,
                        ripped: gone,
                        laid: came,
                        stats: vec![("pad".into(), json!(pad)), ("open before".into(), json!(t.open_before)), ("open after".into(), json!(t.open_after)), ("verdict".into(), json!(verdict))],
                        ..Default::default()
                    },
                );
            }
        }
    }

    fn tight(&mut self, now: &Board, net: u32, old: &[Item], new: &[ItemId]) {
        let Some(fb) = self.fb.as_mut() else { return };
        self.seen.count += 1;
        let window = fb.window;
        let moved_from = fb.intern_items(now, old);
        let laid: Vec<u32> = new.iter().filter_map(|&id| now.item(id).filter(|it| it.shape.bbox().overlaps(window.grow(500_000))).map(|it| fb.intern(now, id, it))).collect();
        let name = now.net_name(net);
        fb.push_frame(
            now,
            format!("Chain #{} ({name}): pulled {} grid segments into {} straight track(s)", self.seen.count, old.len(), new.len()),
            FrameOverlays {
                moved_from,
                laid,
                stats: vec![("chain".into(), json!(self.seen.count)), ("net".into(), json!(name)), ("before segs".into(), json!(old.len())), ("after segs".into(), json!(new.len()))],
                ..Default::default()
            },
        );
    }

    fn cleaned(&mut self, now: &Board, item: &Item, how: Cleaned) {
        let Some(fb) = self.fb.as_mut() else { return };
        let ripped = fb.intern_items(now, std::slice::from_ref(item));
        let at = centre(item.shape.bbox());
        let s = &mut self.seen;
        let caption = match how {
            Cleaned::Duplicate => {
                s.count += 1;
                format!("Dropped duplicate track #{} on {}", s.count, now.net_name(item.net))
            }
            Cleaned::Dangling => {
                s.second += 1;
                let what = if matches!(item.kind, Kind::Via) { "single-layer via" } else { "dangling stub" };
                format!("Pruned {what} #{} on {}", s.second, now.net_name(item.net))
            }
        };
        fb.push_frame(
            now,
            caption,
            FrameOverlays {
                important: how == Cleaned::Duplicate,
                ripped,
                focus: json!({"net": item.net, "at": xy(at)}),
                stats: vec![("doubled".into(), json!(s.count)), ("pruned".into(), json!(s.second)), ("open".into(), json!(s.open0))],
                ..Default::default()
            },
        );
    }

    fn bump(&mut self, now: &Board, bump: &grid::TuneBump) {
        let pours_block = self.pours_block;
        let Some(fb) = self.fb.as_mut() else { return };
        self.seen.count += 1;
        let ripped = fb.intern_items(now, std::slice::from_ref(bump.replaced));
        let laid: Vec<u32> = bump.laid.iter().filter_map(|&id| now.item(id).map(|it| fb.intern(now, id, it))).collect();
        let at = centre(bump.replaced.shape.bbox());
        let name = format!("{}/{}", now.net_name(bump.pair.p), now.net_name(bump.pair.n));
        fb.push_frame(
            now,
            format!("{name} bump #{} on {}: skew {:.3} -> {:.3} mm (+{:.3} mm of track)", self.seen.count, now.net_name(bump.net), bump.before * 1e-6, bump.after * 1e-6, (bump.before - bump.after) * 1e-6),
            FrameOverlays {
                important: true,
                ripped,
                laid,
                focus: json!({"net": bump.net, "at": xy(at)}),
                stats: vec![("pair".into(), json!(name)), ("bump".into(), json!(self.seen.count)), ("skew mm".into(), json!((bump.after * 1e-3).round() / 1000.0)), ("open".into(), json!(open_airlines(now, pours_block).len()))],
                ..Default::default()
            },
        );
    }
}

fn film_stage(feature: &str, stage: &'static str, cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out) = (pos[0].clone(), PathBuf::from(&pos[1]));
    let extra: &[&str] = if feature == "nudge" && !cli.router.iter().any(|f| f == "--nudge") { &["--nudge"] } else { &[] };
    let a = cli.router_args(&board_path, extra);
    let mut film = StageFilm {
        cli,
        feature: feature.to_string(),
        stage,
        board_path: board_path.clone(),
        out,
        pours_block: a.pours_block,
        fb: None,
        base: None,
        seen: Seen::default(),
        latest: Arc::default(),
        gave_up: Arc::default(),
        written: false,
    };
    route_with(cli, &a, &board_path, &mut film);
    if !film.written {
        eprintln!("feature_film {feature}: the run never reached {stage} with the router flags {:?}", cli.router);
        std::process::exit(1);
    }
}

fn main() {
    let mut raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let sub = raw.remove(0);
    let (args, router) = match raw.iter().position(|a| a == "--") {
        Some(i) => (raw[..i].to_vec(), raw[i + 1..].to_vec()),
        None => (raw, Vec::new()),
    };
    let cli = Cli { args, router };
    match sub.as_str() {
        "flow" => standalone::run_flow(&cli),
        "fill" => standalone::run_fill(&cli),
        "index" => {
            let pos = cli.positional();
            assert!(!pos.is_empty(), "usage: feature_film index FILMS_DIR");
            write_films_index(Path::new(&pos[0]));
        }
        other => match stage_of(other) {
            Some(stage) => film_stage(other, stage, &cli),
            None => {
                eprintln!("unknown feature `{other}`\n{USAGE}");
                std::process::exit(2);
            }
        },
    }
}
