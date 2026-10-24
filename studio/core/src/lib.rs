//! The board as the live stream describes it, turned into triangles for the GPU, and the
//! picking a pointer needs. Shared by the web client (through WASM) and the native one.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde_json::Value;

#[cfg(feature = "wasm")]
pub mod wasm;

/// Floats per vertex: position, segment ends, radius, colour.
pub const STRIDE: usize = 11;
const EVENTS_KEPT: usize = 200;

type Rgba = [f32; 4];

const PAD: Rgba = [0.93, 0.81, 0.53, 1.0];
const DRILL: Rgba = [0.06, 0.11, 0.15, 1.0];
const OUTLINE: Rgba = [0.51, 0.77, 0.69, 1.0];
const AIRLINE: Rgba = [0.95, 0.95, 0.95, 0.55];
const PROBE: Rgba = [1.0, 0.85, 0.2, 0.16];
const DOOR: Rgba = [1.0, 0.85, 0.2, 0.9];
const BODY: Rgba = [1.0, 1.0, 1.0, 0.05];
const PICKED: Rgba = [1.0, 1.0, 1.0, 0.9];

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
    pub open: Option<usize>,
    pub seq: u64,
    pub ms: u64,
    pub events: VecDeque<String>,
    pub done: Option<Value>,
    pub hidden: Vec<bool>,
    pub offsets: HashMap<i32, [f32; 2]>,
    pub picked: Option<i32>,
    pub hot_net: Option<i32>,
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
                *self = Scene::default();
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
                self.open = v["open"].as_u64().map(|n| n as usize).or(self.open);
                self.done = Some(v);
                true
            }
            _ => false,
        }
    }

    pub fn layer_color(&self, layer: usize) -> Rgba {
        let last = self.layers.len().saturating_sub(1);
        let name = self.layers.get(layer).map(String::as_str).unwrap_or("");
        if name == "F.Cu" || layer == 0 {
            [0.95, 0.43, 0.47, 0.85]
        } else if name == "B.Cu" || layer == last {
            [0.41, 0.72, 1.0, 0.85]
        } else {
            [[0.81, 0.62, 1.0, 0.85], [1.0, 0.65, 0.34, 0.85], [0.51, 0.77, 0.69, 0.85], [1.0, 0.56, 0.78, 0.85]][(layer - 1) % 4]
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
        let mut m = Mesh { v: Vec::with_capacity(self.items.len() * 64 * STRIDE), aa: px };
        let back_to_front: Vec<usize> = (0..self.layers.len().max(1)).rev().collect();

        for item in self.items.values() {
            if let Geom::Area { kind, part, tiles } = &item.geom {
                let o = self.offset(*part);
                for (layer, g) in tiles {
                    if !self.shown(*layer) {
                        continue;
                    }
                    let color = match kind {
                        AreaKind::Body => BODY,
                        AreaKind::Pour => with_alpha(self.layer_color(*layer), 0.22),
                        AreaKind::Keepout | AreaKind::ViaKeepout => with_alpha(self.layer_color(*layer), 0.05),
                    };
                    m.fill(g, o, color);
                }
            }
        }
        for &layer in &back_to_front {
            if !self.shown(layer) {
                continue;
            }
            let color = self.layer_color(layer);
            for item in self.items.values() {
                if let Geom::Trace { layer: l, width, points } = &item.geom
                    && *l == layer
                {
                    m.polyline(points, width / 2.0, self.tint(color, item.net));
                }
            }
        }
        for &layer in &back_to_front {
            if !self.shown(layer) {
                continue;
            }
            for item in self.items.values() {
                if let Geom::Pin { part, pads, .. } = &item.geom {
                    let o = self.offset(*part);
                    for (l, p) in pads {
                        if *l == layer {
                            m.prim(p, o, self.tint(PAD, item.net));
                        }
                    }
                }
            }
        }
        for item in self.items.values() {
            match &item.geom {
                Geom::Via { shape: Some(shape), at, drill } => {
                    m.prim(shape, [0.0, 0.0], self.tint(PAD, item.net));
                    if let Some(d) = drill {
                        m.disc(*at, *d, DRILL);
                    }
                }
                Geom::Pin { part, at, drill: Some(d), .. } => {
                    let o = self.offset(*part);
                    m.disc([at[0] + o[0], at[1] + o[1]], *d, DRILL);
                }
                Geom::Outline { polys } => {
                    for p in polys {
                        m.ring(p, px, OUTLINE);
                    }
                }
                _ => {}
            }
        }
        for a in &self.airlines {
            m.segment([a[0], a[1]], [a[2], a[3]], px * 0.6, self.tint(AIRLINE, a[4] as i32));
        }
        for probe in &self.probes {
            if probe.layer.is_some_and(|l| !self.shown(l)) {
                continue;
            }
            m.fill(&probe.room, [0.0, 0.0], PROBE);
            if probe.door.len() == 4 {
                m.segment([probe.door[0], probe.door[1]], [probe.door[2], probe.door[3]], px * 1.2, DOOR);
            }
        }
        if let Some(b) = self.picked.and_then(|p| self.part_box(p)) {
            m.ring(&[b[0], b[1], b[2], b[1], b[2], b[3], b[0], b[3]], px, PICKED);
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

    pub fn counts(&self) -> (usize, usize) {
        let traces = self.items.values().filter(|i| matches!(i.geom, Geom::Trace { .. })).count();
        let vias = self.items.values().filter(|i| matches!(i.geom, Geom::Via { .. })).count();
        (traces, vias)
    }
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

    fn ring(&mut self, points: &[f32], r: f32, c: Rgba) {
        let n = points.len() / 2;
        for i in 0..n {
            let j = (i + 1) % n;
            self.segment([points[2 * i], points[2 * i + 1]], [points[2 * j], points[2 * j + 1]], r, c);
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
