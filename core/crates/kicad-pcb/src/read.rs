//! S-expression tree → [`Pcb`].
//!
//! Tolerant by construction: a form this reader does not know is counted,
//! not rejected, and reported as ONE warning per file (§2.2 of the spec).
//! Strict where silence would be wrong: a footprint on a layer other than
//! `F.Cu`/`B.Cu`, a missing uuid, a duplicate uuid, a missing thickness and
//! an unsupported version are loud errors.

use std::collections::{BTreeMap, HashSet};

use crate::model::*;
use crate::outline::circumcenter;
use crate::sexpr::Node;
use crate::KicadParse;

/// KiCad 6.0's `(version …)` stamp: the first S-expression board format
/// with `footprint`/`uuid|tstamp`/three-point arcs. Older files use the
/// `module` grammar and are refused.
pub const MIN_VERSION: u32 = 20211014;

const MM: f64 = 1e-3;

/// Top-level forms this reader consumes or deliberately passes over
/// without counting them as unknown.
const KNOWN_TOP_LEVEL: &[&str] = &[
    "version",
    "generator",
    "generator_version",
    "host",
    "general",
    "paper",
    "page",
    "title_block",
    "layers",
    "setup",
    "property",
    "net",
    "footprint",
    "gr_line",
    "gr_arc",
    "gr_circle",
    "gr_rect",
    "gr_poly",
    "gr_curve",
    "gr_text",
    "gr_text_box",
    "segment",
    "arc",
    "via",
    "zone",
    "group",
    "target",
    "dimension",
    "image",
    "table",
    "embedded_fonts",
    "embedded_files",
];

pub fn read(root: &Node) -> Result<Pcb, KicadParse> {
    match root.head() {
        Some("kicad_pcb") => {}
        other => {
            return Err(KicadParse::NotAKicadPcb {
                found: other.unwrap_or("").to_string(),
            })
        }
    }
    let version = match root.find("version") {
        Some(v) => v.number_arg(0, "version")? as u32,
        None => {
            let p = root.pos();
            return Err(KicadParse::Syntax {
                line: p.line,
                col: p.col,
                expected: "(version N) in kicad_pcb".to_string(),
            });
        }
    };
    if version < MIN_VERSION {
        return Err(KicadParse::UnsupportedVersion {
            found: version,
            min: MIN_VERSION,
        });
    }
    let generator = root
        .find("generator")
        .or_else(|| root.find("host"))
        .and_then(|g| g.args().first())
        .and_then(|a| a.text())
        .map(str::to_string);

    let mut warnings = Vec::new();

    let thickness_general_m = match root.find("general").and_then(|g| g.find("thickness")) {
        Some(t) => Some(t.number_arg(0, "thickness")? * MM),
        None => None,
    };
    let thickness_stackup_m = read_stackup(root)?;
    let thickness_m = match (thickness_stackup_m, thickness_general_m) {
        (Some(s), Some(g)) => {
            if (s - g).abs() > 1e-9 {
                warnings.push(format!(
                    "stackup thickness {:.6} mm differs from general thickness {:.6} mm; using the stackup",
                    s / MM,
                    g / MM
                ));
                s
            } else {
                // They agree: the declared number, not a float sum of layers
                // (1.6 mm, not 1.6000000000000003 mm).
                g
            }
        }
        (Some(s), None) => s,
        (None, Some(g)) => g,
        (None, None) => return Err(KicadParse::MissingThickness),
    };

    let title_block = read_title_block(root)?;

    let copper_layers = root
        .find("layers")
        .map(|layers| {
            layers
                .args()
                .iter()
                .filter(|l| match l {
                    Node::List(items, _) => items
                        .get(1)
                        .and_then(|n| n.text())
                        .is_some_and(|name| name.ends_with(".Cu")),
                    _ => false,
                })
                .count() as u32
        })
        .unwrap_or(0);

    let mut nets = Vec::new();
    for n in root.find_all("net") {
        nets.push(read_net(n)?);
    }

    let mut outline = Vec::new();
    let mut footprints = Vec::new();
    let mut seen_uuids = HashSet::new();
    let mut unknown: BTreeMap<String, usize> = BTreeMap::new();

    for child in root.args() {
        let Some(head) = child.head() else {
            continue;
        };
        match head {
            "gr_line" | "gr_arc" | "gr_circle" | "gr_rect" | "gr_poly" => {
                if layer_of(child) == Some("Edge.Cuts") {
                    read_graphic(child, None, &mut outline, &mut warnings)?;
                }
            }
            "footprint" => {
                let fp = read_footprint(child, &mut outline, &mut warnings)?;
                if !seen_uuids.insert(fp.uuid.clone()) {
                    return Err(KicadParse::DuplicateFootprintUuid { uuid: fp.uuid });
                }
                footprints.push(fp);
            }
            "module" => {
                let p = child.pos();
                return Err(KicadParse::Syntax {
                    line: p.line,
                    col: p.col,
                    expected: "footprint (the legacy `module` form is not supported)".to_string(),
                });
            }
            h if KNOWN_TOP_LEVEL.contains(&h) => {}
            other => *unknown.entry(other.to_string()).or_default() += 1,
        }
    }
    if !unknown.is_empty() {
        let total: usize = unknown.values().sum();
        let detail = unknown
            .iter()
            .map(|(k, v)| format!("{k} ×{v}"))
            .collect::<Vec<_>>()
            .join(", ");
        warnings.push(format!(
            "skipped {total} unknown top-level forms ({detail})"
        ));
    }

    Ok(Pcb {
        version,
        generator,
        thickness_m,
        thickness_general_m,
        thickness_stackup_m,
        title_block,
        copper_layers,
        nets,
        outline,
        footprints,
        warnings,
    })
}

fn read_stackup(root: &Node) -> Result<Option<f64>, KicadParse> {
    let Some(stackup) = root.find("setup").and_then(|s| s.find("stackup")) else {
        return Ok(None);
    };
    let mut sum = 0.0;
    let mut any = false;
    for layer in stackup.find_all("layer") {
        let kind = layer
            .find("type")
            .and_then(|t| t.args().first())
            .and_then(|a| a.text())
            .unwrap_or("");
        if !matches!(kind, "copper" | "core" | "prepreg") {
            continue;
        }
        if let Some(t) = layer.find("thickness") {
            sum += t.number_arg(0, "stackup layer thickness")? * MM;
            any = true;
        }
    }
    Ok(any.then_some(sum))
}

fn read_title_block(root: &Node) -> Result<TitleBlock, KicadParse> {
    let mut tb = TitleBlock::default();
    let Some(block) = root.find("title_block") else {
        return Ok(tb);
    };
    let text = |name: &str| -> String {
        block
            .find(name)
            .and_then(|n| n.args().first())
            .and_then(|a| a.text())
            .unwrap_or("")
            .to_string()
    };
    tb.title = text("title");
    tb.date = text("date");
    tb.rev = text("rev");
    tb.company = text("company");
    let mut comments: Vec<(u32, String)> = Vec::new();
    for c in block.find_all("comment") {
        let n = c.number_arg(0, "comment")? as u32;
        let t = c.text_arg(1, "comment")?.to_string();
        comments.push((n, t));
    }
    comments.sort_by_key(|(n, _)| *n);
    tb.comments = comments.into_iter().map(|(_, t)| t).collect();
    Ok(tb)
}

fn read_net(n: &Node) -> Result<Net, KicadParse> {
    Ok(Net {
        number: n.number_arg(0, "net")? as u32,
        name: n.text_arg(1, "net")?.to_string(),
    })
}

fn layer_of(n: &Node) -> Option<&str> {
    n.find("layer")
        .and_then(|l| l.args().first())
        .and_then(|a| a.text())
}

fn point(n: &Node, form: &str) -> Result<[f64; 2], KicadParse> {
    Ok([n.number_arg(0, form)? * MM, n.number_arg(1, form)? * MM])
}

fn required<'a>(n: &'a Node, name: &str, form: &str) -> Result<&'a Node, KicadParse> {
    n.find(name).ok_or_else(|| {
        let p = n.pos();
        KicadParse::Syntax {
            line: p.line,
            col: p.col,
            expected: format!("({name} …) in {form}"),
        }
    })
}

/// Placement of local coordinates into the board frame: identity for a
/// board-level graphic, the footprint's `place` for an `fp_*` one.
#[derive(Clone, Copy)]
struct Placement {
    at: [f64; 2],
    rotation_deg: f64,
}

impl Placement {
    const IDENTITY: Placement = Placement {
        at: [0.0, 0.0],
        rotation_deg: 0.0,
    };
    fn apply(&self, p: [f64; 2]) -> [f64; 2] {
        place(self.at, self.rotation_deg, p)
    }
}

fn read_graphic(
    n: &Node,
    placement: Option<(&Placement, &str)>,
    out: &mut Vec<OutlinePrimitive>,
    warnings: &mut Vec<String>,
) -> Result<(), KicadParse> {
    let (pl, footprint) = match placement {
        Some((pl, uuid)) => (*pl, Some(uuid.to_string())),
        None => (Placement::IDENTITY, None),
    };
    let form = n.head().unwrap_or("graphic");
    let mut push = |shape: OutlineShape| {
        out.push(OutlinePrimitive {
            shape,
            footprint: footprint.clone(),
        })
    };
    match form {
        "gr_line" | "fp_line" => {
            let s = pl.apply(point(required(n, "start", form)?, form)?);
            let e = pl.apply(point(required(n, "end", form)?, form)?);
            push(OutlineShape::Line { start: s, end: e });
        }
        "gr_arc" | "fp_arc" => {
            let s = pl.apply(point(required(n, "start", form)?, form)?);
            let m = pl.apply(point(required(n, "mid", form)?, form)?);
            let e = pl.apply(point(required(n, "end", form)?, form)?);
            push(arc_or_line(s, m, e, form, n, warnings));
        }
        "gr_circle" | "fp_circle" => {
            let c = point(required(n, "center", form)?, form)?;
            let e = point(required(n, "end", form)?, form)?;
            let radius = ((e[0] - c[0]).powi(2) + (e[1] - c[1]).powi(2)).sqrt();
            push(OutlineShape::Circle {
                center: pl.apply(c),
                radius,
            });
        }
        "gr_rect" | "fp_rect" => {
            let a = point(required(n, "start", form)?, form)?;
            let b = point(required(n, "end", form)?, form)?;
            let corners = [a, [b[0], a[1]], b, [a[0], b[1]]].map(|p| pl.apply(p));
            for i in 0..4 {
                push(OutlineShape::Line {
                    start: corners[i],
                    end: corners[(i + 1) % 4],
                });
            }
        }
        "gr_poly" | "fp_poly" => {
            let pts = required(n, "pts", form)?;
            // A polygon's point list mixes `(xy x y)` and, since KiCad 7,
            // `(arc (start) (mid) (end))` items; consecutive items connect.
            let mut first: Option<[f64; 2]> = None;
            let mut prev: Option<[f64; 2]> = None;
            for item in pts.args() {
                match item.head() {
                    Some("xy") => {
                        let p = pl.apply(point(item, "xy")?);
                        if let Some(q) = prev {
                            push(OutlineShape::Line { start: q, end: p });
                        }
                        first.get_or_insert(p);
                        prev = Some(p);
                    }
                    Some("arc") => {
                        let s = pl.apply(point(required(item, "start", "pts arc")?, "arc")?);
                        let m = pl.apply(point(required(item, "mid", "pts arc")?, "arc")?);
                        let e = pl.apply(point(required(item, "end", "pts arc")?, "arc")?);
                        if let Some(q) = prev {
                            if dist(q, s) > 0.0 {
                                push(OutlineShape::Line { start: q, end: s });
                            }
                        }
                        first.get_or_insert(s);
                        push(arc_or_line(s, m, e, form, item, warnings));
                        prev = Some(e);
                    }
                    _ => {
                        let p = item.pos();
                        return Err(KicadParse::Syntax {
                            line: p.line,
                            col: p.col,
                            expected: "(xy …) or (arc …) in pts".to_string(),
                        });
                    }
                }
            }
            if let (Some(f), Some(l)) = (first, prev) {
                if dist(f, l) > 0.0 {
                    push(OutlineShape::Line { start: l, end: f });
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

/// Three-point arc, or — when the points are collinear (spec §3 O7) — the
/// chord as a line plus a warning. Never a silent degenerate arc.
fn arc_or_line(
    s: [f64; 2],
    m: [f64; 2],
    e: [f64; 2],
    form: &str,
    n: &Node,
    warnings: &mut Vec<String>,
) -> OutlineShape {
    if circumcenter(s, m, e).is_some() {
        OutlineShape::Arc {
            start: s,
            mid: m,
            end: e,
        }
    } else {
        let p = n.pos();
        warnings.push(format!(
            "{form} at line {}: start/mid/end are collinear; read as a line",
            p.line
        ));
        OutlineShape::Line { start: s, end: e }
    }
}

fn read_footprint(
    n: &Node,
    outline: &mut Vec<OutlinePrimitive>,
    warnings: &mut Vec<String>,
) -> Result<Footprint, KicadParse> {
    let form = "footprint";
    let library_id = n.text_arg(0, form)?.to_string();
    let side = match layer_of(n) {
        Some("F.Cu") => Side::Front,
        Some("B.Cu") => Side::Back,
        _ => {
            let p = n.pos();
            return Err(KicadParse::Syntax {
                line: p.line,
                col: p.col,
                expected: "(layer \"F.Cu\") or (layer \"B.Cu\") in footprint".to_string(),
            });
        }
    };
    let uuid = match n.find("uuid").or_else(|| n.find("tstamp")) {
        Some(u) => u.text_arg(0, "uuid")?.to_string(),
        None => {
            let p = n.pos();
            return Err(KicadParse::Syntax {
                line: p.line,
                col: p.col,
                expected: "(uuid …) or (tstamp …) in footprint".to_string(),
            });
        }
    };
    let at_node = required(n, "at", form)?;
    let at = point(at_node, "at")?;
    let rotation_deg = match at_node.args().get(2) {
        Some(r) => r.number()?,
        None => 0.0,
    };
    let placement = Placement { at, rotation_deg };

    // Properties: KiCad 7+ `(property "Reference" "R1" …)`; KiCad 6
    // `(fp_text reference "R1" …)`.
    let mut reference = String::new();
    let mut value = String::new();
    let mut footprint = String::new();
    let mut datasheet = String::new();
    for p in n.find_all("property") {
        let key = p.text_arg(0, "property")?;
        let val = p.args().get(1).and_then(|a| a.text()).unwrap_or("");
        match key {
            "Reference" => reference = val.to_string(),
            "Value" => value = val.to_string(),
            "Footprint" => footprint = val.to_string(),
            "Datasheet" => datasheet = val.to_string(),
            _ => {}
        }
    }
    for t in n.find_all("fp_text") {
        let kind = t.text_arg(0, "fp_text")?;
        let val = t.args().get(1).and_then(|a| a.text()).unwrap_or("");
        match kind {
            "reference" if reference.is_empty() => reference = val.to_string(),
            "value" if value.is_empty() => value = val.to_string(),
            _ => {}
        }
    }
    if footprint.is_empty() {
        footprint = library_id.clone();
    }

    let attrs = n
        .find("attr")
        .map(|a| {
            a.args()
                .iter()
                .filter_map(|x| x.text())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let mut pads = Vec::new();
    for p in n.find_all("pad") {
        pads.push(read_pad(p, &placement)?);
    }

    let mut models = Vec::new();
    for m in n.find_all("model") {
        models.push(read_model(m)?);
    }

    for child in n.args() {
        if matches!(
            child.head(),
            Some("fp_line" | "fp_arc" | "fp_circle" | "fp_rect" | "fp_poly")
        ) && layer_of(child) == Some("Edge.Cuts")
        {
            read_graphic(child, Some((&placement, &uuid)), outline, warnings)?;
        }
    }

    Ok(Footprint {
        uuid,
        library_id,
        side,
        at,
        rotation_deg,
        reference,
        value,
        footprint,
        datasheet,
        attrs,
        pads,
        models,
    })
}

fn read_pad(p: &Node, placement: &Placement) -> Result<Pad, KicadParse> {
    let form = "pad";
    let number = p.text_arg(0, form)?.to_string();
    let kind = match p.text_arg(1, form)? {
        "thru_hole" => PadKind::ThruHole,
        "smd" => PadKind::Smd,
        "np_thru_hole" => PadKind::NpThruHole,
        "connect" => PadKind::Connect,
        _ => {
            let pos = p.pos();
            return Err(KicadParse::Syntax {
                line: pos.line,
                col: pos.col,
                expected: "pad type thru_hole|smd|np_thru_hole|connect".to_string(),
            });
        }
    };
    let shape = p.text_arg(2, form)?.to_string();
    let at_node = required(p, "at", form)?;
    let local = point(at_node, "pad at")?;
    let pad_rot = match at_node.args().get(2) {
        Some(r) => r.number()?,
        None => 0.0,
    };
    let size_node = required(p, "size", form)?;
    let size = point(size_node, "pad size")?;
    let drill = match p.find("drill") {
        Some(d) => {
            let args = d.args();
            if args.first().and_then(|a| a.text()) == Some("oval") {
                let w = d.number_arg(1, "drill oval")? * MM;
                let h = match args.get(2) {
                    Some(h) if h.text().is_some() && h.number().is_ok() => h.number()? * MM,
                    _ => w,
                };
                Some([w, h])
            } else {
                match args.first() {
                    Some(a) if a.text().is_some() => {
                        let dia = a.number()? * MM;
                        Some([dia, dia])
                    }
                    _ => None,
                }
            }
        }
        None => None,
    };
    let net = match p.find("net") {
        Some(n) => Some(read_net(n)?),
        None => None,
    };
    Ok(Pad {
        number,
        kind,
        shape,
        position: placement.apply(local),
        rotation_deg: placement.rotation_deg + pad_rot,
        size,
        drill,
        net,
    })
}

fn read_model(m: &Node) -> Result<Model, KicadParse> {
    let path = m.text_arg(0, "model")?.to_string();
    let xyz = |name: &str, default: [f64; 3], scale: f64| -> Result<[f64; 3], KicadParse> {
        match m.find(name).and_then(|n| n.find("xyz")) {
            Some(v) => Ok([
                v.number_arg(0, name)? * scale,
                v.number_arg(1, name)? * scale,
                v.number_arg(2, name)? * scale,
            ]),
            None => Ok(default),
        }
    };
    let hidden = m.args().iter().any(|a| {
        a.text() == Some("hide")
            || (a.head() == Some("hide") && a.args().first().and_then(|x| x.text()) != Some("no"))
    });
    Ok(Model {
        path,
        offset_m: xyz("offset", [0.0; 3], MM)?,
        scale: xyz("scale", [1.0; 3], 1.0)?,
        rotate_deg: xyz("rotate", [0.0; 3], 1.0)?,
        hidden,
    })
}
