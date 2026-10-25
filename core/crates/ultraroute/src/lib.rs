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

use std::collections::HashMap;
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

pub const USAGE: &str = "usage: ultraroute route IN.kicad_pcb -o OUT.kicad_pcb [--strip] [--strip-all] [--negotiate] [--rip-up] [--nudge] [--tighten] [--gpu] \
[--timeout SECONDS] [--cell MM] [--push X,Y,RADIUS,OUTWARD[,DX,DY]] [--push-steps N] [--escape REF,...|auto] [--escape-zone MM] [--place REF=X,Y]... [--lock REF,...] [--pours-block] [--no-route] [--threads N] \
[--live PATH|-] [--live-fps N] [--blame]";

pub struct Args {
    pub input: String,
    pub output: String,
    pub strip: bool,
    pub strip_all: bool,
    pub negotiate: bool,
    pub rip_up: bool,
    pub tighten: bool,
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
    /// Grid cell for every stage, in nanometres.
    pub cell: Option<Nm>,
    /// A push field applied after routing, in millimetres: centre, radius, outward flow and an
    /// optional flow along a direction.
    pub push: Option<[f64; 6]>,
    pub push_steps: usize,
    /// Parts whose escapes are combed into lanes after routing, or "auto" for fine-pitch ones.
    pub escape: Vec<String>,
    pub escape_zone: Option<f64>,
    pub blame: bool,
}

pub fn parse(args: &[String]) -> Result<Args, String> {
    if args.first().map(String::as_str) != Some("route") {
        return Err(USAGE.to_string());
    }
    let mut a = Args {
        input: String::new(),
        output: String::new(),
        strip: false,
        strip_all: false,
        negotiate: false,
        rip_up: false,
        tighten: false,
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
        cell: None,
        push: None,
        push_steps: 20,
        escape: Vec::new(),
        escape_zone: None,
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
            "--negotiate" => a.negotiate = true,
            "--rip-up" => a.rip_up = true,
            "--tighten" => a.tighten = true,
            "--gpu" => a.gpu = true,
            "--pours-block" => a.pours_block = true,
            "--no-route" => a.no_route = true,
            "--threads" => a.threads = Some(value()?.parse().map_err(|e| format!("--threads: {e}"))?),
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
            "--lock" => a.lock.extend(value()?.split(',').filter(|r| !r.is_empty()).map(str::to_string)),
            "--nudge" => a.nudge = true,
            "--push" => {
                let v = value()?;
                let n: Vec<f64> = v.split(',').map(|s| s.trim().parse::<f64>()).collect::<Result<_, _>>().map_err(|e| format!("--push {v}: {e}"))?;
                if n.len() != 4 && n.len() != 6 {
                    return Err(format!("--push {v}: give X,Y,RADIUS,OUTWARD or X,Y,RADIUS,OUTWARD,DX,DY in mm"));
                }
                a.push = Some([n[0], n[1], n[2], n[3], n.get(4).copied().unwrap_or(0.0), n.get(5).copied().unwrap_or(0.0)]);
            }
            "--escape" => a.escape.extend(value()?.split(',').filter(|r| !r.is_empty()).map(str::to_string)),
            "--escape-zone" => a.escape_zone = Some(value()?.parse().map_err(|e| format!("--escape-zone: {e}"))?),
            "--push-steps" => a.push_steps = value()?.parse().map_err(|e| format!("--push-steps: {e}"))?,
            "--blame" => a.blame = true,
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
}

impl Live {
    /// Frames written to `out`, at most `fps` a second.
    pub fn new(out: Box<dyn Write + Send>, fps: f64, pours_join: bool) -> Live {
        Live { out, activity: Vec::new(), focus: Value::Null, sent: HashMap::new(), seq: 0, started: Instant::now(), last: None, interval: Duration::from_secs_f64(1.0 / fps.max(1.0)), pours_join }
    }

    /// Takes the router's activity; when the board has not changed for a frame, sends it alone.
    fn note(&mut self, act: grid::Activity) {
        self.activity.push(json!(format!("{}: {}", act.phase, act.text)));
        match act.focus {
            Some(grid::Focus::Connection { net, from, to }) => {
                self.focus = json!({"phase": act.phase, "net": net, "a": mm_xy(from), "b": mm_xy(to)});
            }
            Some(grid::Focus::Part(f)) => self.focus = json!({"phase": act.phase, "part": f + 1}),
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
        let v = json!({
            "t": "frame", "seq": self.seq, "ms": self.started.elapsed().as_millis() as u64,
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
        let v = json!({
            "t": "frame", "seq": self.seq, "ms": self.started.elapsed().as_millis() as u64,
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
        let del: Vec<ItemId> = self.sent.keys().filter(|id| !now.contains_key(id)).copied().collect();
        if self.seq == 0 {
            let polys: Vec<Vec<f64>> = board.outline.iter().map(|p| flat(p)).collect();
            add.push(json!({"id": u32::MAX, "k": "outline", "polys": polys}));
        }
        self.sent = now;
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
            "t": "frame", "seq": self.seq, "ms": self.started.elapsed().as_millis() as u64,
            "add": add, "del": del, "parts": parts, "probes": [],
            "events": events.into_iter().chain(std::mem::take(&mut self.activity)).collect::<Vec<_>>(), "focus": self.focus,
            "open": air.len(), "air": air, "trial": [], "trial_vias": [],
        });
        self.write(&v);
    }
}

/// Every part that moved, by hand or by the nudger, as the `(at …)` it now has.
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

/// Routes `board`, read from `text`, as `a` asks, and returns the routed board's text. `live`
/// receives the run as frames; setting `cancel` stops it at the next connection.
pub fn run(a: &Args, mut board: Board, text: &str, mut live: Option<Live>, cancel: Arc<AtomicBool>) -> String {
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
    if let Some(l) = live.as_mut() {
        l.header(&board);
    }

    if a.no_route {
        let open = if a.pours_block { connectivity(&board) } else { connectivity_without_pours(&board) }.airlines.len();
        if let Some(l) = live.as_mut() {
            l.write(&json!({"t": "done", "open": open, "passes": 0, "stopped": null}));
        }
        if a.blame {
            print_blame(&mut board, a.pours_block);
        }
        let replace = moved_parts(&board, &original_at);
        return apply(text, &Edit { remove: removed, replace, ..Edit::default() });
    }
    let started = Instant::now();
    let mut solver: Box<dyn grid::FieldSolver> = Box::new(grid::CpuSolver);
    #[cfg(feature = "gpu")]
    if a.gpu {
        match grid::GpuSolver::new() {
            Some(gpu) => {
                eprintln!("field solver on {}", gpu.adapter_name);
                solver = Box::new(gpu);
            }
            None => eprintln!("no GPU; solving on the CPU"),
        }
    }
    let mut laid = Vec::new();
    let streaming = live.is_some();
    let live = Arc::new(std::sync::Mutex::new(live));
    let shared = live.clone();
    let log = streaming.then(|| {
        grid::Log(Arc::new(move |act| {
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
    say("start", format!("{} connections to make", connectivity_without_pours(&board).airlines.len()));
    if a.negotiate {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get() / 2).max(1);
        let threads = a.threads.unwrap_or(cores);
        let n = grid::route_negotiated(
            &mut board,
            solver.as_mut(),
            &grid::NegotiationOptions {
                max_seconds: a.timeout.map(|s| s / 2.0),
                threads,
                cell: a.cell.unwrap_or(grid::NegotiationOptions::default().cell),
                pours_block: a.pours_block,
                cancel: Some(cancel.clone()),
                log: log.clone(),
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
        max_seconds: a.timeout.map(|s| (s - started.elapsed().as_secs_f64()).max(0.0)),
        pours_block: a.pours_block,
        cancel: Some(cancel.clone()),
        log: log.clone(),
        ..Default::default()
    };
    say("finishing", "routing what negotiation left, one connection at a time".to_string());
    let stats = grid::route_open(&mut board, solver.as_mut(), &options, &mut watch);
    laid.extend(stats.laid.iter().copied());
    if a.rip_up {
        let r = grid::rip_up(
            &mut board,
            solver.as_mut(),
            &grid::RipOptions {
                cell: a.cell.unwrap_or(grid::RipOptions::default().cell),
                max_seconds: a.timeout.map(|s| (s - started.elapsed().as_secs_f64()).max(0.0)),
                pours_block: a.pours_block,
                cancel: Some(cancel.clone()),
                log: log.clone(),
                ..Default::default()
            },
            &mut laid,
            &mut watch,
        );
        eprintln!("rip-up: {} tries, {} kept, {} ripped, {:.1} s", r.attempts, r.kept, r.ripped, started.elapsed().as_secs_f64());
    }
    if let Some([x, y, radius, outward, dx, dy]) = a.push {
        // Same coordinates as --place: the board's y runs the other way.
        let field = grid::Field {
            center: Point::new((x * 1e6).round() as i64, (-y * 1e6).round() as i64),
            outward: outward * 1e6,
            vector: (dx * 1e6, -dy * 1e6),
            radius: radius * 1e6,
            decay: 2.0,
        };
        say("pushing", format!("flowing everything free to move away from ({x}, {y}) mm"));
        let locked = board.footprints.iter().enumerate().filter(|(_, f)| f.locked).map(|(i, _)| i).collect();
        let p = grid::push(
            &mut board,
            &field,
            &grid::PushOptions { steps: a.push_steps, locked, pours_block: a.pours_block, log: log.clone(), ..Default::default() },
            &mut laid,
            &mut |b: &Board| {
                if let Some(l) = live.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    l.frame(b, true, Vec::new());
                }
            },
        );
        removed.extend(p.removed_spans.iter().copied());
        let again = grid::route_open(&mut board, solver.as_mut(), &grid::Options { max_seconds: a.timeout.map(|s| (s - started.elapsed().as_secs_f64()).max(0.0)), ..options.clone() }, &mut watch);
        laid.extend(again.laid.iter().copied());
        eprintln!(
            "push: {} parts moved, {} held back, {} copper moves, {} ripped, {} rerouted, {:.1} s",
            p.moved.len(),
            p.held_back,
            p.flowed,
            p.ripped,
            again.routed,
            started.elapsed().as_secs_f64()
        );
    }
    if !a.escape.is_empty() {
        let parts: Vec<usize> = if a.escape.iter().any(|r| r == "auto") {
            morph::fine_pitch(&board, 500_000)
        } else {
            a.escape.iter().filter_map(|r| board.footprints.iter().position(|f| &f.reference == r).or_else(|| { eprintln!("warning: --escape {r}: no such part"); None })).collect()
        };
        let opts = morph::EscapeOptions { zone: a.escape_zone.map_or(morph::EscapeOptions::default().zone, |mm| (mm * 1e6).round() as Nm), ..Default::default() };
        let before = if a.pours_block { connectivity(&board) } else { connectivity_without_pours(&board) }.airlines.len();
        for &f in &parts {
            let e = morph::comb(&mut board, f, &opts, &mut laid);
            removed.extend(e.removed_spans.iter().copied());
            let (ripped, spans) = grid::rip_broken(&mut board, &mut laid, a.pours_block);
            removed.extend(spans);
            say("escaping", format!("{}: {} lanes, {} combed, {} pushed out, {ripped} ripped", board.footprints[f].reference, e.lanes, e.combed, e.pushed_out));
            eprintln!("escape {}: {} lanes, {} combed onto them, {} pushed out of the zone, {ripped} ripped", board.footprints[f].reference, e.lanes, e.combed, e.pushed_out);
            watch(&board);
        }
        let reroute = grid::Options { max_seconds: a.timeout.map(|s| (s - started.elapsed().as_secs_f64()).max(0.0)), ..options.clone() };
        let again = grid::route_open(&mut board, solver.as_mut(), &reroute, &mut watch);
        laid.extend(again.laid.iter().copied());
        if a.rip_up {
            grid::rip_up(
                &mut board,
                solver.as_mut(),
                &grid::RipOptions {
                    cell: a.cell.unwrap_or(grid::RipOptions::default().cell),
                    max_seconds: a.timeout.map(|s| (s - started.elapsed().as_secs_f64()).max(0.0)),
                    pours_block: a.pours_block,
                    cancel: Some(cancel.clone()),
                    log: log.clone(),
                    ..Default::default()
                },
                &mut laid,
                &mut watch,
            );
        }
        let after = if a.pours_block { connectivity(&board) } else { connectivity_without_pours(&board) }.airlines.len();
        eprintln!("escape: {before} open before combing, {after} after rerouting, {:.1} s", started.elapsed().as_secs_f64());
    }
    if a.nudge {
        let locked = board.footprints.iter().enumerate().filter(|(_, f)| f.locked).map(|(i, _)| i).collect();
        let n = grid::nudge(
            &mut board,
            solver.as_mut(),
            &grid::NudgeOptions {
                cell: a.cell.unwrap_or(grid::NudgeOptions::default().cell),
                max_seconds: a.timeout.map(|s| (s - started.elapsed().as_secs_f64()).max(0.0)),
                locked,
                pours_block: a.pours_block,
                cancel: Some(cancel.clone()),
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
    if a.tighten {
        say("tightening", "straightening the routed copper".to_string());
        let c = grid::pull_tight(&mut board, &mut laid);
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
    let open = if a.pours_block { connectivity(&board) } else { connectivity_without_pours(&board) }.airlines.len();
    eprintln!("routed {} more, {} failed, {} open, {:.1} s", stats.routed, stats.failed, open, started.elapsed().as_secs_f64());
    if a.blame {
        print_blame(&mut board, a.pours_block);
    }

    let replace = moved_parts(&board, &original_at);
    let (tracks, vias) = grid::edits(&board, &laid);
    let routed = apply(text, &Edit { remove: removed, replace, tracks, vias });
    let mut live = live.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(l) = live.as_mut() {
        l.frame(&board, true, Vec::new());
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
