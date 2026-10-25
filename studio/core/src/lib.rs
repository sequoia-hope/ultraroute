//! The board as the live stream describes it, turned into triangles for the GPU, and the
//! picking a pointer needs. Shared by the web client (through WASM) and the native one.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde_json::Value;

#[cfg(feature = "wasm")]
pub mod wasm;

const FOCUS: Rgba = [1.0, 1.0, 1.0, 1.0];
const COLLIDING: Rgba = [1.0, 0.55, 0.12, 0.95];
const TRIAL_VIA: Rgba = [1.0, 1.0, 1.0, 0.75];
/// Drawn size of a via a negotiated route would need, in millimetres.
const TRIAL_VIA_MM: f32 = 0.2;
/// Congestion below this is not drawn; a change smaller than `HEAT_CHANGE` does not pop.
const HEAT_FLOOR: f32 = 0.15;
const HEAT_CHANGE: f32 = 0.05;
const HEAT_FADE_MS: f64 = 1500.0;

/// Polygons with more corners than this are drawn as outlines rather than fans.
const LARGE_POLYGON: usize = 24;

/// Floats per vertex: position, segment ends, radius, colour.
pub const STRIDE: usize = 11;
const EVENTS_KEPT: usize = 200;

type Rgba = [f32; 4];

// KiCad's default colour theme.
/// Through-hole pads, whatever the layer.
const PAD: Rgba = [0.89, 0.72, 0.18, 1.0];
const VIA: Rgba = [0.86, 0.86, 0.86, 1.0];
const DRILL: Rgba = [0.0, 0.03, 0.07, 1.0];
const UNPLATED_HOLE: Rgba = [0.1, 0.77, 0.82, 1.0];
const OUTLINE: Rgba = [0.82, 0.82, 0.8, 1.0];
const AIRLINE: Rgba = [0.96, 1.0, 0.84, 0.6];
/// How bright the layers other than the active one stay in high-contrast mode.
const DIMMED: f32 = 0.22;
const PROBE: Rgba = [1.0, 0.85, 0.2, 0.16];
const DOOR: Rgba = [1.0, 0.85, 0.2, 0.9];
const BODY: Rgba = [1.0, 1.0, 1.0, 0.05];
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
pub enum Prim {
    Circle([f32; 3]),
    Poly(Vec<f32>),
}

#[derive(Clone, Debug)]
pub enum Geom {
    Trace { layer: usize, width: f32, points: Vec<f32> },
    Via { at: [f32; 2], shape: Option<Prim>, drill: Option<f32> },
    Pin { part: i32, pads: Vec<(usize, Prim)>, at: [f32; 2], drill: Option<f32> },
    Outline { polys: Vec<Vec<f32>> },
    Area { kind: AreaKind, part: i32, tiles: Vec<(usize, Vec<f32>)> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AreaKind {
    Keepout,
    ViaKeepout,
    Pour,
    Body,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub net: i32,
    pub fixed: bool,
    pub geom: Geom,
}

impl Item {
    fn part(&self) -> i32 {
        match self.geom {
            Geom::Pin { part, .. } | Geom::Area { part, .. } => part,
            _ => 0,
        }
    }
}

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
    Part { phase: String, part: i32 },
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
    pub layers: Vec<String>,
    pub nets: HashMap<i32, String>,
    pub bbox: [f32; 4],
    pub items: BTreeMap<u32, Item>,
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
    pub hidden: Vec<bool>,
    pub offsets: HashMap<i32, [f32; 2]>,
    pub picked: Option<i32>,
    pub hot_net: Option<i32>,
    pub selected: Option<i32>,
    pub show_congestion: bool,
    /// Zone fills as the board file has them; the router leaves them for KiCad to refill.
    pub show_pours: bool,
    /// The layer drawn on top, and in high-contrast mode the only one drawn bright.
    pub active_layer: usize,
    pub high_contrast: bool,
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

fn parse_item(v: &Value) -> Option<(u32, Item)> {
    let id = v.get("id")?.as_u64()? as u32;
    let net = v.get("net").and_then(Value::as_i64).unwrap_or(0) as i32;
    let fixed = v.get("fixed").and_then(Value::as_bool).unwrap_or(false);
    let part = v.get("part").and_then(Value::as_i64).unwrap_or(0) as i32;
    let at = |v: &Value| {
        let a = floats(&v["at"]);
        [a.first().copied().unwrap_or(0.0), a.get(1).copied().unwrap_or(0.0)]
    };
    let drill = v.get("drill").and_then(Value::as_f64).map(|d| d as f32);
    let geom = match v.get("k")?.as_str()? {
        "trace" => Geom::Trace { layer: layer(&v["layer"]), width: f(&v["w"]), points: floats(&v["p"]) },
        "via" => Geom::Via { at: at(v), shape: v.get("shape").and_then(prim), drill },
        "pin" => Geom::Pin {
            part,
            at: at(v),
            drill,
            pads: v["pads"]
                .as_array()
                .map(|pads| pads.iter().filter_map(|p| Some((layer(&p["layer"]), prim(&p["shape"])?))).collect())
                .unwrap_or_default(),
        },
        "outline" => Geom::Outline {
            polys: v["polys"].as_array().map(|p| p.iter().map(floats).collect()).unwrap_or_default(),
        },
        kind => Geom::Area {
            kind: match kind {
                "pour" => AreaKind::Pour,
                "body" => AreaKind::Body,
                "via-keepout" => AreaKind::ViaKeepout,
                _ => AreaKind::Keepout,
            },
            part,
            tiles: v["tiles"]
                .as_array()
                .map(|t| t.iter().map(|t| (layer(&t["layer"]), floats(&t["g"]))).collect())
                .unwrap_or_default(),
        },
    };
    Some((id, Item { net, fixed, geom }))
}

impl Scene {
    /// Applies one line of the stream. Returns whether anything drawn changed.
    pub fn apply(&mut self, line: &str) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        match v.get("t").and_then(Value::as_str) {
            Some("board") => {
                let hidden = std::mem::take(&mut self.hidden);
                let show_congestion = self.show_congestion;
                let (show_pours, active_layer, high_contrast) = (self.show_pours, self.active_layer, self.high_contrast);
                let highlight = std::mem::take(&mut self.highlight);
                let event_total = self.event_total;
                let events = std::mem::take(&mut self.events);
                *self = Scene::default();
                self.show_congestion = show_congestion;
                (self.show_pours, self.active_layer, self.high_contrast) = (show_pours, active_layer, high_contrast);
                self.highlight = highlight;
                // The event count runs on across boards, so a watcher never sees one twice.
                self.event_total = event_total;
                self.events = events;
                self.layers = v["layers"]
                    .as_array()
                    .map(|l| l.iter().map(|l| l["name"].as_str().unwrap_or("").to_string()).collect())
                    .unwrap_or_default();
                self.hidden = if hidden.len() == self.layers.len() { hidden } else { vec![false; self.layers.len()] };
                if let Some(nets) = v["nets"].as_object() {
                    for (k, name) in nets {
                        if let Ok(n) = k.parse() {
                            self.nets.insert(n, name.as_str().unwrap_or("").to_string());
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
                        self.items.remove(&(id as u32));
                    }
                }
                for item in v["add"].as_array().into_iter().flatten() {
                    if let Some((id, item)) = parse_item(item) {
                        self.items.insert(id, item);
                    }
                }
                for p in v["parts"].as_array().into_iter().flatten() {
                    let at = floats(&p["at"]);
                    let id = p["id"].as_i64().unwrap_or(0) as i32;
                    self.parts.insert(
                        id,
                        Part {
                            id,
                            name: p["name"].as_str().unwrap_or("").to_string(),
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
                        Some(Focus::Part { phase, part: part as i32 })
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

    /// KiCad's default copper colours: F.Cu red, B.Cu blue, the inner layers in its order.
    pub fn layer_color(&self, layer: usize) -> Rgba {
        let last = self.layers.len().saturating_sub(1);
        let name = self.layers.get(layer).map(String::as_str).unwrap_or("");
        let rgb = |r: f32, g: f32, b: f32| [r / 255.0, g / 255.0, b / 255.0, 0.9];
        if name == "F.Cu" || layer == 0 {
            rgb(200.0, 52.0, 52.0)
        } else if name == "B.Cu" || layer == last {
            rgb(77.0, 127.0, 196.0)
        } else {
            [
                rgb(127.0, 200.0, 127.0),
                rgb(206.0, 125.0, 44.0),
                rgb(79.0, 203.0, 203.0),
                rgb(219.0, 98.0, 139.0),
                rgb(167.0, 165.0, 198.0),
                rgb(40.0, 204.0, 217.0),
            ][(layer - 1) % 6]
        }
    }

    /// A layer's colour as drawn now: dimmed in high-contrast mode unless it is the active one.
    fn drawn_color(&self, layer: usize) -> Rgba {
        let c = self.layer_color(layer);
        if self.high_contrast && layer != self.active_layer {
            [c[0] * DIMMED, c[1] * DIMMED, c[2] * DIMMED, c[3]]
        } else {
            c
        }
    }

    fn shown(&self, layer: usize) -> bool {
        !self.hidden.get(layer).copied().unwrap_or(false)
    }

    fn offset(&self, part: i32) -> [f32; 2] {
        if part == 0 {
            return [0.0, 0.0];
        }
        self.offsets.get(&part).copied().unwrap_or([0.0, 0.0])
    }

    fn tint(&self, color: Rgba, net: i32) -> Rgba {
        if net != 0 && self.hot_net == Some(net) {
            [(color[0] + 1.0) / 2.0, (color[1] + 1.0) / 2.0, (color[2] + 1.0) / 2.0, 1.0]
        } else {
            color
        }
    }

    /// Triangles for everything shown, back to front, `STRIDE` floats per vertex. `px` is the
    /// size of one screen pixel in millimetres, for lines drawn a fixed number of pixels wide.
    pub fn mesh(&self, px: f32) -> Vec<f32> {
        self.mesh_with(px, u64::MAX, 0.0)
    }

    /// The mesh for the layers in `mask` only, at time `now` (milliseconds, for what animates).
    pub fn mesh_with(&self, px: f32, mask: u64, now: f64) -> Vec<f32> {
        let mut m = Mesh { v: Vec::with_capacity(self.items.len() * 64 * STRIDE), aa: px };
        // Back to front, the active layer last so it is drawn on top.
        let mut back_to_front: Vec<usize> = (0..self.layers.len().max(1)).rev().filter(|l| *l != self.active_layer).collect();
        back_to_front.push(self.active_layer);
        let shown = |layer: usize| self.shown(layer) && layer < 64 && mask & (1 << layer) != 0;
        let part_shown = |part: i32| {
            self.items.values().any(|it| matches!(&it.geom, Geom::Pin { part: p, pads, .. } if *p == part && pads.iter().any(|(l, _)| shown(*l))))
        };

        for item in self.items.values() {
            if let Geom::Area { kind, part, tiles } = &item.geom {
                if *kind == AreaKind::Pour && !self.show_pours {
                    continue;
                }
                let o = self.offset(*part);
                for (layer, g) in tiles {
                    if !shown(*layer) {
                        continue;
                    }
                    let color = match kind {
                        AreaKind::Body => BODY,
                        AreaKind::Pour => with_alpha(self.drawn_color(*layer), 0.22),
                        AreaKind::Keepout | AreaKind::ViaKeepout => with_alpha(self.drawn_color(*layer), 0.05),
                    };
                    // A fan fills only convex shapes; a large outline (a whole pour or rule area)
                    // is drawn as its edge.
                    if g.len() / 2 > LARGE_POLYGON {
                        let edge = if *kind == AreaKind::Pour { 0.45 } else { 0.3 };
                        let shifted: Vec<f32> = g.chunks_exact(2).flat_map(|c| [c[0] + o[0], c[1] + o[1]]).collect();
                        m.ring(&shifted, px * 0.7, with_alpha(color, edge));
                    } else {
                        m.fill(g, o, color);
                    }
                }
            }
        }
        for &layer in &back_to_front {
            if !shown(layer) {
                continue;
            }
            let color = self.drawn_color(layer);
            for item in self.items.values() {
                if let Geom::Trace { layer: l, width, points } = &item.geom
                    && *l == layer
                {
                    m.polyline(points, width / 2.0, self.tint(color, item.net));
                }
            }
        }
        for &layer in &back_to_front {
            if !shown(layer) {
                continue;
            }
            for item in self.items.values() {
                if let Geom::Pin { part, pads, drill, .. } = &item.geom {
                    let o = self.offset(*part);
                    // As KiCad draws them: a through-hole pad in gold, a surface pad in its layer's colour.
                    let through = drill.is_some() && pads.len() > 1;
                    let color = if through {
                        if self.high_contrast && layer != self.active_layer { [PAD[0] * DIMMED, PAD[1] * DIMMED, PAD[2] * DIMMED, 1.0] } else { PAD }
                    } else {
                        with_alpha(self.drawn_color(layer), 1.0)
                    };
                    for (l, p) in pads {
                        if *l == layer {
                            m.prim(p, o, self.tint(color, item.net));
                        }
                    }
                }
            }
        }
        for item in self.items.values() {
            match &item.geom {
                Geom::Via { shape: Some(shape), at, drill } => {
                    m.prim(shape, [0.0, 0.0], self.tint(VIA, item.net));
                    if let Some(d) = drill {
                        m.disc(*at, *d, DRILL);
                    }
                }
                Geom::Pin { part, at, drill: Some(d), pads } if pads.is_empty() || pads.iter().any(|(l, _)| shown(*l)) => {
                    let o = self.offset(*part);
                    m.disc([at[0] + o[0], at[1] + o[1]], *d, if pads.is_empty() { UNPLATED_HOLE } else { DRILL });
                }
                Geom::Outline { polys } => {
                    for p in polys {
                        m.ring(p, px, OUTLINE);
                    }
                }
                _ => {}
            }
        }
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
                m.polyline(&t.points, px * 1.3, self.tint(COLLIDING, t.net));
            } else {
                let c = self.drawn_color(t.layer);
                let pale = [(c[0] + 0.6) / 1.6, (c[1] + 0.6) / 1.6, (c[2] + 0.6) / 1.6, 0.85];
                m.dashed_polyline(&t.points, px * 0.9, px * 5.0, self.tint(pale, t.net));
            }
        }
        for v in &self.trial_vias {
            m.ring(&circle_points(*v, TRIAL_VIA_MM), px * 0.8, TRIAL_VIA);
        }
        for a in &self.airlines {
            m.segment([a[0], a[1]], [a[2], a[3]], px * 0.6, self.tint(AIRLINE, a[4] as i32));
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
                && part_shown(part.id)
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
            Some(Focus::Part { part, .. }) => {
                if let Some(b) = self.part_box(*part) {
                    let e = px * (4.0 + 4.0 * pulse);
                    m.ring(&[b[0] - e, b[1] - e, b[2] + e, b[1] - e, b[2] + e, b[3] + e, b[0] - e, b[3] + e], px * 2.0, with_alpha(FOCUS, pulse));
                }
            }
            None => {}
        }
        m.v
    }

    /// The part's extent with its drag offset, as x0, y0, x1, y1.
    pub fn part_box(&self, part: i32) -> Option<[f32; 4]> {
        let o = self.offset(part);
        let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        let mut grow = |x: f32, y: f32| {
            b = [b[0].min(x + o[0]), b[1].min(y + o[1]), b[2].max(x + o[0]), b[3].max(y + o[1])];
        };
        for item in self.items.values().filter(|i| i.part() == part) {
            match &item.geom {
                Geom::Pin { pads, .. } => {
                    for (_, p) in pads {
                        match p {
                            Prim::Circle([x, y, r]) => {
                                grow(x - r, y - r);
                                grow(x + r, y + r);
                            }
                            Prim::Poly(g) => g.chunks_exact(2).for_each(|c| grow(c[0], c[1])),
                        }
                    }
                }
                Geom::Area { kind: AreaKind::Body, tiles, .. } => {
                    for (_, g) in tiles {
                        g.chunks_exact(2).for_each(|c| grow(c[0], c[1]));
                    }
                }
                _ => {}
            }
        }
        (b[0] <= b[2]).then_some(b)
    }

    /// The smallest part whose extent holds the point.
    pub fn part_at(&self, x: f32, y: f32) -> Option<i32> {
        self.parts
            .keys()
            .filter_map(|&p| self.part_box(p).map(|b| (p, b)))
            .filter(|(_, b)| x >= b[0] && x <= b[2] && y >= b[1] && y <= b[3])
            .min_by(|a, b| area(&a.1).total_cmp(&area(&b.1)))
            .map(|(p, _)| p)
    }

    /// The net of the copper nearest the point, within `reach` millimetres.
    pub fn net_at(&self, x: f32, y: f32, reach: f32) -> Option<i32> {
        let mut best: Option<(f32, i32)> = None;
        let mut consider = |d: f32, net: i32| {
            if net != 0 && d <= reach && best.is_none_or(|(b, _)| d < b) {
                best = Some((d, net));
            }
        };
        for item in self.items.values() {
            match &item.geom {
                Geom::Trace { layer, width, points } if self.shown(*layer) => {
                    for w in points.windows(4).step_by(2) {
                        consider(seg_dist([x, y], [w[0], w[1]], [w[2], w[3]]) - width / 2.0, item.net);
                    }
                }
                Geom::Via { at, .. } => consider(((x - at[0]).powi(2) + (y - at[1]).powi(2)).sqrt() - 0.3, item.net),
                Geom::Pin { at, part, .. } => {
                    let o = self.offset(*part);
                    consider(((x - at[0] - o[0]).powi(2) + (y - at[1] - o[1]).powi(2)).sqrt() - 0.3, item.net);
                }
                _ => {}
            }
        }
        best.map(|(_, n)| n)
    }

    pub fn is_locked(&self, part: i32) -> bool {
        self.lock_overrides
            .get(&part)
            .copied()
            .unwrap_or_else(|| self.parts.get(&part).is_some_and(|p| p.fixed))
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
        for item in self.items.values() {
            match &item.geom {
                Geom::Trace { width, points, .. } => {
                    for w in points.windows(4).step_by(2) {
                        line([w[0], w[1]], [w[2], w[3]], *width, &mut add);
                    }
                }
                Geom::Via { shape: Some(shape), .. } => {
                    spread(shape, [0.0, 0.0], prim_area(shape) * self.layers.len().max(1) as f32, &mut add);
                }
                Geom::Pin { pads, part, .. } => {
                    let o = self.offset(*part);
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
        let capacity = CELL_MM * CELL_MM * ROUTABLE_SHARE * self.layers.len().max(1) as f32;
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
        let traces = self.items.values().filter(|i| matches!(i.geom, Geom::Trace { .. })).count();
        let vias = self.items.values().filter(|i| matches!(i.geom, Geom::Via { .. })).count();
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

fn circle_points(c: [f32; 2], r: f32) -> Vec<f32> {
    (0..16)
        .flat_map(|k| {
            let t = std::f32::consts::TAU * k as f32 / 16.0;
            [c[0] + r * t.cos(), c[1] + r * t.sin()]
        })
        .collect()
}

fn area(b: &[f32; 4]) -> f32 {
    (b[2] - b[0]) * (b[3] - b[1])
}

fn with_alpha(c: Rgba, a: f32) -> Rgba {
    [c[0], c[1], c[2], a]
}

fn seg_dist(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 { 0.0 } else { (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0) };
    ((p[0] - a[0] - t * dx).powi(2) + (p[1] - a[1] - t * dy).powi(2)).sqrt()
}

/// Vertices carry the segment they belong to and its radius; the fragment shader cuts the quad
/// to a capsule. A negative radius marks a plain fill.
struct Mesh {
    v: Vec<f32>,
    aa: f32,
}

impl Mesh {
    fn vertex(&mut self, p: [f32; 2], a: [f32; 2], b: [f32; 2], r: f32, c: Rgba) {
        self.v.extend_from_slice(&[p[0], p[1], a[0], a[1], b[0], b[1], r, c[0], c[1], c[2], c[3]]);
    }

    fn segment(&mut self, a: [f32; 2], b: [f32; 2], r: f32, c: Rgba) {
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let len = (dx * dx + dy * dy).sqrt();
        let (ux, uy) = if len > 0.0 { (dx / len, dy / len) } else { (1.0, 0.0) };
        let e = r + self.aa;
        let (nx, ny) = (-uy * e, ux * e);
        let (tx, ty) = (ux * e, uy * e);
        let p0 = [a[0] - tx + nx, a[1] - ty + ny];
        let p1 = [a[0] - tx - nx, a[1] - ty - ny];
        let p2 = [b[0] + tx + nx, b[1] + ty + ny];
        let p3 = [b[0] + tx - nx, b[1] + ty - ny];
        for p in [p0, p1, p2, p2, p1, p3] {
            self.vertex(p, a, b, r, c);
        }
    }

    fn disc(&mut self, at: [f32; 2], r: f32, c: Rgba) {
        self.segment(at, at, r, c);
    }

    fn polyline(&mut self, points: &[f32], r: f32, c: Rgba) {
        if points.len() == 2 {
            self.disc([points[0], points[1]], r, c);
        }
        for w in points.windows(4).step_by(2) {
            self.segment([w[0], w[1]], [w[2], w[3]], r, c);
        }
    }

    /// Dashes `dash` long with gaps as long, the pattern running on across corners.
    /// Each step ends either the current dash or gap (`left` drops to exactly zero) or the
    /// segment (`rest` does), so it always finishes; dashes too fine to see are drawn solid.
    fn dashed_polyline(&mut self, points: &[f32], r: f32, dash: f32, c: Rgba) {
        let total: f32 = points
            .windows(4)
            .step_by(2)
            .map(|w| ((w[2] - w[0]).powi(2) + (w[3] - w[1]).powi(2)).sqrt())
            .sum();
        if !(dash > 0.0 && dash.is_finite() && total.is_finite()) || total / dash > 10_000.0 {
            self.polyline(points, r, c);
            return;
        }
        let (mut on, mut left) = (true, dash);
        for w in points.windows(4).step_by(2) {
            let (a, b) = ([w[0], w[1]], [w[2], w[3]]);
            let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
            let at = |s: f32| [a[0] + (b[0] - a[0]) * s / len, a[1] + (b[1] - a[1]) * s / len];
            let (mut t, mut rest) = (0.0f32, len);
            while rest > 0.0 {
                let take = left.min(rest);
                if on {
                    self.segment(at(t), at(t + take), r, c);
                }
                t += take;
                rest -= take;
                left -= take;
                if left <= 0.0 {
                    on = !on;
                    left = dash;
                }
            }
        }
    }

    fn ring(&mut self, points: &[f32], r: f32, c: Rgba) {
        let n = points.len() / 2;
        for i in 0..n {
            let j = (i + 1) % n;
            self.segment([points[2 * i], points[2 * i + 1]], [points[2 * j], points[2 * j + 1]], r, c);
        }
    }

    fn dashed_ring(&mut self, points: &[f32], r: f32, dash: f32, c: Rgba) {
        let n = points.len() / 2;
        for i in 0..n {
            let j = (i + 1) % n;
            let (a, b) = ([points[2 * i], points[2 * i + 1]], [points[2 * j], points[2 * j + 1]]);
            let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
            let dashes = ((len / (dash * 1.8)).floor() as usize).max(1);
            for k in 0..dashes {
                let (t0, t1) = (k as f32 / dashes as f32, (k as f32 + 0.55) / dashes as f32);
                let at = |t: f32| [a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1])];
                self.segment(at(t0), at(t1), r, c);
            }
        }
    }

    /// A fan from the first corner: tile shapes and pads are convex.
    fn fill(&mut self, points: &[f32], o: [f32; 2], c: Rgba) {
        let n = points.len() / 2;
        let at = |i: usize| [points[2 * i] + o[0], points[2 * i + 1] + o[1]];
        for i in 1..n.saturating_sub(1) {
            for p in [at(0), at(i), at(i + 1)] {
                self.vertex(p, p, p, -1.0, c);
            }
        }
    }

    fn prim(&mut self, p: &Prim, o: [f32; 2], c: Rgba) {
        match p {
            Prim::Circle([x, y, r]) => self.disc([x + o[0], y + o[1]], *r, c),
            Prim::Poly(g) => self.fill(g, o, c),
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
        assert_eq!(s.items.len(), 2);
        assert_eq!(s.counts(), (1, 0));
        s.apply(&frame("", "1"));
        assert_eq!(s.items.len(), 1);
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
        s.hidden[0] = true;
        assert!(s.mesh(0.01).is_empty());
    }

    #[test]
    fn parts_are_picked_through_their_pads_and_follow_their_offset() {
        let mut s = Scene::default();
        s.apply(BOARD);
        s.apply(&frame(PIN, ""));
        assert_eq!(s.part_at(2.0, 2.0), Some(7));
        assert_eq!(s.part_at(4.0, 4.0), None);
        s.offsets.insert(7, [2.0, 2.0]);
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

    /// Quads in a mesh, each segment being one.
    fn quads(m: &Mesh) -> usize {
        m.v.len() / (6 * STRIDE)
    }

    fn mesh() -> Mesh {
        Mesh { v: Vec::new(), aa: 0.0 }
    }

    #[test]
    fn dashes_run_on_across_corners() {
        // 2 mm then 1 mm at a 0.5 mm dash: on, off, on, off | on, off.
        let mut m = mesh();
        m.dashed_polyline(&[0.0, 0.0, 2.0, 0.0, 2.0, 1.0], 0.1, 0.5, FOCUS);
        assert_eq!(quads(&m), 3);
        // 0.25 mm then 0.75 mm: the first dash breaks over the corner and carries on 0.25 mm,
        // where restarting the pattern would draw 0.5 mm.
        let mut m = mesh();
        m.dashed_polyline(&[0.0, 0.0, 0.25, 0.0, 0.25, 0.75], 0.1, 0.5, FOCUS);
        assert_eq!(quads(&m), 2);
        assert_eq!(&m.v[6 * STRIDE + 4..6 * STRIDE + 6], &[0.25, 0.25], "second piece ends 0.25 mm up");
    }

    #[test]
    fn dashing_always_finishes() {
        // The first version recomputed its place in the pattern from a running total with `%`;
        // a rounding step too small to move `t` spun forever, growing the mesh until killed.
        let awkward = [0.1f32, 0.3, 1.0 / 3.0, 1e-3, 0.007, 12.345];
        for &dash in &awkward {
            let mut points = vec![0.0f32, 0.0];
            let mut x = 0.0f32;
            for k in 0..300 {
                x += 0.1 + k as f32 * 0.0137;
                points.extend([x, (k % 7) as f32 * 0.31]);
            }
            let mut m = mesh();
            m.dashed_polyline(&points, 0.05, dash, FOCUS);
            let total: f32 = points.windows(4).step_by(2).map(|w| ((w[2] - w[0]).powi(2) + (w[3] - w[1]).powi(2)).sqrt()).sum();
            // Each dash is at most one piece per segment it crosses.
            assert!(quads(&m) <= (total / dash) as usize + 300 + 2, "dash {dash}: {} quads", quads(&m));
        }
    }

    #[test]
    fn dashes_too_fine_or_broken_are_drawn_solid() {
        let line = [0.0f32, 0.0, 1000.0, 0.0, 1000.0, 5.0];
        for dash in [0.0, -1.0, f32::NAN, f32::INFINITY, 1e-6] {
            let mut m = mesh();
            m.dashed_polyline(&line, 0.1, dash, FOCUS);
            assert_eq!(quads(&m), 2, "dash {dash}");
        }
        let mut m = mesh();
        m.dashed_polyline(&[0.0, 0.0, f32::INFINITY, 0.0], 0.1, 0.5, FOCUS);
        assert_eq!(quads(&m), 1);
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
        s.hidden[1] = true;
        s.offsets.insert(7, [1.0, 0.0]);
        s.apply(BOARD);
        assert!(s.hidden[1]);
        assert!(s.offsets.is_empty());
    }
}
