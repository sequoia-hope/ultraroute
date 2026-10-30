//! Footprints moved, turned and flipped to the other side in a `.kicad_pcb`, as pcbnew 9 does it.
//!
//! [`apply`] gives each named part a position, a rotation and a side, and rewrites only those
//! footprints: every other byte of the file is kept. A footprint is redone the way pcbnew's
//! `FOOTPRINT::Flip`, `SetPosition` and `SetOrientation` do it (KiCad 9.0 source, `footprint.cpp`,
//! `pad.cpp`, `pcb_text.cpp`, `eda_shape.cpp`, `trigo.cpp`): its items are read into the board
//! frame in whole nanometres, as pcbnew holds them, moved there with pcbnew's own rounding, then
//! written back in the footprint's frame, sorted as pcbnew sorts them and laid out by a port of
//! KiCad's `Prettify`. The result is byte for byte what pcbnew saves after the same calls (the
//! golden tests hold it to that).
//!
//! What a flip does to a footprint, and so to the text: the side layers swap (`F.SilkS` ↔
//! `B.SilkS`, a pad's `F.Cu` ↔ `B.Cu`), every point is mirrored top to bottom about the part's
//! origin, the rotation is negated, text on a side's layer is mirrored (`(justify mirror)`), and a
//! rectangle turned off the right angles becomes a four-point polygon, as pcbnew makes it.
//!
//! Forms a part can hold that are not handled here — a dimension, a text box, an image, a pad
//! stack with per-layer shapes, a polygon with arcs in it — make [`apply`] refuse the part rather
//! than write it half moved.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::model::Side;
use crate::sexpr::{self, Node};
use crate::KicadParse;

/// Where one part goes.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    /// The part's reference designator, `"U1"`.
    pub reference: String,
    /// The footprint origin in the board frame, metres (Y down, as the file is).
    pub at: [f64; 2],
    /// Degrees counter-clockwise as KiCad shows it: the footprint's `(at x y rotation)`.
    pub rotation_deg: f64,
    pub side: Side,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PlaceError {
    #[error(transparent)]
    Parse(#[from] KicadParse),
    #[error("no part {0} on the board")]
    NoSuchPart(String),
    #[error("two footprints are both {0}")]
    AmbiguousPart(String),
    #[error("{reference}: {what} cannot be moved yet")]
    Unsupported { reference: String, what: String },
}

/// `text` with each placement's footprint moved, rotated and flipped to its side.
pub fn apply(text: &str, placements: &[Placement]) -> Result<String, PlaceError> {
    let root = sexpr::parse(text)?;
    let layers = LayerIds::of(&root);
    let mut by_ref: HashMap<String, Vec<&Node>> = HashMap::new();
    for fp in root.find_all("footprint") {
        if let Some(r) = reference(fp) {
            by_ref.entry(r).or_default().push(fp);
        }
    }
    let mut placed: HashMap<usize, String> = HashMap::new();
    for p in placements {
        let fp = match by_ref.get(&p.reference).map(Vec::as_slice) {
            Some([one]) => *one,
            Some([]) | None => return Err(PlaceError::NoSuchPart(p.reference.clone())),
            Some(_) => return Err(PlaceError::AmbiguousPart(p.reference.clone())),
        };
        let tree = place(Tree::of(fp, text), p, &layers)?;
        placed.insert(fp.pos().start, prettify(&tree.compact(), 1));
    }
    // The board's footprints are written sorted by `BOARD_ITEM::ptr_cmp`: side, then uuid. A part
    // that changed side takes its new place among them.
    let footprints: Vec<&Node> = root.find_all("footprint").collect();
    let mut sorted: Vec<(i64, String, usize)> = footprints
        .iter()
        .enumerate()
        .map(|(k, fp)| {
            let node_text = |name: &str| fp.find(name).and_then(|n| n.args().first()).and_then(Node::text).unwrap_or("").to_string();
            let mut layer = node_text("layer");
            if let Some(new) = placed.get(&fp.pos().start) {
                let new = sexpr::parse(new).ok();
                layer = new.as_ref().and_then(|n| n.find("layer")).and_then(|l| l.args().first()).and_then(Node::text).unwrap_or("").to_string();
            }
            (layers.id(&layer), node_text("uuid"), k)
        })
        .collect();
    let in_file_order = sorted.windows(2).all(|w| (w[0].0, &w[0].1) <= (w[1].0, &w[1].1));
    if !in_file_order {
        sorted.sort();
    } else {
        sorted.sort_by_key(|s| s.2);
    }
    let mut out = String::with_capacity(text.len());
    let mut kept = 0;
    for (slot, (_, _, from)) in footprints.iter().zip(sorted) {
        let pos = slot.pos();
        out.push_str(&text[kept..pos.start]);
        let fp = footprints[from];
        match placed.get(&fp.pos().start) {
            Some(new) => out.push_str(new),
            None => out.push_str(&text[fp.pos().start..fp.pos().end]),
        }
        kept = pos.end;
    }
    out.push_str(&text[kept..]);
    Ok(out)
}

fn reference(fp: &Node) -> Option<String> {
    let prop = fp.find_all("property").find(|p| p.args().first().and_then(Node::text) == Some("Reference"));
    let legacy = || fp.find_all("fp_text").find(|t| matches!(t.args().first(), Some(Node::Atom(a, _)) if a == "reference"));
    prop.or_else(legacy).and_then(|n| n.args().get(1)).and_then(Node::text).map(str::to_string)
}

// --- The tree, with each token's text as the file had it ----------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tree {
    List(Vec<Tree>),
    /// An atom or a quoted string, quotes and escapes as written.
    Token(String),
}

impl Tree {
    fn of(node: &Node, text: &str) -> Tree {
        match node {
            Node::List(items, _) => Tree::List(items.iter().map(|n| Tree::of(n, text)).collect()),
            _ => Tree::Token(text[node.pos().start..node.pos().end].to_string()),
        }
    }

    fn list(head: &str, args: Vec<String>) -> Tree {
        Tree::List(std::iter::once(head.to_string()).chain(args).map(Tree::Token).collect())
    }

    fn head(&self) -> Option<&str> {
        match self {
            Tree::List(items) => match items.first() {
                Some(Tree::Token(t)) => Some(t),
                _ => None,
            },
            Tree::Token(_) => None,
        }
    }

    fn items(&self) -> &[Tree] {
        match self {
            Tree::List(items) => items,
            Tree::Token(_) => &[],
        }
    }

    fn items_mut(&mut self) -> &mut Vec<Tree> {
        match self {
            Tree::List(items) => items,
            Tree::Token(_) => unreachable!("a token has no items"),
        }
    }

    fn token(&self) -> Option<&str> {
        match self {
            Tree::Token(t) => Some(t),
            Tree::List(_) => None,
        }
    }

    fn find(&self, head: &str) -> Option<&Tree> {
        self.items().iter().skip(1).find(|t| t.head() == Some(head))
    }

    fn find_mut(&mut self, head: &str) -> Option<&mut Tree> {
        self.items_mut().iter_mut().skip(1).find(|t| t.head() == Some(head))
    }

    /// The `i`th argument (after the head) as text, quotes removed.
    fn arg(&self, i: usize) -> Option<String> {
        self.items().get(i + 1).and_then(Tree::token).map(unquote)
    }

    fn number(&self, i: usize) -> Option<f64> {
        self.items().get(i + 1).and_then(Tree::token).and_then(|t| t.parse().ok())
    }

    /// `(head x y …)`: the point its first two arguments make, nanometres.
    fn point(&self) -> Option<P> {
        Some([iu(self.number(0)?), iu(self.number(1)?)])
    }

    /// Replace arguments `from..` with `args`.
    fn set_args(&mut self, from: usize, args: Vec<String>) {
        let items = self.items_mut();
        items.truncate(from + 1);
        items.extend(args.into_iter().map(Tree::Token));
    }

    fn set_point(&mut self, p: P) {
        self.set_args(0, vec![fmt_iu(p[0]), fmt_iu(p[1])]);
    }

    /// Every list in this one, at any depth, whose head is `head`.
    fn visit_mut(&mut self, head: &str, f: &mut dyn FnMut(&mut Tree)) {
        if let Tree::List(items) = self {
            for t in items.iter_mut() {
                if t.head() == Some(head) {
                    f(t);
                } else {
                    t.visit_mut(head, f);
                }
            }
        }
    }

    fn contains(&self, head: &str) -> bool {
        self.items().iter().any(|t| t.head() == Some(head) || t.contains(head))
    }

    /// `(a b (c d))`, one space between tokens: what KiCad's formatter writes before `Prettify`.
    fn compact(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Tree::Token(t) => out.push_str(t),
            Tree::List(items) => {
                out.push('(');
                for (i, t) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    t.write(out);
                }
                out.push(')');
            }
        }
    }
}

fn unquote(t: &str) -> String {
    match t.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        Some(inner) => {
            let mut s = String::new();
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                s.push(if c == '\\' { chars.next().unwrap_or('\\') } else { c });
            }
            s
        }
        None => t.to_string(),
    }
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

// --- KiCad's numbers ------------------------------------------------------------------------------

/// A point in KiCad's internal units, nanometres.
type P = [i64; 2];

/// `KiROUND`: half away from zero, by adding a half and truncating.
fn kiround(v: f64) -> i64 {
    (if v < 0.0 { v - 0.5 } else { v + 0.5 }) as i64
}

/// Millimetres read from the file into nanometres, as the parser's `parseBoardUnits` does.
fn iu(mm: f64) -> i64 {
    kiround(mm * 1e6)
}

/// `FormatInternalUnits`: nanometres as millimetres.
fn fmt_iu(v: i64) -> String {
    let mm = v as f64 / 1e6;
    if mm != 0.0 && mm.abs() <= 0.0001 {
        let s = format!("{mm:.10}");
        let s = s.trim_end_matches('0');
        s.strip_suffix('.').unwrap_or(s).to_string()
    } else {
        g10(mm)
    }
}

/// C's `%.10g`.
fn g10(v: f64) -> String {
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.to_string();
    }
    let e = format!("{v:.9e}");
    let (mantissa, exp) = e.split_once('e').expect("exponent form");
    let exp: i32 = exp.parse().expect("exponent");
    let trim = |s: &str| {
        if s.contains('.') {
            let s = s.trim_end_matches('0');
            s.strip_suffix('.').unwrap_or(s).to_string()
        } else {
            s.to_string()
        }
    };
    if !(-4..10).contains(&exp) {
        return format!("{}e{}{:02}", trim(mantissa), if exp < 0 { '-' } else { '+' }, exp.abs());
    }
    trim(&format!("{v:.*}", (9 - exp) as usize))
}

/// `EDA_ANGLE::FormatAngle`.
fn fmt_angle(deg: f64) -> String {
    g10(deg)
}

/// `EDA_ANGLE::Normalize`: [0, 360).
fn normalize(mut a: f64) -> f64 {
    while a < -0.0 {
        a += 360.0;
    }
    while a >= 360.0 {
        a -= 360.0;
    }
    a
}

/// `EDA_ANGLE::Normalize180`: (-180, 180].
fn normalize180(mut a: f64) -> f64 {
    while a <= -180.0 {
        a += 360.0;
    }
    while a > 180.0 {
        a -= 360.0;
    }
    a
}

fn is_cardinal(a: f64) -> bool {
    let mut t = a;
    while t < 0.0 {
        t += 90.0;
    }
    while t >= 90.0 {
        t -= 90.0;
    }
    t == 0.0
}

const DEGREES_TO_RADIANS: f64 = std::f64::consts::PI / 180.0;

fn sin(a: f64) -> f64 {
    let t = normalize(a);
    if t == 0.0 || t == 180.0 {
        0.0
    } else if t == 45.0 || t == 135.0 {
        std::f64::consts::FRAC_1_SQRT_2
    } else if t == 225.0 || t == 315.0 {
        -std::f64::consts::FRAC_1_SQRT_2
    } else if t == 90.0 {
        1.0
    } else if t == 270.0 {
        -1.0
    } else {
        (t * DEGREES_TO_RADIANS).sin()
    }
}

fn cos(a: f64) -> f64 {
    let t = normalize(a);
    if t == 0.0 {
        1.0
    } else if t == 180.0 {
        -1.0
    } else if t == 90.0 || t == 270.0 {
        0.0
    } else if t == 45.0 || t == 315.0 {
        std::f64::consts::FRAC_1_SQRT_2
    } else if t == 135.0 || t == 225.0 {
        -std::f64::consts::FRAC_1_SQRT_2
    } else {
        (t * DEGREES_TO_RADIANS).cos()
    }
}

/// `RotatePoint(VECTOR2I&, EDA_ANGLE)`.
fn rotate(p: P, angle: f64) -> P {
    let [x, y] = p;
    let a = normalize(angle);
    if a == 0.0 {
        [x, y]
    } else if a == 90.0 {
        [y, -x]
    } else if a == 180.0 {
        [-x, -y]
    } else if a == 270.0 {
        [-y, x]
    } else {
        let (s, c) = (sin(a), cos(a));
        let (x, y) = (x as f64, y as f64);
        [kiround(y * s + x * c), kiround(y * c - x * s)]
    }
}

fn rotate_about(p: P, centre: P, angle: f64) -> P {
    let r = rotate([p[0] - centre[0], p[1] - centre[1]], angle);
    [r[0] + centre[0], r[1] + centre[1]]
}

/// `MIRROR(…, FLIP_DIRECTION::TOP_BOTTOM)`.
fn mirror(p: P, centre: P) -> P {
    [p[0], 2 * centre[1] - p[1]]
}

fn add(p: P, d: P) -> P {
    [p[0] + d[0], p[1] + d[1]]
}

/// `EDA_ANGLE(VECTOR2D)`: degrees.
fn angle_of(x: f64, y: f64) -> f64 {
    if x == 0.0 && y == 0.0 {
        0.0
    } else if y == 0.0 {
        if x >= 0.0 { 0.0 } else { -180.0 }
    } else if x == 0.0 {
        if y >= 0.0 { 90.0 } else { -90.0 }
    } else if x == y {
        if x >= 0.0 { 45.0 } else { -180.0 + 45.0 }
    } else if x == -y {
        if x >= 0.0 { -45.0 } else { 180.0 - 45.0 }
    } else {
        y.atan2(x) / DEGREES_TO_RADIANS
    }
}

/// `CalcArcCenter(VECTOR2I…)`: the centre KiCad keeps for an arc read as start, mid and end.
fn arc_centre(start: P, mid: P, end: P) -> P {
    let c = arc_centre_f([start[0] as f64, start[1] as f64], [mid[0] as f64, mid[1] as f64], [end[0] as f64, end[1] as f64]);
    let clamp = |v: f64| kiround(v.clamp(i32::MIN as f64 + 100.0, i32::MAX as f64 - 100.0));
    [clamp(c[0]), clamp(c[1])]
}

/// `CalcArcCenter(VECTOR2D…)`, its rounding to round numbers within the error included.
#[allow(clippy::many_single_char_names)]
fn arc_centre_f(s: [f64; 2], m: [f64; 2], e: [f64; 2]) -> [f64; 2] {
    let y21 = m[1] - s[1];
    let mut x21 = m[0] - s[0];
    let y32 = e[1] - m[1];
    let mut x32 = e[0] - m[0];
    if (x21 == 0.0 && y32 == 0.0) || (y21 == 0.0 && x32 == 0.0) {
        return [(s[0] + e[0]) / 2.0, (s[1] + e[1]) / 2.0];
    }
    if x21 == 0.0 {
        x21 = f64::EPSILON;
    }
    if x32 == 0.0 {
        x32 = -f64::EPSILON;
    }
    let mut a = y21 / x21;
    let mut b = y32 / x32;
    let norm = |x: f64, y: f64| (x * x + y * y).sqrt();
    let da = a * norm(0.5 / y21, 0.5 / x21);
    let db = b * norm(0.5 / y32, 0.5 / x32);
    if a == b {
        if s == e {
            return [(s[0] + m[0]) / 2.0, (s[1] + m[1]) / 2.0];
        }
        a += f64::EPSILON;
        b -= f64::EPSILON;
    }
    if a == 0.0 {
        a = 1e-10;
    }
    if b == 0.0 {
        b = 1e-10;
    }
    let r = std::f64::consts::FRAC_1_SQRT_2;
    let ab_se_y = a * b * (s[1] - e[1]);
    let d_ab_se_y = ab_se_y * ((da / a * da / a) + (db / b * db / b) + (r / (s[1] - e[1]) * r / (s[1] - e[1]))).sqrt();
    let b_sm_x = b * (s[0] + m[0]);
    let d_b_sm_x = b_sm_x * ((db / b * db / b) + (r / (s[0] + m[0]) * r / (s[0] + m[0]))).sqrt();
    let a_me_x = a * (m[0] + e[0]);
    let d_a_me_x = a_me_x * ((da / a * da / a) + (r / (m[0] + e[0]) * r / (m[0] + e[0]))).sqrt();
    let twice = 2.0 * (b - a);
    let d_twice = 2.0 * (db * db + da * da).sqrt();
    let num_x = ab_se_y + b_sm_x - a_me_x;
    let d_num_x = (d_ab_se_y * d_ab_se_y + d_b_sm_x * d_b_sm_x + d_a_me_x * d_a_me_x).sqrt();
    let cx = (ab_se_y + b_sm_x - a_me_x) / twice;
    let dcx = cx * ((d_num_x / num_x * d_num_x / num_x) + (d_twice / twice * d_twice / twice)).sqrt();
    let num_y = (s[0] + m[0]) / 2.0 - cx;
    let d_num_y = (1.0 / 8.0 + dcx * dcx).sqrt();
    let first = num_y / a;
    let d_first = first * ((d_num_y / num_y * d_num_y / num_y) + (da / a * da / a)).sqrt();
    let cy = first + (s[1] + m[1]) / 2.0;
    let dcy = (d_first * d_first + 1.0 / 8.0).sqrt();
    let r100 = |v: f64| ((v + 50.0) / 100.0).floor() * 100.0;
    let r10 = |v: f64| ((v + 5.0) / 10.0).floor() * 10.0;
    if (r100(cx) - cx).abs() < dcx && (r100(cy) - cy).abs() < dcy {
        [r100(cx), r100(cy)]
    } else if (r10(cx) - cx).abs() < dcx && (r10(cy) - cy).abs() < dcy {
        [r10(cx), r10(cy)]
    } else {
        [cx, cy]
    }
}

// --- Layers ---------------------------------------------------------------------------------------

/// Layer ids for sorting, from the board's `(layers …)` table and KiCad 9's numbering.
struct LayerIds {
    ids: HashMap<String, i64>,
    copper: usize,
}

impl LayerIds {
    fn of(root: &Node) -> LayerIds {
        let mut ids: HashMap<String, i64> = KICAD9_LAYERS.iter().map(|(n, i)| (n.to_string(), *i)).collect();
        for k in 1..=30 {
            ids.insert(format!("In{k}.Cu"), 2 + 2 * k as i64);
        }
        for k in 1..=45 {
            ids.insert(format!("User.{k}"), 37 + 2 * k as i64);
        }
        let mut copper = 0;
        if let Some(table) = root.find("layers") {
            for l in table.args() {
                let (Some(id), Some(name)) = (l.head().and_then(|h| h.parse::<i64>().ok()), l.args().first().and_then(Node::text)) else { continue };
                ids.insert(name.to_string(), id);
                if name.ends_with(".Cu") {
                    copper += 1;
                }
            }
        }
        LayerIds { ids, copper }
    }

    fn id(&self, name: &str) -> i64 {
        self.ids.get(name).copied().unwrap_or(i64::MAX)
    }

    /// `BOARD::FlipLayer`: front ↔ back, and inner copper mirrored through the stack.
    fn flip(&self, name: &str) -> String {
        if let Some(rest) = name.strip_prefix("F.") {
            return format!("B.{rest}");
        }
        if let Some(rest) = name.strip_prefix("B.") {
            return format!("F.{rest}");
        }
        if let Some(k) = name.strip_prefix("In").and_then(|r| r.strip_suffix(".Cu")).and_then(|k| k.parse::<usize>().ok()) {
            if self.copper > 2 && k < self.copper - 1 {
                return format!("In{}.Cu", self.copper - 1 - k);
            }
        }
        name.to_string()
    }
}

const KICAD9_LAYERS: [(&str, i64); 22] = [
    ("F.Cu", 0),
    ("B.Cu", 2),
    ("F.Mask", 1),
    ("B.Mask", 3),
    ("F.SilkS", 5),
    ("B.SilkS", 7),
    ("F.Adhes", 9),
    ("B.Adhes", 11),
    ("F.Paste", 13),
    ("B.Paste", 15),
    ("Dwgs.User", 17),
    ("Cmts.User", 19),
    ("Eco1.User", 21),
    ("Eco2.User", 23),
    ("Edge.Cuts", 25),
    ("Margin", 27),
    ("B.CrtYd", 29),
    ("F.CrtYd", 31),
    ("B.Fab", 33),
    ("F.Fab", 35),
    ("Rescue", 37),
    ("*.Cu", -1),
];

/// `LSET::SideSpecificMask`: copper and each side's technical layers.
fn is_side_specific(layer: &str) -> bool {
    layer.starts_with("F.") || layer.starts_with("B.") || layer.ends_with(".Cu")
}

// --- One footprint ------------------------------------------------------------------------------

/// The geometry of one item of a footprint, in the board frame, as pcbnew holds it.
#[derive(Debug, Clone)]
enum Geo {
    /// A property or `fp_text`: its position and its absolute angle.
    Text { pos: P, angle: f64 },
    Shape(Shape),
    /// Its centre and absolute orientation.
    Pad { pos: P, orient: f64 },
    /// Every `(xy)` in it, in the board frame already.
    Zone,
    /// Nothing that moves.
    Fixed,
}

#[derive(Debug, Clone)]
enum Shape {
    Segment(P, P),
    Rect(P, P),
    /// Centre and a point on the circle.
    Circle(P, P),
    /// Start, end and centre, and the start, end, centre and mid read from the file while they
    /// still hold (`m_arcMidData`).
    Arc { start: P, end: P, centre: P, cache: Option<[P; 4]> },
    Poly(Vec<P>),
    Bezier([P; 4]),
}

impl Shape {
    fn points_mut(&mut self) -> Vec<&mut P> {
        match self {
            Shape::Segment(a, b) | Shape::Rect(a, b) | Shape::Circle(a, b) => vec![a, b],
            Shape::Arc { start, end, centre, cache } => {
                let mut v = vec![start, end, centre];
                if let Some(c) = cache {
                    v.extend(c.iter_mut());
                }
                v
            }
            Shape::Poly(pts) => pts.iter_mut().collect(),
            Shape::Bezier(pts) => pts.iter_mut().collect(),
        }
    }

    fn translate(&mut self, d: P) {
        for p in self.points_mut() {
            *p = add(*p, d);
        }
    }

    /// `EDA_SHAPE::rotate`; a rectangle turned off the right angles becomes a polygon.
    fn rotate(&mut self, centre: P, angle: f64) {
        if let Shape::Rect(s, e) = *self {
            if !is_cardinal(angle) {
                *self = Shape::Poly(vec![s, [e[0], s[1]], e, [s[0], e[1]]]);
            }
        }
        for p in self.points_mut() {
            *p = rotate_about(*p, centre, angle);
        }
    }

    /// `EDA_SHAPE::flip`, top to bottom: an arc's ends swap.
    fn flip(&mut self, centre: P) {
        if let Shape::Arc { start, end, centre: c, .. } = self {
            // The kept data is not mirrored: an arc symmetric about the flip lands back on it.
            *start = mirror(*start, centre);
            *end = mirror(*end, centre);
            *c = mirror(*c, centre);
            std::mem::swap(start, end);
            return;
        }
        for p in self.points_mut() {
            *p = mirror(*p, centre);
        }
    }

    /// `EDA_SHAPE::GetArcMid`.
    fn arc_mid(start: P, end: P, centre: P, cache: Option<[P; 4]>) -> P {
        if let Some([cs, ce, cc, cm]) = cache {
            if cs == start && ce == end && cc == centre {
                return cm;
            }
        }
        let radial = |p: P| ((p[0] - centre[0]) as f64, (p[1] - centre[1]) as f64);
        let (sx, sy) = radial(start);
        let (ex, ey) = radial(end);
        let a0 = angle_of(sx, sy);
        let mut a1 = angle_of(ex, ey);
        if a1 == a0 {
            a1 = a0 + 360.0;
        }
        while a1 < a0 {
            a1 += 360.0;
        }
        rotate_about(start, centre, -(a1 - a0) / 2.0)
    }

    fn rank(&self) -> u8 {
        match self {
            Shape::Segment(..) => 0,
            Shape::Rect(..) => 1,
            Shape::Arc { .. } => 2,
            Shape::Circle(..) => 3,
            Shape::Poly(..) => 4,
            Shape::Bezier(..) => 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Field,
    Drawing,
    Pad,
    Zone,
    Other,
}

struct Item {
    tree: Tree,
    role: Role,
    geo: Geo,
}

/// The footprint `tree` placed by `p`.
fn place(mut tree: Tree, p: &Placement, layers: &LayerIds) -> Result<Tree, PlaceError> {
    let unsupported = |what: &str| PlaceError::Unsupported { reference: p.reference.clone(), what: what.to_string() };
    let at = tree.find("at").ok_or_else(|| unsupported("a footprint with no (at …)"))?;
    let mut pos = at.point().ok_or_else(|| unsupported("its (at …)"))?;
    let mut orient = normalize180(at.number(2).unwrap_or(0.0));
    let side = match tree.find("layer").and_then(|l| l.arg(0)).as_deref() {
        Some("B.Cu") => Side::Back,
        _ => Side::Front,
    };

    // Read every item into the board frame, as the parser does: rotated by the footprint's
    // orientation about the origin, then moved to its position.
    let to_board = |local: P, pos: P, orient: f64| add(rotate(local, orient), pos);
    let mut items: Vec<Item> = Vec::new();
    let mut header: Vec<Tree> = Vec::new();
    for (i, child) in std::mem::take(tree.items_mut()).into_iter().enumerate() {
        if i == 0 {
            header.push(child);
            continue;
        }
        let head = child.head().unwrap_or("").to_string();
        let (role, geo) = match head.as_str() {
            "property" | "fp_text" => {
                let at = child.find("at");
                let local = at.and_then(Tree::point).unwrap_or([0, 0]);
                let angle = normalize(at.and_then(|a| a.number(2)).unwrap_or(0.0));
                let role = if head == "property" { Role::Field } else { Role::Drawing };
                (role, Geo::Text { pos: to_board(local, pos, orient), angle })
            }
            "fp_line" | "fp_rect" | "fp_circle" | "fp_arc" | "fp_poly" | "fp_curve" => {
                let pt = |name: &str| child.find(name).and_then(Tree::point).ok_or_else(|| unsupported(&format!("a {head} with no ({name} …)")));
                let mut shape = match head.as_str() {
                    "fp_line" => Shape::Segment(pt("start")?, pt("end")?),
                    "fp_rect" => Shape::Rect(pt("start")?, pt("end")?),
                    "fp_circle" => Shape::Circle(pt("center")?, pt("end")?),
                    "fp_arc" => {
                        let (s, m, e) = (pt("start")?, pt("mid")?, pt("end")?);
                        let c = arc_centre(s, m, e);
                        Shape::Arc { start: s, end: e, centre: c, cache: Some([s, e, c, m]) }
                    }
                    _ => {
                        let pts = child.find("pts").ok_or_else(|| unsupported(&format!("a {head} with no points")))?;
                        if pts.contains("arc") {
                            return Err(unsupported("a polygon with arcs in it"));
                        }
                        let v: Vec<P> = pts.items().iter().skip(1).filter_map(Tree::point).collect();
                        if head == "fp_curve" {
                            Shape::Bezier(v.try_into().map_err(|_| unsupported("a curve without four points"))?)
                        } else {
                            Shape::Poly(v)
                        }
                    }
                };
                shape.rotate([0, 0], orient);
                shape.translate(pos);
                (Role::Drawing, Geo::Shape(shape))
            }
            "pad" => {
                if child.find("padstack").is_some() {
                    return Err(unsupported("a pad stack with per-layer shapes"));
                }
                let at = child.find("at");
                let local = at.and_then(Tree::point).unwrap_or([0, 0]);
                let pad_orient = normalize(at.and_then(|a| a.number(2)).unwrap_or(0.0));
                (Role::Pad, Geo::Pad { pos: to_board(local, pos, orient), orient: pad_orient })
            }
            "zone" => (Role::Zone, Geo::Zone),
            "dimension" | "fp_text_box" | "image" | "table" => return Err(unsupported(&format!("a {head}"))),
            _ => (Role::Other, Geo::Fixed),
        };
        items.push(Item { tree: child, role, geo });
    }

    // `FOOTPRINT::Flip(GetPosition(), TOP_BOTTOM)`.
    if p.side != side {
        let new_orient = normalize180(-orient);
        for item in items.iter_mut() {
            flip_item(item, pos, layers);
        }
        orient = new_orient;
        for t in items.iter_mut().filter(|i| i.role == Role::Other) {
            if t.tree.head() == Some("layer") {
                let name = t.tree.arg(0).unwrap_or_default();
                t.tree.set_args(0, vec![quote(&layers.flip(&name))]);
            }
        }
    }

    // `SetPosition`.
    let target = [kiround(p.at[0] * 1e9), kiround(p.at[1] * 1e9)];
    let delta = [target[0] - pos[0], target[1] - pos[1]];
    for item in items.iter_mut() {
        match &mut item.geo {
            Geo::Text { pos, .. } | Geo::Pad { pos, .. } => *pos = add(*pos, delta),
            Geo::Shape(s) => s.translate(delta),
            Geo::Zone => item.tree.visit_mut("xy", &mut |xy| {
                let q = xy.point().expect("xy");
                xy.set_point(add(q, delta));
            }),
            Geo::Fixed => {}
        }
    }
    pos = target;

    // `SetOrientation`.
    let change = p.rotation_deg - orient;
    orient = normalize180(p.rotation_deg);
    for item in items.iter_mut() {
        match &mut item.geo {
            Geo::Text { pos: tp, angle } => {
                *tp = rotate_about(*tp, pos, change);
                *angle = normalize(*angle + change);
            }
            Geo::Pad { pos: pp, orient: o } => {
                *pp = rotate_about(*pp, pos, change);
                *o = normalize(*o + change);
            }
            Geo::Shape(s) => s.rotate(pos, change),
            Geo::Zone => item.tree.visit_mut("xy", &mut |xy| {
                let q = xy.point().expect("xy");
                xy.set_point(rotate_about(q, pos, change));
            }),
            Geo::Fixed => {}
        }
    }

    // Written back in the footprint's frame.
    let local = |q: P| rotate([q[0] - pos[0], q[1] - pos[1]], -orient);
    for item in items.iter_mut() {
        write_geo(item, &local);
    }
    for item in items.iter_mut().filter(|i| i.role == Role::Other) {
        if item.tree.head() == Some("at") {
            let mut args = vec![fmt_iu(pos[0]), fmt_iu(pos[1])];
            if orient != 0.0 {
                args.push(fmt_angle(orient));
            }
            item.tree.set_args(0, args);
        }
    }

    // Sorted as the formatter's sets sort them, each kind back into its own places.
    let keys: Vec<Key> = items.iter().map(|i| Key::of(i, layers, &local)).collect();
    let mut order: Vec<usize> = (0..items.len()).collect();
    for role in [Role::Drawing, Role::Pad, Role::Zone] {
        let slots: Vec<usize> = (0..items.len()).filter(|&i| items[i].role == role).collect();
        let mut sorted = slots.clone();
        sorted.sort_by(|&a, &b| keys[a].cmp(&keys[b]));
        for (slot, from) in slots.into_iter().zip(sorted) {
            order[slot] = from;
        }
    }
    let mut items: Vec<Option<Item>> = items.into_iter().map(Some).collect();
    let mut out = header;
    for i in order {
        out.push(items[i].take().expect("each item once").tree);
    }
    *tree.items_mut() = out;
    Ok(tree)
}

/// One item of a footprint flipped top to bottom about the footprint's origin, as each item's
/// `Flip` does it while the footprint's own orientation is held at zero.
fn flip_item(item: &mut Item, centre: P, layers: &LayerIds) {
    let flip_layer = |t: &mut Tree| {
        if let Some(l) = t.find_mut("layer") {
            let name = l.arg(0).unwrap_or_default();
            let rest: Vec<String> = l.items().iter().skip(2).filter_map(Tree::token).map(str::to_string).collect();
            l.set_args(0, std::iter::once(quote(&layers.flip(&name))).chain(rest).collect());
        }
    };
    match &mut item.geo {
        Geo::Text { pos, angle } => {
            *pos = mirror(*pos, centre);
            *angle = 180.0 - *angle;
            let layer = item.tree.find("layer").and_then(|l| l.arg(0)).unwrap_or_default();
            flip_layer(&mut item.tree);
            if is_side_specific(&layer) {
                toggle_mirror(&mut item.tree);
            }
        }
        Geo::Shape(s) => {
            s.flip(centre);
            flip_layer(&mut item.tree);
        }
        Geo::Pad { pos, orient } => {
            *pos = mirror(*pos, centre);
            *orient = normalize(-*orient);
            flip_pad(&mut item.tree, layers);
        }
        Geo::Zone => {
            item.tree.visit_mut("xy", &mut |xy| {
                let q = xy.point().expect("xy");
                xy.set_point(mirror(q, centre));
            });
            item.tree.visit_mut("layer", &mut |l| {
                let name = l.arg(0).unwrap_or_default();
                l.set_args(0, vec![quote(&layers.flip(&name))]);
            });
            if let Some(l) = item.tree.find_mut("layers") {
                flip_layer_list(l, layers);
            }
        }
        Geo::Fixed => {}
    }
}

/// A pad's layers, drill offset, trapezoid delta, chamfered corners and custom shape, flipped.
fn flip_pad(pad: &mut Tree, layers: &LayerIds) {
    if let Some(l) = pad.find_mut("layers") {
        flip_layer_list(l, layers);
    }
    if let Some(offset) = pad.find_mut("drill").and_then(|d| d.find_mut("offset")) {
        if let Some([x, y]) = offset.point() {
            offset.set_point([x, -y]);
        }
    }
    if let Some(delta) = pad.find_mut("rect_delta") {
        if let Some([x, y]) = delta.point() {
            delta.set_point([x, -y]);
        }
    }
    if let Some(chamfer) = pad.find_mut("chamfer") {
        let corners: Vec<String> = chamfer.items().iter().skip(1).filter_map(Tree::token).map(str::to_string).collect();
        let swapped = |c: &str| match c {
            "top_left" => "bottom_left",
            "bottom_left" => "top_left",
            "top_right" => "bottom_right",
            "bottom_right" => "top_right",
            other => other,
        }
        .to_string();
        let have: Vec<String> = corners.iter().map(|c| swapped(c)).collect();
        let written: Vec<String> = ["top_left", "top_right", "bottom_left", "bottom_right"].iter().filter(|c| have.iter().any(|h| h == *c)).map(|c| c.to_string()).collect();
        chamfer.set_args(0, written);
    }
    if let Some(prims) = pad.find_mut("primitives") {
        for prim in prims.items_mut().iter_mut().skip(1) {
            for name in ["start", "end", "mid", "center"] {
                if let Some(q) = prim.find_mut(name) {
                    if let Some(pt) = q.point() {
                        q.set_point(mirror(pt, [0, 0]));
                    }
                }
            }
            prim.visit_mut("xy", &mut |xy| {
                let q = xy.point().expect("xy");
                xy.set_point(mirror(q, [0, 0]));
            });
            if prim.head() == Some("gr_arc") {
                let items = prim.items_mut();
                let s = items.iter().position(|t| t.head() == Some("start"));
                let e = items.iter().position(|t| t.head() == Some("end"));
                if let (Some(s), Some(e)) = (s, e) {
                    let (a, b) = (items[s].clone(), items[e].clone());
                    items[s] = Tree::list("start", b.items()[1..].iter().filter_map(Tree::token).map(str::to_string).collect());
                    items[e] = Tree::list("end", a.items()[1..].iter().filter_map(Tree::token).map(str::to_string).collect());
                }
            }
        }
    }
}

/// `(layers …)` with each named layer flipped, wildcards first and the rest in id order, copper
/// before the technical layers, as `formatLayers` writes them.
fn flip_layer_list(l: &mut Tree, layers: &LayerIds) {
    let names: Vec<String> = (0..l.items().len() - 1).filter_map(|i| l.arg(i)).collect();
    let (wild, named): (Vec<String>, Vec<String>) = names.into_iter().partition(|n| n.contains('*') || n.contains('&'));
    let mut flipped: Vec<String> = named.iter().map(|n| layers.flip(n)).collect();
    flipped.sort_by_key(|n| (!n.ends_with(".Cu"), layers.id(n)));
    l.set_args(0, wild.iter().chain(flipped.iter()).map(|n| quote(n)).collect());
}

/// Text mirrored: `mirror` in its `(effects (justify …))` toggled.
fn toggle_mirror(text: &mut Tree) {
    let Some(effects) = text.find_mut("effects") else { return };
    if let Some(i) = effects.items().iter().position(|t| t.head() == Some("justify")) {
        let justify = &mut effects.items_mut()[i];
        let tokens: Vec<String> = justify.items().iter().skip(1).filter_map(Tree::token).map(str::to_string).collect();
        let had = tokens.iter().any(|t| t == "mirror");
        let mut kept: Vec<String> = tokens.into_iter().filter(|t| t != "mirror").collect();
        if !had {
            kept.push("mirror".to_string());
        }
        if kept.is_empty() {
            effects.items_mut().remove(i);
        } else {
            justify.set_args(0, kept);
        }
    } else {
        effects.items_mut().push(Tree::list("justify", vec!["mirror".to_string()]));
    }
}

/// An item's geometry put back into its tree in the footprint's frame.
fn write_geo(item: &mut Item, local: &dyn Fn(P) -> P) {
    let tree = &mut item.tree;
    match &item.geo {
        Geo::Text { pos, angle } => {
            if let Some(at) = tree.find_mut("at") {
                let q = local(*pos);
                at.set_args(0, vec![fmt_iu(q[0]), fmt_iu(q[1]), fmt_angle(*angle)]);
            }
        }
        Geo::Pad { pos, orient } => {
            if let Some(at) = tree.find_mut("at") {
                let q = local(*pos);
                let mut args = vec![fmt_iu(q[0]), fmt_iu(q[1])];
                if *orient != 0.0 {
                    args.push(fmt_angle(*orient));
                }
                at.set_args(0, args);
            }
        }
        Geo::Shape(shape) => {
            let set = |tree: &mut Tree, name: &str, q: P| {
                if let Some(t) = tree.find_mut(name) {
                    t.set_point(local(q));
                }
            };
            match shape {
                Shape::Segment(s, e) | Shape::Rect(s, e) => {
                    set(tree, "start", *s);
                    set(tree, "end", *e);
                }
                Shape::Circle(c, e) => {
                    set(tree, "center", *c);
                    set(tree, "end", *e);
                }
                Shape::Arc { start, end, centre, cache } => {
                    let mid = Shape::arc_mid(*start, *end, *centre, *cache);
                    set(tree, "start", *start);
                    set(tree, "mid", mid);
                    set(tree, "end", *end);
                }
                Shape::Poly(pts) if tree.head() == Some("fp_rect") => {
                    // The rectangle pcbnew turned into a polygon.
                    let pts = Tree::List(
                        std::iter::once(Tree::Token("pts".to_string()))
                            .chain(pts.iter().map(|q| {
                                let q = local(*q);
                                Tree::list("xy", vec![fmt_iu(q[0]), fmt_iu(q[1])])
                            }))
                            .collect(),
                    );
                    let items = tree.items_mut();
                    items[0] = Tree::Token("fp_poly".to_string());
                    let s = items.iter().position(|t| t.head() == Some("start")).expect("a rectangle's start");
                    items[s] = pts;
                    items.retain(|t| t.head() != Some("end"));
                }
                Shape::Poly(pts) => write_pts(tree, pts, local),
                Shape::Bezier(pts) => write_pts(tree, pts, local),
            }
        }
        Geo::Zone | Geo::Fixed => {}
    }
}

fn write_pts(tree: &mut Tree, pts: &[P], local: &dyn Fn(P) -> P) {
    let Some(list) = tree.find_mut("pts") else { return };
    let mut it = pts.iter();
    for xy in list.items_mut().iter_mut().skip(1).filter(|t| t.head() == Some("xy")) {
        if let Some(q) = it.next() {
            xy.set_point(local(*q));
        }
    }
}

// --- Sorting ------------------------------------------------------------------------------------

/// What `FOOTPRINT::cmp_drawings`, `cmp_pads` and `cmp_zones` compare, in their order.
#[derive(Debug, Clone, PartialEq)]
enum Key {
    /// Type, layer, shape, its points, width, uuid.
    Drawing { kind: u8, layer: i64, shape: u8, points: Vec<P>, poly: bool, width: i64, uuid: String },
    /// Number, position in the footprint, size, shape, layers, uuid.
    Pad { number: String, at: P, size: P, shape: u8, layers: Vec<i64>, uuid: String },
    /// Priority, layers, outline, uuid.
    Zone { priority: i64, layers: Vec<i64>, outline: Vec<P>, uuid: String },
    None,
}

impl Key {
    fn of(item: &Item, layers: &LayerIds, local: &dyn Fn(P) -> P) -> Key {
        let t = &item.tree;
        let uuid = t.find("uuid").or_else(|| t.find("tstamp")).and_then(|u| u.arg(0)).unwrap_or_default();
        let layer = t.find("layer").and_then(|l| l.arg(0)).map_or(i64::MAX, |l| layers.id(&l));
        let layer_seq = |t: &Tree| {
            let mut v: Vec<i64> = t.find("layers").map_or(Vec::new(), |l| (0..l.items().len() - 1).filter_map(|i| l.arg(i)).map(|n| layers.id(&n)).collect());
            v.sort();
            v
        };
        match (&item.role, &item.geo) {
            (Role::Drawing, Geo::Text { .. }) => Key::Drawing { kind: 6, layer, shape: 0, points: vec![], poly: false, width: 0, uuid },
            (Role::Drawing, Geo::Shape(s)) => {
                let points = match s {
                    Shape::Segment(a, b) | Shape::Rect(a, b) | Shape::Circle(a, b) => vec![*a, *b],
                    Shape::Arc { start, end, centre, .. } => vec![*start, *end, *centre],
                    Shape::Poly(pts) => pts.clone(),
                    Shape::Bezier([s, c1, c2, e]) => vec![*s, *e, *c1, *c2],
                };
                let width = t.find("stroke").and_then(|s| s.find("width")).or_else(|| t.find("width")).and_then(|w| w.number(0)).map_or(0, iu);
                Key::Drawing { kind: 2, layer, shape: s.rank(), points, poly: matches!(s, Shape::Poly(_)), width, uuid }
            }
            (Role::Pad, Geo::Pad { pos, .. }) => Key::Pad {
                number: t.arg(0).unwrap_or_default(),
                at: local(*pos),
                size: t.find("size").and_then(Tree::point).unwrap_or([0, 0]),
                shape: match t.arg(2).as_deref() {
                    Some("circle") => 0,
                    Some("rect") => 1,
                    Some("oval") => 2,
                    Some("trapezoid") => 3,
                    Some("roundrect") if t.find("chamfer_ratio").is_some() => 5,
                    Some("roundrect") => 4,
                    _ => 6,
                },
                layers: layer_seq(t),
                uuid,
            },
            (Role::Zone, _) => {
                let mut outline = Vec::new();
                if let Some(poly) = t.find("polygon").and_then(|p| p.find("pts")) {
                    outline = poly.items().iter().skip(1).filter_map(Tree::point).collect();
                }
                Key::Zone { priority: t.find("priority").and_then(|p| p.number(0)).map_or(0, |v| v as i64), layers: layer_seq(t), outline, uuid }
            }
            _ => Key::None,
        }
    }

    fn cmp(&self, other: &Key) -> Ordering {
        match (self, other) {
            (
                Key::Drawing { kind, layer, shape, points, poly, width, uuid },
                Key::Drawing { kind: k2, layer: l2, shape: s2, points: p2, width: w2, uuid: u2, .. },
            ) => kind
                .cmp(k2)
                .then(layer.cmp(l2))
                .then(shape.cmp(s2))
                .then_with(|| if *poly { points.len().cmp(&p2.len()) } else { Ordering::Equal })
                .then_with(|| points.iter().zip(p2).map(|(a, b)| a[0].cmp(&b[0]).then(a[1].cmp(&b[1]))).find(|o| o.is_ne()).unwrap_or(Ordering::Equal))
                .then(width.cmp(w2))
                .then_with(|| uuid.cmp(u2)),
            (Key::Pad { number, at, size, shape, layers, uuid }, Key::Pad { number: n2, at: a2, size: z2, shape: s2, layers: l2, uuid: u2 }) => {
                str_num_cmp(number, n2)
                    .then(at[0].cmp(&a2[0]))
                    .then(at[1].cmp(&a2[1]))
                    .then(size[0].cmp(&z2[0]))
                    .then(size[1].cmp(&z2[1]))
                    .then(shape.cmp(s2))
                    .then_with(|| layers.cmp(l2))
                    .then_with(|| uuid.cmp(u2))
            }
            (Key::Zone { priority, layers, outline, uuid }, Key::Zone { priority: p2, layers: l2, outline: o2, uuid: u2 }) => priority
                .cmp(p2)
                .then_with(|| layers.cmp(l2))
                .then(outline.len().cmp(&o2.len()))
                .then_with(|| outline.iter().zip(o2).map(|(a, b)| a[0].cmp(&b[0]).then(a[1].cmp(&b[1]))).find(|o| o.is_ne()).unwrap_or(Ordering::Equal))
                .then_with(|| uuid.cmp(u2)),
            _ => Ordering::Equal,
        }
    }
}

/// `StrNumCmp`: runs of digits compare as numbers.
fn str_num_cmp(a: &str, b: &str) -> Ordering {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (mut c1, mut c2) = (a[i], b[j]);
        if c1.is_ascii_digit() && c2.is_ascii_digit() {
            let (mut n1, mut n2) = (0i64, 0i64);
            while i < a.len() && a[i].is_ascii_digit() {
                n1 = n1 * 10 + (a[i] as i64 - '0' as i64);
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                n2 = n2 * 10 + (b[j] as i64 - '0' as i64);
                j += 1;
            }
            if n1 != n2 {
                return n1.cmp(&n2);
            }
            c1 = a.get(i).copied().unwrap_or('\0');
            c2 = b.get(j).copied().unwrap_or('\0');
        }
        if c1 != c2 {
            return c1.cmp(&c2);
        }
        if i < a.len() {
            i += 1;
        }
        if j < b.len() {
            j += 1;
        }
    }
    match (i < a.len(), j < b.len()) {
        (false, true) => Ordering::Less,
        (true, false) => Ordering::Greater,
        _ => Ordering::Equal,
    }
}

// --- KiCad's layout -----------------------------------------------------------------------------

/// `KICAD_FORMAT::Prettify` (KiCad 9, not the compact save) for one list written at `depth` in
/// the file, after a sibling: what follows the indent of its first line, up to its closing paren.
pub(crate) fn prettify(source: &str, depth: usize) -> String {
    const XY_COLUMN_LIMIT: usize = 99;
    const WRAP_THRESHOLD: usize = 72;
    let src: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len() + source.len() / 4);
    let mut list_depth = depth;
    let mut last_non_ws = ')';
    let mut in_quote = false;
    let mut inserted_space = false;
    let mut in_multiline = false;
    let mut in_xy = false;
    let mut column = 0usize;
    let mut backslashes = 0usize;
    let is_ws = |c: char| c == ' ' || c == '\t' || c == '\n' || c == '\r';
    let indent = |n: usize| "\t".repeat(n);
    let mut first = true;
    for (k, &c) in src.iter().enumerate() {
        let next = src[k..].iter().copied().find(|&c| !is_ws(c)).unwrap_or('\0');
        if is_ws(c) && !in_quote {
            if !inserted_space && list_depth > 0 && last_non_ws != '(' && next != ')' && next != '(' {
                if in_xy || column < WRAP_THRESHOLD {
                    out.push(' ');
                    column += 1;
                } else {
                    out.push('\n');
                    out.push_str(&indent(list_depth));
                    column = list_depth;
                    in_multiline = true;
                }
                inserted_space = true;
            }
            continue;
        }
        inserted_space = false;
        if c == '(' && !in_quote {
            let current_xy = src.get(k + 1) == Some(&'x') && src.get(k + 2) == Some(&'y') && src.get(k + 3) == Some(&' ');
            if first {
                out.push('(');
                column = list_depth + 1;
                first = false;
            } else if in_xy && current_xy && column < XY_COLUMN_LIMIT {
                out.push_str(" (");
                column += 2;
            } else {
                out.push('\n');
                out.push_str(&indent(list_depth));
                out.push('(');
                column = list_depth + 1;
            }
            in_xy = current_xy;
            list_depth += 1;
        } else if c == ')' && !in_quote {
            list_depth = list_depth.saturating_sub(1);
            if last_non_ws == ')' || in_multiline {
                out.push('\n');
                out.push_str(&indent(list_depth));
                out.push(')');
                column = list_depth + 1;
                in_multiline = false;
            } else {
                out.push(')');
                column += 1;
            }
        } else {
            if c == '\\' {
                backslashes += 1;
            } else if c == '"' && backslashes.is_multiple_of(2) {
                in_quote = !in_quote;
            }
            if c != '\\' {
                backslashes = 0;
            }
            out.push(c);
            column += 1;
        }
        last_non_ws = c;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_written_as_kicad_writes_them() {
        assert_eq!(fmt_iu(103_575_000), "103.575");
        assert_eq!(fmt_iu(-1_160_000), "-1.16");
        assert_eq!(fmt_iu(0), "0");
        assert_eq!(fmt_iu(50), "0.00005");
        assert_eq!(fmt_iu(108_837_499), "108.837499");
        assert_eq!(g10(-90.0), "-90");
        assert_eq!(g10(102.5), "102.5");
        assert_eq!(g10(1.0 / 3.0), "0.3333333333");
        assert_eq!(g10(1e-5), "1e-05");
        assert_eq!(g10(-0.0), "-0");
    }

    #[test]
    fn rotation_rounds_as_kicad_does() {
        assert_eq!(rotate([1000, 0], 90.0), [0, -1000]);
        assert_eq!(rotate([1000, 0], -90.0), [0, 1000]);
        assert_eq!(rotate([1000, 0], 30.0), [866, -500]);
    }

    #[test]
    fn pad_numbers_sort_as_numbers() {
        assert_eq!(str_num_cmp("2", "10"), Ordering::Less);
        assert_eq!(str_num_cmp("A2", "A10"), Ordering::Less);
        assert_eq!(str_num_cmp("10", "10"), Ordering::Equal);
        assert_eq!(str_num_cmp("B1", "A2"), Ordering::Greater);
    }

    #[test]
    fn prettify_keeps_what_kicad_wrote() {
        let text = "\t(footprint \"R\"\n\t\t(layer \"F.Cu\")\n\t\t(uuid \"u\")\n\t\t(at 1 2 90)\n\t\t(fp_poly\n\t\t\t(pts\n\t\t\t\t(xy -0.5 -0.25) (xy -0.5 0.25) (xy 0.5 0.25) (xy 0.5 -0.25)\n\t\t\t)\n\t\t)\n\t)";
        let node = sexpr::parse(text.trim()).unwrap();
        let tree = Tree::of(&node, text.trim());
        assert_eq!(prettify(&tree.compact(), 1), text.trim_start());
    }
}
