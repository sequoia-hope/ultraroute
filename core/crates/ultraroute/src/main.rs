//! `ultraroute route in.kicad_pcb -o out.kicad_pcb`: routes a KiCad board on the MIT core.
//!
//! The project's `.kicad_pro` beside the input supplies the rules. Pours are left as they are
//! for KiCad to refill. With `--live -` the board streams to standard output as the studio's
//! newline-delimited JSON frames, and a line `stop` on standard input stops the run.

use std::collections::HashSet;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use board::{connectivity, connectivity_without_pours, Board, Item, ItemId, Kind, Point, Shape};
use kicad_pcb::{apply, mm, Edit};
use serde_json::{json, Value};

const USAGE: &str = "usage: ultraroute route IN.kicad_pcb -o OUT.kicad_pcb [--strip] [--negotiate] [--gpu] \
[--timeout SECONDS] [--place REF=X,Y]... [--lock REF,...] [--pours-block] [--no-route] [--threads N] \
[--live PATH|-] [--live-fps N]";

struct Args {
    input: String,
    output: String,
    strip: bool,
    negotiate: bool,
    gpu: bool,
    pours_block: bool,
    timeout: Option<f64>,
    place: Vec<(String, f64, f64)>,
    live: Option<String>,
    fps: f64,
    no_route: bool,
    threads: Option<usize>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    if args.first().map(String::as_str) != Some("route") {
        return Err(USAGE.to_string());
    }
    let mut a = Args {
        input: String::new(),
        output: String::new(),
        strip: false,
        negotiate: false,
        gpu: false,
        pours_block: false,
        timeout: None,
        place: Vec::new(),
        live: None,
        fps: 30.0,
        no_route: false,
        threads: None,
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
            "--negotiate" => a.negotiate = true,
            "--gpu" => a.gpu = true,
            "--pours-block" => a.pours_block = true,
            "--no-route" => a.no_route = true,
            "--threads" => a.threads = Some(value()?.parse().map_err(|e| format!("--threads: {e}"))?),
            "--timeout" => a.timeout = Some(value()?.parse().map_err(|e| format!("--timeout: {e}"))?),
            "--live" => a.live = Some(value()?),
            "--live-fps" => a.fps = value()?.parse().map_err(|e| format!("--live-fps: {e}"))?,
            "--lock" => {
                value()?;
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
struct Live {
    out: Box<dyn Write>,
    sent: HashSet<ItemId>,
    seq: u64,
    started: Instant,
    last: Option<Instant>,
    interval: Duration,
    pours_join: bool,
}

impl Live {
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
    fn trials(&mut self, round: usize, trials: &[grid::Trial]) {
        if self.last.is_some_and(|t| t.elapsed() < self.interval) {
            return;
        }
        self.last = Some(Instant::now());
        let mut runs: Vec<Value> = Vec::new();
        for (net, corners) in trials {
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
                    runs.push(json!({"net": net, "layer": layer, "p": flat(&points)}));
                }
            }
        }
        self.seq += 1;
        let v = json!({
            "t": "frame", "seq": self.seq, "ms": self.started.elapsed().as_millis() as u64,
            "add": [], "del": [], "parts": [], "probes": [],
            "events": [format!("negotiation round {}: {} trial routes", round + 1, trials.len())],
            "trial": runs,
        });
        self.write(&v);
    }

    fn frame(&mut self, board: &Board, force: bool, events: Vec<Value>) {
        if !force && self.last.is_some_and(|t| t.elapsed() < self.interval) {
            return;
        }
        self.last = Some(Instant::now());
        let now: HashSet<ItemId> = board.items().map(|(id, _)| id).collect();
        let mut add: Vec<Value> = board.items().filter(|(id, _)| !self.sent.contains(id)).map(|(id, it)| item_json(board, id, it)).collect();
        let del: Vec<ItemId> = self.sent.difference(&now).copied().collect();
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
            "add": add, "del": del, "parts": parts, "probes": [], "events": events,
            "open": air.len(), "air": air, "trial": [],
        });
        self.write(&v);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a = match parse(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let (mut board, text) = match Board::load(a.input.as_ref()) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    for w in &board.rules.warnings {
        eprintln!("warning: {w}");
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let mut live = a.live.as_ref().map(|path| {
        let out: Box<dyn Write> = if path == "-" {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                for line in std::io::stdin().lines() {
                    match line {
                        Ok(l) if l.trim() == "stop" => cancel.store(true, Ordering::Relaxed),
                        Ok(_) => {}
                        Err(_) => return,
                    }
                }
            });
            Box::new(std::io::stdout())
        } else {
            Box::new(std::io::BufWriter::new(std::fs::File::create(path).expect("create the live file")))
        };
        Live { out, sent: HashSet::new(), seq: 0, started: Instant::now(), last: None, interval: Duration::from_secs_f64(1.0 / a.fps.max(1.0)), pours_join: a.pours_block }
    });

    let mut removed = Vec::new();
    let mut replace = Vec::new();
    for (name, x, y) in &a.place {
        let Some(fp) = board.footprints.iter().position(|f| &f.reference == name) else {
            eprintln!("warning: --place {name}: no such part on the board");
            continue;
        };
        let target = Point::new((x * 1e6).round() as i64, (-y * 1e6).round() as i64);
        let at = board.footprints[fp].at;
        let ripped = board.move_footprint(fp, Point::new(target.x - at.x, target.y - at.y));
        removed.extend(ripped.iter().filter_map(|it| it.span));
        let f = &board.footprints[fp];
        if let Some(span) = f.at_span {
            let angle = if f.rotation_deg == 0.0 { String::new() } else { format!(" {}", f.rotation_deg) };
            replace.push((span, format!("(at {} {}{angle})", mm(f.at.x as f64 * 1e-9), mm(f.at.y as f64 * 1e-9))));
        }
    }
    if a.strip {
        let loose: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked).map(|(id, _)| id).collect();
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
        let _ = std::fs::write(&a.output, apply(&text, &Edit { remove: removed, replace, ..Edit::default() }));
        return;
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
    let live = std::cell::RefCell::new(live);
    let mut watch = |b: &Board| {
        if let Some(l) = live.borrow_mut().as_mut() {
            l.frame(b, false, Vec::new());
        }
    };
    if a.negotiate {
        // Streaming live, connections reroute one after another so their trials show as they
        // change; otherwise every core takes a share of each round.
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get() / 2).max(1);
        let threads = a.threads.unwrap_or(if a.live.is_some() { 1 } else { cores });
        let n = grid::route_negotiated(
            &mut board,
            solver.as_mut(),
            &grid::NegotiationOptions {
                max_seconds: a.timeout.map(|s| s / 2.0),
                threads,
                pours_block: a.pours_block,
                cancel: Some(cancel.clone()),
                ..Default::default()
            },
            &mut watch,
            &mut |round, trials| {
                if let Some(l) = live.borrow_mut().as_mut() {
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
        max_seconds: a.timeout.map(|s| (s - started.elapsed().as_secs_f64()).max(0.0)),
        pours_block: a.pours_block,
        cancel: Some(cancel.clone()),
        ..Default::default()
    };
    let stats = grid::route_open(&mut board, solver.as_mut(), &options, &mut watch);
    laid.extend(stats.laid.iter().copied());
    let open = if a.pours_block { connectivity(&board) } else { connectivity_without_pours(&board) }.airlines.len();
    eprintln!("routed {} more, {} failed, {} open, {:.1} s", stats.routed, stats.failed, open, started.elapsed().as_secs_f64());

    let (tracks, vias) = grid::edits(&board, &laid);
    if let Err(e) = std::fs::write(&a.output, apply(&text, &Edit { remove: removed, replace, tracks, vias })) {
        eprintln!("{}: {e}", a.output);
        std::process::exit(1);
    }
    let mut live = live.into_inner();
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
}
