//! `feature_film <feature> ...`: runs one router feature on a board and records every step as a
//! scrubbable JSON film for `lab/features.html`.
//!
//! Each film carries the static board geometry in its window (outline, footprints with courtyards,
//! pads and pours), an interned pool of copper shapes (tracks and vias with layers and nets), and
//! a capped sequence of frames. Every frame records incremental `add`/`del` indices into the copper
//! pool so files stay compact, alongside explicit step highlights (`moved_from`, `moved_to`,
//! `ripped`, `laid`), moved parts, open airlines, debug overlays (displacement arrows, paths,
//! corridors, poles, congestion heat maps, zone fills) and a one-line caption.
//!
//! `--prepare STAGE` strips the board bare and takes it through the router's default recipe up to
//! and including STAGE first (see `RECIPE`), so a later stage is filmed on the board it gets.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use board::{
    check_item, check_item_with_pours, connectivity, connectivity_without_pours, Airline, BBox,
    Board, Item, ItemId, Kind, Nm, Point, Shape,
};
use serde_json::{json, Value};

/// What a progress activity says and where on the board it looks.
type Focused = (String, Option<grid::Focus>);

const USAGE: &str = "\
usage:
  feature_film dipole     BOARD.kicad_pcb OUT.json X,Y STRENGTH RADIUS STEPS [--pair X2,Y2[,STRENGTH2]] [--guard] [--rip] [--reroute S]
  feature_film push       BOARD.kicad_pcb OUT.json X,Y,RADIUS,OUTWARD[,DX,DY] [--steps N] [--reroute S]
  feature_film flow       BOARD.kicad_pcb OUT.json REF=DX,DY [--steps N] [--rip] [--reroute S]
  feature_film comb       BOARD.kicad_pcb OUT.json [--part REF|auto] [--zone MM] [--reroute S]
  feature_film clear-lane BOARD.kicad_pcb OUT.json [--part REF|auto] [--zone MM] [--trials N] [--reroute S]
  feature_film shove      BOARD.kicad_pcb OUT.json [--plough warp|linear|rip|gentle|dipole] [--part REF|auto] [--cascade N] [--ties] [--repair S]
  feature_film nudge      BOARD.kicad_pcb OUT.json [--prepare-route] [--guided-only] [--seconds S]
  feature_film negotiate  BOARD.kicad_pcb OUT.json [--strip-all] [--rounds N] [--threads N] [--seed N]
  feature_film route      BOARD.kicad_pcb OUT.json [--strip-all] [--after-negotiate] [--seconds S] [--seed N]
  feature_film rip-up     BOARD.kicad_pcb OUT.json [--prepare-route] [--seconds S]
  feature_film chase      BOARD.kicad_pcb OUT.json [--prepare-route] [--links N] [--seconds S]
  feature_film tighten    BOARD.kicad_pcb OUT.json [--prepare-route]
  feature_film cleanup    BOARD.kicad_pcb OUT.json [--prepare-messy|--prepare-route]
  feature_film topo       BOARD.kicad_pcb OUT.json [--strip-all] [--rip-passes N] [--seconds S]
  feature_film prefanout  BOARD.kicad_pcb OUT.json [--part REF,..|auto]
  feature_film pairs      BOARD.kicad_pcb OUT.json
  feature_film tune       BOARD.kicad_pcb OUT.json
  feature_film fill       BOARD.kicad_pcb OUT.json
  feature_film index      FILMS_DIR
common flags: [--max-frames N] [--title TEXT] [--subtitle TEXT] [--pours-block] [--crop X0,Y0,X1,Y1]
              [--prepare strip|prefanout|pairs|negotiate|finish|rip-up|nudge|tighten|cleanup [--seed N] [--threads N]]";

fn mm(v: f64) -> f64 {
    (v * 1e-6 * 1e4).round() / 1e4
}

fn xy(p: Point) -> [f64; 2] {
    [mm(p.x as f64), mm(p.y as f64)]
}

fn nm_mm(v: f64) -> Nm {
    (v * 1e6).round() as Nm
}

fn shape_json(s: &Shape) -> Value {
    match s {
        Shape::Capsule { a, b, r } => json!({"a": xy(*a), "b": xy(*b), "r": mm(*r as f64)}),
        Shape::Polygon(p) => json!({"poly": p.iter().map(|q| xy(*q)).collect::<Vec<_>>()}),
    }
}

fn layers_of(board: &Board, it: &Item) -> Vec<usize> {
    (0..board.layers.len()).filter(|l| it.on(*l)).collect()
}

fn open_airlines(board: &Board, pours_block: bool) -> Vec<Airline> {
    if pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines
}

fn airline_json(board: &Board, a: &Airline) -> Value {
    json!({
        "net": a.net,
        "name": board.net_name(a.net),
        "from": xy(a.from),
        "to": xy(a.to),
        "items": [a.from_item, a.to_item],
    })
}

fn board_bounds(board: &Board) -> BBox {
    let pts: Vec<Point> = board.outline.iter().flatten().copied().collect();
    if !pts.is_empty() {
        return BBox::of(pts).grow(600_000);
    }
    board
        .items()
        .map(|(_, it)| it.shape.bbox())
        .reduce(BBox::union)
        .unwrap_or_else(|| BBox::of([Point::new(0, 0)]))
        .grow(1_000_000)
}

fn pad_box_of(board: &Board, fp: usize) -> Option<BBox> {
    board
        .items()
        .filter(|(_, it)| matches!(it.kind, Kind::Pad { footprint, .. } if footprint == fp))
        .map(|(_, it)| it.shape.bbox())
        .reduce(BBox::union)
}

fn part_centre(board: &Board, fp: usize) -> Point {
    pad_box_of(board, fp)
        .map(|b| Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2))
        .unwrap_or(board.footprints[fp].at)
}

fn adopt_soft(board: &mut Board) -> Vec<ItemId> {
    board
        .items()
        .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked)
        .map(|(id, _)| id)
        .collect()
}

fn strip_unlocked(board: &mut Board, all: bool) {
    let ids: Vec<ItemId> = board
        .items()
        .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && (all || !it.locked))
        .map(|(id, _)| id)
        .collect();
    for id in ids {
        board.remove(id);
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct PoolKey {
    net: u32,
    via: bool,
    layers: u64,
    coords: Vec<Nm>,
}

impl PoolKey {
    fn of(it: &Item) -> Self {
        let coords = match &it.shape {
            Shape::Capsule { a, b, r } => vec![a.x, a.y, b.x, b.y, *r],
            Shape::Polygon(pts) => pts.iter().flat_map(|p| [p.x, p.y]).collect(),
        };
        PoolKey {
            net: it.net,
            via: matches!(it.kind, Kind::Via),
            layers: it.layers,
            coords,
        }
    }
}

#[derive(Clone, Default)]
struct FrameOverlays {
    important: bool,
    initial: bool,
    summary: bool,
    moved_from: Vec<u32>,
    moved_to: Vec<u32>,
    ripped: Vec<u32>,
    laid: Vec<u32>,
    moved_parts: Vec<Value>,
    arrows: Vec<Value>,
    paths: Vec<Value>,
    corridors: Vec<Value>,
    poles: Vec<Value>,
    heat: Value,
    focus: Value,
    stats: Vec<(String, Value)>,
    /// Zone fills (indices into the film's `fills`) shown, and the one poured this frame.
    fills: Vec<usize>,
    fill_new: Option<usize>,
}

struct RawFrame {
    caption: String,
    copper: Vec<u32>,
    part_offsets: Vec<Value>,
    open: Vec<Value>,
    overlays: FrameOverlays,
}

struct FilmBuilder {
    feature: String,
    name: String,
    title: String,
    subtitle: String,
    board_path: String,
    cropped: bool,
    window: BBox,
    pours_block: bool,
    max_frames: usize,
    layers: Vec<String>,
    nets: Vec<String>,
    outline: Vec<Vec<[f64; 2]>>,
    footprints: Vec<Value>,
    initial_fp_at: Vec<Point>,
    pads: Vec<Value>,
    pours: Vec<Value>,
    pool: Vec<Value>,
    pool_index: HashMap<PoolKey, u32>,
    frames: Vec<RawFrame>,
    started: Instant,
    /// Further top-level fields of the film, such as a fill film's `fills`.
    extra: serde_json::Map<String, Value>,
}

/// What a film is called and where it goes: the part of `FilmBuilder::new`'s inputs that is not the board.
struct FilmSpec<'a> {
    feature: &'a str,
    out_path: &'a Path,
    board_path: &'a str,
    cropped: bool,
    max_frames: usize,
    title: String,
    subtitle: String,
}

impl FilmBuilder {
    fn new(spec: FilmSpec<'_>, board: &Board, window: BBox, pours_block: bool) -> Self {
        let FilmSpec { feature, out_path, board_path, cropped, max_frames, title, subtitle } = spec;
        let name = out_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("film")
            .to_string();
        let near = |it: &Item| it.shape.bbox().overlaps(window.grow(500_000));
        let pads: Vec<Value> = board
            .items()
            .filter(|(_, it)| matches!(it.kind, Kind::Pad { .. }) && near(it))
            .map(|(id, it)| {
                let Kind::Pad { footprint, number, .. } = &it.kind else { unreachable!() };
                json!({
                    "id": id,
                    "fp": footprint,
                    "ref": board.footprints[*footprint].reference,
                    "number": number,
                    "net": it.net,
                    "layers": layers_of(board, it),
                    "shape": shape_json(&it.shape),
                })
            })
            .collect();
        let pours: Vec<Value> = board
            .items()
            .filter_map(|(_, it)| match it.kind {
                Kind::Pour { zone, kept } if near(it) => Some(json!({
                    "zone": zone,
                    "kept": kept,
                    "net": it.net,
                    "layers": layers_of(board, it),
                    "shape": shape_json(&it.shape),
                })),
                _ => None,
            })
            .collect();
        let fp_near: HashSet<usize> = board
            .items()
            .filter_map(|(_, it)| match it.kind {
                Kind::Pad { footprint, .. } if near(it) => Some(footprint),
                _ => None,
            })
            .collect();
        let footprints: Vec<Value> = board
            .footprints
            .iter()
            .enumerate()
            .filter(|(i, f)| {
                fp_near.contains(i)
                    || window.grow(1_500_000).contains(f.at)
                    || f.courtyard.as_ref().is_some_and(|y| Shape::Polygon(y.clone()).bbox().overlaps(window))
            })
            .map(|(i, f)| {
                json!({
                    "fp": i,
                    "ref": f.reference,
                    "at": xy(f.at),
                    "back": f.back,
                    "locked": f.locked,
                    "courtyard": f.courtyard.as_ref().map(|y| y.iter().map(|p| xy(*p)).collect::<Vec<_>>()),
                })
            })
            .collect();
        let outline: Vec<Vec<[f64; 2]>> = board
            .outline
            .iter()
            .map(|poly| poly.iter().map(|p| xy(*p)).collect())
            .collect();
        let initial_fp_at = board.footprints.iter().map(|f| f.at).collect();
        FilmBuilder {
            feature: feature.to_string(),
            name,
            title,
            subtitle,
            board_path: board_path.to_string(),
            cropped,
            window,
            pours_block,
            max_frames: max_frames.max(2),
            layers: board.layers.clone(),
            nets: board.nets.clone(),
            outline,
            footprints,
            initial_fp_at,
            pads,
            pours,
            pool: Vec::new(),
            pool_index: HashMap::new(),
            frames: Vec::new(),
            started: Instant::now(),
            extra: serde_json::Map::new(),
        }
    }

    fn intern(&mut self, board: &Board, id: ItemId, it: &Item) -> u32 {
        let key = PoolKey::of(it);
        if let Some(&idx) = self.pool_index.get(&key) {
            return idx;
        }
        let idx = self.pool.len() as u32;
        self.pool.push(json!({
            "id": if id == usize::MAX { Value::Null } else { json!(id) },
            "net": it.net,
            "via": matches!(it.kind, Kind::Via),
            "layers": layers_of(board, it),
            "shape": shape_json(&it.shape),
        }));
        self.pool_index.insert(key, idx);
        idx
    }

    fn intern_items(&mut self, board: &Board, items: &[Item]) -> Vec<u32> {
        let pad_win = self.window.grow(500_000);
        items
            .iter()
            .filter(|it| matches!(it.kind, Kind::Track | Kind::Via) && it.shape.bbox().overlaps(pad_win))
            .map(|it| self.intern(board, usize::MAX, it))
            .collect()
    }

    fn snapshot_copper(&mut self, board: &Board) -> Vec<u32> {
        let pad_win = self.window.grow(500_000);
        let items: Vec<(ItemId, Item)> = board
            .items()
            .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && it.shape.bbox().overlaps(pad_win))
            .map(|(id, it)| (id, it.clone()))
            .collect();
        let mut out: Vec<u32> = items.iter().map(|(id, it)| self.intern(board, *id, it)).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    fn part_offsets(&self, board: &Board) -> Vec<Value> {
        board
            .footprints
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                let d = Point::new(f.at.x - self.initial_fp_at[i].x, f.at.y - self.initial_fp_at[i].y);
                (d != Point::new(0, 0)).then(|| json!([i, mm(d.x as f64), mm(d.y as f64)]))
            })
            .collect()
    }

    fn push_frame(&mut self, board: &Board, caption: impl Into<String>, overlays: FrameOverlays) {
        let copper = self.snapshot_copper(board);
        let part_offsets = self.part_offsets(board);
        let open_all = open_airlines(board, self.pours_block);
        let open: Vec<Value> = open_all.iter().map(|a| airline_json(board, a)).collect();
        self.frames.push(RawFrame {
            caption: caption.into(),
            copper,
            part_offsets,
            open,
            overlays,
        });
    }

    fn decimate(&mut self) {
        if self.frames.len() <= self.max_frames {
            return;
        }
        let n = self.frames.len();
        let cap = self.max_frames;
        let mut keep: HashSet<usize> = HashSet::from([0, n - 1]);
        for (i, f) in self.frames.iter().enumerate() {
            if f.overlays.important && keep.len() < cap * 3 / 4 {
                keep.insert(i);
            }
        }
        let rem = cap.saturating_sub(keep.len());
        if rem > 0 {
            for k in 0..rem {
                let idx = (k + 1) * (n - 1) / (rem + 1);
                keep.insert(idx);
            }
        }
        // Fill any remaining slots if collisions happened.
        for i in 0..n {
            if keep.len() >= cap {
                break;
            }
            keep.insert(i);
        }
        let mut indices: Vec<usize> = keep.into_iter().collect();
        indices.sort_unstable();
        indices.truncate(cap);
        if *indices.last().unwrap_or(&0) != n - 1 {
            if let Some(last) = indices.last_mut() {
                *last = n - 1;
            }
        }
        let old = std::mem::take(&mut self.frames);
        self.frames = indices.into_iter().map(|idx| old[idx].clone_raw()).collect();
    }

    fn write(mut self, out: &Path) {
        self.decimate();
        let mut prev: HashSet<u32> = HashSet::new();
        let mut json_frames = Vec::with_capacity(self.frames.len());
        for f in &self.frames {
            let now: HashSet<u32> = f.copper.iter().copied().collect();
            let mut add: Vec<u32> = now.difference(&prev).copied().collect();
            let mut del: Vec<u32> = prev.difference(&now).copied().collect();
            add.sort_unstable();
            del.sort_unstable();
            prev = now;
            let mut obj = serde_json::Map::new();
            obj.insert("caption".into(), json!(f.caption));
            obj.insert("add".into(), json!(add));
            obj.insert("del".into(), json!(del));
            obj.insert("open".into(), json!(f.open));
            if f.overlays.initial {
                obj.insert("initial".into(), json!(true));
            }
            if f.overlays.summary {
                obj.insert("summary".into(), json!(true));
            }
            if !f.part_offsets.is_empty() {
                obj.insert("part_offsets".into(), json!(f.part_offsets));
            }
            if !f.overlays.moved_from.is_empty() {
                obj.insert("moved_from".into(), json!(f.overlays.moved_from));
            }
            if !f.overlays.moved_to.is_empty() {
                obj.insert("moved_to".into(), json!(f.overlays.moved_to));
            }
            if !f.overlays.ripped.is_empty() {
                obj.insert("ripped".into(), json!(f.overlays.ripped));
            }
            if !f.overlays.laid.is_empty() {
                obj.insert("laid".into(), json!(f.overlays.laid));
            }
            if !f.overlays.moved_parts.is_empty() {
                obj.insert("moved_parts".into(), json!(f.overlays.moved_parts));
            }
            if !f.overlays.arrows.is_empty() {
                obj.insert("arrows".into(), json!(f.overlays.arrows));
            }
            if !f.overlays.paths.is_empty() {
                obj.insert("paths".into(), json!(f.overlays.paths));
            }
            if !f.overlays.corridors.is_empty() {
                obj.insert("corridors".into(), json!(f.overlays.corridors));
            }
            if !f.overlays.poles.is_empty() {
                obj.insert("poles".into(), json!(f.overlays.poles));
            }
            if !f.overlays.heat.is_null() {
                obj.insert("heat".into(), f.overlays.heat.clone());
            }
            if !f.overlays.focus.is_null() {
                obj.insert("focus".into(), f.overlays.focus.clone());
            }
            if !f.overlays.fills.is_empty() {
                obj.insert("fills".into(), json!(f.overlays.fills));
            }
            if let Some(k) = f.overlays.fill_new {
                obj.insert("fill_new".into(), json!(k));
            }
            if !f.overlays.stats.is_empty() {
                obj.insert(
                    "stats".into(),
                    json!(f.overlays.stats.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>()),
                );
            }
            json_frames.push(Value::Object(obj));
        }

        let secs = (self.started.elapsed().as_secs_f64() * 100.0).round() / 100.0;
        let board_name = Path::new(&self.board_path).file_name().and_then(|s| s.to_str()).unwrap_or(&self.board_path);
        let board_name = match PREPARED.get() {
            Some(how) => format!("{board_name} {how}"),
            None => board_name.to_string(),
        };
        let mut doc = json!({
            "feature": self.feature,
            "name": self.name,
            "title": self.title,
            "subtitle": self.subtitle,
            "board": board_name,
            "cropped": self.cropped,
            "window": [xy(self.window.min), xy(self.window.max)],
            "layers": self.layers,
            "nets": self.nets,
            "outline": self.outline,
            "footprints": self.footprints,
            "pads": self.pads,
            "pours": self.pours,
            "pool": self.pool,
            "frames": json_frames,
            "seconds": secs,
        });
        doc.as_object_mut().unwrap().append(&mut self.extra);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| panic!("{}: {e}", parent.display()));
        }
        let tmp = out.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string(&doc).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", tmp.display()));
        std::fs::rename(&tmp, out).unwrap_or_else(|e| panic!("{}: {e}", out.display()));
        eprintln!("wrote {} ({} frames, {} pool items, {:.2} s)", out.display(), doc["frames"].as_array().unwrap().len(), doc["pool"].as_array().unwrap().len(), secs);

        // If written inside `<films_dir>/<feature>/<name>.json`, refresh `<films_dir>/index.json`.
        if let Some(films_dir) = out.parent().and_then(Path::parent) {
            if films_dir.file_name().and_then(|s| s.to_str()) == Some("films") {
                write_films_index(films_dir);
            }
        }
    }
}

impl RawFrame {
    fn clone_raw(&self) -> Self {
        RawFrame {
            caption: self.caption.clone(),
            copper: self.copper.clone(),
            part_offsets: self.part_offsets.clone(),
            open: self.open.clone(),
            overlays: self.overlays.clone(),
        }
    }
}

fn write_films_index(films_dir: &Path) {
    let order = [
        "dipole", "push", "flow", "comb", "clear-lane", "shove", "nudge", "prefanout", "pairs",
        "negotiate", "route", "rip-up", "chase", "tighten", "cleanup", "tune", "fill", "topo",
    ];
    let rank = |f: &str| order.iter().position(|&x| x == f).unwrap_or(999);
    let mut entries: Vec<Value> = Vec::new();
    let Ok(rd) = std::fs::read_dir(films_dir) else { return };
    for sub in rd.flatten() {
        if !sub.path().is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(sub.path()) else { continue };
        for file in files.flatten() {
            let p = file.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
            let Some(feature) = v["feature"].as_str() else { continue };
            let Some(name) = v["name"].as_str() else { continue };
            let frames = v["frames"].as_array().map_or(0, Vec::len);
            entries.push(json!({
                "feature": feature,
                "name": name,
                "title": v["title"].as_str().unwrap_or(name),
                "subtitle": v["subtitle"].as_str().unwrap_or(""),
                "board": v["board"].as_str().unwrap_or(""),
                "cropped": v["cropped"].as_bool().unwrap_or(false),
                "frames": frames,
                "path": format!("films/{feature}/{name}.json"),
            }));
        }
    }
    entries.sort_by(|a, b| {
        let fa = a["feature"].as_str().unwrap_or("");
        let fb = b["feature"].as_str().unwrap_or("");
        rank(fa)
            .cmp(&rank(fb))
            .then_with(|| fa.cmp(fb))
            .then_with(|| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")))
    });
    let idx = films_dir.join("index.json");
    let tmp = films_dir.join("index.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&entries).unwrap()).expect("write index.json");
    std::fs::rename(&tmp, &idx).expect("rename index.json");
}

fn diff_pools(before: &[u32], after: &[u32]) -> (Vec<u32>, Vec<u32>) {
    let b: HashSet<u32> = before.iter().copied().collect();
    let a: HashSet<u32> = after.iter().copied().collect();
    let mut gone: Vec<u32> = b.difference(&a).copied().collect();
    let mut came: Vec<u32> = a.difference(&b).copied().collect();
    gone.sort_unstable();
    came.sort_unstable();
    (gone, came)
}

fn sample_arrows(window: BBox, grid_n: usize, eval: impl Fn(Point) -> (f64, f64)) -> Vec<Value> {
    let w = (window.max.x - window.min.x).max(1) as f64;
    let h = (window.max.y - window.min.y).max(1) as f64;
    let step = (w.max(h) / grid_n as f64).max(100_000.0);
    let nx = (w / step).ceil() as usize;
    let ny = (h / step).ceil() as usize;
    let mut out = Vec::new();
    for j in 0..ny {
        for i in 0..nx {
            let p = Point::new(
                window.min.x + ((i as f64 + 0.5) * step).round() as Nm,
                window.min.y + ((j as f64 + 0.5) * step).round() as Nm,
            );
            let (dx, dy) = eval(p);
            if dx.hypot(dy) >= 500.0 {
                out.push(json!([mm(p.x as f64), mm(p.y as f64), mm(dx), mm(dy)]));
            }
        }
    }
    out
}

fn count_violations(board: &Board, pours_block: bool) -> usize {
    board
        .items()
        .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked)
        .filter(|(id, _)| {
            let v = if pours_block { check_item_with_pours(board, *id) } else { check_item(board, *id) };
            !v.is_empty()
        })
        .count()
}

struct Cli {
    args: Vec<String>,
}

impl Cli {
    fn flag(&self, name: &str) -> Option<String> {
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

    fn has(&self, name: &str) -> bool {
        self.args.iter().any(|a| a == name)
    }

    fn positional(&self) -> Vec<String> {
        let mut out = Vec::new();
        let takes_val = [
            "--pair", "--reroute", "--steps", "--part", "--zone", "--trials",
            "--plough", "--cascade", "--repair", "--seconds", "--rounds",
            "--threads", "--seed", "--links", "--rip-passes", "--max-frames",
            "--title", "--subtitle", "--crop", "--prepare",
        ];
        let mut i = 0;
        while i < self.args.len() {
            let a = &self.args[i];
            if a.starts_with("--") {
                if !a.contains('=') && takes_val.contains(&a.as_str()) {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            out.push(a.clone());
            i += 1;
        }
        out
    }

    fn crop_override(&self) -> Option<BBox> {
        let s = self.flag("--crop")?;
        let ns: Vec<f64> = s.split(',').map(|x| x.trim().parse().expect("--crop X0,Y0,X1,Y1")).collect();
        assert_eq!(ns.len(), 4, "--crop X0,Y0,X1,Y1");
        Some(BBox::of([
            Point::new(nm_mm(ns[0]), nm_mm(ns[1])),
            Point::new(nm_mm(ns[2]), nm_mm(ns[3])),
        ]))
    }
}

fn parse_xy(s: &str) -> Point {
    let (x, y) = s.split_once(',').unwrap_or_else(|| panic!("expected X,Y in mm, got {s}"));
    Point::new(nm_mm(x.trim().parse().expect("X mm")), nm_mm(y.trim().parse().expect("Y mm")))
}

fn prepare_fresh_route(board: &mut Board, pours_block: bool) -> Vec<ItemId> {
    strip_unlocked(board, true);
    let mut laid = Vec::new();
    let n = grid::route_negotiated(
        board,
        &mut grid::CpuSolver,
        &grid::NegotiationOptions {
            threads: 4,
            max_seconds: Some(25.0),
            pours_block,
            ..Default::default()
        },
        &mut |_| {},
        &mut |_, _| {},
    );
    laid.extend(n.laid);
    let r = grid::route_open(
        board,
        &mut grid::CpuSolver,
        &grid::Options { max_seconds: Some(15.0), pours_block, ..Default::default() },
        &mut |_| {},
    );
    laid.extend(r.laid);
    laid
}

/// The router's default recipe (`ultraroute route --strip-all --prefanout --diff-pairs --negotiate
/// --rip-up --nudge --tighten`), stage by stage in the order it runs them, for `--prepare STAGE`.
/// `finish` is the grid's pass over what negotiation left; `cleanup` drops doubled tracks and
/// prunes dangling copper; length matching comes after it.
const RECIPE: [&str; 9] = ["strip", "prefanout", "pairs", "negotiate", "finish", "rip-up", "nudge", "tighten", "cleanup"];

/// How the board was prepared, for the film's board label.
static PREPARED: OnceLock<String> = OnceLock::new();

/// Strips the board bare and takes it through the recipe up to and including `upto`, with the
/// router's own options, returning the copper the next stage may move or rip as the router holds
/// it then: the escapes join it after rip-up, the pairs' runs before cleanup.
fn prepare_recipe(board: &mut Board, upto: &str, seed: u64, threads: usize, pours_block: bool) -> Vec<ItemId> {
    let last = RECIPE.iter().position(|s| *s == upto).unwrap_or_else(|| panic!("--prepare {upto}: not one of {}", RECIPE.join(", ")));
    let runs = |stage: &str| RECIPE.iter().position(|s| *s == stage).is_some_and(|i| i <= last);
    let started = Instant::now();
    strip_unlocked(board, true);
    let mut solver = grid::CpuSolver;
    let (mut soft, mut fanout, mut pair_laid) = (Vec::new(), Vec::new(), Vec::new());
    if runs("prefanout") {
        let parts = morph::fine_pitch(board, 500_000);
        // A pair whose ends share a layer is left for its coupled run to reach straight.
        let skip_nets = grid::diff_pairs(board).iter().filter(|p| grid::shares_layer(board, p)).flat_map(|p| [p.p, p.n]).collect();
        fanout = grid::prefanout(board, &grid::PrefanoutOptions { parts, skip_nets, pours_block, ..Default::default() }, &mut |_| {}).laid;
    }
    if runs("pairs") {
        pair_laid = grid::route_pairs(board, &grid::PairOptions { pours_block, ..Default::default() }, &mut |_| {}).laid;
    }
    if runs("negotiate") {
        let n = grid::route_negotiated(
            board,
            &mut solver,
            &grid::NegotiationOptions { threads, seed, pours_block, ..Default::default() },
            &mut |_| {},
            &mut |_, _| {},
        );
        soft.extend(n.laid);
    }
    if runs("finish") {
        let r = grid::route_open(board, &mut solver, &grid::Options { seed, pours_block, ..Default::default() }, &mut |_| {});
        soft.extend(r.laid);
        grid::prune_closed_escapes(board, &mut fanout, pours_block);
    }
    if runs("rip-up") {
        grid::rip_up(board, &mut solver, &grid::RipOptions { pours_block, ..Default::default() }, &mut soft, &mut |_| {});
        soft.append(&mut fanout);
    }
    if runs("nudge") {
        let locked = board.footprints.iter().enumerate().filter(|(_, f)| f.locked).map(|(i, _)| i).collect();
        grid::nudge(board, &mut solver, &grid::NudgeOptions { locked, pours_block, ..Default::default() }, &mut soft, &mut |_| {});
    }
    if runs("tighten") {
        grid::pull_tight(board, &mut soft);
        soft.extend(pair_laid.iter().copied());
    }
    if runs("cleanup") {
        grid::drop_duplicates(board, &mut soft);
        grid::prune_dangling(board, &mut soft);
    }
    let mut seen = HashSet::new();
    soft.retain(|id| board.item(*id).is_some() && seen.insert(*id));
    eprintln!(
        "prepared through {upto}: {} open, {} soft pieces, {:.1} s",
        open_airlines(board, pours_block).len(),
        soft.len(),
        started.elapsed().as_secs_f64()
    );
    soft
}

/// With `--prepare STAGE`, the board taken through the recipe and the copper the router holds
/// as its own; without it, nothing is done.
fn prepared(cli: &Cli, board: &mut Board, pours_block: bool) -> Option<Vec<ItemId>> {
    let upto = cli.flag("--prepare")?;
    let seed: u64 = cli.flag("--seed").map_or(0, |s| s.parse().expect("--seed"));
    let threads: usize = cli.flag("--threads").map_or(1, |s| s.parse().expect("--threads"));
    let soft = prepare_recipe(board, &upto, seed, threads, pours_block);
    let last = RECIPE.iter().position(|s| *s == upto).unwrap();
    let _ = PREPARED.set(format!("after {} (seed {seed})", RECIPE[..=last].join(" → ")));
    Some(soft)
}

// --- 1. Dipole -----------------------------------------------------------------------------

fn run_dipole(cli: &Cli) {
    let pos = cli.positional();
    // Accept either `dipole BOARD OUT X,Y STRENGTH RADIUS STEPS` or `dipole X,Y STRENGTH RADIUS STEPS BOARD OUT`.
    let (board_path, out_path, spec) = if pos.len() >= 6 && pos[0].ends_with(".kicad_pcb") {
        (pos[0].clone(), PathBuf::from(&pos[1]), &pos[2..6])
    } else if pos.len() >= 6 && pos[4].ends_with(".kicad_pcb") {
        (pos[4].clone(), PathBuf::from(&pos[5]), &pos[0..4])
    } else {
        panic!("{USAGE}");
    };
    let c1 = parse_xy(&spec[0]);
    let s1_mm: f64 = spec[1].parse().expect("STRENGTH in mm");
    let radius_mm: f64 = spec[2].parse().expect("RADIUS in mm");
    let steps: usize = spec[3].parse::<usize>().expect("STEPS").max(1);
    let pours_block = cli.has("--pours-block");
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));
    let guard = cli.has("--guard");
    let do_rip = cli.has("--rip");
    let reroute_s: Option<f64> = cli.flag("--reroute").map(|s| s.parse().expect("--reroute S"));

    let mut total_poles = vec![morph::Pole {
        center: c1,
        strength: s1_mm * 1e6,
        radius: radius_mm * 1e6,
    }];
    if let Some(pair) = cli.flag("--pair") {
        let parts: Vec<&str> = pair.split(',').collect();
        assert!(parts.len() == 2 || parts.len() == 3, "--pair X2,Y2[,STRENGTH2]");
        let c2 = Point::new(nm_mm(parts[0].parse().unwrap()), nm_mm(parts[1].parse().unwrap()));
        let s2_mm = parts.get(2).map_or(-s1_mm, |s| s.parse().unwrap());
        total_poles.push(morph::Pole { center: c2, strength: s2_mm * 1e6, radius: radius_mm * 1e6 });
    }
    let step_poles: Vec<morph::Pole> = total_poles
        .iter()
        .map(|p| morph::Pole { strength: p.strength / steps as f64, ..*p })
        .collect();

    let (mut board, _) = Board::load(Path::new(&board_path)).unwrap_or_else(|e| panic!("{e}"));
    let mut soft = prepared(cli, &mut board, pours_block).unwrap_or_else(|| adopt_soft(&mut board));
    let margin = ((radius_mm * 2.6).max(5.5) * 1e6).round() as Nm;
    let window = cli.crop_override().unwrap_or_else(|| {
        total_poles.iter().map(|p| BBox::of([p.center])).reduce(BBox::union).unwrap().grow(margin)
    });

    let kind_label = if total_poles.len() > 1 {
        "Dipole pair"
    } else if s1_mm < 0.0 {
        "Attractor"
    } else {
        "Repulsor"
    };
    let default_title = format!("{kind_label} at ({:.1}, {:.1}) mm", mm(c1.x as f64), mm(c1.y as f64));
    let default_sub = format!("strength {:+.2} mm, radius {:.2} mm, {steps} steps{}", s1_mm, radius_mm, if guard { ", guarded" } else { "" });
    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "dipole",
            out_path: &out_path,
            board_path: &board_path,
            cropped: true,
            max_frames,
            title: cli.flag("--title").unwrap_or(default_title),
            subtitle: cli.flag("--subtitle").unwrap_or(default_sub),
        },
        &board,
        window,
        pours_block,
    );

    let poles_json: Vec<Value> = total_poles
        .iter()
        .map(|p| json!({"at": xy(p.center), "strength": mm(p.strength), "radius": mm(p.radius)}))
        .collect();
    // Show total displacement field as arrows so direction and falloff read clearly at every step.
    let arrows = sample_arrows(window, 22, |p| morph::poles_at(&total_poles, p));
    let initial_copper = fb.snapshot_copper(&board);
    let open0 = open_airlines(&board, pours_block).len();

    fb.push_frame(
        &board,
        format!("Initial board: {kind_label} placed at ({:.1}, {:.1}) mm, {open0} open", mm(c1.x as f64), mm(c1.y as f64)),
        FrameOverlays {
            initial: true,
            important: true,
            arrows: arrows.clone(),
            poles: poles_json.clone(),
            stats: vec![
                ("step".into(), json!(format!("0 / {steps}"))),
                ("open".into(), json!(open0)),
                ("strength mm".into(), json!(s1_mm)),
                ("radius mm".into(), json!(radius_mm)),
            ],
            ..Default::default()
        },
    );

    for step in 1..=steps {
        let before_items: Vec<(ItemId, Item)> = if guard {
            board
                .items()
                .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked && it.shape.bbox().overlaps(window.grow(1_000_000)))
                .map(|(id, it)| (id, it.clone()))
                .collect()
        } else {
            Vec::new()
        };
        let first_new = board.items().map(|(id, _)| id).max().map_or(0, |m| m + 1);
        let (moved, _) = morph::shift_poles(&mut board, &mut soft, &step_poles, 150_000.0);
        let mut reverted_nets = 0usize;
        if guard {
            let was: HashMap<ItemId, &Item> = before_items.iter().map(|(id, it)| (*id, it)).collect();
            let mut done: HashSet<u32> = HashSet::new();
            for _ in 0..5 {
                let broken: HashSet<u32> = board
                    .items()
                    .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked && !done.contains(&it.net))
                    .filter(|(id, _)| !(if pours_block { check_item_with_pours(&board, *id) } else { check_item(&board, *id) }).is_empty())
                    .map(|(_, it)| it.net)
                    .collect();
                if broken.is_empty() {
                    break;
                }
                for &net in &broken {
                    let changed: Vec<ItemId> = board
                        .items()
                        .filter(|(_, it)| it.net == net && matches!(it.kind, Kind::Track | Kind::Via) && !it.locked)
                        .filter(|(id, it)| *id >= first_new || was.get(id).is_some_and(|w| w.shape != it.shape))
                        .map(|(id, _)| id)
                        .collect();
                    for id in changed {
                        board.remove(id);
                    }
                    for (id, it) in before_items.iter().filter(|(_, it)| it.net == net) {
                        if board.item(*id).is_none() {
                            board.restore(*id, it.clone());
                        }
                    }
                }
                reverted_nets += broken.len();
                done.extend(broken);
            }
            soft.retain(|id| board.item(*id).is_some());
        }
        let now_copper = fb.snapshot_copper(&board);
        let (gone_from_init, came_from_init) = diff_pools(&initial_copper, &now_copper);
        let clashes = count_violations(&board, pours_block);
        let open_now = open_airlines(&board, pours_block).len();
        fb.push_frame(
            &board,
            format!(
                "Step {step}/{steps}: {moved} pieces shifted, {} cumulative drifted, {clashes} rule clashes, {open_now} open{}",
                came_from_init.len(),
                if reverted_nets > 0 { format!(" ({reverted_nets} nets held back)") } else { String::new() }
            ),
            FrameOverlays {
                important: step == steps,
                moved_from: gone_from_init,
                moved_to: came_from_init,
                arrows: arrows.clone(),
                poles: poles_json.clone(),
                stats: vec![
                    ("step".into(), json!(format!("{step} / {steps}"))),
                    ("shifted now".into(), json!(moved)),
                    ("clashes".into(), json!(clashes)),
                    ("open".into(), json!(open_now)),
                ],
                ..Default::default()
            },
        );
    }

    if !do_rip && reroute_s.is_none() {
        let now_copper = fb.snapshot_copper(&board);
        let (gone_from_init, came_from_init) = diff_pools(&initial_copper, &now_copper);
        let clashes = count_violations(&board, pours_block);
        let open_now = open_airlines(&board, pours_block).len();
        fb.push_frame(
            &board,
            format!(
                "Result after {steps} steps: {} drifted pieces vs pre-dipole ghost ({clashes} rule clashes, {open0} -> {open_now} open)",
                came_from_init.len()
            ),
            FrameOverlays {
                important: true,
                summary: true,
                moved_from: gone_from_init,
                moved_to: came_from_init.clone(),
                poles: poles_json.clone(),
                stats: vec![
                    ("steps".into(), json!(steps)),
                    ("drifted".into(), json!(came_from_init.len())),
                    ("clashes".into(), json!(clashes)),
                    ("open".into(), json!(open_now)),
                ],
                ..Default::default()
            },
        );
    }

    if do_rip {
        let (ripped_items, _) = grid::rip_broken_items(&mut board, &mut soft, pours_block);
        let ripped = fb.intern_items(&board, &ripped_items);
        let open_now = open_airlines(&board, pours_block).len();
        fb.push_frame(
            &board,
            format!("Ripped {} rule-breaking pieces after drift; {open_now} connections open", ripped_items.len()),
            FrameOverlays {
                important: true,
                ripped,
                poles: poles_json.clone(),
                stats: vec![("ripped".into(), json!(ripped_items.len())), ("open".into(), json!(open_now))],
                ..Default::default()
            },
        );
    }
    if let Some(secs) = reroute_s {
        let before = fb.snapshot_copper(&board);
        let r = grid::route_open(
            &mut board,
            &mut grid::CpuSolver,
            &grid::Options { max_seconds: Some(secs), pours_block, ..Default::default() },
            &mut |_| {},
        );
        soft.extend(r.laid);
        let after = fb.snapshot_copper(&board);
        let (_, laid) = diff_pools(&before, &after);
        let open_now = open_airlines(&board, pours_block).len();
        fb.push_frame(
            &board,
            format!("Rerouted {} connections ({} failed); {open_now} open", r.routed, r.failed),
            FrameOverlays {
                important: true,
                summary: true,
                laid,
                poles: poles_json,
                stats: vec![("rerouted".into(), json!(r.routed)), ("open".into(), json!(open_now))],
                ..Default::default()
            },
        );
    }
    fb.write(&out_path);
}

// --- 2. Push -------------------------------------------------------------------------------

fn run_push(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 3, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let ns: Vec<f64> = pos[2].split(',').map(|x| x.trim().parse().expect("X,Y,RADIUS,OUTWARD[,DX,DY]")).collect();
    assert!(ns.len() == 4 || ns.len() == 6, "expected X,Y,RADIUS,OUTWARD[,DX,DY]");
    let (cx, cy, radius, outward) = (ns[0], ns[1], ns[2], ns[3]);
    let (dx, dy) = (ns.get(4).copied().unwrap_or(0.0), ns.get(5).copied().unwrap_or(0.0));
    let steps: usize = cli.flag("--steps").map_or(16, |s| s.parse().expect("--steps"));
    let reroute_s: f64 = cli.flag("--reroute").map_or(15.0, |s| s.parse().expect("--reroute"));
    let pours_block = cli.has("--pours-block");
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    let mut soft = prepared(cli, &mut board, pours_block).unwrap_or_else(|| adopt_soft(&mut board));
    let center = Point::new(nm_mm(cx), nm_mm(cy));
    let field = grid::Field {
        center,
        outward: outward * 1e6,
        vector: (dx * 1e6, dy * 1e6),
        radius: radius * 1e6,
        decay: 2.0,
    };
    let margin = ((radius * 2.6).max(6.5) * 1e6).round() as Nm;
    let window = cli.crop_override().unwrap_or_else(|| BBox::of([center]).grow(margin));
    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "push",
            out_path: &out_path,
            board_path,
            cropped: true,
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| format!("Push field at ({cx:.1}, {cy:.1}) mm")),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| format!("radius {radius:.1} mm, outward {outward:+.2} mm, drift ({dx:+.2}, {dy:+.2}) mm, {steps} steps")),
        },
        &board,
        window,
        pours_block,
    );

    let arrows = sample_arrows(window, 20, |p| field.at(p));
    let pole = vec![json!({"at": [cx, cy], "strength": outward, "radius": radius})];
    let init_copper = fb.snapshot_copper(&board);
    let open0 = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!("Initial board before push at ({cx:.1}, {cy:.1}) mm; {open0} open"),
        FrameOverlays {
            initial: true,
            important: true,
            arrows: arrows.clone(),
            poles: pole.clone(),
            stats: vec![("step".into(), json!(format!("0 / {steps}"))), ("open".into(), json!(open0))],
            ..Default::default()
        },
    );

    let locked: HashSet<usize> = board.footprints.iter().enumerate().filter(|(_, f)| f.locked).map(|(i, _)| i).collect();
    let mut prev_copper = init_copper.clone();
    let mut prev_fp_at: Vec<Point> = board.footprints.iter().map(|f| f.at).collect();
    let p_stats = grid::push_watch(
        &mut board,
        &field,
        &grid::PushOptions { steps, locked, pours_block, log: None, ..Default::default() },
        &mut soft,
        &mut |now, step| {
            let ripped = fb.intern_items(now, step.ripped);
            let ripped_set: HashSet<u32> = ripped.iter().copied().collect();
            let now_copper = fb.snapshot_copper(now);
            let (gone, came) = diff_pools(&prev_copper, &now_copper);
            let moved_from: Vec<u32> = gone.into_iter().filter(|id| !ripped_set.contains(id)).collect();
            let mut moved_parts = Vec::new();
            for (&fp, &by) in step.moves {
                let d0 = Point::new(prev_fp_at[fp].x - fb.initial_fp_at[fp].x, prev_fp_at[fp].y - fb.initial_fp_at[fp].y);
                let d1 = Point::new(now.footprints[fp].at.x - fb.initial_fp_at[fp].x, now.footprints[fp].at.y - fb.initial_fp_at[fp].y);
                if (by.x as f64).hypot(by.y as f64) >= 10_000.0 {
                    moved_parts.push(json!({
                        "fp": fp,
                        "ref": now.footprints[fp].reference,
                        "from": [mm(d0.x as f64), mm(d0.y as f64)],
                        "to": [mm(d1.x as f64), mm(d1.y as f64)],
                        "status": "moved",
                    }));
                }
            }
            let shown_parts = moved_parts.len();
            prev_fp_at = now.footprints.iter().map(|f| f.at).collect();
            prev_copper = now_copper;
            let open_now = open_airlines(now, pours_block).len();
            fb.push_frame(
                now,
                format!(
                    "Push step {}/{steps}: {shown_parts} local parts moved ({} held back), {} copper flowed, {} ripped; {open_now} open",
                    step.step + 1,
                    step.held_back,
                    step.flowed,
                    step.ripped.len()
                ),
                FrameOverlays {
                    moved_from,
                    moved_to: came,
                    ripped,
                    moved_parts,
                    arrows: arrows.clone(),
                    poles: pole.clone(),
                    stats: vec![
                        ("step".into(), json!(format!("{} / {steps}", step.step + 1))),
                        ("parts moved".into(), json!(shown_parts)),
                        ("ripped".into(), json!(step.ripped.len())),
                        ("open".into(), json!(open_now)),
                    ],
                    ..Default::default()
                },
            );
        },
    );

    let moved_parts: Vec<Value> = p_stats
        .moved
        .iter()
        .filter(|&&(_, by)| (by.x as f64).hypot(by.y as f64) >= 50_000.0)
        .map(|&(fp, by)| {
            json!({
                "fp": fp,
                "ref": board.footprints[fp].reference,
                "from": [0.0, 0.0],
                "to": [mm(by.x as f64), mm(by.y as f64)],
                "status": "kept",
            })
        })
        .collect();
    let notable_parts = moved_parts.len();

    if reroute_s > 0.0 {
        let before = fb.snapshot_copper(&board);
        let r = grid::route_open(
            &mut board,
            &mut grid::CpuSolver,
            &grid::Options { max_seconds: Some(reroute_s), pours_block, ..Default::default() },
            &mut |_| {},
        );
        soft.extend(r.laid);
        let after = fb.snapshot_copper(&board);
        let (_, laid) = diff_pools(&before, &after);
        let laid_set: HashSet<u32> = laid.iter().copied().collect();
        let (init_gone, init_came) = diff_pools(&init_copper, &after);
        let moved_to: Vec<u32> = init_came.into_iter().filter(|id| !laid_set.contains(id)).collect();
        let open_now = open_airlines(&board, pours_block).len();
        fb.push_frame(
            &board,
            format!(
                "After rerouting: {notable_parts} parts shifted >=0.05 mm, {} ripped connections rerouted ({} failed); {open0} -> {open_now} open",
                r.routed,
                r.failed
            ),
            FrameOverlays {
                important: true,
                summary: true,
                moved_from: init_gone,
                moved_to,
                laid,
                moved_parts,
                poles: pole,
                stats: vec![
                    ("parts >=0.05mm".into(), json!(notable_parts)),
                    ("total ripped".into(), json!(p_stats.ripped)),
                    ("rerouted".into(), json!(r.routed)),
                    ("open".into(), json!(open_now)),
                ],
                ..Default::default()
            },
        );
    } else {
        let after = fb.snapshot_copper(&board);
        let (init_gone, init_came) = diff_pools(&init_copper, &after);
        let open_now = open_airlines(&board, pours_block).len();
        fb.push_frame(
            &board,
            format!("Push finished: {notable_parts} parts shifted >=0.05 mm, {} pieces ripped; {open0} -> {open_now} open", p_stats.ripped),
            FrameOverlays {
                important: true,
                summary: true,
                moved_from: init_gone,
                moved_to: init_came,
                moved_parts,
                poles: pole,
                stats: vec![
                    ("parts >=0.05mm".into(), json!(notable_parts)),
                    ("total ripped".into(), json!(p_stats.ripped)),
                    ("open".into(), json!(open_now)),
                ],
                ..Default::default()
            },
        );
    }
    fb.write(&out_path);
}

// --- 3. Flow -------------------------------------------------------------------------------

fn run_flow(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 3, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let (ref_name, delta_str) = pos[2].split_once('=').expect("expected REF=DX,DY in mm");
    let delta = parse_xy(delta_str);
    let steps: usize = cli.flag("--steps").map_or(16, |s| s.parse().expect("--steps")).max(1);
    let do_rip = cli.has("--rip");
    let reroute_s: Option<f64> = cli.flag("--reroute").map(|s| s.parse().expect("--reroute"));
    let pours_block = cli.has("--pours-block");
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    let fp = board
        .footprints
        .iter()
        .position(|f| f.reference == ref_name)
        .unwrap_or_else(|| panic!("no footprint {ref_name}"));
    let mut soft = prepared(cli, &mut board, pours_block).unwrap_or_else(|| adopt_soft(&mut board));
    let c0 = part_centre(&board, fp);
    let window = cli.crop_override().unwrap_or_else(|| BBox::of([c0, Point::new(c0.x + delta.x, c0.y + delta.y)]).grow(5_500_000));

    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "flow",
            out_path: &out_path,
            board_path,
            cropped: true,
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| format!("Harmonic copper flow moving {ref_name}")),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| format!("shift ({:+.2}, {:+.2}) mm across {steps} warp steps", mm(delta.x as f64), mm(delta.y as f64))),
        },
        &board,
        window,
        pours_block,
    );

    let full_moves = HashMap::from([(fp, delta)]);
    let full_warp = morph::warp_for(&board, &full_moves);
    let init_arrows = sample_arrows(window, 22, |p| full_warp.displacement(p));
    let init_copper = fb.snapshot_copper(&board);
    let open0 = open_airlines(&board, pours_block).len();

    fb.push_frame(
        &board,
        format!("Initial board: {ref_name} about to move ({:+.2}, {:+.2}) mm; harmonic warp field shown", mm(delta.x as f64), mm(delta.y as f64)),
        FrameOverlays {
            initial: true,
            important: true,
            arrows: init_arrows,
            stats: vec![
                ("part".into(), json!(ref_name)),
                ("step".into(), json!(format!("0 / {steps}"))),
                ("open".into(), json!(open0)),
            ],
            ..Default::default()
        },
    );

    let mut moved_so_far = Point::new(0, 0);
    for step in 1..=steps {
        let target = Point::new(
            (delta.x as f64 * step as f64 / steps as f64).round() as Nm,
            (delta.y as f64 * step as f64 / steps as f64).round() as Nm,
        );
        let step_by = Point::new(target.x - moved_so_far.x, target.y - moved_so_far.y);
        let prev_offset = moved_so_far;
        moved_so_far = target;

        let step_moves = HashMap::from([(fp, step_by)]);
        let warp = morph::warp_for(&board, &step_moves);
        // Scale step arrows by `steps` so the visual vector field stays legible at every frame.
        let arrows = sample_arrows(window, 22, |p| {
            let (ux, uy) = warp.displacement(p);
            (ux * steps as f64, uy * steps as f64)
        });
        let f_stats = morph::flow(&mut board, &step_moves, &mut soft);
        let now_copper = fb.snapshot_copper(&board);
        let (gone_init, came_init) = diff_pools(&init_copper, &now_copper);
        let clashes = count_violations(&board, pours_block);
        let open_now = open_airlines(&board, pours_block).len();

        fb.push_frame(
            &board,
            format!(
                "Step {step}/{steps}: {ref_name} at ({:+.2}, {:+.2}) mm, {} copper pieces flowed, {clashes} rule clashes, {open_now} open",
                mm(target.x as f64),
                mm(target.y as f64),
                f_stats.flowed
            ),
            FrameOverlays {
                important: step == steps,
                moved_from: gone_init,
                moved_to: came_init,
                moved_parts: vec![json!({
                    "fp": fp,
                    "ref": ref_name,
                    "from": if step == steps { [0.0, 0.0] } else { [mm(prev_offset.x as f64), mm(prev_offset.y as f64)] },
                    "to": [mm(target.x as f64), mm(target.y as f64)],
                    "status": "moved",
                })],
                arrows,
                stats: vec![
                    ("step".into(), json!(format!("{step} / {steps}"))),
                    ("flowed".into(), json!(f_stats.flowed)),
                    ("clashes".into(), json!(clashes)),
                    ("open".into(), json!(open_now)),
                ],
                ..Default::default()
            },
        );
    }

    if !do_rip && reroute_s.is_none() {
        let after = fb.snapshot_copper(&board);
        let (init_gone, init_came) = diff_pools(&init_copper, &after);
        let clashes = count_violations(&board, pours_block);
        let open_now = open_airlines(&board, pours_block).len();
        fb.push_frame(
            &board,
            format!(
                "Harmonic flow complete: {ref_name} shifted ({:+.2}, {:+.2}) mm, {} copper pieces stretched ({clashes} clashes, {open0} -> {open_now} open)",
                mm(delta.x as f64),
                mm(delta.y as f64),
                init_came.len()
            ),
            FrameOverlays {
                important: true,
                summary: true,
                moved_from: init_gone,
                moved_to: init_came.clone(),
                moved_parts: vec![json!({
                    "fp": fp,
                    "ref": ref_name,
                    "from": [0.0, 0.0],
                    "to": [mm(delta.x as f64), mm(delta.y as f64)],
                    "status": "kept",
                })],
                stats: vec![
                    ("part".into(), json!(ref_name)),
                    ("flowed".into(), json!(init_came.len())),
                    ("clashes".into(), json!(clashes)),
                    ("open".into(), json!(open_now)),
                ],
                ..Default::default()
            },
        );
    }

    if do_rip {
        let (ripped_items, _) = grid::rip_broken_items(&mut board, &mut soft, pours_block);
        let ripped = fb.intern_items(&board, &ripped_items);
        let open_now = open_airlines(&board, pours_block).len();
        fb.push_frame(
            &board,
            format!("Ripped {} pieces pushed into obstacles; {open_now} open", ripped_items.len()),
            FrameOverlays {
                important: true,
                ripped,
                stats: vec![("ripped".into(), json!(ripped_items.len())), ("open".into(), json!(open_now))],
                ..Default::default()
            },
        );
    }
    if let Some(secs) = reroute_s {
        let before = fb.snapshot_copper(&board);
        let r = grid::route_open(
            &mut board,
            &mut grid::CpuSolver,
            &grid::Options { max_seconds: Some(secs), pours_block, ..Default::default() },
            &mut |_| {},
        );
        soft.extend(r.laid);
        let after = fb.snapshot_copper(&board);
        let (_, laid) = diff_pools(&before, &after);
        let laid_set: HashSet<u32> = laid.iter().copied().collect();
        let (init_gone, init_came) = diff_pools(&init_copper, &after);
        let moved_to: Vec<u32> = init_came.into_iter().filter(|id| !laid_set.contains(id)).collect();
        let flowed_cnt = moved_to.len();
        let open_now = open_airlines(&board, pours_block).len();
        fb.push_frame(
            &board,
            format!("Rerouted {} connections ({} failed); {open0} -> {open_now} open", r.routed, r.failed),
            FrameOverlays {
                important: true,
                summary: true,
                moved_from: init_gone,
                moved_to,
                laid,
                moved_parts: vec![json!({
                    "fp": fp,
                    "ref": ref_name,
                    "from": [0.0, 0.0],
                    "to": [mm(delta.x as f64), mm(delta.y as f64)],
                    "status": "kept",
                })],
                stats: vec![
                    ("flowed".into(), json!(flowed_cnt)),
                    ("rerouted".into(), json!(r.routed)),
                    ("open".into(), json!(open_now)),
                ],
                ..Default::default()
            },
        );
    }
    fb.write(&out_path);
}

// --- 4. Comb -------------------------------------------------------------------------------

fn edge_corridors(board: &Board, fp: usize, opts: &morph::EscapeOptions) -> (Vec<Value>, Vec<Value>) {
    let edges = morph::edges(board, fp);
    let zone = opts.zone as f64;
    let full = zone * opts.full;
    let mut corridors = Vec::new();
    let mut arrows = Vec::new();
    for e in &edges {
        let (t0, t1) = (e.from - zone, e.to + zone);
        let at = |t: f64, d: f64| [mm(e.along.0 * t + e.out.0 * (e.row + d)), mm(e.along.1 * t + e.out.1 * (e.row + d))];
        corridors.push(json!({"poly": [at(t0, 0.0), at(t1, 0.0), at(t1, zone), at(t0, zone)], "kind": "zone"}));
        corridors.push(json!({"poly": [at(t0, 0.0), at(t1, 0.0), at(t1, full), at(t0, full)], "kind": "inner"}));
        for (&net, ts) in &e.lanes {
            for &t in ts {
                corridors.push(json!({"line": [at(t, 0.0), at(t, zone)], "net": net, "kind": "lane"}));
            }
        }
        // Foreign push-out arrows across the zone.
        let n_samples = 7;
        for k in 0..n_samples {
            let t = e.from + (e.to - e.from) * (k as f64 + 0.5) / n_samples as f64;
            let d = zone * 0.45;
            let p = at(t, d);
            let push = zone + opts.clear as f64 - d;
            arrows.push(json!([p[0], p[1], mm(e.out.0 * push), mm(e.out.1 * push)]));
        }
    }
    (corridors, arrows)
}

fn resolve_part(board: &Board, spec: Option<&str>) -> usize {
    match spec {
        None | Some("auto") => *morph::fine_pitch(board, 500_000).first().expect("no fine-pitch part on board"),
        Some(r) => board.footprints.iter().position(|f| f.reference == r).unwrap_or_else(|| panic!("no part {r}")),
    }
}

fn run_comb(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.has("--pours-block");
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));
    let reroute_s: f64 = cli.flag("--reroute").map_or(25.0, |s| s.parse().expect("--reroute"));
    let zone_nm = cli.flag("--zone").map_or(morph::EscapeOptions::default().zone, |s| nm_mm(s.parse().expect("--zone")));
    let opts = morph::EscapeOptions { zone: zone_nm, ..Default::default() };

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    let fp = resolve_part(&board, cli.flag("--part").as_deref());
    let ref_name = board.footprints[fp].reference.clone();
    let mut soft = prepared(cli, &mut board, pours_block).unwrap_or_else(|| adopt_soft(&mut board));
    let pad_box = pad_box_of(&board, fp).expect("part has pads");
    let window = cli.crop_override().unwrap_or_else(|| pad_box.grow(opts.zone + 1_800_000));

    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "comb",
            out_path: &out_path,
            board_path,
            cropped: true,
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| format!("Comb escape lanes on {ref_name}")),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| format!("zone {:.2} mm: comb onto lanes, rip broken copper, reroute open connections", mm(opts.zone as f64))),
        },
        &board,
        window,
        pours_block,
    );

    let (corridors, arrows) = edge_corridors(&board, fp, &opts);
    let open0 = open_airlines(&board, pours_block).len();
    let before_comb = fb.snapshot_copper(&board);
    fb.push_frame(
        &board,
        format!("As routed around {ref_name}: {open0} connections open; fan-out zones and pad lanes shown"),
        FrameOverlays {
            initial: true,
            important: true,
            corridors: corridors.clone(),
            arrows: arrows.clone(),
            stats: vec![("stage".into(), json!("routed")), ("open".into(), json!(open0))],
            ..Default::default()
        },
    );

    let e = morph::comb(&mut board, fp, &opts, &mut soft);
    let after_comb = fb.snapshot_copper(&board);
    let (comb_gone, comb_came) = diff_pools(&before_comb, &after_comb);
    let clashes = count_violations(&board, pours_block);
    let open1 = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!(
            "Combed {ref_name}: {} pieces pulled onto {} lanes, {} foreign pieces pushed out; {clashes} clashes, {open1} open",
            e.combed, e.lanes, e.pushed_out
        ),
        FrameOverlays {
            important: true,
            moved_from: comb_gone,
            moved_to: comb_came,
            corridors: corridors.clone(),
            stats: vec![
                ("stage".into(), json!("combed")),
                ("combed".into(), json!(e.combed)),
                ("pushed out".into(), json!(e.pushed_out)),
                ("clashes".into(), json!(clashes)),
            ],
            ..Default::default()
        },
    );

    let (ripped_items, _) = grid::rip_broken_items(&mut board, &mut soft, pours_block);
    let ripped = fb.intern_items(&board, &ripped_items);
    let open2 = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!("Ripped {} rule-breaking pieces left by combing; open connections jumped {open1} -> {open2}", ripped_items.len()),
        FrameOverlays {
            important: true,
            ripped,
            corridors: corridors.clone(),
            stats: vec![
                ("stage".into(), json!("ripped")),
                ("ripped".into(), json!(ripped_items.len())),
                ("open".into(), json!(open2)),
            ],
            ..Default::default()
        },
    );

    let before_reroute = fb.snapshot_copper(&board);
    let mut prev = before_reroute.clone();
    let last_act: Arc<Mutex<Option<Focused>>> = Arc::new(Mutex::new(None));
    let shared = last_act.clone();
    let log = grid::Log(Arc::new(move |act| {
        if act.phase == "routing" {
            *shared.lock().unwrap() = Some((act.text, act.focus));
        }
    }));
    let mut step_n = 0usize;
    let r = grid::route_open(
        &mut board,
        &mut grid::CpuSolver,
        &grid::Options { max_seconds: Some(reroute_s), pours_block, log: Some(log), ..Default::default() },
        &mut |now| {
            step_n += 1;
            let cur = fb.snapshot_copper(now);
            let (_, laid) = diff_pools(&prev, &cur);
            prev = cur;
            let open_now = open_airlines(now, pours_block).len();
            let info = last_act.lock().unwrap().clone();
            let label = info.as_ref().map_or_else(|| "connection".to_string(), |(t, _)| t.clone());
            let focus = match info.and_then(|(_, f)| f) {
                Some(grid::Focus::Connection { net, from, to }) => json!({"net": net, "from": xy(from), "to": xy(to)}),
                _ => Value::Null,
            };
            fb.push_frame(
                now,
                format!("Reroute #{step_n}: laid {label}; {open_now} open"),
                FrameOverlays {
                    laid,
                    corridors: corridors.clone(),
                    focus,
                    stats: vec![
                        ("stage".into(), json!("rerouting")),
                        ("rerouted".into(), json!(step_n)),
                        ("open".into(), json!(open_now)),
                    ],
                    ..Default::default()
                },
            );
        },
    );
    soft.extend(r.laid);
    let end_copper = fb.snapshot_copper(&board);
    let (_, all_laid) = diff_pools(&before_reroute, &end_copper);
    let open3 = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!(
            "Comb finished on {ref_name}: {} rerouted, {} failed; open {open0} -> {open3} ({})",
            r.routed,
            r.failed,
            if open3 < open0 { "kept" } else { "worse, would undo" }
        ),
        FrameOverlays {
            important: true,
            summary: true,
            laid: all_laid,
            corridors,
            stats: vec![
                ("open before".into(), json!(open0)),
                ("open after".into(), json!(open3)),
                ("rerouted".into(), json!(r.routed)),
                ("failed".into(), json!(r.failed)),
            ],
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

// --- 5. Clear-lane -------------------------------------------------------------------------

fn pad_lane_corridor(board: &Board, fp: usize, pad: ItemId, opts: &morph::EscapeOptions) -> (Vec<Value>, Vec<Value>) {
    let Some(p) = board.item(pad) else { return (Vec::new(), Vec::new()) };
    let bb = p.shape.bbox();
    let c = Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2);
    let edges = morph::edges(board, fp);
    let Some(e) = edges
        .iter()
        .filter(|e| e.lanes.get(&p.net).is_some_and(|l| l.iter().any(|t| (t - e.lateral(c)).abs() < 1.0)))
        .min_by(|a, b| (-a.depth(c)).total_cmp(&-b.depth(c)))
    else {
        return (Vec::new(), Vec::new());
    };
    let lane = e.lateral(c);
    let pitch = e.lanes.values().flatten().filter(|t| (*t - lane).abs() > 1.0).map(|t| (t - lane).abs()).fold(f64::INFINITY, f64::min);
    let half = if pitch.is_finite() { pitch / 2.0 } else { 200_000.0 };
    let zone = opts.zone as f64;
    let at = |t: f64, d: f64| [mm(e.along.0 * t + e.out.0 * (e.row + d)), mm(e.along.1 * t + e.out.1 * (e.row + d))];
    let corridors = vec![
        json!({"poly": [at(lane - half, 0.0), at(lane + half, 0.0), at(lane + half, zone), at(lane - half, zone)], "kind": "corridor"}),
        json!({"line": [at(lane, 0.0), at(lane, zone)], "net": p.net, "kind": "lane"}),
    ];
    let mut arrows = Vec::new();
    for step_d in [0.2, 0.45, 0.7] {
        for side in [-1.0, 1.0] {
            let p0 = at(lane + side * half * 0.35, zone * step_d);
            let by = side * half * 0.65;
            arrows.push(json!([p0[0], p0[1], mm(e.along.0 * by), mm(e.along.1 * by)]));
        }
    }
    (corridors, arrows)
}

fn run_clear_lane(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.has("--pours-block");
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));
    let trials: usize = cli.flag("--trials").map_or(14, |s| s.parse().expect("--trials"));
    let reroute_s: f64 = cli.flag("--reroute").map_or(20.0, |s| s.parse().expect("--reroute"));
    let zone_nm = cli.flag("--zone").map_or(morph::EscapeOptions::default().zone, |s| nm_mm(s.parse().expect("--zone")));
    let opts = morph::EscapeOptions { zone: zone_nm, ..Default::default() };

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    let fp = resolve_part(&board, cli.flag("--part").as_deref());
    let ref_name = board.footprints[fp].reference.clone();
    let mut soft = prepared(cli, &mut board, pours_block).unwrap_or_else(|| adopt_soft(&mut board));
    let pad_box = pad_box_of(&board, fp).expect("part has pads");
    let window = cli.crop_override().unwrap_or_else(|| pad_box.grow(opts.zone + 1_800_000));

    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "clear-lane",
            out_path: &out_path,
            board_path,
            cropped: true,
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| format!("Clear lanes pad by pad on {ref_name}")),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| format!("slide foreign copper out of each trapped pad's {:.2} mm lane", mm(opts.zone as f64))),
        },
        &board,
        window,
        pours_block,
    );

    let init_copper = fb.snapshot_copper(&board);
    let open0 = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!("Initial routed board around {ref_name}: {open0} open connections"),
        FrameOverlays {
            initial: true,
            important: true,
            stats: vec![("open".into(), json!(open0)), ("trials".into(), json!(0))],
            ..Default::default()
        },
    );

    let mut done: HashSet<ItemId> = HashSet::new();
    let (mut tried_n, mut kept_n) = (0usize, 0usize);
    while tried_n < trials {
        let open_before = open_airlines(&board, pours_block);
        let Some(pad) = open_before.iter().flat_map(|c| [c.from_item, c.to_item]).find(|id| {
            !done.contains(id) && board.item(*id).is_some_and(|it| matches!(it.kind, Kind::Pad { footprint, .. } if footprint == fp))
        }) else {
            break;
        };
        done.insert(pad);
        tried_n += 1;
        let (pad_num, net) = match board.item(pad) {
            Some(Item { kind: Kind::Pad { number, .. }, net, .. }) => (number.clone(), *net),
            _ => (String::new(), 0),
        };
        let net_name = board.net_name(net).to_string();
        let pad_bb = board.item(pad).unwrap().shape.bbox();
        let pad_pt = Point::new((pad_bb.min.x + pad_bb.max.x) / 2, (pad_bb.min.y + pad_bb.max.y) / 2);
        let focus = json!({"pad": pad, "net": net, "at": xy(pad_pt)});
        let (corridors, arrows) = pad_lane_corridor(&board, fp, pad, &opts);

        let snapshot = (board.clone(), soft.clone());
        let c0 = fb.snapshot_copper(&board);
        let (moved, _) = morph::clear_lane(&mut board, fp, pad, &opts, &mut soft);
        let c1 = fb.snapshot_copper(&board);
        let (slid_from, slid_to) = diff_pools(&c0, &c1);
        fb.push_frame(
            &board,
            format!("Pad {ref_name}-{pad_num} ({net_name}): slid {moved} foreign pieces sideways out of its lane"),
            FrameOverlays {
                moved_from: slid_from,
                moved_to: slid_to,
                corridors: corridors.clone(),
                arrows,
                focus: focus.clone(),
                stats: vec![
                    ("pad".into(), json!(format!("{ref_name}-{pad_num}"))),
                    ("slid".into(), json!(moved)),
                    ("open".into(), json!(open_before.len())),
                ],
                ..Default::default()
            },
        );

        let (ripped_items, _) = grid::rip_broken_items(&mut board, &mut soft, pours_block);
        let ripped = fb.intern_items(&board, &ripped_items);
        let open_rip = open_airlines(&board, pours_block).len();
        fb.push_frame(
            &board,
            format!("Pad {ref_name}-{pad_num} ({net_name}): ripped {} clashing pieces; {open_rip} open", ripped_items.len()),
            FrameOverlays {
                ripped,
                corridors: corridors.clone(),
                focus: focus.clone(),
                stats: vec![
                    ("pad".into(), json!(format!("{ref_name}-{pad_num}"))),
                    ("ripped".into(), json!(ripped_items.len())),
                    ("open".into(), json!(open_rip)),
                ],
                ..Default::default()
            },
        );

        let c2 = fb.snapshot_copper(&board);
        let r = grid::route_open(
            &mut board,
            &mut grid::CpuSolver,
            &grid::Options { max_seconds: Some(reroute_s), pours_block, ..Default::default() },
            &mut |_| {},
        );
        soft.extend(r.laid);
        let c3 = fb.snapshot_copper(&board);
        let (_, laid) = diff_pools(&c2, &c3);
        let open_after = open_airlines(&board, pours_block).len();
        let kept = open_after < open_before.len();
        if kept {
            kept_n += 1;
        }
        fb.push_frame(
            &board,
            format!(
                "Pad {ref_name}-{pad_num} ({net_name}): rerouted {}, open {} -> {} ({})",
                r.routed,
                open_before.len(),
                open_after,
                if kept { "kept" } else { "undone" }
            ),
            FrameOverlays {
                important: true,
                laid,
                corridors,
                focus,
                stats: vec![
                    ("pad".into(), json!(format!("{ref_name}-{pad_num}"))),
                    ("open before".into(), json!(open_before.len())),
                    ("open after".into(), json!(open_after)),
                    ("verdict".into(), json!(if kept { "kept" } else { "undone" })),
                ],
                ..Default::default()
            },
        );
        if !kept {
            (board, soft) = snapshot;
        }
    }

    let end_copper = fb.snapshot_copper(&board);
    let (gone_total, came_total) = diff_pools(&init_copper, &end_copper);
    let open_end = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!("Clear-lane finished on {ref_name}: {tried_n} pads tried, {kept_n} kept; open {open0} -> {open_end}"),
        FrameOverlays {
            important: true,
            summary: true,
            moved_from: gone_total,
            laid: came_total,
            stats: vec![
                ("pads tried".into(), json!(tried_n)),
                ("kept".into(), json!(kept_n)),
                ("open before".into(), json!(open0)),
                ("open after".into(), json!(open_end)),
            ],
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

// --- 6. Shove ------------------------------------------------------------------------------

fn run_shove(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.has("--pours-block");
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));
    let plough_name = cli.flag("--plough").unwrap_or_else(|| "warp".to_string());
    let plough = match plough_name.as_str() {
        "warp" => grid::Plough::Warp,
        "linear" => grid::Plough::Linear,
        "rip" => grid::Plough::Rip,
        "gentle" => grid::Plough::Gentle,
        "dipole" => grid::Plough::Dipole,
        "region" => grid::Plough::Region,
        other => panic!("unknown --plough {other}"),
    };
    let cascade: usize = cli.flag("--cascade").map_or(0, |s| s.parse().expect("--cascade"));
    let repair_s: f64 = cli.flag("--repair").map_or(10.0, |s| s.parse().expect("--repair"));
    let accept_ties = cli.has("--ties");

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    let mut soft = prepared(cli, &mut board, pours_block).unwrap_or_else(|| adopt_soft(&mut board));
    let parts: HashSet<usize> = match cli.flag("--part").as_deref() {
        None | Some("auto") => morph::fine_pitch(&board, 500_000).into_iter().collect(),
        Some(list) => list
            .split(',')
            .map(|r| board.footprints.iter().position(|f| f.reference == r).unwrap_or_else(|| panic!("no part {r}")))
            .collect(),
    };
    let part_box = parts
        .iter()
        .filter_map(|&f| pad_box_of(&board, f))
        .reduce(BBox::union)
        .unwrap_or_else(|| board_bounds(&board));
    let window = cli.crop_override().unwrap_or_else(|| part_box.grow(5_000_000));

    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "shove",
            out_path: &out_path,
            board_path,
            cropped: true,
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| format!("Plough escape ({plough_name})")),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| format!("plough={plough_name}, cascade={cascade}, repair={repair_s:.0}s")),
        },
        &board,
        window,
        pours_block,
    );

    let init_copper = fb.snapshot_copper(&board);
    let open0 = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!("Initial routed board before {plough_name} plough: {open0} open connections"),
        FrameOverlays {
            initial: true,
            important: true,
            stats: vec![("plough".into(), json!(plough_name.clone())), ("open".into(), json!(open0))],
            ..Default::default()
        },
    );

    let opts = grid::ShoveOptions {
        parts: Some(parts),
        plough,
        cascade,
        repair_seconds: repair_s,
        accept_ties,
        reroute_seconds: 15.0,
        pours_block,
        ..Default::default()
    };

    let fb_cell = Mutex::new(fb);
    let prev = Mutex::new(init_copper.clone());
    let stats = grid::shove_escape_steps(
        &mut board,
        &mut grid::CpuSolver,
        &opts,
        &mut soft,
        &mut |now, event| {
            let mut fb = fb_cell.lock().unwrap();
            match event {
                grid::ShoveEvent::Start(t) => {
                    *prev.lock().unwrap() = fb.snapshot_copper(now);
                    let pad_name = grid::item_name(now, t.pad);
                    let net_name = now.net_name(t.net);
                    let at = now.item(t.pad).map_or(t.to, |it| {
                        let bb = it.shape.bbox();
                        Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2)
                    });
                    fb.push_frame(
                        now,
                        format!("Trapped {pad_name} ({net_name}): searching for cheapest path out ({} open)", t.open_before),
                        FrameOverlays {
                            focus: json!({"pad": t.pad, "net": t.net, "at": xy(at), "to": xy(t.to)}),
                            stats: vec![
                                ("pad".into(), json!(pad_name)),
                                ("net".into(), json!(net_name)),
                                ("open".into(), json!(t.open_before)),
                            ],
                            ..Default::default()
                        },
                    );
                }
                grid::ShoveEvent::Round(t, r) => {
                    let cur = fb.snapshot_copper(now);
                    let prev_snap = std::mem::replace(&mut *prev.lock().unwrap(), cur.clone());
                    let (gone, came) = diff_pools(&prev_snap, &cur);
                    let ripped = fb.intern_items(now, &r.ripped_items);
                    let ripped_set: HashSet<u32> = ripped.iter().copied().collect();
                    let moved_from: Vec<u32> = gone.into_iter().filter(|id| !ripped_set.contains(id)).collect();
                    let mut laid = Vec::new();
                    let mut moved_to = Vec::new();
                    for idx in came {
                        if fb.pool[idx as usize]["net"].as_u64() == Some(u64::from(r.net)) {
                            laid.push(idx);
                        } else {
                            moved_to.push(idx);
                        }
                    }
                    let path_json = if r.path.is_empty() {
                        Vec::new()
                    } else {
                        vec![json!({
                            "net": r.net,
                            "collides": false,
                            "points": r.path.iter().map(|(l, p)| json!([l, xy(*p)[0], xy(*p)[1]])).collect::<Vec<_>>(),
                        })]
                    };
                    // Sample warp plough arrows when Warp mode ran on round 1.
                    let arrows = if plough == grid::Plough::Warp && t.rounds.len() == 1 && !r.path.is_empty() {
                        let fields = grid::plough_fields(now, &r.path, 250_000.0, opts.reach as f64, r.net);
                        let path_bb = r.path.iter().map(|(_, p)| BBox::of([*p])).reduce(BBox::union).unwrap().grow(opts.reach);
                        sample_arrows(path_bb, 16, |p| {
                            fields
                                .values()
                                .map(|f| f.displacement(p))
                                .max_by(|a, b| a.0.hypot(a.1).total_cmp(&b.0.hypot(b.1)))
                                .unwrap_or((0.0, 0.0))
                        })
                    } else {
                        Vec::new()
                    };
                    let at = now.item(t.pad).map_or(t.to, |it| {
                        let bb = it.shape.bbox();
                        Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2)
                    });
                    let pad_name = grid::item_name(now, t.pad);
                    let net_name = now.net_name(r.net);
                    let open_now = open_airlines(now, pours_block).len();
                    fb.push_frame(
                        now,
                        format!(
                            "{pad_name} ({net_name}) round {}: {} moved, {} crossed, {} ripped -> {}; {open_now} open",
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
                            arrows,
                            paths: path_json,
                            focus: json!({"pad": t.pad, "net": t.net, "at": xy(at), "to": xy(t.to)}),
                            stats: vec![
                                ("round".into(), json!(t.rounds.len())),
                                ("moved".into(), json!(r.moved)),
                                ("ripped".into(), json!(r.ripped + r.crossed)),
                                ("open".into(), json!(open_now)),
                            ],
                            ..Default::default()
                        },
                    );
                }
                grid::ShoveEvent::Done(t) => {
                    let cur = fb.snapshot_copper(now);
                    let prev_snap = std::mem::replace(&mut *prev.lock().unwrap(), cur.clone());
                    let (gone, came) = diff_pools(&prev_snap, &cur);
                    let pad_name = grid::item_name(now, t.pad);
                    let net_name = now.net_name(t.net);
                    fb.push_frame(
                        now,
                        format!(
                            "{pad_name} ({net_name}) repair done: open {} -> {} ({})",
                            t.open_before,
                            t.open_after,
                            if t.kept { "kept" } else { "undone" }
                        ),
                        FrameOverlays {
                            important: true,
                            ripped: gone,
                            laid: came,
                            stats: vec![
                                ("pad".into(), json!(pad_name)),
                                ("open before".into(), json!(t.open_before)),
                                ("open after".into(), json!(t.open_after)),
                                ("verdict".into(), json!(if t.kept { "kept" } else { "undone" })),
                            ],
                            ..Default::default()
                        },
                    );
                }
            }
        },
        &mut |now, t, r, step_idx| {
            let mut fb = fb_cell.lock().unwrap();
            let cur = fb.snapshot_copper(now);
            let prev_snap = std::mem::replace(&mut *prev.lock().unwrap(), cur.clone());
            let (gone, came) = diff_pools(&prev_snap, &cur);
            let pad_name = grid::item_name(now, t.pad);
            let net_name = now.net_name(t.net);
            let at = now.item(t.pad).map_or(t.to, |it| {
                let bb = it.shape.bbox();
                Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2)
            });
            let poles = if plough == grid::Plough::Dipole {
                vec![json!({"at": xy(at), "strength": 0.35, "radius": mm(opts.reach as f64)})]
            } else {
                Vec::new()
            };
            let path_json = if r.path.is_empty() {
                Vec::new()
            } else {
                vec![json!({
                    "net": r.net,
                    "collides": false,
                    "points": r.path.iter().map(|(l, p)| json!([l, xy(*p)[0], xy(*p)[1]])).collect::<Vec<_>>(),
                })]
            };
            fb.push_frame(
                now,
                format!(
                    "{pad_name} ({net_name}) {plough_name} micro-step {}/{}: {} cumulative shifted{}",
                    step_idx + 1,
                    opts.steps,
                    r.moved,
                    if r.routed { ", routed out" } else { "" }
                ),
                FrameOverlays {
                    moved_from: gone,
                    moved_to: came,
                    paths: path_json,
                    poles,
                    focus: json!({"pad": t.pad, "net": t.net, "at": xy(at), "to": xy(t.to)}),
                    stats: vec![
                        ("micro-step".into(), json!(format!("{} / {}", step_idx + 1, opts.steps))),
                        ("shifted".into(), json!(r.moved)),
                        ("routed".into(), json!(if r.routed { "yes" } else { "no" })),
                    ],
                    ..Default::default()
                },
            );
        },
    );
    let mut fb = fb_cell.into_inner().unwrap();

    let end_copper = fb.snapshot_copper(&board);
    let (gone_total, came_total) = diff_pools(&init_copper, &end_copper);
    let open_end = open_airlines(&board, pours_block).len();
    let kept_count = stats.tries.iter().filter(|t| t.kept).count();
    let out_count = stats.tries.iter().filter(|t| t.routed).count();
    fb.push_frame(
        &board,
        format!(
            "Plough ({plough_name}) complete: {} tried, {out_count} routed out, {kept_count} kept; open {open0} -> {open_end}",
            stats.tries.len()
        ),
        FrameOverlays {
            important: true,
            summary: true,
            moved_from: gone_total,
            laid: came_total,
            stats: vec![
                ("tried".into(), json!(stats.tries.len())),
                ("routed out".into(), json!(out_count)),
                ("kept".into(), json!(kept_count)),
                ("open".into(), json!(open_end)),
            ],
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

// --- 7. Nudge ------------------------------------------------------------------------------

fn run_nudge(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.has("--pours-block");
    let prep = cli.has("--prepare-route");
    let guided_only = cli.has("--guided-only");
    let max_s: f64 = cli.flag("--seconds").map_or(25.0, |s| s.parse().expect("--seconds"));
    let max_frames = cli.flag("--max-frames").map_or(140, |s| s.parse().expect("--max-frames"));

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    let mut soft = if let Some(s) = prepared(cli, &mut board, pours_block) {
        s
    } else if prep {
        strip_unlocked(&mut board, true);
        let r = grid::route_open(
            &mut board,
            &mut grid::CpuSolver,
            &grid::Options { max_seconds: Some(20.0), pours_block, ..Default::default() },
            &mut |_| {},
        );
        r.laid
    } else {
        adopt_soft(&mut board)
    };
    let window = cli.crop_override().unwrap_or_else(|| board_bounds(&board));
    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "nudge",
            out_path: &out_path,
            board_path,
            cropped: cli.crop_override().is_some(),
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| format!("Nudge parts ({})", if guided_only { "blame-guided" } else { "guided + blind" })),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| "step unlocked parts aside with flowing copper and reroute".to_string()),
        },
        &board,
        window,
        pours_block,
    );

    let init_copper = fb.snapshot_copper(&board);
    let open0 = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!("Initial routed board before nudging: {open0} open connections"),
        FrameOverlays {
            initial: true,
            important: true,
            stats: vec![("open".into(), json!(open0)), ("tried".into(), json!(0))],
            ..Default::default()
        },
    );

    let locked: HashSet<usize> = board.footprints.iter().enumerate().filter(|(_, f)| f.locked).map(|(i, _)| i).collect();
    let baseline_copper = Mutex::new(init_copper.clone());
    let baseline_fp: Mutex<Vec<Point>> = Mutex::new(board.footprints.iter().map(|f| f.at).collect());
    let mut trial_idx = 0usize;

    let n_stats = grid::nudge_watch(
        &mut board,
        &mut grid::CpuSolver,
        &grid::NudgeOptions {
            blind: !guided_only,
            max_seconds: Some(max_s),
            reroute_seconds: 3.0,
            locked,
            pours_block,
            ..Default::default()
        },
        &mut soft,
        &mut |now, ev| match ev {
            grid::NudgeEvent::Blame { airline, blamed } => {
                if blamed.is_empty() {
                    return;
                }
                let arrows: Vec<Value> = blamed
                    .iter()
                    .map(|b| {
                        let c = part_centre(now, b.footprint);
                        json!([mm(c.x as f64), mm(c.y as f64), mm(b.dx), mm(b.dy)])
                    })
                    .collect();
                let names: Vec<String> = blamed
                    .iter()
                    .map(|b| format!("{} ({:+.2},{:+.2} mm)", now.footprints[b.footprint].reference, mm(b.dx), mm(b.dy)))
                    .collect();
                fb.push_frame(
                    now,
                    format!("Blame for open {}: asks to move {}", now.net_name(airline.net), names.join(", ")),
                    FrameOverlays {
                        important: true,
                        arrows,
                        focus: json!({"net": airline.net, "from": xy(airline.from), "to": xy(airline.to)}),
                        stats: vec![
                            ("net".into(), json!(now.net_name(airline.net))),
                            ("blamed parts".into(), json!(blamed.len())),
                        ],
                        ..Default::default()
                    },
                );
            }
            grid::NudgeEvent::Step { airline, footprint, requested, moved, guided, ripped, open_before, open_after, outcome } => {
                trial_idx += 1;
                let cur_copper = fb.snapshot_copper(now);
                let base_c = baseline_copper.lock().unwrap().clone();
                let (gone, came) = diff_pools(&base_c, &cur_copper);
                let base_fps = baseline_fp.lock().unwrap().clone();
                let d0 = Point::new(base_fps[footprint].x - fb.initial_fp_at[footprint].x, base_fps[footprint].y - fb.initial_fp_at[footprint].y);
                let shown_to = if moved == Point::new(0, 0) {
                    Point::new(d0.x + requested.x, d0.y + requested.y)
                } else {
                    Point::new(now.footprints[footprint].at.x - fb.initial_fp_at[footprint].x, now.footprints[footprint].at.y - fb.initial_fp_at[footprint].y)
                };
                let c = part_centre(now, footprint);
                let ref_name = &now.footprints[footprint].reference;
                let kind_str = if guided { "blame" } else { "blind" };
                fb.push_frame(
                    now,
                    format!(
                        "Trial #{trial_idx} ({kind_str}): {ref_name} ({:+.2}, {:+.2}) mm for {} -> {outcome} (open {open_before} -> {open_after})",
                        mm(requested.x as f64),
                        mm(requested.y as f64),
                        now.net_name(airline.net)
                    ),
                    FrameOverlays {
                        important: outcome == "kept" || guided,
                        moved_from: gone,
                        moved_to: came,
                        moved_parts: vec![json!({
                            "fp": footprint,
                            "ref": ref_name,
                            "from": [mm(d0.x as f64), mm(d0.y as f64)],
                            "to": [mm(shown_to.x as f64), mm(shown_to.y as f64)],
                            "status": outcome,
                        })],
                        arrows: vec![json!([mm(c.x as f64), mm(c.y as f64), mm(requested.x as f64), mm(requested.y as f64)])],
                        focus: json!({"net": airline.net, "from": xy(airline.from), "to": xy(airline.to)}),
                        stats: vec![
                            ("trial".into(), json!(trial_idx)),
                            ("part".into(), json!(ref_name)),
                            ("ripped".into(), json!(ripped)),
                            ("outcome".into(), json!(outcome)),
                        ],
                        ..Default::default()
                    },
                );
                if outcome == "kept" {
                    *baseline_copper.lock().unwrap() = cur_copper;
                    *baseline_fp.lock().unwrap() = now.footprints.iter().map(|f| f.at).collect();
                }
            }
        },
    );

    let end_copper = fb.snapshot_copper(&board);
    let (gone_total, came_total) = diff_pools(&init_copper, &end_copper);
    let moved_parts: Vec<Value> = board
        .footprints
        .iter()
        .enumerate()
        .filter_map(|(i, f)| {
            let d = Point::new(f.at.x - fb.initial_fp_at[i].x, f.at.y - fb.initial_fp_at[i].y);
            (d != Point::new(0, 0)).then(|| {
                json!({
                    "fp": i,
                    "ref": f.reference,
                    "from": [0.0, 0.0],
                    "to": [mm(d.x as f64), mm(d.y as f64)],
                    "status": "kept",
                })
            })
        })
        .collect();
    let open_end = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!(
            "Nudge finished: {} tried, {} illegal, {} kept ({} guided); open {open0} -> {open_end}",
            n_stats.tried, n_stats.illegal, n_stats.kept, n_stats.guided
        ),
        FrameOverlays {
            important: true,
            summary: true,
            moved_from: gone_total,
            laid: came_total,
            moved_parts,
            stats: vec![
                ("tried".into(), json!(n_stats.tried)),
                ("illegal".into(), json!(n_stats.illegal)),
                ("kept".into(), json!(n_stats.kept)),
                ("guided".into(), json!(n_stats.guided)),
                ("open".into(), json!(open_end)),
            ],
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

// --- 8. Negotiate --------------------------------------------------------------------------

fn build_heatmap(rv: &grid::RoundView, window: BBox) -> Value {
    let g = rv.grid;
    let bin = 4usize; // 4x4 grid cells = 0.2 mm tiles at 0.05 mm cell size
    let bnx = g.nx.div_ceil(bin);
    let bny = g.ny.div_ceil(bin);
    let cells_per_layer = g.nx * g.ny;
    let layers = g.layers.len();
    let step_nm = g.h * bin as Nm;
    let mut cells = Vec::new();
    for bj in 0..bny {
        for bi in 0..bnx {
            let p = Point::new(g.x0 + (bi * bin) as Nm * g.h, g.y0 + (bj * bin) as Nm * g.h);
            if !window.grow(step_nm).contains(p) {
                continue;
            }
            let mut max_over = 0u16;
            let mut max_hist = 0u32;
            for l in 0..layers {
                for dj in 0..bin {
                    let j = bj * bin + dj;
                    if j >= g.ny {
                        continue;
                    }
                    for di in 0..bin {
                        let i = bi * bin + di;
                        if i >= g.nx {
                            continue;
                        }
                        let idx = l * cells_per_layer + j * g.nx + i;
                        max_over = max_over.max(rv.overlap[idx]);
                        max_hist = max_hist.max(rv.history[idx]);
                    }
                }
            }
            if max_over > 1 || max_hist > 0 {
                cells.push(json!([bi, bj, max_over, max_hist]));
            }
        }
    }
    json!({
        "x0": mm(g.x0 as f64),
        "y0": mm(g.y0 as f64),
        "step": mm(step_nm as f64),
        "nx": bnx,
        "ny": bny,
        "cells": cells,
    })
}

fn run_negotiate(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.has("--pours-block");
    let strip_all = cli.has("--strip-all");
    let max_rounds: usize = cli.flag("--rounds").map_or(30, |s| s.parse().expect("--rounds"));
    let threads: usize = cli.flag("--threads").map_or(4, |s| s.parse().expect("--threads"));
    let seed: u64 = cli.flag("--seed").map_or(0, |s| s.parse().expect("--seed"));
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    if prepared(cli, &mut board, pours_block).is_none() && strip_all {
        strip_unlocked(&mut board, true);
    }
    let window = cli.crop_override().unwrap_or_else(|| board_bounds(&board));
    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "negotiate",
            out_path: &out_path,
            board_path,
            cropped: cli.crop_override().is_some(),
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| "PathFinder negotiated congestion".to_string()),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| format!("up to {max_rounds} rounds, congestion heat map and trial routes")),
        },
        &board,
        window,
        pours_block,
    );

    let base_board = board.clone();
    let init_copper = fb.snapshot_copper(&board);
    let open0 = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!("Unrouted board: {open0} open connections ready for PathFinder negotiation"),
        FrameOverlays {
            initial: true,
            important: true,
            stats: vec![("round".into(), json!(0)), ("connections".into(), json!(open0))],
            ..Default::default()
        },
    );

    let last_heat: Mutex<Value> = Mutex::new(Value::Null);
    let n_stats = grid::route_negotiated_rounds(
        &mut board,
        &mut grid::CpuSolver,
        &grid::NegotiationOptions {
            max_rounds,
            threads,
            seed,
            pours_block,
            ..Default::default()
        },
        &mut |_| {},
        &mut |_, _| {},
        Some(&mut |rv| {
            let heat = build_heatmap(&rv, window);
            *last_heat.lock().unwrap() = heat.clone();
            let paths: Vec<Value> = rv
                .trials
                .iter()
                .filter(|(_, pts, _)| pts.iter().any(|(_, p)| window.contains(*p)))
                .map(|(net, pts, collides)| {
                    json!({
                        "net": net,
                        "collides": collides,
                        "points": pts.iter().map(|(l, p)| json!([l, xy(*p)[0], xy(*p)[1]])).collect::<Vec<_>>(),
                    })
                })
                .collect();
            let hot_bins = heat["cells"].as_array().map_or(0, Vec::len);
            fb.push_frame(
                &base_board,
                format!(
                    "Round {}: {} of {} trial routes collide (present price {:.1}, {hot_bins} congested tiles)",
                    rv.round + 1,
                    rv.colliding,
                    rv.trials.len(),
                    rv.present
                ),
                FrameOverlays {
                    important: true,
                    paths,
                    heat,
                    stats: vec![
                        ("round".into(), json!(rv.round + 1)),
                        ("colliding".into(), json!(rv.colliding)),
                        ("trials".into(), json!(rv.trials.len())),
                        ("present".into(), json!((rv.present * 10.0).round() / 10.0)),
                    ],
                    ..Default::default()
                },
            );
        }),
    );

    let end_copper = fb.snapshot_copper(&board);
    let (_, committed_laid) = diff_pools(&init_copper, &end_copper);
    let open_end = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!(
            "Committed {} of {} conflict-free routes ({} vias) after {} rounds; {open_end} left open",
            n_stats.committed, n_stats.connections, n_stats.vias, n_stats.rounds
        ),
        FrameOverlays {
            important: true,
            summary: true,
            laid: committed_laid,
            heat: last_heat.into_inner().unwrap(),
            stats: vec![
                ("rounds".into(), json!(n_stats.rounds)),
                ("committed".into(), json!(n_stats.committed)),
                ("vias".into(), json!(n_stats.vias)),
                ("open left".into(), json!(open_end)),
            ],
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

// --- 9. Route ------------------------------------------------------------------------------

fn run_route(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.has("--pours-block");
    let strip_all = cli.has("--strip-all");
    let after_neg = cli.has("--after-negotiate");
    let max_s: Option<f64> = cli.flag("--seconds").map(|s| s.parse().expect("--seconds"));
    let seed: u64 = cli.flag("--seed").map_or(0, |s| s.parse().expect("--seed"));
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    let prep = prepared(cli, &mut board, pours_block).is_some();
    if !prep && (strip_all || after_neg) {
        strip_unlocked(&mut board, true);
    }
    if !prep && after_neg {
        grid::route_negotiated(
            &mut board,
            &mut grid::CpuSolver,
            &grid::NegotiationOptions { threads: 4, max_seconds: Some(25.0), seed, pours_block, ..Default::default() },
            &mut |_| {},
            &mut |_, _| {},
        );
    }
    let window = cli.crop_override().unwrap_or_else(|| board_bounds(&board));
    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "route",
            out_path: &out_path,
            board_path,
            cropped: cli.crop_override().is_some(),
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| {
                if after_neg {
                    "Grid route_open finishing after negotiation".to_string()
                } else {
                    "Grid route_open from scratch (shortest first)".to_string()
                }
            }),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| "one connection per frame on the obstacle grid".to_string()),
        },
        &board,
        window,
        pours_block,
    );

    let init_copper = fb.snapshot_copper(&board);
    let open0 = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!("Start of route_open: {open0} open connections to try shortest-first"),
        FrameOverlays {
            initial: true,
            important: true,
            stats: vec![("routed".into(), json!(0)), ("open".into(), json!(open0))],
            ..Default::default()
        },
    );

    let current_conn: Arc<Mutex<Option<Focused>>> = Arc::new(Mutex::new(None));
    let gave_up_list: Arc<Mutex<Vec<Focused>>> = Arc::new(Mutex::new(Vec::new()));
    let c_shared = current_conn.clone();
    let g_shared = gave_up_list.clone();
    let log = grid::Log(Arc::new(move |act| match act.phase {
        "routing" => {
            *c_shared.lock().unwrap() = Some((act.text, act.focus));
        }
        "gave up" => {
            let focus = c_shared.lock().unwrap().as_ref().and_then(|(_, f)| f.clone());
            g_shared.lock().unwrap().push((act.text, focus));
        }
        _ => {}
    }));

    let mut prev = init_copper.clone();
    let mut count = 0usize;
    let stats = grid::route_open(
        &mut board,
        &mut grid::CpuSolver,
        &grid::Options { max_seconds: max_s, pours_block, seed, log: Some(log), ..Default::default() },
        &mut |now| {
            count += 1;
            let cur = fb.snapshot_copper(now);
            let (_, laid) = diff_pools(&prev, &cur);
            prev = cur;
            let open_now = open_airlines(now, pours_block).len();
            let info = current_conn.lock().unwrap().clone();
            let desc = info.as_ref().map_or_else(|| "connection".to_string(), |(t, _)| t.clone());
            let focus = match info.and_then(|(_, f)| f) {
                Some(grid::Focus::Connection { net, from, to }) => json!({"net": net, "from": xy(from), "to": xy(to)}),
                _ => Value::Null,
            };
            fb.push_frame(
                now,
                format!("Connection #{count}: routed {desc} ({} pieces); {open_now} open", laid.len()),
                FrameOverlays {
                    laid,
                    focus,
                    stats: vec![
                        ("routed".into(), json!(count)),
                        ("pieces".into(), json!(prev.len())),
                        ("open".into(), json!(open_now)),
                    ],
                    ..Default::default()
                },
            );
        },
    );

    for (fail_text, focus) in gave_up_list.lock().unwrap().iter().take(16) {
        let f_json = match focus {
            Some(grid::Focus::Connection { net, from, to }) => json!({"net": *net, "from": xy(*from), "to": xy(*to), "failed": true}),
            _ => Value::Null,
        };
        let open_now = open_airlines(&board, pours_block).len();
        fb.push_frame(
            &board,
            format!("Gave up on {fail_text}; {open_now} open"),
            FrameOverlays {
                important: true,
                focus: f_json,
                stats: vec![
                    ("routed".into(), json!(stats.routed)),
                    ("failed".into(), json!(stats.failed)),
                    ("open".into(), json!(open_now)),
                ],
                ..Default::default()
            },
        );
    }

    let end_copper = fb.snapshot_copper(&board);
    let (_, total_laid) = diff_pools(&init_copper, &end_copper);
    let open_end = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!(
            "route_open complete: {} routed ({} vias), {} failed; {open0} -> {open_end} open",
            stats.routed, stats.vias, stats.failed
        ),
        FrameOverlays {
            important: true,
            summary: true,
            laid: total_laid,
            stats: vec![
                ("routed".into(), json!(stats.routed)),
                ("vias".into(), json!(stats.vias)),
                ("failed".into(), json!(stats.failed)),
                ("open".into(), json!(open_end)),
            ],
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

// --- 10. Rip-up ----------------------------------------------------------------------------

fn run_rip_up(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.has("--pours-block");
    let prep = cli.has("--prepare-route");
    let max_s: f64 = cli.flag("--seconds").map_or(25.0, |s| s.parse().expect("--seconds"));
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    let mut soft = if let Some(s) = prepared(cli, &mut board, pours_block) {
        s
    } else if prep {
        strip_unlocked(&mut board, true);
        let r = grid::route_open(
            &mut board,
            &mut grid::CpuSolver,
            &grid::Options { max_seconds: Some(20.0), pours_block, ..Default::default() },
            &mut |_| {},
        );
        r.laid
    } else {
        adopt_soft(&mut board)
    };

    let open_init = open_airlines(&board, pours_block);
    let window = cli.crop_override().unwrap_or_else(|| {
        if !prep && !open_init.is_empty() {
            open_init
                .iter()
                .map(|a| BBox::of([a.from, a.to]))
                .reduce(BBox::union)
                .unwrap()
                .grow(4_000_000)
        } else {
            board_bounds(&board)
        }
    });

    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "rip-up",
            out_path: &out_path,
            board_path,
            cropped: cli.crop_override().is_some() || !prep,
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| {
                if prep {
                    "Rip-up and reroute after greedy routing".to_string()
                } else {
                    "Rip-up attempts on the routed earring".to_string()
                }
            }),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| "route through soft copper at a price, rip conflicts, reroute victims".to_string()),
        },
        &board,
        window,
        pours_block,
    );

    let init_copper = fb.snapshot_copper(&board);
    let open0 = open_init.len();
    fb.push_frame(
        &board,
        format!("Before rip-up: {open0} open connections"),
        FrameOverlays {
            initial: true,
            important: true,
            stats: vec![("open".into(), json!(open0)), ("kept".into(), json!(0))],
            ..Default::default()
        },
    );

    let mut try_n = 0usize;
    let r_stats = grid::rip_up_watch(
        &mut board,
        &mut grid::CpuSolver,
        &grid::RipOptions { max_seconds: Some(max_s), pours_block, ..Default::default() },
        &mut soft,
        &mut |now, att| {
            try_n += 1;
            let ripped = fb.intern_items(now, &att.ripped);
            let mut laid = Vec::new();
            for &id in &att.laid {
                if let Some(it) = now.item(id) {
                    if it.shape.bbox().overlaps(window.grow(500_000)) {
                        laid.push(fb.intern(now, id, it));
                    }
                }
            }
            let paths = if att.path.is_empty() {
                Vec::new()
            } else {
                vec![json!({
                    "net": att.airline.net,
                    "collides": att.outcome == "hard conflict",
                    "points": att.path.iter().map(|(l, p)| json!([l, xy(*p)[0], xy(*p)[1]])).collect::<Vec<_>>(),
                })]
            };
            let net_name = now.net_name(att.airline.net);
            let a_name = grid::item_name(now, att.airline.from_item);
            let b_name = grid::item_name(now, att.airline.to_item);
            fb.push_frame(
                now,
                format!(
                    "Attempt #{try_n} {net_name} ({a_name} to {b_name}): ripped {}, laid {} -> {} (open {} -> {})",
                    att.ripped.len(),
                    att.laid.len(),
                    att.outcome,
                    att.open_before,
                    att.open_after
                ),
                FrameOverlays {
                    important: att.outcome == "kept" || !prep,
                    ripped,
                    laid,
                    paths,
                    focus: json!({"net": att.airline.net, "from": xy(att.airline.from), "to": xy(att.airline.to), "failed": att.outcome != "kept"}),
                    stats: vec![
                        ("attempt".into(), json!(try_n)),
                        ("ripped".into(), json!(att.ripped.len())),
                        ("open".into(), json!(att.open_after)),
                        ("outcome".into(), json!(att.outcome)),
                    ],
                    ..Default::default()
                },
            );
        },
    );

    let end_copper = fb.snapshot_copper(&board);
    let (gone_total, came_total) = diff_pools(&init_copper, &end_copper);
    let open_end = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!(
            "Rip-up finished: {} attempts, {} kept ({} pieces ripped); open {open0} -> {open_end}",
            r_stats.attempts, r_stats.kept, r_stats.ripped
        ),
        FrameOverlays {
            important: true,
            summary: true,
            ripped: gone_total,
            laid: came_total,
            stats: vec![
                ("attempts".into(), json!(r_stats.attempts)),
                ("kept".into(), json!(r_stats.kept)),
                ("no better".into(), json!(r_stats.no_better)),
                ("open".into(), json!(open_end)),
            ],
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

// --- 11. Chase -----------------------------------------------------------------------------

fn run_chase(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.has("--pours-block");
    let prep = cli.has("--prepare-route");
    let links: usize = cli.flag("--links").map_or(20, |s| s.parse().expect("--links"));
    let max_s: f64 = cli.flag("--seconds").map_or(25.0, |s| s.parse().expect("--seconds"));
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    let mut soft = if let Some(s) = prepared(cli, &mut board, pours_block) {
        s
    } else if prep {
        strip_unlocked(&mut board, true);
        let r = grid::route_open(
            &mut board,
            &mut grid::CpuSolver,
            &grid::Options { max_seconds: Some(20.0), pours_block, ..Default::default() },
            &mut |_| {},
        );
        r.laid
    } else {
        adopt_soft(&mut board)
    };
    let open_init = open_airlines(&board, pours_block);
    let window = cli.crop_override().unwrap_or_else(|| {
        if !open_init.is_empty() {
            open_init
                .iter()
                .map(|a| BBox::of([a.from, a.to]))
                .reduce(BBox::union)
                .unwrap()
                .grow(4_500_000)
        } else {
            board_bounds(&board)
        }
    });

    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "chase",
            out_path: &out_path,
            board_path,
            cropped: true,
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| "Chained rip-up (chase)".to_string()),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| format!("up to {links} links per chain; ripped nets rip in turn")),
        },
        &board,
        window,
        pours_block,
    );

    let init_copper = fb.snapshot_copper(&board);
    let open0 = open_init.len();
    fb.push_frame(
        &board,
        format!("Before chase: {open0} open connections"),
        FrameOverlays {
            initial: true,
            important: true,
            stats: vec![("open".into(), json!(open0)), ("max links".into(), json!(links))],
            ..Default::default()
        },
    );

    let latest_msg: Arc<Mutex<(String, Option<grid::Focus>)>> = Arc::new(Mutex::new((String::new(), None)));
    let m_shared = latest_msg.clone();
    let log = grid::Log(Arc::new(move |act| {
        if act.phase == "chasing" {
            *m_shared.lock().unwrap() = (act.text.trim().to_string(), act.focus);
        }
    }));

    let mut prev = init_copper.clone();
    let c_stats = grid::chase(
        &mut board,
        &mut grid::CpuSolver,
        &grid::ChaseOptions { links, max_seconds: Some(max_s), pours_block, log: Some(log), ..Default::default() },
        &mut soft,
        &mut |now| {
            let cur = fb.snapshot_copper(now);
            let (gone, came) = diff_pools(&prev, &cur);
            prev = cur;
            let (msg, focus) = latest_msg.lock().unwrap().clone();
            let put_back = msg.contains("put back");
            let open_now = open_airlines(now, pours_block).len();
            let f_json = match focus {
                Some(grid::Focus::Connection { net, from, to }) => json!({"net": net, "from": xy(from), "to": xy(to), "failed": put_back}),
                _ => Value::Null,
            };
            fb.push_frame(
                now,
                format!("Chase: {msg} ({open_now} open)"),
                FrameOverlays {
                    important: put_back || msg.contains("kept"),
                    moved_from: if put_back { gone.clone() } else { Vec::new() },
                    ripped: if put_back { Vec::new() } else { gone },
                    laid: if put_back { Vec::new() } else { came },
                    focus: f_json,
                    stats: vec![
                        ("open".into(), json!(open_now)),
                        ("event".into(), json!(if put_back { "reverted" } else { "link" })),
                    ],
                    ..Default::default()
                },
            );
        },
    );

    let end_copper = fb.snapshot_copper(&board);
    let (gone_total, came_total) = diff_pools(&init_copper, &end_copper);
    let open_end = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!(
            "Chase complete: {} chains tried, {} kept ({} links); open {open0} -> {open_end}",
            c_stats.attempts, c_stats.kept, c_stats.links
        ),
        FrameOverlays {
            important: true,
            summary: true,
            ripped: gone_total,
            laid: came_total,
            stats: vec![
                ("chains".into(), json!(c_stats.attempts)),
                ("kept".into(), json!(c_stats.kept)),
                ("links".into(), json!(c_stats.links)),
                ("open".into(), json!(open_end)),
            ],
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

// --- 12. Tighten ---------------------------------------------------------------------------

fn run_tighten(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.has("--pours-block");
    let prep = cli.has("--prepare-route");
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    let mut soft = if let Some(s) = prepared(cli, &mut board, pours_block) {
        s
    } else if prep {
        prepare_fresh_route(&mut board, pours_block)
    } else {
        adopt_soft(&mut board)
    };
    let window = cli.crop_override().unwrap_or_else(|| board_bounds(&board));

    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "tighten",
            out_path: &out_path,
            board_path,
            cropped: cli.crop_override().is_some(),
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| "Pull routed tracks tight".to_string()),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| "replace stair-stepped grid chains with longest legal straight tracks".to_string()),
        },
        &board,
        window,
        pours_block,
    );

    let init_copper = fb.snapshot_copper(&board);
    let segs0 = soft.iter().filter(|&&id| board.item(id).is_some_and(|it| it.kind == Kind::Track)).count();
    fb.push_frame(
        &board,
        format!("Before pull_tight: {segs0} soft track segments on the grid"),
        FrameOverlays {
            initial: true,
            important: true,
            stats: vec![("segments".into(), json!(segs0)), ("tightened".into(), json!(0))],
            ..Default::default()
        },
    );

    let mut shortened_chains = 0usize;
    let stats = grid::pull_tight_watch(&mut board, &mut soft, &mut |now, net, old_items, new_ids| {
        shortened_chains += 1;
        let moved_from = fb.intern_items(now, old_items);
        let mut laid = Vec::new();
        for &id in new_ids {
            if let Some(it) = now.item(id) {
                if it.shape.bbox().overlaps(window.grow(500_000)) {
                    laid.push(fb.intern(now, id, it));
                }
            }
        }
        let net_name = now.net_name(net);
        fb.push_frame(
            now,
            format!(
                "Chain #{shortened_chains} ({net_name}): pulled {} grid segments into {} straight track(s)",
                old_items.len(),
                new_ids.len()
            ),
            FrameOverlays {
                moved_from,
                laid,
                stats: vec![
                    ("chain".into(), json!(shortened_chains)),
                    ("net".into(), json!(net_name)),
                    ("before segs".into(), json!(old_items.len())),
                    ("after segs".into(), json!(new_ids.len())),
                ],
                ..Default::default()
            },
        );
    });

    let end_copper = fb.snapshot_copper(&board);
    let (gone_total, came_total) = diff_pools(&init_copper, &end_copper);
    fb.push_frame(
        &board,
        format!(
            "Tighten finished: {} -> {} segments, {:.1} -> {:.1} mm ({shortened_chains} chains shortened)",
            stats.segments_before, stats.segments_after, stats.length_before, stats.length_after
        ),
        FrameOverlays {
            important: true,
            summary: true,
            moved_from: gone_total,
            laid: came_total,
            stats: vec![
                ("segs before".into(), json!(stats.segments_before)),
                ("segs after".into(), json!(stats.segments_after)),
                ("mm before".into(), json!((stats.length_before * 10.0).round() / 10.0)),
                ("mm after".into(), json!((stats.length_after * 10.0).round() / 10.0)),
            ],
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

// --- 13. Cleanup ---------------------------------------------------------------------------

fn run_cleanup(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.has("--pours-block");
    let prep_messy = cli.has("--prepare-messy");
    let prep_route = cli.has("--prepare-route");
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    let mut soft = if let Some(s) = prepared(cli, &mut board, pours_block) {
        s
    } else if prep_messy {
        let mut s = adopt_soft(&mut board);
        // Combing + ripping broken pieces + rerouting along existing net copper leaves real
        // duplicate segments and multi-piece dangling branches and single-layer vias to clean up.
        if let Some(&fp) = morph::fine_pitch(&board, 500_000).first() {
            morph::comb(&mut board, fp, &morph::EscapeOptions::default(), &mut s);
            grid::rip_broken(&mut board, &mut s, pours_block);
            let r = grid::route_open(
                &mut board,
                &mut grid::CpuSolver,
                &grid::Options { max_seconds: Some(15.0), pours_block, ..Default::default() },
                &mut |_| {},
            );
            s.extend(r.laid);
            // Also duplicate a handful of soft tracks to exercise drop_duplicates if reroute made none.
            let sample_dups: Vec<Item> = s
                .iter()
                .filter_map(|&id| board.item(id).cloned())
                .filter(|it| it.kind == Kind::Track)
                .take(4)
                .collect();
            for mut dup in sample_dups {
                dup.span = None;
                s.push(board.insert(dup));
            }
        }
        s
    } else if prep_route {
        prepare_fresh_route(&mut board, pours_block)
    } else {
        adopt_soft(&mut board)
    };

    let window = cli.crop_override().unwrap_or_else(|| {
        if prep_messy {
            if let Some(&fp) = morph::fine_pitch(&board, 500_000).first() {
                pad_box_of(&board, fp).unwrap().grow(4_500_000)
            } else {
                board_bounds(&board)
            }
        } else {
            board_bounds(&board)
        }
    });

    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "cleanup",
            out_path: &out_path,
            board_path,
            cropped: cli.crop_override().is_some() || prep_messy,
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| "Cleanup: drop duplicates and prune dangling copper".to_string()),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| "remove doubled tracks and dead-end antennas without breaking connectivity".to_string()),
        },
        &board,
        window,
        pours_block,
    );

    let init_copper = fb.snapshot_copper(&board);
    let open0 = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!("Before cleanup: {open0} open connections"),
        FrameOverlays {
            initial: true,
            important: true,
            stats: vec![("open".into(), json!(open0)), ("doubled".into(), json!(0)), ("pruned".into(), json!(0))],
            ..Default::default()
        },
    );

    let mut dup_n = 0usize;
    let (doubled, _) = grid::drop_duplicates_watch(&mut board, &mut soft, &mut |now, item| {
        dup_n += 1;
        let ripped = fb.intern_items(now, std::slice::from_ref(item));
        let bb = item.shape.bbox();
        let at = Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2);
        fb.push_frame(
            now,
            format!("Dropped duplicate track #{dup_n} on {}", now.net_name(item.net)),
            FrameOverlays {
                important: true,
                ripped,
                focus: json!({"net": item.net, "at": xy(at)}),
                stats: vec![("doubled".into(), json!(dup_n)), ("pruned".into(), json!(0)), ("open".into(), json!(open0))],
                ..Default::default()
            },
        );
    });

    let mut prune_n = 0usize;
    let (pruned, _) = grid::prune_dangling_watch(&mut board, &mut soft, &mut |now, item| {
        prune_n += 1;
        let ripped = fb.intern_items(now, std::slice::from_ref(item));
        let bb = item.shape.bbox();
        let at = Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2);
        let what = if matches!(item.kind, Kind::Via) { "single-layer via" } else { "dangling stub" };
        fb.push_frame(
            now,
            format!("Pruned {what} #{prune_n} on {}", now.net_name(item.net)),
            FrameOverlays {
                ripped,
                focus: json!({"net": item.net, "at": xy(at)}),
                stats: vec![("doubled".into(), json!(doubled)), ("pruned".into(), json!(prune_n)), ("open".into(), json!(open0))],
                ..Default::default()
            },
        );
    });

    let end_copper = fb.snapshot_copper(&board);
    let (gone_total, _) = diff_pools(&init_copper, &end_copper);
    let open_end = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!("Cleanup complete: dropped {doubled} duplicates, pruned {pruned} dangling pieces; open {open0} -> {open_end}"),
        FrameOverlays {
            important: true,
            summary: true,
            ripped: gone_total,
            stats: vec![
                ("doubled".into(), json!(doubled)),
                ("pruned".into(), json!(pruned)),
                ("open before".into(), json!(open0)),
                ("open after".into(), json!(open_end)),
            ],
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

// --- 14. Topo ------------------------------------------------------------------------------

fn run_topo(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.has("--pours-block");
    let strip_all = cli.has("--strip-all");
    let rip_passes: usize = cli.flag("--rip-passes").map_or(4, |s| s.parse().expect("--rip-passes"));
    let max_s: Option<f64> = cli.flag("--seconds").map(|s| s.parse().expect("--seconds")).or(Some(35.0));
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    if prepared(cli, &mut board, pours_block).is_none() && strip_all {
        strip_unlocked(&mut board, true);
    }
    let window = cli.crop_override().unwrap_or_else(|| board_bounds(&board));
    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "topo",
            out_path: &out_path,
            board_path,
            cropped: cli.crop_override().is_some(),
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| "Topological rubber-band router (topo)".to_string()),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| format!("Delaunay channel search + funnel shortest paths, {rip_passes} rip-up passes")),
        },
        &board,
        window,
        pours_block,
    );

    let init_copper = fb.snapshot_copper(&board);
    let open0 = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!("Start of topological routing: {open0} open connections"),
        FrameOverlays {
            initial: true,
            important: true,
            stats: vec![("connections".into(), json!(open0)), ("routed".into(), json!(0))],
            ..Default::default()
        },
    );

    let latest_act: Arc<Mutex<(String, String, Option<grid::Focus>)>> =
        Arc::new(Mutex::new((String::new(), String::new(), None)));
    let shared = latest_act.clone();
    let log = grid::Log(Arc::new(move |act| {
        if matches!(act.phase, "routing" | "ripping up") {
            let mut g = shared.lock().unwrap();
            let focus = act.focus.or_else(|| g.2.clone());
            *g = (act.phase.to_string(), act.text, focus);
        }
    }));

    let mut prev = init_copper.clone();
    let mut step_n = 0usize;
    let t_stats = topo::route_topological(
        &mut board,
        &topo::Options {
            max_seconds: max_s,
            rip_passes,
            pours_block,
            log: Some(log),
            ..Default::default()
        },
        &mut |now| {
            step_n += 1;
            let (phase, text, focus) = latest_act.lock().unwrap().clone();
            let is_rip_phase = phase == "ripping up";
            if !is_rip_phase && step_n > 12 && !step_n.is_multiple_of(4) {
                return;
            }
            let cur = fb.snapshot_copper(now);
            let (gone, came) = diff_pools(&prev, &cur);
            prev = cur;
            let open_now = open_airlines(now, pours_block).len();
            let is_rip = is_rip_phase || !gone.is_empty();
            let f_json = match focus {
                Some(grid::Focus::Connection { net, from, to }) => json!({"net": net, "from": xy(from), "to": xy(to)}),
                _ => Value::Null,
            };
            fb.push_frame(
                now,
                format!("Topo #{step_n} ({phase}): {text}; {open_now} open"),
                FrameOverlays {
                    important: is_rip,
                    ripped: gone,
                    laid: came,
                    focus: f_json,
                    stats: vec![
                        ("step".into(), json!(step_n)),
                        ("mode".into(), json!(if is_rip { "rip-up" } else { "rubber band" })),
                        ("open".into(), json!(open_now)),
                    ],
                    ..Default::default()
                },
            );
        },
    );

    let end_copper = fb.snapshot_copper(&board);
    let (_, total_laid) = diff_pools(&init_copper, &end_copper);
    let open_end = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!(
            "Topological router complete: {} of {} routed ({} rip-ups, {} vias); {open_end} open",
            t_stats.routed, t_stats.connections, t_stats.rip_ups, t_stats.vias
        ),
        FrameOverlays {
            important: true,
            summary: true,
            laid: total_laid,
            stats: vec![
                ("routed".into(), json!(t_stats.routed)),
                ("rip-ups".into(), json!(t_stats.rip_ups)),
                ("vias".into(), json!(t_stats.vias)),
                ("open".into(), json!(open_end)),
            ],
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

// --- 15. Prefanout -------------------------------------------------------------------------

fn focus_json(board: &Board, focus: Option<&grid::Focus>, failed: bool) -> Value {
    match focus {
        Some(grid::Focus::Connection { net, from, to }) if from == to => json!({"net": net, "at": xy(*from), "failed": failed}),
        Some(grid::Focus::Connection { net, from, to }) => json!({"net": net, "from": xy(*from), "to": xy(*to), "failed": failed}),
        Some(grid::Focus::Part(fp)) => json!({"at": xy(part_centre(board, *fp)), "failed": failed}),
        _ => Value::Null,
    }
}

fn run_prefanout(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.has("--pours-block");
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    // From bare pads, as the router starts, unless --prepare says otherwise.
    if prepared(cli, &mut board, pours_block).is_none() {
        strip_unlocked(&mut board, true);
    }
    let parts: Vec<usize> = match cli.flag("--part").as_deref() {
        None | Some("auto") => morph::fine_pitch(&board, 500_000),
        Some(list) => list
            .split(',')
            .map(|r| board.footprints.iter().position(|f| f.reference == r).unwrap_or_else(|| panic!("no part {r}")))
            .collect(),
    };
    let names: Vec<String> = parts.iter().map(|&f| board.footprints[f].reference.clone()).collect();
    let window = cli.crop_override().unwrap_or_else(|| {
        parts.iter().filter_map(|&f| pad_box_of(&board, f)).reduce(BBox::union).map_or_else(|| board_bounds(&board), |b| b.grow(2_500_000))
    });
    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "prefanout",
            out_path: &out_path,
            board_path,
            cropped: true,
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| "Pre-fanout: escape fine-pitch pins to vias".to_string()),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| format!("{} parts ({}), pad by pad before any routing", parts.len(), names.join(", "))),
        },
        &board,
        window,
        pours_block,
    );

    let init_copper = fb.snapshot_copper(&board);
    let open0 = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!("Bare pads: {open0} open connections; {} fine-pitch parts to escape", parts.len()),
        FrameOverlays {
            initial: true,
            important: true,
            stats: vec![("parts".into(), json!(parts.len())), ("escaped".into(), json!(0)), ("open".into(), json!(open0))],
            ..Default::default()
        },
    );

    let latest: Arc<Mutex<(String, Option<grid::Focus>)>> = Arc::new(Mutex::new((String::new(), None)));
    let shared = latest.clone();
    let log = grid::Log(Arc::new(move |act| {
        if act.phase == "prefanout" {
            *shared.lock().unwrap() = (act.text, act.focus);
        }
    }));
    // A pair whose ends share a layer is left for its coupled run, as the router does with --diff-pairs.
    let skip_nets = grid::diff_pairs(&board).iter().filter(|p| grid::shares_layer(&board, p)).flat_map(|p| [p.p, p.n]).collect();
    let mut prev = init_copper.clone();
    let mut step_n = 0usize;
    let mut escaped_n = 0usize;
    let stats = grid::prefanout(
        &mut board,
        &grid::PrefanoutOptions { parts: parts.clone(), skip_nets, pours_block, log: Some(log), ..Default::default() },
        &mut |now| {
            step_n += 1;
            let cur = fb.snapshot_copper(now);
            let (gone, laid) = diff_pools(&prev, &cur);
            prev = cur;
            let (text, focus) = latest.lock().unwrap().clone();
            let bridged = text.ends_with("preparing escapes");
            if !bridged {
                escaped_n += 1;
            }
            let caption = if bridged {
                format!("{}: pads bridged inward to their pours and neighbours ({} pieces)", text.trim_end_matches(": preparing escapes"), laid.len())
            } else {
                format!("Escape #{escaped_n}: {text} ({} pieces)", laid.len())
            };
            let vias = laid.iter().filter(|&&i| fb.pool[i as usize]["via"].as_bool() == Some(true)).count();
            let open_now = open_airlines(now, pours_block).len();
            fb.push_frame(
                now,
                format!("{caption}; {open_now} open"),
                FrameOverlays {
                    important: bridged,
                    ripped: gone,
                    laid,
                    focus: focus_json(now, focus.as_ref(), false),
                    stats: vec![
                        ("escaped".into(), json!(escaped_n)),
                        ("vias now".into(), json!(vias)),
                        ("open".into(), json!(open_now)),
                    ],
                    ..Default::default()
                },
            );
        },
    );

    let end_copper = fb.snapshot_copper(&board);
    let (_, total_laid) = diff_pools(&init_copper, &end_copper);
    let open_end = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!(
            "Pre-fanout done: {} of {} pads escaped ({} failed), {} inward bridges, {} vias; {open0} -> {open_end} open",
            stats.escaped, stats.candidates, stats.failed, stats.bridged_inward, stats.vias
        ),
        FrameOverlays {
            important: true,
            summary: true,
            laid: total_laid,
            stats: vec![
                ("escaped".into(), json!(stats.escaped)),
                ("failed".into(), json!(stats.failed)),
                ("vias".into(), json!(stats.vias)),
                ("open".into(), json!(open_end)),
            ],
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

// --- 16. Pairs -----------------------------------------------------------------------------

fn run_pairs(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.has("--pours-block");
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    if prepared(cli, &mut board, pours_block).is_none() {
        strip_unlocked(&mut board, true);
    }
    let pairs = grid::diff_pairs(&board);
    let names: Vec<String> = pairs.iter().map(|p| format!("{}/{}", board.net_name(p.p), board.net_name(p.n))).collect();
    let window = cli.crop_override().unwrap_or_else(|| board_bounds(&board));
    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "pairs",
            out_path: &out_path,
            board_path,
            cropped: cli.crop_override().is_some(),
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| "Differential pairs routed coupled".to_string()),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| format!("{}: one centreline per run between pin pairs, a track laid each side", names.join(", "))),
        },
        &board,
        window,
        pours_block,
    );

    let init_copper = fb.snapshot_copper(&board);
    let pair_nets: HashSet<u32> = pairs.iter().flat_map(|p| [p.p, p.n]).collect();
    let pair_open = |b: &Board| open_airlines(b, pours_block).iter().filter(|a| pair_nets.contains(&a.net)).count();
    let open0 = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!("{} pairs found ({}); {} of their connections open, {open0} in all", pairs.len(), names.join(", "), pair_open(&board)),
        FrameOverlays {
            initial: true,
            important: true,
            stats: vec![("pairs".into(), json!(pairs.len())), ("pair open".into(), json!(pair_open(&board))), ("open".into(), json!(open0))],
            ..Default::default()
        },
    );

    let mut prev = init_copper.clone();
    let mut runs: Vec<Value> = Vec::new();
    let mut run_n = 0usize;
    let stats = grid::route_pairs_watch(&mut board, &grid::PairOptions { pours_block, ..Default::default() }, &mut |now, run| {
        run_n += 1;
        let cur = fb.snapshot_copper(now);
        let (_, laid) = diff_pools(&prev, &cur);
        prev = cur;
        let name = format!("{}/{}", now.net_name(run.pair.p), now.net_name(run.pair.n));
        let length: f64 = run.centreline.windows(2).map(|w| w[0].distance(w[1])).sum();
        if let Some(layer) = run.layer {
            runs.push(json!({
                "net": run.pair.p,
                "kind": "centreline",
                "points": run.centreline.iter().map(|p| json!([layer, xy(*p)[0], xy(*p)[1]])).collect::<Vec<_>>(),
            }));
        }
        let caption = match run.layer {
            Some(layer) => format!(
                "Run #{run_n} {name}: coupled on {} for {:.2} mm, {:.2} mm pitch ({} pieces)",
                now.layers[layer],
                length * 1e-6,
                run.pair.pitch() as f64 * 1e-6,
                run.laid.len()
            ),
            None => format!("Run #{run_n} {name}: no coupled run lays cleanly between these pin pairs; left to the router"),
        };
        fb.push_frame(
            now,
            format!("{caption}; {} pair connections open", pair_open(now)),
            FrameOverlays {
                important: true,
                laid,
                paths: runs.clone(),
                focus: json!({"net": run.pair.p, "from": xy(run.from), "to": xy(run.to), "failed": run.layer.is_none()}),
                stats: vec![
                    ("pair".into(), json!(name)),
                    ("run mm".into(), json!((length * 1e-4).round() / 100.0)),
                    ("pair open".into(), json!(pair_open(now))),
                ],
                ..Default::default()
            },
        );
    });

    let end_copper = fb.snapshot_copper(&board);
    let (_, total_laid) = diff_pools(&init_copper, &end_copper);
    let open_end = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!(
            "Pairs done: {} of {} runs coupled, {:.1} mm along their centrelines; the legs to the pads are left to the router ({open_end} open)",
            stats.coupled, stats.runs, stats.coupled_mm
        ),
        FrameOverlays {
            important: true,
            summary: true,
            laid: total_laid,
            paths: runs,
            stats: vec![
                ("pairs".into(), json!(stats.pairs)),
                ("coupled".into(), json!(format!("{} / {}", stats.coupled, stats.runs))),
                ("coupled mm".into(), json!((stats.coupled_mm * 100.0).round() / 100.0)),
                ("open".into(), json!(open_end)),
            ],
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

// --- 17. Tune ------------------------------------------------------------------------------

fn run_tune(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.has("--pours-block");
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    let mut soft = prepared(cli, &mut board, pours_block).unwrap_or_else(|| adopt_soft(&mut board));
    let pairs = grid::diff_pairs(&board);
    let pair_nets: HashSet<u32> = pairs.iter().flat_map(|p| [p.p, p.n]).collect();
    let window = cli.crop_override().unwrap_or_else(|| {
        board
            .items()
            .filter(|(_, it)| pair_nets.contains(&it.net) && matches!(it.kind, Kind::Track | Kind::Via | Kind::Pad { .. }))
            .map(|(_, it)| it.shape.bbox())
            .reduce(BBox::union)
            .map_or_else(|| board_bounds(&board), |b| b.grow(1_500_000))
    });
    let skew_of = |b: &Board, p: &grid::Pair| grid::pair_lengths(b, p).map(|((lp, _), (ln, _))| (lp - ln).abs() * 1e-6);
    let name_of = |b: &Board, p: &grid::Pair| format!("{}/{}", b.net_name(p.p), b.net_name(p.n));
    let skews0: Vec<String> = pairs
        .iter()
        .map(|p| skew_of(&board, p).map_or_else(|| format!("{} not joined end to end", name_of(&board, p)), |s| format!("{} {s:.2} mm", name_of(&board, p))))
        .collect();
    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "tune",
            out_path: &out_path,
            board_path,
            cropped: true,
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| "Length matching: bumps on the shorter net".to_string()),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| format!("skew before: {}; tolerance {:.2} mm", skews0.join(", "), grid::tune::TOLERANCE * 1e-6)),
        },
        &board,
        window,
        pours_block,
    );

    let init_copper = fb.snapshot_copper(&board);
    let open0 = open_airlines(&board, pours_block).len();
    fb.push_frame(
        &board,
        format!("Settled copper, pairs apart in length by {}; {open0} open", skews0.join(", ")),
        FrameOverlays {
            initial: true,
            important: true,
            stats: vec![("pairs".into(), json!(pairs.len())), ("bumps".into(), json!(0)), ("open".into(), json!(open0))],
            ..Default::default()
        },
    );

    let mut bump_n = 0usize;
    let stats = grid::match_lengths_watch(&mut board, &pairs, &mut soft, &mut |now, bump| {
        bump_n += 1;
        let ripped = fb.intern_items(now, std::slice::from_ref(bump.replaced));
        let laid: Vec<u32> = bump.laid.iter().filter_map(|&id| now.item(id).map(|it| fb.intern(now, id, it))).collect();
        let bb = bump.replaced.shape.bbox();
        let at = Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2);
        let name = name_of(now, bump.pair);
        fb.push_frame(
            now,
            format!(
                "{name} bump #{bump_n} on {}: skew {:.3} -> {:.3} mm (+{:.3} mm of track)",
                now.net_name(bump.net),
                bump.before * 1e-6,
                bump.after * 1e-6,
                (bump.before - bump.after) * 1e-6
            ),
            FrameOverlays {
                important: true,
                ripped,
                laid,
                focus: json!({"net": bump.net, "at": xy(at)}),
                stats: vec![
                    ("pair".into(), json!(name)),
                    ("bump".into(), json!(bump_n)),
                    ("skew mm".into(), json!((bump.after * 1e-3).round() / 1000.0)),
                    ("open".into(), json!(open_airlines(now, pours_block).len())),
                ],
                ..Default::default()
            },
        );
    });

    let end_copper = fb.snapshot_copper(&board);
    let (gone_total, came_total) = diff_pools(&init_copper, &end_copper);
    let open_end = open_airlines(&board, pours_block).len();
    let skews: Vec<String> = stats.skews.iter().map(|(n, b, a)| format!("{n} {b:.2} -> {a:.2} mm")).collect();
    fb.push_frame(
        &board,
        format!("Tuned with {} bumps: {}; {open0} -> {open_end} open", stats.bumps, skews.join(", ")),
        FrameOverlays {
            important: true,
            summary: true,
            ripped: gone_total,
            laid: came_total,
            stats: stats
                .skews
                .iter()
                .map(|(n, _, a)| (n.clone(), json!(format!("{a:.2} mm"))))
                .chain([("bumps".to_string(), json!(stats.bumps)), ("open".to_string(), json!(open_end))])
                .collect(),
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

// --- 18. Fill ------------------------------------------------------------------------------

fn run_fill(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.has("--pours-block");
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));

    let (mut board, text) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    prepared(cli, &mut board, pours_block);
    let (_, copper) = kicad_pcb::parse_board(&text).unwrap_or_else(|e| panic!("{board_path}: {e}"));
    let window = cli.crop_override().unwrap_or_else(|| board_bounds(&board));
    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "fill",
            out_path: &out_path,
            board_path,
            cropped: cli.crop_override().is_some(),
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| "Zone fills as KiCad would pour them".to_string()),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| format!("board::fill_zones on a {} µm raster, highest priority first", board::fill::STEP / 1000)),
        },
        &board,
        window,
        pours_block,
    );
    // The fills the file holds are stale once the router has moved copper; only fresh ones show.
    fb.pours.clear();

    let started = Instant::now();
    let fills = board::fill_zones(&board, &copper);
    let secs = started.elapsed().as_secs_f64();
    let zone_name = |z: usize| {
        let zone = &copper.zones[z];
        if zone.name.is_empty() { format!("zone {z}") } else { zone.name.clone() }
    };
    let area = |f: &board::Fill| f.rects.iter().map(|r| (r[2] - r[0]) as f64 * (r[3] - r[1]) as f64).sum::<f64>() * 1e-12;
    fb.extra.insert(
        "fills".into(),
        json!(fills
            .iter()
            .map(|f| json!({
                "zone": f.zone,
                "name": zone_name(f.zone),
                "net": f.net,
                "layer": f.layer,
                "r": f.rects.iter().flat_map(|r| [mm(r[0] as f64), mm(r[1] as f64), mm(r[2] as f64), mm(r[3] as f64)]).collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>()),
    );

    let open0 = open_airlines(&board, pours_block).len();
    let zones = fills.iter().map(|f| f.zone).collect::<HashSet<_>>().len();
    fb.push_frame(
        &board,
        format!("Routed board, zones unpoured: {zones} zones to fill on {} layer pours; {open0} open without pours", fills.len()),
        FrameOverlays {
            initial: true,
            important: true,
            stats: vec![("zones".into(), json!(zones)), ("poured".into(), json!(0)), ("open".into(), json!(open0))],
            ..Default::default()
        },
    );
    for (k, f) in fills.iter().enumerate() {
        fb.push_frame(
            &board,
            format!(
                "Poured {} ({}) on {}: {:.1} mm² in {} rectangles",
                zone_name(f.zone),
                board.net_name(f.net),
                board.layers[f.layer],
                area(f),
                f.rects.len()
            ),
            FrameOverlays {
                important: true,
                fills: (0..=k).collect(),
                fill_new: Some(k),
                stats: vec![
                    ("zone".into(), json!(zone_name(f.zone))),
                    ("layer".into(), json!(board.layers[f.layer])),
                    ("mm²".into(), json!((area(f) * 10.0).round() / 10.0)),
                    ("poured".into(), json!(format!("{} / {}", k + 1, fills.len()))),
                ],
                ..Default::default()
            },
        );
    }
    let total: f64 = fills.iter().map(area).sum();
    fb.push_frame(
        &board,
        format!("All {} pours filled in {secs:.2} s: {total:.0} mm² of copper over {zones} zones", fills.len()),
        FrameOverlays {
            important: true,
            summary: true,
            fills: (0..fills.len()).collect(),
            stats: vec![
                ("pours".into(), json!(fills.len())),
                ("mm²".into(), json!(total.round())),
                ("seconds".into(), json!((secs * 100.0).round() / 100.0)),
                ("open".into(), json!(open0)),
            ],
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

fn main() {
    let mut raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let sub = raw.remove(0);
    let cli = Cli { args: raw };
    match sub.as_str() {
        "dipole" => run_dipole(&cli),
        "push" => run_push(&cli),
        "flow" => run_flow(&cli),
        "comb" => run_comb(&cli),
        "clear-lane" => run_clear_lane(&cli),
        "shove" => run_shove(&cli),
        "nudge" => run_nudge(&cli),
        "negotiate" => run_negotiate(&cli),
        "route" => run_route(&cli),
        "rip-up" => run_rip_up(&cli),
        "chase" => run_chase(&cli),
        "tighten" => run_tighten(&cli),
        "cleanup" => run_cleanup(&cli),
        "topo" => run_topo(&cli),
        "prefanout" => run_prefanout(&cli),
        "pairs" => run_pairs(&cli),
        "tune" => run_tune(&cli),
        "fill" => run_fill(&cli),
        "index" => {
            let pos = cli.positional();
            assert!(!pos.is_empty(), "usage: feature_film index FILMS_DIR");
            write_films_index(Path::new(&pos[0]));
        }
        other => {
            eprintln!("unknown feature `{other}`\n{USAGE}");
            std::process::exit(2);
        }
    }
}
