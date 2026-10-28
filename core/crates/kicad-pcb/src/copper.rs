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
/// Chord length for arcs in zone outlines and pad shapes.
const ARC_STEP_M: f64 = 0.01 * MM;

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
    /// Where each footprint's `(at x y angle)` is in the text, in the order of `Pcb::footprints`.
    pub footprint_at: Vec<Option<Span>>,
    /// The board's groups, each a named set of items by uuid.
    pub groups: Vec<Group>,
    /// Each footprint's courtyard as the convex hull of its courtyard drawing, in board
    /// coordinates; `None` for a footprint without one. In the order of `Pcb::footprints`.
    pub courtyards: Vec<Option<Vec<[f64; 2]>>>,
    /// `(setup (pad_to_mask_clearance X))`: how far a pad's solder mask opening reaches beyond
    /// its copper, where neither the pad nor its footprint says; metres.
    #[serde(default)]
    pub pad_to_mask_clearance: f64,
    /// Copper drawn rather than routed: lines, arcs, rectangles, circles, polygons and visible
    /// text on copper layers, the board's and its footprints', as polygons in board coordinates.
    #[serde(default)]
    pub graphics: Vec<CopperGraphic>,
    /// Openings drawn in the solder mask, on `F.Mask` or `B.Mask`, as `graphics` are read.
    #[serde(default)]
    pub mask_graphics: Vec<CopperGraphic>,
    /// The outer copper sides whose vias the mask leaves bare (`(setup (tenting ...))` without
    /// that side): `[front, back]`.
    #[serde(default)]
    pub vias_bare: [bool; 2],
    pub warnings: Vec<String>,
}

/// One drawn piece of copper; text as the box round it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CopperGraphic {
    pub layer: String,
    /// 0 for none, as most drawn copper has.
    pub net: u32,
    pub polygons: Vec<Vec<[f64; 2]>>,
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

/// A KiCad group: its name and its members' uuids, footprints, zones, copper or other groups.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    pub uuid: String,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Zone {
    /// `(name …)`, empty when the zone has none.
    pub name: String,
    pub net: u32,
    pub net_name: String,
    /// Layer names as written, wildcards (`*.Cu`, `F&B.Cu`) included; see [`Copper::expand`].
    pub layers: Vec<String>,
    pub outline: Vec<[f64; 2]>,
    /// The poured copper KiCad last computed, per layer; empty for a keepout or an unfilled zone.
    pub filled: Vec<(String, Vec<[f64; 2]>)>,
    /// Where each of `filled` is written, so a stale fill can be taken out of the file.
    #[serde(default)]
    pub fill_spans: Vec<Span>,
    pub keepout: Option<Keepout>,
    pub priority: u32,
    /// `(connect_pads (clearance …))`: the gap the pour keeps from other nets, in metres; 0 when unset.
    pub clearance: f64,
    /// `(min_thickness …)`: the narrowest copper the fill keeps, in metres; 0 when unset.
    pub min_thickness: f64,
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
    /// `(solder_mask_margin X)` of the pad, else of its footprint: how far its mask opening
    /// reaches beyond its copper; `None` where the board's `pad_to_mask_clearance` holds.
    #[serde(default)]
    pub mask_margin: Option<f64>,
    /// `(clearance X)` of the pad, else of its footprint: a clearance of its own, kept where it
    /// is wider than its net class's.
    #[serde(default)]
    pub clearance: Option<f64>,
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
        footprint_at: Vec::new(),
        groups: Vec::new(),
        courtyards: Vec::new(),
        graphics: Vec::new(),
        mask_graphics: Vec::new(),
        vias_bare: match root.find("setup").and_then(|s| s.find("tenting")) {
            Some(t) => {
                let sides: Vec<&str> = t.args().iter().filter_map(Node::text).collect();
                [!sides.contains(&"front"), !sides.contains(&"back")]
            }
            None => [false, false],
        },
        pad_to_mask_clearance: root
            .find("setup")
            .and_then(|s| s.find("pad_to_mask_clearance"))
            .and_then(|c| c.number_arg(0, "pad_to_mask_clearance").ok())
            .map_or(0.0, |mm| mm * MM),
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
            Some("group") => copper.groups.push(Group {
                name: child.args().first().and_then(Node::text).unwrap_or("").to_string(),
                uuid: uuid(child),
                members: child.find("members").map(|m| m.args().iter().filter_map(Node::text).map(str::to_string).collect()).unwrap_or_default(),
            }),
            Some("gr_line" | "gr_arc" | "gr_circle" | "gr_rect" | "gr_poly" | "gr_text") => match graphic(child, &|p| p, 0.0) {
                Ok(Some(g)) if g.layer.ends_with(".Mask") => copper.mask_graphics.push(g),
                Ok(Some(g)) => copper.graphics.push(g),
                Ok(None) => {}
                Err(_) => copper_graphics += 1,
            },
            Some("footprint") => {
                let Some(fp) = footprints.get(footprint) else {
                    return Err(syntax(child, "footprint the model read"));
                };
                copper.locked_footprints.push(locked(child));
                copper.footprint_at.push(child.find("at").map(|a| a.pos().into()));
                copper.courtyards.push(courtyard(child, fp)?);
                for g in child.args() {
                    if matches!(g.head(), Some("fp_line" | "fp_arc" | "fp_circle" | "fp_rect" | "fp_poly" | "fp_text" | "property")) {
                        match graphic(g, &|p| fp.place(p), fp.rotation_deg) {
                            Ok(Some(g)) if g.layer.ends_with(".Mask") => copper.mask_graphics.push(g),
                            Ok(Some(g)) => copper.graphics.push(g),
                            Ok(None) => {}
                            Err(_) => copper_graphics += 1,
                        }
                    }
                }
                let margin = child.find("solder_mask_margin").and_then(|m| m.number_arg(0, "solder_mask_margin").ok());
                let clearance = child.find("clearance").and_then(|m| m.number_arg(0, "clearance").ok());
                for (index, pad) in child.find_all("pad").enumerate() {
                    let mut pad_copper = read_pad(pad, footprint, index, fp)?;
                    pad_copper.mask_margin = pad.find("solder_mask_margin").and_then(|m| m.number_arg(0, "solder_mask_margin").ok()).or(margin).map(|mm| mm * MM);
                    pad_copper.clearance = pad.find("clearance").and_then(|m| m.number_arg(0, "clearance").ok()).or(clearance).map(|mm| mm * MM).filter(|c| *c > 0.0);
                    copper.pads.push(pad_copper);
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

/// The convex hull of a footprint's courtyard drawing, placed on the board.
/// A drawing or text on a copper or solder mask layer as polygons, `place` taking its points to the
/// board, `rotation` the footprint's (0 on the board); `None` off copper or hidden. Text is the
/// box its strokes stay within, a little generously, as the router needs no more than that.
fn graphic(g: &Node, place: &dyn Fn([f64; 2]) -> [f64; 2], rotation: f64) -> Result<Option<CopperGraphic>, KicadParse> {
    let Some(layer) = g.find("layer").and_then(|l| l.args().first()).and_then(Node::text).filter(|l| l.ends_with(".Cu") || *l == "F.Mask" || *l == "B.Mask") else {
        return Ok(None);
    };
    let width = g.find("stroke").and_then(|s| s.find("width")).or_else(|| g.find("width")).and_then(|w| w.number_arg(0, "width").ok()).unwrap_or(0.0) * MM;
    let filled = g.find("fill").and_then(|f| f.args().first()).and_then(Node::text).is_some_and(|f| f == "solid" || f == "yes");
    let stroke = |points: &[[f64; 2]], closed: bool| -> Vec<Vec<[f64; 2]>> {
        if width <= 0.0 {
            return Vec::new();
        }
        let n = points.len();
        (0..if closed { n } else { n.saturating_sub(1) }).map(|k| capsule(points[k], points[(k + 1) % n], width / 2.0)).collect()
    };
    let head = g.head().unwrap_or("");
    let kind = head.split('_').nth(1).unwrap_or(head);
    let local: Vec<Vec<[f64; 2]>> = match kind {
        "line" => stroke(&[point(g, "start")?, point(g, "end")?], false),
        "arc" => stroke(&arc_points(point(g, "start")?, point(g, "mid")?, point(g, "end")?), false),
        "rect" => {
            let (a, b) = (point(g, "start")?, point(g, "end")?);
            let corners = vec![a, [b[0], a[1]], b, [a[0], b[1]]];
            let mut out = stroke(&corners, true);
            if filled {
                out.push(corners);
            }
            out
        }
        "circle" => {
            let (c, e) = (point(g, "center")?, point(g, "end")?);
            let r = (e[0] - c[0]).hypot(e[1] - c[1]);
            if filled { vec![circle(c, r + width / 2.0)] } else { stroke(&circle(c, r), true) }
        }
        // KiCad 5 polygons are always filled; later ones say, but an outline-only copper
        // polygon is rare enough to take as filled.
        "poly" => {
            let Some(p) = g.find("pts") else { return Ok(None) };
            let points = pts(p)?;
            let mut out = stroke(&points, true);
            out.push(points);
            out
        }
        "text" | "property" => {
            let hidden = |n: &Node| n.args().iter().any(|a| a.text() == Some("hide") || (a.head() == Some("hide") && a.args().first().and_then(Node::text) != Some("no")));
            let effects = g.find("effects");
            if hidden(g) || effects.is_some_and(hidden) {
                return Ok(None);
            }
            let string = g.args().iter().filter_map(Node::text).filter(|t| !matches!(*t, "reference" | "value" | "user" | "hide")).last().unwrap_or("");
            let font = effects.and_then(|e| e.find("font"));
            let size = font.and_then(|f| f.find("size")).map(|s| point_of(s, "size")).transpose()?.unwrap_or([1e-3, 1e-3]);
            let thick = font.and_then(|f| f.find("thickness")).and_then(|t| t.number_arg(0, "thickness").ok()).map_or(size[0] * 0.15, |t| t * MM);
            let justify: Vec<&str> = effects.and_then(|e| e.find("justify")).map(|j| j.args().iter().filter_map(Node::text).collect()).unwrap_or_default();
            let lines = string.split("\\n").collect::<Vec<_>>();
            let chars = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as f64;
            let (w, h) = (chars * size[1] * 1.1 + thick, lines.len() as f64 * size[0] * 1.6 + thick);
            let x0 = if justify.contains(&"left") { 0.0 } else if justify.contains(&"right") { -w } else { -w / 2.0 };
            let x0 = if justify.contains(&"mirror") { -x0 - w } else { x0 };
            let Some(at) = g.find("at") else { return Ok(None) };
            let anchor = point_of(at, "at")?;
            // The file holds a footprint text's angle on the board, as with pads.
            let angle = at.args().get(2).map(Node::number).transpose()?.unwrap_or(0.0) - rotation;
            let corners = [[x0, -h / 2.0], [x0 + w, -h / 2.0], [x0 + w, h / 2.0], [x0, h / 2.0]];
            vec![corners.iter().map(|&c| crate::model::place(anchor, angle, c)).collect()]
        }
        _ => return Ok(None),
    };
    let polygons: Vec<Vec<[f64; 2]>> = local.into_iter().filter(|p| p.len() >= 3).map(|p| p.into_iter().map(place).collect()).collect();
    if polygons.is_empty() {
        return Ok(None);
    }
    let net = g.find("net").and_then(|n| n.number_arg(0, "net").ok()).map_or(0, |n| n as u32);
    Ok(Some(CopperGraphic { layer: layer.to_string(), net, polygons }))
}

fn courtyard(fp_node: &Node, fp: &Footprint) -> Result<Option<Vec<[f64; 2]>>, KicadParse> {
    let mut local: Vec<[f64; 2]> = Vec::new();
    for g in fp_node.args() {
        let on_courtyard = g.find("layer").and_then(|l| l.args().first()).and_then(Node::text).is_some_and(|l| l.ends_with(".CrtYd"));
        if !on_courtyard {
            continue;
        }
        match g.head() {
            Some("fp_line") | Some("fp_rect") => {
                let (a, b) = (point(g, "start")?, point(g, "end")?);
                if g.head() == Some("fp_rect") {
                    local.extend([a, [b[0], a[1]], b, [a[0], b[1]]]);
                } else {
                    local.extend([a, b]);
                }
            }
            Some("fp_poly") => {
                if let Some(p) = g.find("pts") {
                    local.extend(pts(p)?);
                }
            }
            Some("fp_circle") => {
                let (c, e) = (point(g, "center")?, point(g, "end")?);
                local.extend(circle(c, (e[0] - c[0]).hypot(e[1] - c[1])));
            }
            Some("fp_arc") => local.extend(arc_points(point(g, "start")?, point(g, "mid")?, point(g, "end")?)),
            _ => {}
        }
    }
    if local.len() < 3 {
        return Ok(None);
    }
    let placed: Vec<[f64; 2]> = local.into_iter().map(|p| fp.place(p)).collect();
    Ok(Some(hull(placed)))
}

/// Andrew's monotone chain.
fn hull(mut points: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    points.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    points.dedup();
    if points.len() < 3 {
        return points;
    }
    let cross = |o: [f64; 2], a: [f64; 2], b: [f64; 2]| (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0]);
    let mut lower: Vec<[f64; 2]> = Vec::new();
    for &p in &points {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<[f64; 2]> = Vec::new();
    for &p in points.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
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
    let (mut filled, mut fill_spans) = (Vec::new(), Vec::new());
    for f in z.find_all("filled_polygon") {
        let layer = f.find("layer").and_then(|l| l.args().first()).and_then(Node::text).unwrap_or("").to_string();
        if let Some(p) = f.find("pts") {
            filled.push((layer, pts(p)?));
            fill_spans.push(f.pos().into());
        }
    }
    Ok(Zone {
        name: z.find("name").and_then(|n| n.args().first()).and_then(Node::text).unwrap_or("").to_string(),
        net: net(z),
        net_name: z.find("net_name").and_then(|n| n.args().first()).and_then(Node::text).unwrap_or("").to_string(),
        layers,
        outline,
        filled,
        fill_spans,
        keepout,
        priority: z.find("priority").and_then(|p| p.number_arg(0, "priority").ok()).unwrap_or(0.0) as u32,
        clearance: z
            .find("connect_pads")
            .and_then(|c| c.find("clearance"))
            .and_then(|c| c.number_arg(0, "clearance").ok())
            .map_or(0.0, |mm| mm * MM),
        min_thickness: z.find("min_thickness").and_then(|t| t.number_arg(0, "min_thickness").ok()).map_or(0.0, |mm| mm * MM),
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
    // `(drill ... (offset x y))` moves the pad's copper off its hole, in the pad's own frame.
    let offset = p.find("drill").and_then(|d| d.find("offset")).map(|o| point_of(o, "offset")).transpose()?.unwrap_or([0.0, 0.0]);
    let shifted = |q: [f64; 2]| place(model.position, angle, [q[0] + offset[0], q[1] + offset[1]]);
    let outline = match local_outline {
        None => Outline::Circle { centre: shifted([0.0, 0.0]), radius: size[0] / 2.0 },
        Some(polys) => Outline::Polygons(polys.into_iter().map(|poly| poly.into_iter().map(shifted).collect()).collect()),
    };
    Ok(PadCopper { footprint, pad: index, layers, shape, outline, mask_margin: None, clearance: None })
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
                    let poly = pts(p)?;
                    // A stroked outline widens the shape by half its width all round.
                    if width > 0.0 {
                        for k in 0..poly.len() {
                            out.push(capsule(poly[k], poly[(k + 1) % poly.len()], width / 2.0));
                        }
                    }
                    out.push(poly);
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
    let r = outside(r, std::f64::consts::TAU / n as f64);
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

/// The radius whose chords, `step` radians apart, pass outside a circle of radius `r`: a pad's
/// outline may be a hair too big, never too small, so clearance to it is never overstated.
fn outside(r: f64, step: f64) -> f64 {
    r / (step / 2.0).cos()
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
    let ro = outside(r, std::f64::consts::FRAC_PI_2 / n as f64);
    let mut out = Vec::new();
    for [cx, cy, a0] in corners {
        for k in 0..=n {
            let a = (a0 + 90.0 * k as f64 / n as f64).to_radians();
            out.push([cx + ro * a.cos(), cy + ro * a.sin()]);
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
    let ro = outside(r, std::f64::consts::PI / n as f64);
    let mut out = Vec::new();
    for (c, from) in [(b, base - std::f64::consts::FRAC_PI_2), (a, base + std::f64::consts::FRAC_PI_2)] {
        for k in 0..=n {
            let t = from + std::f64::consts::PI * k as f64 / n as f64;
            out.push([c[0] + ro * t.cos(), c[1] + ro * t.sin()]);
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
