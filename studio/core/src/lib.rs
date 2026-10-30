//! The board as the live stream describes it, and what the router is doing to it. The board
//! itself is a board-draw scene, which meshes it and picks in it; this adds the run's state
//! (trials, airlines, probes, congestion, focus) and draws it over the board. Shared by the web
//! client (through WASM) and the native one.

use std::collections::{BTreeMap, HashMap, VecDeque};

use board_draw::{AreaKind, Fill, Geom, Item, Mesh, Pose, Prim, circle_points, with_alpha};
use serde_json::Value;

pub use board_draw::{Rgba, STRIDE};

#[cfg(feature = "wasm")]
pub mod wasm;

const FOCUS: Rgba = [1.0, 1.0, 1.0, 1.0];
const COLLIDING: Rgba = [1.0, 0.55, 0.12, 0.95];
/// A part being tried somewhere by the nudger, before the move is judged.
const GHOST: Rgba = [0.55, 0.85, 1.0, 0.9];
const TRIAL_VIA: Rgba = [1.0, 1.0, 1.0, 0.75];
/// Drawn size of a via a negotiated route would need, in millimetres.
const TRIAL_VIA_MM: f32 = 0.2;
/// Congestion below this is not drawn; a change smaller than `HEAT_CHANGE` does not pop.
const HEAT_FLOOR: f32 = 0.15;
const HEAT_CHANGE: f32 = 0.05;
const HEAT_FADE_MS: f64 = 1500.0;

const EVENTS_KEPT: usize = 200;

const AIRLINE: Rgba = [0.96, 1.0, 0.84, 0.6];
const PROBE: Rgba = [1.0, 0.85, 0.2, 0.16];
const DOOR: Rgba = [1.0, 0.85, 0.2, 0.9];
const PICKED: Rgba = [1.0, 1.0, 1.0, 0.9];
const SELECTED: Rgba = [0.51, 0.77, 0.69, 1.0];
const LOCKED: Rgba = [1.0, 0.95, 0.6, 0.95];
/// Side of a congestion cell, in millimetres.
pub const CELL_MM: f32 = 1.0;
/// The congestion map is at most this many cells a side, however large or broken the board's
/// bounds, so a stray coordinate cannot ask for gigabytes every frame.
pub const CELLS_MAX: usize = 1000;
/// Share of a cell's area on one layer that copper can fill once clearances are kept.
const ROUTABLE_SHARE: f32 = 0.5;
/// Width an open connection will take once routed, track and clearance, in millimetres.
const AIRLINE_WIDTH_MM: f32 = 0.35;
/// Width a trial route is counted at, track and clearance, in millimetres.
const TRIAL_WIDTH_MM: f32 = 0.3;

#[derive(Clone, Debug)]
pub struct Part {
    pub id: i32,
    pub name: String,
    pub at: [f32; 2],
    pub fixed: bool,
}

/// A route the router is trying on one layer, and whether it collided with another net's.
#[derive(Clone, Debug, PartialEq)]
pub struct Trial {
    pub layer: usize,
    pub net: i32,
    pub points: Vec<f32>,
    pub collides: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Focus {
    Connection { phase: String, net: i32, a: [f32; 2], b: [f32; 2] },
    /// A part in hand; `ghost`, where it is being tried, as an offset from where it stands.
    Part { phase: String, part: i32, ghost: Option<[f32; 2]> },
}

impl Focus {
    pub fn phase(&self) -> &str {
        match self {
            Focus::Connection { phase, .. } | Focus::Part { phase, .. } => phase,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub layer: Option<usize>,
    pub room: Vec<f32>,
    pub door: Vec<f32>,
}

#[derive(Default)]
pub struct Scene {
    /// Layers, nets, copper, parts' poses (a part dragged in the client is posed at its drag
    /// offset, its items staying in board coordinates) and how the board is styled.
    pub board: board_draw::Scene,
    pub bbox: [f32; 4],
    /// The parts as the router lists them.
    pub parts: BTreeMap<i32, Part>,
    pub airlines: Vec<[f32; 5]>,
    pub probes: Vec<Probe>,
    /// Routes a router is trying, until it sends the next ones.
    pub trials: Vec<Trial>,
    /// Where those routes change layer.
    pub trial_vias: Vec<[f32; 2]>,
    pub open: Option<usize>,
    pub seq: u64,
    pub ms: u64,
    pub events: VecDeque<String>,
    /// How many events have ever arrived; the oldest kept is number `event_total - events.len()`.
    pub event_total: u64,
    pub done: Option<Value>,
    pub picked: Option<i32>,
    pub selected: Option<i32>,
    pub show_congestion: bool,
    /// Each congestion cell's last value and when it changed, for popping in and fading out.
    pub heat_memory: std::cell::RefCell<HashMap<usize, (f32, f64)>>,
    /// Where the router says it is working.
    pub focus: Option<Focus>,
    /// Parts to light up, such as the locked ones while their list is hovered.
    pub highlight: Vec<i32>,
    /// Lock states set in the client but not yet seen by the router.
    pub lock_overrides: HashMap<i32, bool>,
}

fn f(v: &Value) -> f32 {
    v.as_f64().unwrap_or(0.0) as f32
}

fn floats(v: &Value) -> Vec<f32> {
    v.as_array().map(|a| a.iter().map(f).collect()).unwrap_or_default()
}

fn prim(v: &Value) -> Option<Prim> {
    if let Some(c) = v.get("c") {
        let c = floats(c);
        return (c.len() == 3).then(|| Prim::Circle([c[0], c[1], c[2]]));
    }
    v.get("g").map(|g| Prim::Poly(floats(g)))
}

fn layer(v: &Value) -> usize {
    v.as_u64().unwrap_or(0) as usize
}

/// An item of the stream as a board-draw item. Items arrive in board coordinates; a part's
/// stand where its pose (identity until dragged) puts them.
fn parse_item(v: &Value) -> Option<(u32, Item)> {
    let id = v.get("id")?.as_u64()? as u32;
    let net = v.get("net").and_then(Value::as_i64).unwrap_or(0) as i32;
    let part = v.get("part").and_then(Value::as_i64).unwrap_or(0) as i32;
    let at = |v: &Value| {
        let a = floats(&v["at"]);
        [a.first().copied().unwrap_or(0.0), a.get(1).copied().unwrap_or(0.0)]
    };
    let drill = v.get("drill").and_then(Value::as_f64).map(|d| d as f32);
    let (part, geom) = match v.get("k")?.as_str()? {
        "trace" => (0, Geom::Track { layer: layer(&v["layer"]), width: f(&v["w"]), points: floats(&v["p"]) }),
        "via" => (0, Geom::Via { at: at(v), shape: v.get("shape").and_then(prim), drill }),
        "pin" => (
            part,
            Geom::Pad {
                at: at(v),
                drill,
                pads: v["pads"]
                    .as_array()
                    .map(|pads| pads.iter().filter_map(|p| Some((layer(&p["layer"]), prim(&p["shape"])?))).collect())
                    .unwrap_or_default(),
            },
        ),
        "outline" => (0, Geom::Outline { polys: v["polys"].as_array().map(|p| p.iter().map(floats).collect()).unwrap_or_default() }),
        kind => (
            part,
            Geom::Area {
                kind: match kind {
                    "pour" => AreaKind::Pour,
                    "body" => AreaKind::Body,
                    "via-keepout" => AreaKind::ViaKeepout,
                    _ => AreaKind::Keepout,
                },
                tiles: v["tiles"]
                    .as_array()
                    .map(|t| t.iter().map(|t| (layer(&t["layer"]), floats(&t["g"]))).collect())
                    .unwrap_or_default(),
            },
        ),
    };
    Some((id, Item { net, part, geom }))
}

impl Scene {
    /// Applies one line of the stream. Returns whether anything drawn changed.
    pub fn apply(&mut self, line: &str) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        match v.get("t").and_then(Value::as_str) {
            Some("board") => {
                let hidden: Vec<bool> = self.board.layers().iter().map(|l| !l.visible).collect();
                let style = board_draw::Style { hot_net: None, ..self.board.style.clone() };
                let show_congestion = self.show_congestion;
                let highlight = std::mem::take(&mut self.highlight);
                let event_total = self.event_total;
                let events = std::mem::take(&mut self.events);
                *self = Scene::default();
                self.board.style = style;
                self.show_congestion = show_congestion;
                self.highlight = highlight;
                // The event count runs on across boards, so a watcher never sees one twice.
                self.event_total = event_total;
                self.events = events;
                let names: Vec<String> = v["layers"]
                    .as_array()
                    .map(|l| l.iter().map(|l| l["name"].as_str().unwrap_or("").to_string()).collect())
                    .unwrap_or_default();
                self.board.set_layers(&names);
                if hidden.len() == names.len() {
                    for (i, h) in hidden.into_iter().enumerate() {
                        self.board.set_layer_visible(i, !h);
                    }
                }
                if let Some(nets) = v["nets"].as_object() {
                    for (k, name) in nets {
                        if let Ok(n) = k.parse() {
                            self.board.nets.insert(n, name.as_str().unwrap_or("").to_string());
                        }
                    }
                }
                let b = floats(&v["bbox"]);
                if b.len() == 4 {
                    self.bbox = [b[0], b[1], b[2], b[3]];
                }
                true
            }
            Some("frame") => {
                self.seq = v["seq"].as_u64().unwrap_or(self.seq);
                self.ms = v["ms"].as_u64().unwrap_or(self.ms);
                for id in v["del"].as_array().into_iter().flatten() {
                    if let Some(id) = id.as_u64() {
                        self.board.remove(id as u32);
                    }
                }
                for item in v["add"].as_array().into_iter().flatten() {
                    if let Some((id, item)) = parse_item(item) {
                        self.board.insert(id, item);
                    }
                }
                for p in v["parts"].as_array().into_iter().flatten() {
                    let at = floats(&p["at"]);
                    let id = p["id"].as_i64().unwrap_or(0) as i32;
                    let name = p["name"].as_str().unwrap_or("").to_string();
                    self.board.set_part(id, &name);
                    self.parts.insert(
                        id,
                        Part {
                            id,
                            name,
                            at: [at.first().copied().unwrap_or(0.0), at.get(1).copied().unwrap_or(0.0)],
                            fixed: p["fixed"].as_bool().unwrap_or(false),
                        },
                    );
                }
                self.probes = v["probes"]
                    .as_array()
                    .map(|p| {
                        p.iter()
                            .map(|p| Probe {
                                layer: p["layer"].as_u64().map(|l| l as usize),
                                room: floats(&p["room"]),
                                door: floats(&p["door"]),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for e in v["events"].as_array().into_iter().flatten() {
                    if self.events.len() == EVENTS_KEPT {
                        self.events.pop_front();
                    }
                    self.events.push_back(e.as_str().map_or_else(|| e.to_string(), str::to_string));
                    self.event_total += 1;
                }
                if let Some(f) = v.get("focus").filter(|f| f.is_object()) {
                    let phase = f["phase"].as_str().unwrap_or("").to_string();
                    self.focus = if let Some(part) = f["part"].as_i64() {
                        let ghost = floats(&f["ghost"]);
                        Some(Focus::Part { phase, part: part as i32, ghost: (ghost.len() == 2).then(|| [ghost[0], ghost[1]]) })
                    } else {
                        let (a, b) = (floats(&f["a"]), floats(&f["b"]));
                        (a.len() == 2 && b.len() == 2).then(|| Focus::Connection {
                            phase,
                            net: f["net"].as_i64().unwrap_or(0) as i32,
                            a: [a[0], a[1]],
                            b: [b[0], b[1]],
                        })
                    };
                }
                if let Some(trial) = v["trial"].as_array() {
                    self.trials = trial
                        .iter()
                        .map(|t| Trial {
                            layer: layer(&t["layer"]),
                            net: t["net"].as_i64().unwrap_or(0) as i32,
                            points: floats(&t["p"]),
                            collides: t["collides"].as_bool().unwrap_or(false),
                        })
                        .collect();
                    self.trial_vias = v["trial_vias"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|p| {
                            let p = floats(p);
                            (p.len() == 2).then(|| [p[0], p[1]])
                        })
                        .collect();
                }
                if let Some(air) = v["air"].as_array() {
                    self.airlines = air
                        .iter()
                        .map(|a| {
                            let a = floats(a);
                            [a[0], a[1], a[2], a[3], a.get(4).copied().unwrap_or(0.0)]
                        })
                        .collect();
                    self.open = v["open"].as_u64().map(|n| n as usize);
                }
                true
            }
            Some("fill") => {
                // Zones the router poured around the copper, as KiCad would: rectangles
                // `x0, y0, x1, y1`, each drawn as a quad.
                let mut fills = Vec::new();
                for f in v["fills"].as_array().into_iter().flatten() {
                    let (net, layer) = (f["net"].as_i64().unwrap_or(0) as i32, layer(&f["layer"]));
                    for r in floats(&f["r"]).chunks_exact(4) {
                        fills.push(Fill { net, layer, polygon: vec![r[0], r[1], r[2], r[1], r[2], r[3], r[0], r[3]] });
                    }
                }
                self.board.set_fills(fills);
                true
            }
            Some("done") => {
                self.trials.clear();
                self.trial_vias.clear();
                self.focus = None;
                self.open = v["open"].as_u64().map(|n| n as usize).or(self.open);
                self.done = Some(v);
                true
            }
            _ => false,
        }
    }

    /// A part's drag offset.
    fn offset(&self, part: i32) -> [f32; 2] {
        let p = self.board.pose(part);
        [p.x, p.y]
    }

    /// Drags a part `dx, dy` from where the router has it.
    pub fn set_offset(&mut self, part: i32, dx: f32, dy: f32) {
        if part != 0 {
            self.board.set_pose(part, Pose::at(dx, dy));
        }
    }

    pub fn clear_offsets(&mut self) {
        self.board.reset_poses();
    }

    pub fn layer_color(&self, layer: usize) -> Rgba {
        self.board.layer_color(layer)
    }

    pub fn part_box(&self, part: i32) -> Option<[f32; 4]> {
        self.board.part_box(part)
    }

    pub fn part_at(&self, x: f32, y: f32) -> Option<i32> {
        self.board.part_at(x, y)
    }

    pub fn net_at(&self, x: f32, y: f32, reach: f32) -> Option<i32> {
        self.board.net_at(x, y, reach)
    }

    /// Triangles for everything shown, back to front, `STRIDE` floats per vertex. `px` is the
    /// size of one screen pixel in millimetres, for lines drawn a fixed number of pixels wide.
    pub fn mesh(&self, px: f32) -> Vec<f32> {
        self.mesh_with(px, u64::MAX, 0.0)
    }

    /// The mesh for the layers in `mask` only, at time `now` (milliseconds, for what animates):
    /// the board, then the run drawn over it.
    pub fn mesh_with(&self, px: f32, mask: u64, now: f64) -> Vec<f32> {
        let b = &self.board;
        let mut m = Mesh::with_capacity(px, b.items().count() * 64 * STRIDE);
        b.draw_into(&mut m, mask);
        let shown = |layer: usize| b.shown(layer, mask);
        if self.show_congestion {
            // A cell pops in when its value changes and fades out while it stays, so only what
            // the router is changing stands out.
            let c = self.congestion();
            let mut memory = self.heat_memory.borrow_mut();
            for (i, v) in c.values.iter().enumerate() {
                let changed = match memory.get(&i) {
                    Some((old, _)) => (old - v).abs() > HEAT_CHANGE,
                    None => *v >= HEAT_FLOOR,
                };
                if changed {
                    memory.insert(i, (*v, now));
                }
                let Some(&(_, at)) = memory.get(&i) else { continue };
                let age = ((now - at) / HEAT_FADE_MS).clamp(0.0, 1.0) as f32;
                if *v < HEAT_FLOOR || age >= 1.0 {
                    continue;
                }
                let fade = (1.0 - age) * (1.0 - age);
                let grow = CELL_MM * 0.25 * fade;
                let (x, y) = (c.x0 + (i % c.cols) as f32 * CELL_MM - grow, c.y0 + (i / c.cols) as f32 * CELL_MM - grow);
                let side = CELL_MM + 2.0 * grow;
                let color = heat(*v);
                m.fill(&[x, y, x + side, y, x + side, y + side, x, y + side], [0.0, 0.0], with_alpha(color, (color[3] + 0.3).min(0.9) * fade));
            }
        }
        // Routes still being negotiated: dashed in a pale version of their layer's colour, or
        // solid orange where they still fight another net for room; their vias as hollow rings.
        for t in &self.trials {
            if !shown(t.layer) {
                continue;
            }
            if t.collides {
                m.polyline(&t.points, px * 1.3, b.tint(COLLIDING, t.net));
            } else {
                let c = b.drawn_color(t.layer);
                let pale = [(c[0] + 0.6) / 1.6, (c[1] + 0.6) / 1.6, (c[2] + 0.6) / 1.6, 0.85];
                m.dashed_polyline(&t.points, px * 0.9, px * 5.0, b.tint(pale, t.net));
            }
        }
        for v in &self.trial_vias {
            m.ring(&circle_points(*v, TRIAL_VIA_MM), px * 0.8, TRIAL_VIA);
        }
        for a in &self.airlines {
            m.segment([a[0], a[1]], [a[2], a[3]], px * 0.6, b.tint(AIRLINE, a[4] as i32));
        }
        for probe in &self.probes {
            if probe.layer.is_some_and(|l| !shown(l)) {
                continue;
            }
            m.fill(&probe.room, [0.0, 0.0], PROBE);
            if probe.door.len() == 4 {
                m.segment([probe.door[0], probe.door[1]], [probe.door[2], probe.door[3]], px * 1.2, DOOR);
            }
        }
        for part in self.parts.values() {
            if self.is_locked(part.id)
                && b.part_shown(part.id, mask)
                && let Some(b) = self.part_box(part.id)
            {
                m.dashed_ring(&[b[0], b[1], b[2], b[1], b[2], b[3], b[0], b[3]], px * 0.9, px * 6.0, LOCKED);
                m.disc([b[2], b[3]], px * 4.0, LOCKED);
            }
        }
        if let Some(b) = self.selected.and_then(|p| self.part_box(p)) {
            let e = px * 3.0;
            m.ring(&[b[0] - e, b[1] - e, b[2] + e, b[1] - e, b[2] + e, b[3] + e, b[0] - e, b[3] + e], px * 1.5, SELECTED);
        }
        if let Some(b) = self.picked.and_then(|p| self.part_box(p)) {
            m.ring(&[b[0], b[1], b[2], b[1], b[2], b[3], b[0], b[3]], px, PICKED);
        }
        let pulse = 0.55 + 0.45 * ((now / 180.0).sin() as f32);
        for &part in &self.highlight {
            if let Some(b) = self.part_box(part) {
                let e = px * 5.0;
                m.ring(&[b[0] - e, b[1] - e, b[2] + e, b[1] - e, b[2] + e, b[3] + e, b[0] - e, b[3] + e], px * 2.0, with_alpha(LOCKED, pulse));
            }
        }
        match &self.focus {
            Some(Focus::Connection { a, b, .. }) => {
                m.segment(*a, *b, px * 2.2, with_alpha(FOCUS, 0.35 + 0.5 * pulse));
                for end in [a, b] {
                    m.ring(&circle_points(*end, px * (7.0 + 4.0 * pulse)), px * 1.4, with_alpha(FOCUS, pulse));
                }
            }
            Some(Focus::Part { part, ghost, .. }) => {
                if let Some(b) = self.part_box(*part) {
                    let e = px * (4.0 + 4.0 * pulse);
                    m.ring(&[b[0] - e, b[1] - e, b[2] + e, b[1] - e, b[2] + e, b[3] + e, b[0] - e, b[3] + e], px * 2.0, with_alpha(FOCUS, pulse));
                    if let Some(g) = ghost {
                        self.draw_ghost(&mut m, *part, *g, b, px);
                    }
                }
            }
            None => {}
        }
        m.into_vec()
    }

    /// A part where it is being tried: its pads outlined `by` from where it stands, a dashed box
    /// round them, and a dashed line from where it stands to there.
    fn draw_ghost(&self, m: &mut Mesh, part: i32, by: [f32; 2], from: [f32; 4], px: f32) {
        let o = self.offset(part);
        let (dx, dy) = (o[0] + by[0], o[1] + by[1]);
        for id in self.board.part_items(part) {
            let Some(Geom::Pad { pads, .. }) = self.board.item(id).map(|i| &i.geom) else { continue };
            for (_, p) in pads {
                match p {
                    Prim::Circle([x, y, r]) => m.ring(&circle_points([x + dx, y + dy], *r), px * 1.2, GHOST),
                    Prim::Poly(g) => {
                        let moved: Vec<f32> = g.chunks_exact(2).flat_map(|c| [c[0] + dx, c[1] + dy]).collect();
                        m.ring(&moved, px * 1.2, GHOST);
                    }
                }
            }
        }
        let e = px * 4.0;
        let b = [from[0] + by[0] - e, from[1] + by[1] - e, from[2] + by[0] + e, from[3] + by[1] + e];
        m.dashed_polyline(&[b[0], b[1], b[2], b[1], b[2], b[3], b[0], b[3], b[0], b[1]], px * 1.0, px * 4.0, GHOST);
        let c = [(from[0] + from[2]) / 2.0, (from[1] + from[3]) / 2.0];
        m.dashed_polyline(&[c[0], c[1], c[0] + by[0], c[1] + by[1]], px * 1.0, px * 3.0, GHOST);
    }

    pub fn is_locked(&self, part: i32) -> bool {
        self.lock_overrides.get(&part).copied().unwrap_or_else(|| self.parts.get(&part).is_some_and(|p| p.fixed))
    }

    /// How full each cell of the board is: copper and pads on the signal layers, plus what
    /// the open connections will need once routed, over what the cell can hold.
    pub fn congestion(&self) -> Congestion {
        let [x0, y0, x1, y1] = if self.bbox.iter().all(|v| v.is_finite()) { self.bbox } else { [0.0; 4] };
        let cells = |span: f32| ((span / CELL_MM).ceil() as usize).clamp(1, CELLS_MAX);
        let (cols, rows) = (cells(x1 - x0), cells(y1 - y0));
        let mut demand = vec![0.0f32; cols * rows];
        let mut add = |x: f32, y: f32, area: f32| {
            let (c, r) = (((x - x0) / CELL_MM).floor(), ((y - y0) / CELL_MM).floor());
            if c >= 0.0 && r >= 0.0 && (c as usize) < cols && (r as usize) < rows {
                demand[r as usize * cols + c as usize] += area;
            }
        };
        let line = |a: [f32; 2], b: [f32; 2], width: f32, add: &mut dyn FnMut(f32, f32, f32)| {
            let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
            let steps = ((len / (CELL_MM / 2.0)).ceil() as usize).clamp(1, 4 * CELLS_MAX);
            let step = len / steps as f32;
            for k in 0..steps {
                let t = (k as f32 + 0.5) / steps as f32;
                add(a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1]), width * step);
            }
        };
        let layers = self.board.layers().len().max(1);
        for (_, item) in self.board.items() {
            match &item.geom {
                Geom::Track { width, points, .. } => {
                    for w in points.windows(4).step_by(2) {
                        line([w[0], w[1]], [w[2], w[3]], *width, &mut add);
                    }
                }
                Geom::Via { shape: Some(shape), .. } => {
                    spread(shape, [0.0, 0.0], prim_area(shape) * layers as f32, &mut add);
                }
                Geom::Pad { pads, .. } => {
                    let o = self.offset(item.part);
                    for (_, p) in pads {
                        spread(p, o, prim_area(p), &mut add);
                    }
                }
                _ => {}
            }
        }
        for a in &self.airlines {
            line([a[0], a[1]], [a[2], a[3]], AIRLINE_WIDTH_MM, &mut add);
        }
        // The routes a router is trying count too, so the map shows where it is working.
        for t in &self.trials {
            for w in t.points.windows(4).step_by(2) {
                line([w[0], w[1]], [w[2], w[3]], TRIAL_WIDTH_MM, &mut add);
            }
        }
        let capacity = CELL_MM * CELL_MM * ROUTABLE_SHARE * layers as f32;
        Congestion { cols, rows, x0, y0, values: demand.into_iter().map(|d| d / capacity).collect() }
    }

    /// The part whose centre is nearest the point.
    pub fn nearest_part(&self, x: f32, y: f32) -> Option<&Part> {
        self.parts
            .values()
            .filter_map(|p| self.part_box(p.id).map(|b| (p, ((b[0] + b[2]) / 2.0 - x).powi(2) + ((b[1] + b[3]) / 2.0 - y).powi(2))))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(p, _)| p)
    }

    /// The events after the first `seen` ever, as many as are still kept.
    pub fn events_since(&self, seen: u64) -> Vec<&str> {
        let first = self.event_total - self.events.len() as u64;
        let skip = seen.saturating_sub(first) as usize;
        self.events.iter().skip(skip).map(String::as_str).collect()
    }

    /// Whether anything drawn is still moving at time `now`.
    pub fn animating(&self, now: f64) -> bool {
        self.focus.is_some()
            || !self.highlight.is_empty()
            || !self.trials.is_empty()
            || (self.show_congestion && self.heat_memory.borrow().values().any(|(_, at)| now - at < HEAT_FADE_MS))
    }

    pub fn counts(&self) -> (usize, usize) {
        let traces = self.board.items().filter(|(_, i)| matches!(i.geom, Geom::Track { .. })).count();
        let vias = self.board.items().filter(|(_, i)| matches!(i.geom, Geom::Via { .. })).count();
        (traces, vias)
    }
}

pub struct Congestion {
    pub cols: usize,
    pub rows: usize,
    pub x0: f32,
    pub y0: f32,
    pub values: Vec<f32>,
}

fn heat(v: f32) -> Rgba {
    let t = (v / 1.2).clamp(0.0, 1.0);
    let rgb = if t < 0.5 {
        let k = t * 2.0;
        [0.2 + 0.8 * k, 0.45 + 0.4 * k, 1.0 - 0.8 * k]
    } else {
        let k = (t - 0.5) * 2.0;
        [1.0, 0.85 - 0.6 * k, 0.2 - 0.1 * k]
    };
    [rgb[0], rgb[1], rgb[2], (0.2 + 0.5 * t).min(0.7)]
}

fn prim_area(p: &Prim) -> f32 {
    match p {
        Prim::Circle([_, _, r]) => std::f32::consts::PI * r * r,
        Prim::Poly(g) => {
            let n = g.len() / 2;
            let twice: f32 = (0..n).map(|i| {
                let j = (i + 1) % n;
                g[2 * i] * g[2 * j + 1] - g[2 * j] * g[2 * i + 1]
            }).sum();
            twice.abs() / 2.0
        }
    }
}

fn prim_box(p: &Prim) -> [f32; 4] {
    match p {
        Prim::Circle([x, y, r]) => [x - r, y - r, x + r, y + r],
        Prim::Poly(g) => g.chunks_exact(2).fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |b, c| {
            [b[0].min(c[0]), b[1].min(c[1]), b[2].max(c[0]), b[3].max(c[1])]
        }),
    }
}

/// Shares `area` out evenly over sample points across the shape's extent, so a large pad
/// fills every cell it covers rather than the one at its centre.
fn spread(p: &Prim, o: [f32; 2], area: f32, add: &mut dyn FnMut(f32, f32, f32)) {
    let b = prim_box(p);
    let (w, h) = (b[2] - b[0], b[3] - b[1]);
    let (nx, ny) = (((w / (CELL_MM / 2.0)).ceil() as usize).max(1), ((h / (CELL_MM / 2.0)).ceil() as usize).max(1));
    let share = area / (nx * ny) as f32;
    for i in 0..nx {
        for j in 0..ny {
            let x = b[0] + w * (i as f32 + 0.5) / nx as f32 + o[0];
            let y = b[1] + h * (j as f32 + 0.5) / ny as f32 + o[1];
            add(x, y, share);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD: &str = r#"{"t":"board","layers":[{"name":"F.Cu"},{"name":"B.Cu"}],"nets":{"3":"GND"},"bbox":[0,0,10,10]}"#;

    fn frame(add: &str, del: &str) -> String {
        format!(r#"{{"t":"frame","seq":1,"ms":5,"add":[{add}],"del":[{del}],"parts":[{{"id":7,"name":"U1","at":[2,2],"fixed":false}}],"probes":[],"events":[]}}"#)
    }

    const TRACE: &str = r#"{"id":1,"k":"trace","net":3,"layer":0,"w":0.2,"p":[0,0,5,0,5,5]}"#;
    const PIN: &str = r#"{"id":2,"k":"pin","net":3,"part":7,"at":[2,2],"pads":[{"layer":0,"shape":{"g":[1.5,1.5,2.5,1.5,2.5,2.5,1.5,2.5]}}]}"#;

    #[test]
    fn frames_add_and_remove_items() {
        let mut s = Scene::default();
        assert!(s.apply(BOARD));
        assert!(s.apply(&frame(&format!("{TRACE},{PIN}"), "")));
        assert_eq!(s.board.items().count(), 2);
        assert_eq!(s.counts(), (1, 0));
        s.apply(&frame("", "1"));
        assert_eq!(s.board.items().count(), 1);
    }

    #[test]
    fn a_trace_is_a_quad_per_segment_and_a_square_pad_two_triangles() {
        let mut s = Scene::default();
        s.apply(BOARD);
        s.apply(&frame(&format!("{TRACE},{PIN}"), ""));
        assert_eq!(s.mesh(0.01).len(), (2 * 6 + 6) * STRIDE);
    }

    #[test]
    fn hidden_layers_draw_nothing() {
        let mut s = Scene::default();
        s.apply(BOARD);
        s.apply(&frame(TRACE, ""));
        s.board.set_layer_visible(0, false);
        assert!(s.mesh(0.01).is_empty());
    }

    #[test]
    fn parts_are_picked_through_their_pads_and_follow_their_offset() {
        let mut s = Scene::default();
        s.apply(BOARD);
        s.apply(&frame(PIN, ""));
        assert_eq!(s.part_at(2.0, 2.0), Some(7));
        assert_eq!(s.part_at(4.0, 4.0), None);
        s.set_offset(7, 2.0, 2.0);
        assert_eq!(s.part_at(4.0, 4.0), Some(7));
    }

    #[test]
    fn copper_and_open_connections_fill_their_cells() {
        let mut s = Scene::default();
        s.apply(BOARD);
        s.apply(&frame(TRACE, ""));
        let c = s.congestion();
        assert_eq!((c.cols, c.rows), (10, 10));
        let at = |x: usize, y: usize| c.values[y * c.cols + x];
        assert!(at(2, 0) > 0.0);
        assert_eq!(at(2, 5), 0.0);
        s.airlines = vec![[0.5, 5.5, 9.5, 5.5, 3.0]];
        assert!(s.congestion().values[5 * 10 + 4] > 0.0);
    }

    #[test]
    fn a_lock_set_in_the_client_overrides_the_routers() {
        let mut s = Scene::default();
        s.apply(BOARD);
        s.apply(&frame(PIN, ""));
        assert!(!s.is_locked(7));
        s.lock_overrides.insert(7, true);
        assert!(s.is_locked(7));
    }

    #[test]
    fn router_fills_replace_the_files_pours_and_go_with_the_board() {
        let mut s = Scene::default();
        s.board.style.show_pours = true;
        s.apply(BOARD);
        s.apply(&frame(r#"{"id":2,"k":"pour","net":3,"tiles":[{"layer":0,"g":[0,0,1,0,1,1]}]}"#, ""));
        let with_file = s.mesh(0.01).len();
        assert!(with_file > 0);
        s.apply(r#"{"t":"fill","fills":[{"zone":0,"net":3,"layer":0,"r":[0,0,2,2,3,3,4,4]}]}"#);
        // Two rectangles of two triangles each, in place of the file's one triangle.
        assert_eq!(s.mesh(0.01).len(), 12 * STRIDE);
        s.board.style.show_pours = false;
        assert!(s.mesh(0.01).is_empty());
        s.apply(BOARD);
        assert!(s.board.fills().is_empty());
    }

    #[test]
    fn trial_routes_show_until_the_next_ones_and_go_when_the_run_ends() {
        let mut s = Scene::default();
        s.apply(BOARD);
        s.apply(r#"{"t":"frame","seq":2,"ms":9,"add":[],"del":[],"parts":[],"probes":[],"events":[],"trial":[{"net":3,"layer":1,"p":[0,0,1,1,2,1]}]}"#);
        assert_eq!(s.trials.len(), 1);
        assert!(!s.trials[0].collides);
        assert!(!s.mesh(0.01).is_empty());
        s.apply(&frame("", ""));
        assert_eq!(s.trials.len(), 1);
        s.apply(r#"{"t":"done","open":0}"#);
        assert!(s.trials.is_empty());
    }

    #[test]
    fn colliding_trials_are_solid_and_planned_vias_are_rings() {
        let trial = |collides: bool, vias: &str| {
            let mut s = Scene::default();
            s.apply(BOARD);
            s.apply(&format!(
                r#"{{"t":"frame","seq":2,"ms":9,"add":[],"del":[],"parts":[],"probes":[],"events":[],"trial":[{{"net":3,"layer":0,"p":[0,0,4,0],"collides":{collides}}}],"trial_vias":{vias}}}"#
            ));
            s
        };
        let calm = trial(false, "[]");
        let fighting = trial(true, "[]");
        assert!(fighting.trials[0].collides);
        // Solid is one quad; 4 mm dashed at 0.05 mm is many.
        assert!(calm.mesh(0.01).len() > 10 * fighting.mesh(0.01).len());
        let with_via = trial(true, "[[4,0],[1],\"x\"]");
        assert_eq!(with_via.trial_vias, vec![[4.0, 0.0]], "malformed points are dropped");
        assert!(with_via.mesh(0.01).len() > fighting.mesh(0.01).len());
        let mut done = with_via;
        done.apply(r#"{"t":"done","open":0}"#);
        assert!(done.trials.is_empty() && done.trial_vias.is_empty());
    }

    #[test]
    fn a_wild_bounding_box_cannot_blow_up_the_congestion_map() {
        for bbox in ["[0,0,1e12,1e12]", "[-1e30,0,1e30,5]", "[0,0,10,10]"] {
            let mut s = Scene::default();
            s.apply(&BOARD.replace("[0,0,10,10]", bbox));
            s.airlines = vec![[0.0, 0.0, 1e9, 1e9, 3.0]];
            let c = s.congestion();
            assert!(c.cols <= CELLS_MAX && c.rows <= CELLS_MAX, "{bbox}: {}x{}", c.cols, c.rows);
        }
        let mut s = Scene::default();
        s.apply(BOARD);
        s.bbox = [f32::NAN, 0.0, f32::INFINITY, 1.0];
        assert_eq!((s.congestion().cols, s.congestion().rows), (1, 1));
    }

    #[test]
    fn a_part_being_tried_is_drawn_as_a_ghost_where_it_would_go() {
        let focus = |ghost: &str| {
            let mut s = Scene::default();
            s.apply(BOARD);
            s.apply(&frame(PIN, ""));
            s.apply(&format!(r#"{{"t":"frame","seq":2,"ms":9,"add":[],"del":[],"parts":[],"probes":[],"events":[],"focus":{{"phase":"nudging","part":7{ghost}}}}}"#));
            s
        };
        let tried = focus(r#","ghost":[0.5,-0.25]"#);
        assert_eq!(tried.focus, Some(Focus::Part { phase: "nudging".into(), part: 7, ghost: Some([0.5, -0.25]) }));
        let held = focus("");
        assert!(tried.mesh(0.01).len() > held.mesh(0.01).len(), "the ghost adds outlines");
        // Once the move is decided the part is in hand without a ghost.
        assert_eq!(held.focus, Some(Focus::Part { phase: "nudging".into(), part: 7, ghost: None }));
    }

    #[test]
    fn a_layer_mask_draws_only_its_layers() {
        let mut s = Scene::default();
        s.apply(BOARD);
        s.apply(&frame(TRACE, ""));
        assert!(!s.mesh_with(0.01, 0b01, 0.0).is_empty());
        assert!(s.mesh_with(0.01, 0b10, 0.0).is_empty());
    }

    #[test]
    fn congestion_pops_in_and_fades_while_it_stays() {
        let mut s = Scene::default();
        s.apply(BOARD);
        let pads: Vec<String> = (0..6)
            .map(|k| format!(r#"{{"id":{},"k":"pin","net":3,"part":7,"at":[2,2],"pads":[{{"layer":0,"shape":{{"g":[1.6,1.6,2.4,1.6,2.4,2.4,1.6,2.4]}}}}]}}"#, 10 + k))
            .collect();
        s.apply(&frame(&pads.join(","), ""));
        let quiet = s.mesh_with(0.01, u64::MAX, 0.0).len();
        s.show_congestion = true;
        let popped = s.mesh_with(0.01, u64::MAX, 0.0).len();
        assert!(popped > quiet);
        assert!(s.animating(500.0));
        assert_eq!(s.mesh_with(0.01, u64::MAX, 5000.0).len(), quiet);
        assert!(!s.animating(5000.0));
    }

    #[test]
    fn the_nearest_copper_names_its_net() {
        let mut s = Scene::default();
        s.apply(BOARD);
        s.apply(&frame(TRACE, ""));
        assert_eq!(s.net_at(2.5, 0.05, 0.2), Some(3));
        assert_eq!(s.net_at(2.5, 3.0, 0.2), None);
    }

    #[test]
    fn a_new_board_keeps_layer_visibility_but_drops_drags() {
        let mut s = Scene::default();
        s.apply(BOARD);
        s.apply(&frame(PIN, ""));
        s.board.set_layer_visible(1, false);
        s.set_offset(7, 1.0, 0.0);
        s.apply(BOARD);
        assert!(!s.board.visible(1));
        assert_eq!(s.board.pose(7), Pose::default());
    }
}
