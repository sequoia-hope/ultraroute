//! A diagnostic, on when `ULTRAROUTE_JOINT_TRACE` names a file: after each stage, every track
//! end that touches its net's copper without meeting it squarely, so the stage that makes such
//! ends can be found. An end meets copper squarely when it lies on another track's end or centre
//! line, on a via's centre, or inside a pad or pour; an end that only overlaps copper off those
//! lines is loose, joined by whatever the overlap leaves. The file holds each stage's loose ends
//! and, for drawing them, the copper near any of them at every stage.

use std::collections::{HashMap, HashSet};

use board::{BBox, Board, ItemId, Kind, Point, Shape};
use serde_json::{Value, json};

/// Ends this close are one point, in nanometres.
const SAME: f64 = 10.0;
/// How far round a loose end the copper is kept for drawing, in nanometres.
const WINDOW: i64 = 700_000;

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    net: u32,
    layer: usize,
    at: (i64, i64),
    r: i64,
}

#[derive(Clone)]
struct Loose {
    key: Key,
    /// What the end overlaps: `cap` another track's round end, `side` its edge from outside, `via` a via off its centre, `pad` a pad's edge, `stub` only the track it bends from.
    kind: &'static str,
    /// From the end's centre to the nearest square meeting: the other's end, centre line or
    /// centre, or a pad's edge.
    off: f64,
    neck: Option<f64>,
    other_r: i64,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Copper {
    kind: &'static str,
    net: u32,
    layers: u64,
    pts: Vec<(i64, i64)>,
    r: i64,
}

struct Stage {
    name: String,
    loose: Vec<Loose>,
    dangling: usize,
    ends: usize,
    copper: Vec<Copper>,
}

#[derive(Default)]
pub struct Trace {
    path: Option<String>,
    stages: Vec<Stage>,
}

/// What the steps within stages ([`board::probe::mark`]) made: the loose ends as the last step
/// left them, each with the step that made it, and per step how many it made and undid.
#[derive(Default)]
struct Steps {
    now: HashMap<Key, String>,
    made: HashMap<String, (usize, usize)>,
    /// The copper as the last step left it, and examples of steps that made loose ends: the
    /// copper round the first end each made, before and after.
    was: Vec<Copper>,
    events: Vec<Value>,
}

/// Examples kept of each step making loose ends, and how far round them.
const EVENTS_PER_STEP: usize = 30;
const EVENT_WINDOW: i64 = 450_000;

fn window_of(copper: &[Copper], at: Point) -> Vec<Value> {
    let w = BBox::of([at]).grow(EVENT_WINDOW);
    copper
        .iter()
        .filter(|c| {
            let b = BBox::of(c.pts.iter().map(|&(x, y)| Point::new(x, y))).grow(c.r);
            w.min.x <= b.max.x && b.min.x <= w.max.x && w.min.y <= b.max.y && b.min.y <= w.max.y && c.kind != "z"
        })
        .map(|c| json!([c.kind, c.net, c.layers, c.r, c.pts]))
        .collect()
}

static STEPS: std::sync::Mutex<Option<Steps>> = std::sync::Mutex::new(None);

fn step(label: &str, board: &Board) {
    let (loose, _, _) = loose_ends(board);
    let mut guard = STEPS.lock().unwrap_or_else(|e| e.into_inner());
    let steps = guard.get_or_insert_with(Steps::default);
    let next: HashMap<Key, String> = loose
        .into_iter()
        .map(|l| {
            let by = steps.now.get(&l.key).cloned().unwrap_or_else(|| label.to_string());
            (l.key, by)
        })
        .collect();
    let born: Vec<&Key> = next.keys().filter(|k| !steps.now.contains_key(*k)).collect();
    let made = born.len();
    let now = copper(board);
    let shown = steps.events.iter().filter(|e| e["step"] == label).count();
    if let Some(k) = born.iter().min_by_key(|k| k.at) {
        if shown < EVENTS_PER_STEP {
            let at = Point::new(k.at.0, k.at.1);
            let ends: Vec<Value> = born.iter().filter(|b| Point::new(b.at.0, b.at.1).distance(at) < EVENT_WINDOW as f64).map(|b| json!([b.at.0, b.at.1, b.layer, b.r])).collect();
            steps.events.push(json!({"step": label, "net": k.net, "layer": k.layer, "at": [k.at.0, k.at.1], "ends": ends, "before": window_of(&steps.was, at), "after": window_of(&now, at)}));
        }
    }
    steps.was = now;
    let undone = steps.now.keys().filter(|k| !next.contains_key(*k)).count();
    let e = steps.made.entry(label.to_string()).or_default();
    e.0 += made;
    e.1 += undone;
    steps.now = next;
}

impl Trace {
    pub fn from_env() -> Trace {
        let path = std::env::var("ULTRAROUTE_JOINT_TRACE").ok().filter(|p| !p.is_empty());
        if path.is_some() {
            board::probe::set(Box::new(step));
        }
        Trace { path, stages: Vec::new() }
    }

    pub fn after(&mut self, name: &str, board: &Board) {
        if self.path.is_none() {
            return;
        }
        step(&format!("{name}/end"), board);
        let (loose, dangling, ends) = loose_ends(board);
        self.stages.push(Stage { name: name.to_string(), loose, dangling, ends, copper: copper(board) });
    }

    pub fn write(&self, board: &Board) {
        let Some(path) = &self.path else { return };
        let mut windows: Vec<BBox> = Vec::new();
        let mut seen: HashSet<Key> = HashSet::new();
        for s in &self.stages {
            for l in &s.loose {
                if seen.insert(l.key.clone()) {
                    let p = Point::new(l.key.at.0, l.key.at.1);
                    windows.push(BBox::of([p]).grow(WINDOW));
                }
            }
        }
        let near = |c: &Copper| {
            let b = BBox::of(c.pts.iter().map(|&(x, y)| Point::new(x, y))).grow(c.r);
            windows.iter().any(|w| w.min.x <= b.max.x && b.min.x <= w.max.x && w.min.y <= b.max.y && b.min.y <= w.max.y)
        };
        let mut table: Vec<&Copper> = Vec::new();
        let mut index: HashMap<&Copper, usize> = HashMap::new();
        let guard = STEPS.lock().unwrap_or_else(|e| e.into_inner());
        let by_step: Vec<Value> = guard.as_ref().map(|st| {
            let mut v: Vec<_> = st.made.iter().map(|(k, (m, u))| json!({"step": k, "made": m, "undone": u})).collect();
            v.sort_by_key(|x| x["step"].as_str().unwrap_or("").to_string());
            v
        }).unwrap_or_default();
        let made_by = |k: &Key| guard.as_ref().and_then(|st| st.now.get(k).cloned());
        let finals: Vec<Value> = self.stages.last().map(|s| s.loose.iter().map(|l| json!({"at": [l.key.at.0, l.key.at.1], "layer": l.key.layer, "net": l.key.net, "r": l.key.r, "step": made_by(&l.key)})).collect()).unwrap_or_default();
        let mut stages = Vec::new();
        for s in &self.stages {
            let mut ids = Vec::new();
            for c in s.copper.iter().filter(|c| near(c)) {
                let i = *index.entry(c).or_insert_with(|| {
                    table.push(c);
                    table.len() - 1
                });
                ids.push(i);
            }
            let loose: Vec<Value> = s
                .loose
                .iter()
                .map(|l| json!({"net": l.key.net, "layer": l.key.layer, "at": [l.key.at.0, l.key.at.1], "r": l.key.r, "kind": l.kind, "off": l.off.round(), "neck": l.neck.map(f64::round), "other_r": l.other_r}))
                .collect();
            stages.push(json!({"name": s.name, "ends": s.ends, "dangling": s.dangling, "loose": loose, "copper": ids}));
        }
        let copper: Vec<Value> = table.iter().map(|c| json!([c.kind, c.net, c.layers, c.r, c.pts])).collect();
        let out = json!({"layers": board.layers, "nets": board.nets, "stages": stages, "copper": copper, "steps": by_step, "final_steps": finals, "events": guard.as_ref().map(|st| st.events.clone()).unwrap_or_default()});
        if let Err(e) = std::fs::write(path, out.to_string()) {
            eprintln!("warning: joint trace {path}: {e}");
        }
    }
}

fn copper(board: &Board) -> Vec<Copper> {
    let mut out = Vec::new();
    for (_, it) in board.items() {
        let kind = match it.kind {
            Kind::Track => "t",
            Kind::Via => "v",
            Kind::Pad { .. } => "p",
            Kind::Pour { .. } => "z",
            _ => continue,
        };
        let (pts, r) = match &it.shape {
            Shape::Capsule { a, b, r } => (vec![(a.x, a.y), (b.x, b.y)], *r),
            Shape::Polygon(p) => (p.iter().map(|q| (q.x, q.y)).collect(), 0),
        };
        out.push(Copper { kind, net: it.net, layers: it.layers, pts, r });
    }
    out
}

fn loose_ends(board: &Board) -> (Vec<Loose>, usize, usize) {
    let (mut out, mut dangling, mut ends) = (Vec::new(), 0, 0);
    for (id, item) in board.items() {
        let (Kind::Track, Shape::Capsule { a, b, r }) = (&item.kind, &item.shape) else { continue };
        if item.net == 0 || a == b {
            continue;
        }
        let layer = item.layers.trailing_zeros() as usize;
        for (p, far) in [(*a, *b), (*b, *a)] {
            ends += 1;
            match end_of(board, id, item.net, layer, p, far, *r) {
                End::Square => {}
                End::Dangling => dangling += 1,
                End::Loose(kind, off, neck, other_r) => out.push(Loose { key: Key { net: item.net, layer, at: (p.x, p.y), r: *r }, kind, off, neck, other_r }),
            }
        }
    }
    (out, dangling, ends)
}

enum End {
    Square,
    Dangling,
    Loose(&'static str, f64, Option<f64>, i64),
}

fn end_of(board: &Board, id: ItemId, net: u32, layer: usize, p: Point, far: Point, r: i64) -> End {
    let near = board.near(layer, BBox::of([p]).grow(4 * r.max(1)));
    let mut touching: Vec<(&'static str, f64, Option<f64>, i64, bool)> = Vec::new();
    for o in near.into_iter().filter(|&o| o != id) {
        let Some(c) = board.item(o) else { continue };
        if c.net != net || !c.on(layer) || !c.is_copper() {
            continue;
        }
        match (&c.kind, &c.shape) {
            (Kind::Track, Shape::Capsule { a: q0, b: q1, r: r2 }) => {
                let d = board::point_segment(p, *q0, *q1);
                let to_end = p.distance(*q0).min(p.distance(*q1));
                if to_end <= SAME || board::meets_squarely(p, r as f64, *q0, *q1, *r2 as f64) {
                    return End::Square;
                }
                if d >= (r + r2) as f64 {
                    continue;
                }
                let neck = board::end_neck(p, r as f64, *q0, *q1, *r2 as f64);
                // The other track bends from this one's far end: this end is the tip of a stub.
                let stub = *q0 == far || *q1 == far;
                let kind = if to_end <= d + 0.5 { "cap" } else { "side" };
                touching.push((kind, d, neck, *r2, stub));
            }
            (Kind::Via, Shape::Capsule { a: q, r: r2, .. }) => {
                let d = p.distance(*q);
                if board::meets_squarely(p, r as f64, *q, *q, *r2 as f64) {
                    return End::Square;
                }
                if d < (r + r2) as f64 {
                    touching.push(("via", d, None, *r2, false));
                }
            }
            (_, shape) => {
                if shape.contains(p) {
                    return End::Square;
                }
                let dot = Shape::Capsule { a: p, b: p, r: 0 };
                let gap = shape.gap_within(&dot, r as f64);
                if gap < r as f64 {
                    touching.push(("pad", gap, None, 0, false));
                }
            }
        }
    }
    if touching.is_empty() {
        return End::Dangling;
    }
    if touching.iter().all(|t| t.4) {
        let t = touching[0];
        return End::Loose("stub", t.1, t.2, t.3);
    }
    // The overlap that joins best: the widest neck, else the deepest.
    let t = touching.into_iter().filter(|t| !t.4).max_by(|x, y| x.2.unwrap_or(0.0).total_cmp(&y.2.unwrap_or(0.0)).then(y.1.total_cmp(&x.1))).expect("one not a stub");
    End::Loose(t.0, t.1, t.2, t.3)
}
