//! A film of a router stage for `lab/features.html`: the static board geometry in its window
//! (outline, footprints with courtyards, pads and pours), an interned pool of copper shapes, and
//! a capped sequence of frames, each with incremental `add`/`del` indices into the pool, step
//! highlights, moved parts, open airlines, overlays and a caption.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use board::{check_item, check_item_with_pours, connectivity, connectivity_without_pours, Airline, BBox, Board, Item, ItemId, Kind, Nm, Point, Shape};
use serde_json::{json, Value};

/// How the board was brought to the filmed stage, for the film's board label.
pub(crate) static PREPARED: OnceLock<String> = OnceLock::new();

pub(crate) fn mm(v: f64) -> f64 {
    (v * 1e-6 * 1e4).round() / 1e4
}

pub(crate) fn xy(p: Point) -> [f64; 2] {
    [mm(p.x as f64), mm(p.y as f64)]
}

pub(crate) fn nm_mm(v: f64) -> Nm {
    (v * 1e6).round() as Nm
}

pub(crate) fn shape_json(s: &Shape) -> Value {
    match s {
        Shape::Capsule { a, b, r } => json!({"a": xy(*a), "b": xy(*b), "r": mm(*r as f64)}),
        Shape::Polygon(p) => json!({"poly": p.iter().map(|q| xy(*q)).collect::<Vec<_>>()}),
    }
}

pub(crate) fn layers_of(board: &Board, it: &Item) -> Vec<usize> {
    (0..board.layers.len()).filter(|l| it.on(*l)).collect()
}

pub(crate) fn open_airlines(board: &Board, pours_block: bool) -> Vec<Airline> {
    if pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines
}

pub(crate) fn airline_json(board: &Board, a: &Airline) -> Value {
    json!({
        "net": a.net,
        "name": board.net_name(a.net),
        "from": xy(a.from),
        "to": xy(a.to),
        "items": [a.from_item, a.to_item],
    })
}

pub(crate) fn board_bounds(board: &Board) -> BBox {
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

pub(crate) fn pad_box_of(board: &Board, fp: usize) -> Option<BBox> {
    board
        .items()
        .filter(|(_, it)| matches!(it.kind, Kind::Pad { footprint, .. } if footprint == fp))
        .map(|(_, it)| it.shape.bbox())
        .reduce(BBox::union)
}

pub(crate) fn part_centre(board: &Board, fp: usize) -> Point {
    pad_box_of(board, fp)
        .map(|b| Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2))
        .unwrap_or(board.footprints[fp].at)
}

pub(crate) fn adopt_soft(board: &mut Board) -> Vec<ItemId> {
    board
        .items()
        .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked)
        .map(|(id, _)| id)
        .collect()
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct PoolKey {
    pub(crate) net: u32,
    pub(crate) via: bool,
    pub(crate) layers: u64,
    pub(crate) coords: Vec<Nm>,
}

impl PoolKey {
    pub(crate) fn of(it: &Item) -> Self {
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
pub(crate) struct FrameOverlays {
    pub(crate) important: bool,
    pub(crate) initial: bool,
    pub(crate) summary: bool,
    pub(crate) moved_from: Vec<u32>,
    pub(crate) moved_to: Vec<u32>,
    pub(crate) ripped: Vec<u32>,
    pub(crate) laid: Vec<u32>,
    pub(crate) moved_parts: Vec<Value>,
    pub(crate) arrows: Vec<Value>,
    pub(crate) paths: Vec<Value>,
    pub(crate) corridors: Vec<Value>,
    pub(crate) poles: Vec<Value>,
    pub(crate) heat: Value,
    pub(crate) focus: Value,
    pub(crate) stats: Vec<(String, Value)>,
    /// Zone fills (indices into the film's `fills`) shown, and the one poured this frame.
    pub(crate) fills: Vec<usize>,
    pub(crate) fill_new: Option<usize>,
}

pub(crate) struct RawFrame {
    pub(crate) caption: String,
    pub(crate) copper: Vec<u32>,
    pub(crate) part_offsets: Vec<Value>,
    pub(crate) open: Vec<Value>,
    pub(crate) overlays: FrameOverlays,
}

pub(crate) struct FilmBuilder {
    pub(crate) feature: String,
    pub(crate) name: String,
    pub(crate) title: String,
    pub(crate) subtitle: String,
    pub(crate) board_path: String,
    pub(crate) cropped: bool,
    pub(crate) window: BBox,
    pub(crate) pours_block: bool,
    pub(crate) max_frames: usize,
    pub(crate) layers: Vec<String>,
    pub(crate) nets: Vec<String>,
    pub(crate) outline: Vec<Vec<[f64; 2]>>,
    pub(crate) footprints: Vec<Value>,
    pub(crate) initial_fp_at: Vec<Point>,
    pub(crate) pads: Vec<Value>,
    pub(crate) pours: Vec<Value>,
    pub(crate) pool: Vec<Value>,
    pub(crate) pool_index: HashMap<PoolKey, u32>,
    pub(crate) frames: Vec<RawFrame>,
    pub(crate) started: Instant,
    /// Further top-level fields of the film, such as a fill film's `fills`.
    pub(crate) extra: serde_json::Map<String, Value>,
}

/// What a film is called and where it goes: the part of `FilmBuilder::new`'s inputs that is not the board.
pub(crate) struct FilmSpec<'a> {
    pub(crate) feature: &'a str,
    pub(crate) out_path: &'a Path,
    pub(crate) board_path: &'a str,
    pub(crate) cropped: bool,
    pub(crate) max_frames: usize,
    pub(crate) title: String,
    pub(crate) subtitle: String,
}

impl FilmBuilder {
    pub(crate) fn new(spec: FilmSpec<'_>, board: &Board, window: BBox, pours_block: bool) -> Self {
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

    pub(crate) fn intern(&mut self, board: &Board, id: ItemId, it: &Item) -> u32 {
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

    pub(crate) fn intern_items(&mut self, board: &Board, items: &[Item]) -> Vec<u32> {
        let pad_win = self.window.grow(500_000);
        items
            .iter()
            .filter(|it| matches!(it.kind, Kind::Track | Kind::Via) && it.shape.bbox().overlaps(pad_win))
            .map(|it| self.intern(board, usize::MAX, it))
            .collect()
    }

    pub(crate) fn snapshot_copper(&mut self, board: &Board) -> Vec<u32> {
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

    pub(crate) fn part_offsets(&self, board: &Board) -> Vec<Value> {
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

    pub(crate) fn push_frame(&mut self, board: &Board, caption: impl Into<String>, overlays: FrameOverlays) {
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

    pub(crate) fn decimate(&mut self) {
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

    pub(crate) fn write(mut self, out: &Path) {
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
    pub(crate) fn clone_raw(&self) -> Self {
        RawFrame {
            caption: self.caption.clone(),
            copper: self.copper.clone(),
            part_offsets: self.part_offsets.clone(),
            open: self.open.clone(),
            overlays: self.overlays.clone(),
        }
    }
}

pub(crate) fn write_films_index(films_dir: &Path) {
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

pub(crate) fn diff_pools(before: &[u32], after: &[u32]) -> (Vec<u32>, Vec<u32>) {
    let b: HashSet<u32> = before.iter().copied().collect();
    let a: HashSet<u32> = after.iter().copied().collect();
    let mut gone: Vec<u32> = b.difference(&a).copied().collect();
    let mut came: Vec<u32> = a.difference(&b).copied().collect();
    gone.sort_unstable();
    came.sort_unstable();
    (gone, came)
}

pub(crate) fn sample_arrows(window: BBox, grid_n: usize, eval: impl Fn(Point) -> (f64, f64)) -> Vec<Value> {
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

pub(crate) fn count_violations(board: &Board, pours_block: bool) -> usize {
    board
        .items()
        .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked)
        .filter(|(id, _)| {
            let v = if pours_block { check_item_with_pours(board, *id) } else { check_item(board, *id) };
            !v.is_empty()
        })
        .count()
}

