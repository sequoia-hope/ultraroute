//! The copper a router works with: copper layers, tracks, arcs, vias, zones and keepouts, and
//! each pad's layers and exact shape. Same frame and units as [`crate::Pcb`] (metres, Y down).
//!
//! Every item keeps the byte span of its text, so [`crate::write`] can replace routed copper and
//! leave the rest of the file untouched.

use serde::{Deserialize, Serialize};

use crate::model::{place, Footprint};
use crate::sexpr::{Node, Pos};
use crate::KicadParse;

const MM: f64 = 1e-3;
/// Chord length for arcs in zone outlines and pad primitives.
const ARC_STEP_M: f64 = 0.05 * MM;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Copper {
    /// Copper layers front to back.
    pub layers: Vec<CopperLayer>,
    pub tracks: Vec<Track>,
    pub arcs: Vec<ArcTrack>,
    pub vias: Vec<Via>,
    pub zones: Vec<Zone>,
    /// One per pad, in the order of `Pcb::footprints` and their `pads`.
    pub pads: Vec<PadCopper>,
    /// Whether each footprint is locked, in the order of `Pcb::footprints`.
    pub locked_footprints: Vec<bool>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopperLayer {
    /// KiCad's layer number, as it appears in the file.
    pub number: u32,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl From<Pos> for Span {
    fn from(p: Pos) -> Span {
        Span { start: p.start, end: p.end }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub start: [f64; 2],
    pub end: [f64; 2],
    pub width: f64,
    pub layer: String,
    pub net: u32,
    pub locked: bool,
    pub uuid: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArcTrack {
    pub start: [f64; 2],
    pub mid: [f64; 2],
    pub end: [f64; 2],
    pub width: f64,
    pub layer: String,
    pub net: u32,
    pub locked: bool,
    pub uuid: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Via {
    pub at: [f64; 2],
    pub size: f64,
    pub drill: f64,
    /// The outermost layers it joins; a through via names the two outer layers.
    pub layers: [String; 2],
    pub net: u32,
    pub locked: bool,
    pub uuid: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Zone {
    pub net: u32,
    pub net_name: String,
    /// Layer names as written, wildcards (`*.Cu`, `F&B.Cu`) included; see [`Copper::expand`].
    pub layers: Vec<String>,
    pub outline: Vec<[f64; 2]>,
    /// The poured copper KiCad last computed, per layer; empty for a keepout or an unfilled zone.
    pub filled: Vec<(String, Vec<[f64; 2]>)>,
    pub keepout: Option<Keepout>,
    pub priority: u32,
    /// Inside a footprint, the footprint's index in `Pcb::footprints`.
    pub footprint: Option<usize>,
    pub uuid: String,
    pub span: Span,
}

/// What a rule area forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Keepout {
    pub tracks: bool,
    pub vias: bool,
    pub pads: bool,
    pub copperpour: bool,
    pub footprints: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PadCopper {
    pub footprint: usize,
    pub pad: usize,
    /// Layer names as written, wildcards included; see [`Copper::expand`].
    pub layers: Vec<String>,
    pub shape: PadShape,
    /// The pad's outline on the board, a polygon in board coordinates (circles as a circle).
    pub outline: Outline,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PadShape {
    Circle,
    Rect,
    Oval,
    RoundRect { ratio: f64 },
    ChamferedRect { ratio: f64 },
    Trapezoid { delta: [f64; 2] },
    Custom,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Outline {
    Circle { centre: [f64; 2], radius: f64 },
    /// Pad shapes are unions; each polygon is closed implicitly.
    Polygons(Vec<Vec<[f64; 2]>>),
}

impl Copper {
    /// The copper layers a layer list names, wildcards expanded.
    pub fn expand(&self, names: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for name in names {
            match name.as_str() {
                "*.Cu" => out.extend(self.layers.iter().map(|l| l.name.clone())),
                "F&B.Cu" => out.extend(["F.Cu".to_string(), "B.Cu".to_string()]),
                n if self.layers.iter().any(|l| l.name == n) => out.push(n.to_string()),
                _ => {}
            }
        }
        out.dedup();
        out
    }
}

pub fn read_copper(root: &Node, footprints: &[Footprint]) -> Result<Copper, KicadParse> {
    let layers = root
        .find("layers")
        .map(|l| {
            l.args()
                .iter()
                .filter_map(|l| {
                    let name = l.args().first()?.text()?;
                    if !name.ends_with(".Cu") {
                        return None;
                    }
                    Some(CopperLayer {
                        number: l.head()?.parse().ok()?,
                        name: name.to_string(),
                        kind: l.args().get(1).and_then(Node::text).unwrap_or("signal").to_string(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut layers = layers;
    layers.sort_by_key(|l| match l.name.as_str() {
        "F.Cu" => (0, 0),
        "B.Cu" => (2, 0),
        _ => (1, l.number),
    });

    let mut copper = Copper {
        layers,
        tracks: Vec::new(),
        arcs: Vec::new(),
        vias: Vec::new(),
        zones: Vec::new(),
        pads: Vec::new(),
        locked_footprints: Vec::new(),
        warnings: Vec::new(),
    };
    let mut copper_graphics = 0;
    let mut footprint = 0;
    for child in root.args() {
        match child.head() {
            Some("segment") => copper.tracks.push(Track {
                start: point(child, "start")?,
                end: point(child, "end")?,
                width: number(child, "width")?,
                layer: text(child, "layer")?,
                net: net(child),
                locked: locked(child),
                uuid: uuid(child),
                span: child.pos().into(),
            }),
            Some("arc") => copper.arcs.push(ArcTrack {
                start: point(child, "start")?,
                mid: point(child, "mid")?,
                end: point(child, "end")?,
                width: number(child, "width")?,
                layer: text(child, "layer")?,
                net: net(child),
                locked: locked(child),
                uuid: uuid(child),
                span: child.pos().into(),
            }),
            Some("via") => {
                let names: Vec<String> = required(child, "layers")?
                    .args()
                    .iter()
                    .filter_map(|n| n.text().map(str::to_string))
                    .collect();
                let (Some(first), Some(last)) = (names.first(), names.last()) else {
                    return Err(syntax(child, "via: (layers a b)"));
                };
                copper.vias.push(Via {
                    at: point(child, "at")?,
                    size: number(child, "size")?,
                    drill: number(child, "drill")?,
                    layers: [first.clone(), last.clone()],
                    net: net(child),
                    locked: locked(child),
                    uuid: uuid(child),
                    span: child.pos().into(),
                });
            }
            Some("zone") => copper.zones.push(read_zone(child, None)?),
            Some("gr_line" | "gr_arc" | "gr_circle" | "gr_rect" | "gr_poly")
                if child.find("layer").and_then(|l| l.args().first()).and_then(Node::text).is_some_and(|l| l.ends_with(".Cu")) =>
            {
                copper_graphics += 1;
            }
            Some("footprint") => {
                let Some(fp) = footprints.get(footprint) else {
                    return Err(syntax(child, "footprint the model read"));
                };
                copper.locked_footprints.push(locked(child));
                for (index, pad) in child.find_all("pad").enumerate() {
                    copper.pads.push(read_pad(pad, footprint, index, fp)?);
                }
                for zone in child.find_all("zone") {
                    copper.zones.push(read_zone(zone, Some(footprint))?);
                }
                footprint += 1;
            }
            _ => {}
        }
    }
    if copper_graphics > 0 {
        copper.warnings.push(format!(
            "{copper_graphics} graphic item(s) on copper layers are not read as copper"
        ));
    }
    Ok(copper)
}

fn read_zone(z: &Node, footprint: Option<usize>) -> Result<Zone, KicadParse> {
    let mut layers: Vec<String> = Vec::new();
    if let Some(l) = z.find("layers") {
        layers.extend(l.args().iter().filter_map(|n| n.text().map(str::to_string)));
    }
    if let Some(l) = z.find("layer") {
        layers.extend(l.args().first().and_then(Node::text).map(str::to_string));
    }
    let keepout = z.find("keepout").map(|k| {
        let forbids = |what: &str| k.find(what).and_then(|n| n.args().first()).and_then(Node::text) == Some("not_allowed");
        Keepout {
            tracks: forbids("tracks"),
            vias: forbids("vias"),
            pads: forbids("pads"),
            copperpour: forbids("copperpour"),
            footprints: forbids("footprints"),
        }
    });
    let outline = z.find("polygon").and_then(|p| p.find("pts")).map(pts).transpose()?.unwrap_or_default();
    let mut filled = Vec::new();
    for f in z.find_all("filled_polygon") {
        let layer = f.find("layer").and_then(|l| l.args().first()).and_then(Node::text).unwrap_or("").to_string();
        if let Some(p) = f.find("pts") {
            filled.push((layer, pts(p)?));
        }
    }
    Ok(Zone {
        net: net(z),
        net_name: z.find("net_name").and_then(|n| n.args().first()).and_then(Node::text).unwrap_or("").to_string(),
        layers,
        outline,
        filled,
        keepout,
        priority: z.find("priority").and_then(|p| p.number_arg(0, "priority").ok()).unwrap_or(0.0) as u32,
        footprint,
        uuid: uuid(z),
        span: z.pos().into(),
    })
}

fn read_pad(p: &Node, footprint: usize, index: usize, fp: &Footprint) -> Result<PadCopper, KicadParse> {
    let Some(model) = fp.pads.get(index) else {
        return Err(syntax(p, "pad the model read"));
    };
    let layers: Vec<String> = p
        .find("layers")
        .map(|l| l.args().iter().filter_map(|n| n.text().map(str::to_string)).collect())
        .unwrap_or_default();
    let ratio = |name: &str| p.find(name).and_then(|n| n.number_arg(0, name).ok()).unwrap_or(0.0);
    let shape = match model.shape.as_str() {
        "circle" => PadShape::Circle,
        "rect" => PadShape::Rect,
        "oval" => PadShape::Oval,
        "roundrect" => PadShape::RoundRect { ratio: ratio("roundrect_rratio") },
        "chamfered_rect" => PadShape::ChamferedRect { ratio: ratio("chamfer_ratio") },
        "trapezoid" => PadShape::Trapezoid {
            delta: p.find("rect_delta").map(|d| point_of(d, "rect_delta")).transpose()?.unwrap_or([0.0, 0.0]),
        },
        "custom" => PadShape::Custom,
        _ => return Err(syntax(p, "pad shape circle|rect|oval|roundrect|chamfered_rect|trapezoid|custom")),
    };
    let size = model.size;
    // The file stores a pad's angle on the board, not relative to its footprint.
    let angle = p.find("at").and_then(|a| a.args().get(2)).map(Node::number).transpose()?.unwrap_or(0.0);
    let local_outline = match &shape {
        PadShape::Circle => None,
        PadShape::Rect | PadShape::ChamferedRect { .. } => Some(vec![rect(size)]),
        PadShape::Oval => Some(vec![stadium(size)]),
        PadShape::RoundRect { ratio } => Some(vec![rounded(size, ratio * size[0].min(size[1]))]),
        PadShape::Trapezoid { delta } => {
            let [w, h] = [size[0] / 2.0, size[1] / 2.0];
            let [dx, dy] = [delta[0] / 2.0, delta[1] / 2.0];
            Some(vec![vec![[-w - dy, h + dx], [w + dy, h - dx], [w - dy, -h + dx], [-w + dy, -h - dx]]])
        }
        PadShape::Custom => {
            let anchor_circle = p
                .find("options")
                .and_then(|o| o.find("anchor"))
                .and_then(|a| a.args().first())
                .and_then(Node::text)
                == Some("circle");
            let mut polys = vec![if anchor_circle { circle([0.0, 0.0], size[0] / 2.0) } else { rect(size) }];
            if let Some(prims) = p.find("primitives") {
                polys.extend(primitives(prims)?);
            }
            Some(polys)
        }
    };
    let outline = match local_outline {
        None => Outline::Circle { centre: model.position, radius: size[0] / 2.0 },
        Some(polys) => Outline::Polygons(
            polys
                .into_iter()
                .map(|poly| poly.into_iter().map(|q| place(model.position, angle, q)).collect())
                .collect(),
        ),
    };
    Ok(PadCopper { footprint, pad: index, layers, shape, outline })
}

/// A custom pad's extra shapes, in the pad's own frame. Lines and arcs with a width become their
/// outline; filled polygons are taken as they are.
fn primitives(prims: &Node) -> Result<Vec<Vec<[f64; 2]>>, KicadParse> {
    let mut out = Vec::new();
    for g in prims.args() {
        let width = g.find("width").and_then(|w| w.number_arg(0, "width").ok()).unwrap_or(0.0) * MM;
        match g.head() {
            Some("gr_poly") => {
                if let Some(p) = g.find("pts") {
                    out.push(pts(p)?);
                }
            }
            Some("gr_circle") => {
                let c = point(g, "center")?;
                let e = point(g, "end")?;
                out.push(circle(c, (e[0] - c[0]).hypot(e[1] - c[1]) + width / 2.0));
            }
            Some("gr_rect") => {
                let a = point(g, "start")?;
                let b = point(g, "end")?;
                out.push(vec![[a[0], a[1]], [b[0], a[1]], [b[0], b[1]], [a[0], b[1]]]);
            }
            Some("gr_line") => out.push(capsule(point(g, "start")?, point(g, "end")?, width / 2.0)),
            _ => {}
        }
    }
    Ok(out)
}

fn rect(size: [f64; 2]) -> Vec<[f64; 2]> {
    let [w, h] = [size[0] / 2.0, size[1] / 2.0];
    vec![[-w, -h], [w, -h], [w, h], [-w, h]]
}

fn circle(c: [f64; 2], r: f64) -> Vec<[f64; 2]> {
    let n = arc_steps(r, std::f64::consts::TAU).max(8);
    (0..n)
        .map(|k| {
            let a = std::f64::consts::TAU * k as f64 / n as f64;
            [c[0] + r * a.cos(), c[1] + r * a.sin()]
        })
        .collect()
}

fn arc_steps(r: f64, sweep: f64) -> usize {
    ((r * sweep.abs() / ARC_STEP_M).ceil() as usize).max(2)
}

/// A rectangle with its corners rounded by `r`.
fn rounded(size: [f64; 2], r: f64) -> Vec<[f64; 2]> {
    let [w, h] = [size[0] / 2.0, size[1] / 2.0];
    let r = r.min(w).min(h);
    if r <= 0.0 {
        return rect(size);
    }
    let corners = [[w - r, h - r, 0.0], [-(w - r), h - r, 90.0], [-(w - r), -(h - r), 180.0], [w - r, -(h - r), 270.0]];
    let n = arc_steps(r, std::f64::consts::FRAC_PI_2);
    let mut out = Vec::new();
    for [cx, cy, a0] in corners {
        for k in 0..=n {
            let a = (a0 + 90.0 * k as f64 / n as f64).to_radians();
            out.push([cx + r * a.cos(), cy + r * a.sin()]);
        }
    }
    out
}

fn stadium(size: [f64; 2]) -> Vec<[f64; 2]> {
    rounded(size, size[0].min(size[1]) / 2.0)
}

fn capsule(a: [f64; 2], b: [f64; 2], r: f64) -> Vec<[f64; 2]> {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len = dx.hypot(dy);
    if len == 0.0 {
        return circle(a, r);
    }
    let base = dy.atan2(dx);
    let n = arc_steps(r, std::f64::consts::PI);
    let mut out = Vec::new();
    for (c, from) in [(b, base - std::f64::consts::FRAC_PI_2), (a, base + std::f64::consts::FRAC_PI_2)] {
        for k in 0..=n {
            let t = from + std::f64::consts::PI * k as f64 / n as f64;
            out.push([c[0] + r * t.cos(), c[1] + r * t.sin()]);
        }
    }
    out
}

/// Points of a `(pts …)` list; an `(arc (start) (mid) (end))` in it is followed in chords.
fn pts(p: &Node) -> Result<Vec<[f64; 2]>, KicadParse> {
    let mut out = Vec::new();
    for n in p.args() {
        match n.head() {
            Some("xy") => out.push(point_of(n, "xy")?),
            Some("arc") => {
                let (s, m, e) = (point(n, "start")?, point(n, "mid")?, point(n, "end")?);
                out.extend(arc_points(s, m, e));
            }
            _ => {}
        }
    }
    Ok(out)
}

fn arc_points(s: [f64; 2], m: [f64; 2], e: [f64; 2]) -> Vec<[f64; 2]> {
    let Some(c) = crate::outline::circumcenter(s, m, e) else {
        return vec![s, m, e];
    };
    let r = (s[0] - c[0]).hypot(s[1] - c[1]);
    let angle = |p: [f64; 2]| (p[1] - c[1]).atan2(p[0] - c[0]);
    let (a0, am, a1) = (angle(s), angle(m), angle(e));
    let wrap = |a: f64| a.rem_euclid(std::f64::consts::TAU);
    let ccw = wrap(am - a0) < wrap(a1 - a0);
    let sweep = if ccw { wrap(a1 - a0) } else { -wrap(a0 - a1) };
    let n = arc_steps(r, sweep);
    (0..=n)
        .map(|k| {
            let a = a0 + sweep * k as f64 / n as f64;
            [c[0] + r * a.cos(), c[1] + r * a.sin()]
        })
        .collect()
}

fn syntax(n: &Node, expected: &str) -> KicadParse {
    let p = n.pos();
    KicadParse::Syntax { line: p.line, col: p.col, expected: expected.to_string() }
}

fn required<'a>(n: &'a Node, name: &str) -> Result<&'a Node, KicadParse> {
    n.find(name).ok_or_else(|| syntax(n, &format!("({name} …) in {}", n.head().unwrap_or("item"))))
}

fn point_of(n: &Node, form: &str) -> Result<[f64; 2], KicadParse> {
    Ok([n.number_arg(0, form)? * MM, n.number_arg(1, form)? * MM])
}

fn point(n: &Node, name: &str) -> Result<[f64; 2], KicadParse> {
    point_of(required(n, name)?, name)
}

fn number(n: &Node, name: &str) -> Result<f64, KicadParse> {
    Ok(required(n, name)?.number_arg(0, name)? * MM)
}

fn text(n: &Node, name: &str) -> Result<String, KicadParse> {
    Ok(required(n, name)?.text_arg(0, name)?.to_string())
}

fn net(n: &Node) -> u32 {
    n.find("net").and_then(|n| n.number_arg(0, "net").ok()).unwrap_or(0.0) as u32
}

/// `(locked yes)` in KiCad 8 and later; a bare `locked` token before that.
fn locked(n: &Node) -> bool {
    n.find("locked").is_some_and(|l| l.args().first().and_then(Node::text) != Some("no"))
        || n.args().iter().any(|a| matches!(a, Node::Atom(s, _) if s == "locked"))
}

fn uuid(n: &Node) -> String {
    n.find("uuid")
        .or_else(|| n.find("tstamp"))
        .and_then(|u| u.args().first())
        .and_then(Node::text)
        .unwrap_or("")
        .to_string()
}
