//! The board as a router sees it: every piece of copper as a shape on a set of layers, holes,
//! keepouts and the outline, indexed by where they are.

use std::collections::HashMap;
use std::path::Path;

use kicad_pcb::{Copper, Keepout, Outline, PadKind, Pcb, Rules, Span};

use crate::geom::{nm, BBox, Nm, Point, Shape};

/// Side of an index cell.
const CELL: Nm = 1_000_000;
/// Chord error allowed when arcs become straight pieces, as KiCad's own default.
const MAX_ERROR_M: f64 = 0.005e-3;

/// Points every `step` radians round a circle, from `from` through `sweep`: on the circle, so the
/// chords between them pass inside it, or (`outside`) far enough out that the chords touch it and
/// pass outside it.
fn around(c: [f64; 2], r: f64, from: f64, sweep: f64, outside: bool) -> Vec<[f64; 2]> {
    let step = 2.0 * (1.0 - (MAX_ERROR_M / r.max(MAX_ERROR_M)).min(1.0)).acos();
    let n = ((sweep.abs() / step.max(1e-3)).ceil() as usize).clamp(1, 4096);
    let r = if outside { r / (sweep.abs() / n as f64 / 2.0).cos().max(0.5) } else { r };
    (0..=n).map(|k| from + sweep * k as f64 / n as f64).map(|t| [c[0] + r * t.cos(), c[1] + r * t.sin()]).collect()
}

/// An arc as two polylines, one whose chords pass inside it and one whose chords pass outside it,
/// so that copper on either side is judged no farther from the arc than it is.
fn arc_both_ways(c: [f64; 2], r: f64, from: f64, sweep: f64) -> [Vec<[f64; 2]>; 2] {
    [around(c, r, from, sweep, false), around(c, r, from, sweep, true)]
}

/// An outline primitive as polylines in metres, which copper keeps the edge clearance from as
/// KiCad keeps it from the primitive: its centre line where it has no width (an Edge.Cuts line),
/// else the boundary of its stroke (a Margin line). Arcs are followed both inside and outside.
fn edge_polylines(shape: &kicad_pcb::OutlineShape, width: f64) -> Vec<Vec<[f64; 2]>> {
    use kicad_pcb::OutlineShape;
    let tau = std::f64::consts::TAU;
    let h = width / 2.0;
    // Centre, radius, start angle and sweep of an arc or circle.
    let arc = match shape {
        OutlineShape::Line { .. } => None,
        OutlineShape::Circle { center, radius } => Some((*center, *radius, 0.0, tau)),
        OutlineShape::Arc { start, mid, end } => kicad_pcb::outline::circumcenter(*start, *mid, *end).map(|c| {
            let angle = |p: [f64; 2]| (p[1] - c[1]).atan2(p[0] - c[0]);
            let (a0, am, a1) = (angle(*start), angle(*mid), angle(*end));
            // The sweep from start to end that passes mid.
            let ccw = (am - a0).rem_euclid(tau) < (a1 - a0).rem_euclid(tau);
            let sweep = if ccw { (a1 - a0).rem_euclid(tau) } else { -(a0 - a1).rem_euclid(tau) };
            (c, (start[0] - c[0]).hypot(start[1] - c[1]), a0, sweep)
        }),
    };
    let ends: [[f64; 2]; 2] = match shape {
        OutlineShape::Line { start, end } | OutlineShape::Arc { start, end, .. } => [*start, *end],
        OutlineShape::Circle { .. } => return match arc {
            Some((c, r, from, sweep)) if h > 0.0 => {
                let mut out = arc_both_ways(c, r + h, from, sweep).to_vec();
                if r > h {
                    out.extend(arc_both_ways(c, r - h, from, sweep));
                }
                out
            }
            Some((c, r, from, sweep)) => arc_both_ways(c, r, from, sweep).to_vec(),
            None => Vec::new(),
        },
    };
    let mut out = Vec::new();
    match arc {
        None if h <= 0.0 => out.push(ends.to_vec()),
        None => {
            let (dx, dy) = (ends[1][0] - ends[0][0], ends[1][1] - ends[0][1]);
            let len = dx.hypot(dy);
            if len > 0.0 {
                let n = [-dy / len * h, dx / len * h];
                for k in [-1.0, 1.0] {
                    out.push(ends.iter().map(|p| [p[0] + k * n[0], p[1] + k * n[1]]).collect());
                }
            }
        }
        Some((c, r, from, sweep)) if h <= 0.0 => out.extend(arc_both_ways(c, r, from, sweep)),
        Some((c, r, from, sweep)) => {
            out.extend(arc_both_ways(c, r + h, from, sweep));
            if r > h {
                out.extend(arc_both_ways(c, r - h, from, sweep));
            }
        }
    }
    // The stroke's round ends: whole circles, whose inner halves lie within the stroke anyway.
    if h > 0.0 {
        for e in ends {
            out.extend(arc_both_ways(e, h, 0.0, tau));
        }
    }
    out
}

/// `shape` grown by `by` all round: a capsule's radius, or a polygon's convex hull with every
/// corner rounded (a little outside the true offset, never inside it).
fn grown(shape: &Shape, by: Nm) -> Shape {
    match shape {
        Shape::Capsule { a, b, r } => Shape::Capsule { a: *a, b: *b, r: r + by },
        Shape::Polygon(p) if by <= 0 => Shape::Polygon(p.clone()),
        Shape::Polygon(p) => {
            const N: usize = 16;
            let r = by as f64 / (std::f64::consts::PI / N as f64).cos();
            let mut pts: Vec<Point> = Vec::with_capacity(p.len() * N);
            for q in p {
                for k in 0..N {
                    let t = std::f64::consts::TAU * k as f64 / N as f64;
                    pts.push(Point::new(q.x + (r * t.cos()).round() as Nm, q.y + (r * t.sin()).round() as Nm));
                }
            }
            Shape::Polygon(hull(pts))
        }
    }
}

/// The convex hull of points, by Andrew's monotone chain.
fn hull(mut points: Vec<Point>) -> Vec<Point> {
    points.sort_by_key(|p| (p.x, p.y));
    points.dedup();
    if points.len() < 3 {
        return points;
    }
    let cross = |o: Point, a: Point, b: Point| (a.x - o.x) as i128 * (b.y - o.y) as i128 - (a.y - o.y) as i128 * (b.x - o.x) as i128;
    let mut lower: Vec<Point> = Vec::new();
    for &p in &points {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<Point> = Vec::new();
    for &p in points.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Pad { footprint: usize, number: String, plated: bool },
    Track,
    Via,
    /// Copper a zone poured, one item per filled polygon. A kept pour belongs to a block: it is
    /// a plane other nets keep clear of, not copper refilled around them. Copper drawn on a
    /// copper layer is a kept pour of zone [`DRAWN`].
    Pour { zone: usize, kept: bool },
    /// A rule area, and the footprint it belongs to, whose own pads it does not keep out.
    Keepout { forbids: Keepout, footprint: Option<usize> },
}

/// How a rule area's name begins when it is an area for vias to avoid rather than a keepout.
pub const SOFT_NO_VIA: &str = "soft-novia";

/// The zone of drawn copper: lines, shapes and text on a copper layer, which stay as drawn.
pub const DRAWN: usize = usize::MAX;

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub kind: Kind,
    /// 0 for no net.
    pub net: u32,
    /// Copper layers, one bit per index into `Board::layers`.
    pub layers: u64,
    pub shape: Shape,
    /// A drilled hole: centre and radius.
    pub hole: Option<(Point, Nm)>,
    pub locked: bool,
    /// The item's text in the board file, for writing routed copper back.
    pub span: Option<Span>,
}

impl Item {
    pub fn on(&self, layer: usize) -> bool {
        self.layers & (1 << layer) != 0
    }

    /// A hole with copper in its wall: a via or a plated pad.
    pub fn plated_hole(&self) -> bool {
        self.hole.is_some() && matches!(self.kind, Kind::Via | Kind::Pad { plated: true, .. })
    }

    /// Poured copper that is refilled around new copper, so routing may treat it as absent.
    /// Copper drawn on a copper layer, which nothing refills.
    pub fn drawn(&self) -> bool {
        matches!(self.kind, Kind::Pour { zone: DRAWN, .. })
    }

    pub fn refilled(&self) -> bool {
        matches!(self.kind, Kind::Pour { kept: false, .. })
    }

    pub fn is_copper(&self) -> bool {
        !matches!(self.kind, Kind::Keepout { .. }) && self.layers != 0
    }
}

pub type ItemId = usize;

#[derive(Debug, Clone)]
pub struct FootprintInfo {
    pub reference: String,
    pub at: Point,
    pub rotation_deg: f64,
    pub back: bool,
    pub locked: bool,
    /// Where its `(at …)` is in the board file.
    pub at_span: Option<Span>,
    /// Its courtyard outline, if it has one.
    pub courtyard: Option<Vec<Point>>,
}

/// A functional block: a KiCad group of parts and zones laid out by hand (an RP2350's regulator,
/// say) that routing must leave whole. Its pours are kept as planes other nets keep clear of, and
/// its parts are held still; moving a block as one piece is left for later.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub name: String,
    /// Footprints, by index.
    pub footprints: Vec<usize>,
    /// Zones, by index into the board's copper zones.
    pub zones: Vec<usize>,
}

impl Block {
    /// Each outermost group with parts or zones in it, nested groups taken in.
    pub fn of(pcb: &Pcb, copper: &Copper) -> Vec<Block> {
        let groups: HashMap<&str, &kicad_pcb::Group> = copper.groups.iter().map(|g| (g.uuid.as_str(), g)).collect();
        let nested: std::collections::HashSet<&str> = copper.groups.iter().flat_map(|g| &g.members).map(String::as_str).filter(|m| groups.contains_key(m)).collect();
        let mut blocks = Vec::new();
        for group in copper.groups.iter().filter(|g| !nested.contains(g.uuid.as_str())) {
            let (mut members, mut stack, mut seen) = (Vec::new(), vec![group], std::collections::HashSet::new());
            while let Some(g) = stack.pop() {
                if !seen.insert(g.uuid.as_str()) {
                    continue;
                }
                for m in &g.members {
                    match groups.get(m.as_str()) {
                        Some(inner) => stack.push(inner),
                        None => members.push(m.as_str()),
                    }
                }
            }
            let footprints: Vec<usize> = pcb.footprints.iter().enumerate().filter(|(_, f)| members.contains(&f.uuid.as_str())).map(|(i, _)| i).collect();
            let zones: Vec<usize> = copper.zones.iter().enumerate().filter(|(_, z)| members.contains(&z.uuid.as_str())).map(|(i, _)| i).collect();
            if !footprints.is_empty() || !zones.is_empty() {
                blocks.push(Block { name: group.name.clone(), footprints, zones });
            }
        }
        blocks
    }
}

#[derive(Debug, Clone)]
pub struct Board {
    pub layers: Vec<String>,
    /// Net names by net number; 0 is the unconnected net.
    pub nets: Vec<String>,
    pub footprints: Vec<FootprintInfo>,
    pub blocks: Vec<Block>,
    pub outline: Vec<Vec<Point>>,
    /// Precomputed outline edge segments `(a, b, BBox::of([a, b]))` for fast edge-clearance checks.
    pub outline_edges: Vec<(Point, Point, BBox)>,
    pub rules: Rules,
    /// Plane layers, one bit per index into `layers`: whole layers given to one net's pour,
    /// which no track crosses, other nets' vias pass through and which joins its net.
    pub planes: u64,
    /// Pads' solder mask openings: the outer copper layers a pad's opening is on, one bit per
    /// index into `layers`, and how far it reaches beyond the pad's copper. Another net's track
    /// or via there would be bared beside the pad, for solder to bridge.
    pub mask_openings: HashMap<ItemId, (u64, Nm)>,
    /// Where the mask leaves vias bare (one bit per outer copper layer it does), and how far
    /// beyond a via its opening reaches.
    pub via_openings: (u64, Nm),
    /// The narrowest web of mask between two openings (`solder_mask_min_width`): two openings
    /// nearer than this run together, and bridge their copper if of two nets.
    pub mask_web: Nm,
    /// Openings drawn in the solder mask: the shape, the copper layer it bares, and the one net
    /// whose pads it bares, which alone may run there; 0 where it bares none or several.
    pub mask_areas: Vec<(Shape, u64, u32)>,
    /// Pads' own clearances, where wider than their nets' (KiCad's local clearance).
    pub pad_clearances: HashMap<ItemId, Nm>,
    /// Areas where a via costs the router dearly but is allowed, on every layer: rule areas
    /// whose name starts `soft-novia`, and boxes given on the command line. Room kept for silk.
    pub via_avoid: Vec<Shape>,
    /// What a via in a via-avoid area costs the router beyond any other, in its search's units
    /// (10 a grid cell): a long way round, but never a wall.
    pub via_avoid_cost: u32,
    /// Routing regions a planner set, with rules for which nets may run where.
    pub regions: Vec<crate::regions::Region>,
    /// What crossing another net's pour (one refilled round new copper, not a plane) costs, as a
    /// multiple of the length inside: crossing cuts the pour into pieces. 0 for nothing.
    pub pour_cost: f64,
    /// Clearances per net number, in nanometres, so `clearance(a, b)` is two array lookups.
    net_clearances: Vec<Nm>,
    min_clearance_nm: Nm,
    items: Vec<Option<Item>>,
    /// Cached shape bounding boxes (`item.shape.bbox()`) parallel to `items`.
    shape_bboxes: Vec<BBox>,
    /// Cached footprint bounding boxes (`footprint_of`) parallel to `items`.
    bboxes: Vec<BBox>,
    /// Item ids per (layer, cell); holes are filed under the layer count.
    cells: HashMap<(usize, Nm, Nm), Vec<ItemId>>,
}

#[derive(Debug)]
pub struct LoadError(pub String);

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoadError {}

impl Board {
    /// Reads a `.kicad_pcb` and the `.kicad_pro` beside it.
    pub fn load(path: &Path) -> Result<(Board, String), LoadError> {
        let text = std::fs::read_to_string(path).map_err(|e| LoadError(format!("{}: {e}", path.display())))?;
        let (pcb, copper) = kicad_pcb::parse_board(&text).map_err(|e| LoadError(format!("{}: {e}", path.display())))?;
        let rules = Rules::read(&path.with_extension("kicad_pro")).map_err(|e| LoadError(e.to_string()))?;
        Ok((Board::new(&pcb, &copper, rules), text))
    }

    pub fn new(pcb: &Pcb, copper: &Copper, rules: Rules) -> Board {
        let layers: Vec<String> = copper.layers.iter().map(|l| l.name.clone()).collect();
        let mask = |names: &[String]| -> u64 {
            copper.expand(names).iter().filter_map(|n| layers.iter().position(|l| l == n)).fold(0, |m, i| m | 1 << i)
        };
        let one = |name: &str| mask(&[name.to_string()]);
        let max_net = pcb.nets.iter().map(|n| n.number).max().unwrap_or(0) as usize;
        let mut nets = vec![String::new(); max_net + 1];
        for n in &pcb.nets {
            nets[n.number as usize] = n.name.clone();
        }
        let min_clearance_nm = nm(rules.min_clearance);
        let net_clearances: Vec<Nm> = nets
            .iter()
            .map(|name| nm(rules.class_of(name).map_or(0.0, |c| c.clearance)).max(min_clearance_nm))
            .collect();
        let blocks = Block::of(pcb, copper);
        let mut board = Board {
            layers: layers.clone(),
            nets,
            footprints: pcb
                .footprints
                .iter()
                .zip(&copper.locked_footprints)
                .zip(&copper.footprint_at)
                .zip(&copper.courtyards)
                .enumerate()
                .map(|(i, (((f, locked), at_span), yard))| FootprintInfo {
                    reference: f.reference.clone(),
                    at: Point::from_m(f.at),
                    rotation_deg: f.rotation_deg,
                    back: matches!(f.side, kicad_pcb::Side::Back),
                    locked: *locked || blocks.iter().any(|b| b.footprints.contains(&i)),
                    at_span: *at_span,
                    courtyard: yard.as_ref().map(|y| y.iter().copied().map(Point::from_m).collect()),
                })
                .collect(),
            blocks: blocks.clone(),
            outline: Vec::new(),
            outline_edges: Vec::new(),
            rules,
            planes: 0,
            mask_openings: HashMap::new(),
            via_openings: (0, 0),
            mask_web: nm(copper.solder_mask_min_width),
            mask_areas: Vec::new(),
            pad_clearances: HashMap::new(),
            via_avoid: Vec::new(),
            via_avoid_cost: 3_000,
            regions: Vec::new(),
            pour_cost: 0.0,
            net_clearances,
            min_clearance_nm,
            items: Vec::new(),
            shape_bboxes: Vec::new(),
            bboxes: Vec::new(),
            cells: HashMap::new(),
        };
        if let Ok(loops) = pcb.outline_loops() {
            for l in std::iter::once(&loops.outer).chain(&loops.holes) {
                board.outline.push(l.points(MAX_ERROR_M).into_iter().map(Point::from_m).collect());
            }
        }
        // Copper keeps clear of every Edge.Cuts and Margin primitive, whether it closes into the
        // board's loops or not: a slot drawn as an open line, or a cutout the loops missed.
        for p in pcb.outline.iter().chain(&pcb.margin) {
            for line in edge_polylines(&p.shape, p.width) {
                let points: Vec<Point> = line.into_iter().map(Point::from_m).collect();
                for w in points.windows(2) {
                    board.outline_edges.push((w[0], w[1], BBox::of([w[0], w[1]])));
                }
            }
        }

        // Openings in the mask, each its shapes, the copper layer it bares, and the one net that
        // may run there if known yet; else the net of the pads it bares, found once they are in.
        let mut apertures: Vec<(Vec<Shape>, u64, Option<u32>)> = Vec::new();
        for pad in &copper.pads {
            let model = &pcb.footprints[pad.footprint].pads[pad.pad];
            let hole = model.drill.map(|d| (Point::from_m(model.position), nm(d[0].min(d[1]) / 2.0)));
            let plated = !matches!(model.kind, PadKind::NpThruHole);
            let bigger_than_hole = model.drill.is_none_or(|d| model.size[0] > d[0] || model.size[1] > d[1]);
            let mut layers = if plated || bigger_than_hole { mask(&pad.layers) } else { 0 };
            // A pad with no hole that lists every copper layer joins its net on its footprint's
            // side alone, as KiCad takes it: nothing joins the layers through it. Its copper is on
            // the other layers all the same, for other nets to keep clear of: drawn copper there.
            let mut unjoined = 0;
            if model.drill.is_none() && pad.layers.iter().any(|l| l == "*.Cu") {
                let side = if matches!(pcb.footprints[pad.footprint].side, kicad_pcb::Side::Back) { one("B.Cu") } else { one("F.Cu") };
                unjoined = layers & !side;
                layers &= side;
            }
            let shape = match &pad.outline {
                Outline::Circle { centre, radius } => {
                    let c = Point::from_m(*centre);
                    Shape::Capsule { a: c, b: c, r: nm(*radius) }
                }
                Outline::Polygons(polys) => {
                    let first = polys.first().cloned().unwrap_or_default();
                    Shape::Polygon(first.into_iter().map(Point::from_m).collect())
                }
            };
            let base = Item {
                kind: Kind::Pad { footprint: pad.footprint, number: model.number.clone(), plated },
                net: model.net.as_ref().map_or(0, |n| n.number),
                layers,
                shape,
                hole,
                locked: true,
                span: None,
            };
            // The outer copper layers whose mask the pad opens, and by how much.
            let opened = |side: &str| pad.layers.iter().any(|l| l == side || l == "*.Mask");
            let mut openings = 0u64;
            if opened("F.Mask") {
                openings |= one("F.Cu");
            }
            if opened("B.Mask") {
                openings |= one("B.Cu");
            }
            let opening = (openings & layers != 0).then(|| (openings & layers, nm(pad.mask_margin.unwrap_or(copper.pad_to_mask_clearance).max(0.0))));
            let mut ids = Vec::new();
            if let Outline::Polygons(polys) = &pad.outline {
                for extra in polys.iter().skip(1) {
                    ids.push(board.insert(Item {
                        shape: Shape::Polygon(extra.iter().copied().map(Point::from_m).collect()),
                        hole: None,
                        ..base.clone()
                    }));
                }
            }
            // Where the pad's mask opens over a side without its copper, the opening bares what
            // lies there: only the pad's net may run there, or, for an opening with no copper at
            // all, the one net whose pads it bares.
            let margin = nm(pad.mask_margin.unwrap_or(copper.pad_to_mask_clearance).max(0.0));
            let pad_shapes: Vec<Shape> = match &pad.outline {
                Outline::Circle { .. } => vec![base.shape.clone()],
                Outline::Polygons(polys) => polys.iter().map(|p| Shape::Polygon(p.iter().copied().map(Point::from_m).collect())).collect(),
            };
            for side in [one("F.Cu"), one("B.Cu")] {
                if openings & side != 0 && layers & side == 0 {
                    let lists_copper = pad.layers.iter().any(|l| l.ends_with(".Cu"));
                    apertures.push((pad_shapes.iter().map(|s| grown(s, margin)).collect(), side, lists_copper.then_some(base.net)));
                }
            }
            if unjoined != 0 {
                for shape in &pad_shapes {
                    board.insert(Item { kind: Kind::Pour { zone: DRAWN, kept: true }, net: base.net, layers: unjoined, shape: shape.clone(), hole: None, locked: true, span: None });
                }
            }
            ids.push(board.insert(base));
            // A slot's hole is longer than the circle `hole` holds: an unplated one keeps tracks
            // and vias clear of all of it, by the edge clearance, as KiCad takes an unplated slot
            // for board edge. (A plated one's copper covers it.)
            if let (Some(d), false) = (model.drill, plated) {
                if (d[0] - d[1]).abs() > 1e-6 {
                    let (s, c) = model.rotation_deg.to_radians().sin_cos();
                    let half = (d[0].max(d[1]) - d[0].min(d[1])) / 2.0;
                    let along = if d[0] > d[1] { [c, -s] } else { [s, c] };
                    let end = |k: f64| Point::from_m([model.position[0] + k * half * along[0], model.position[1] + k * half * along[1]]);
                    board.insert(Item {
                        kind: Kind::Keepout { forbids: Keepout { tracks: true, vias: true, ..Keepout::default() }, footprint: Some(pad.footprint) },
                        net: 0,
                        layers: (1u64 << board.layers.len()) - 1,
                        shape: Shape::Capsule { a: end(-1.0), b: end(1.0), r: nm(d[0].min(d[1]) / 2.0 + board.rules.min_hole_clearance.max(board.rules.min_copper_edge_clearance)) },
                        hole: None,
                        locked: true,
                        span: None,
                    });
                }
            }
            let net = board.items.get(*ids.last().unwrap()).and_then(Option::as_ref).map_or(0, |it| it.net);
            let own = pad.clearance.map(nm).filter(|c| *c > board.net_clearances.get(net as usize).copied().unwrap_or(0));
            for id in ids {
                if let Some(o) = opening.filter(|o| o.1 > 0) {
                    board.mask_openings.insert(id, o);
                }
                if let Some(c) = own {
                    board.pad_clearances.insert(id, c);
                }
            }
        }
        for t in &copper.tracks {
            board.insert(Item {
                kind: Kind::Track,
                net: t.net,
                layers: one(&t.layer),
                shape: Shape::Capsule { a: Point::from_m(t.start), b: Point::from_m(t.end), r: nm(t.width / 2.0) },
                hole: None,
                locked: t.locked,
                span: Some(t.span),
            });
        }
        for arc in &copper.arcs {
            let pts = arc_chords(arc.start, arc.mid, arc.end, arc.width / 2.0);
            for w in pts.windows(2) {
                board.insert(Item {
                    kind: Kind::Track,
                    net: arc.net,
                    layers: one(&arc.layer),
                    shape: Shape::Capsule { a: w[0], b: w[1], r: nm(arc.width / 2.0) },
                    hole: None,
                    locked: arc.locked,
                    span: Some(arc.span),
                });
            }
        }
        for v in &copper.vias {
            let (from, to) = (layers.iter().position(|l| *l == v.layers[0]), layers.iter().position(|l| *l == v.layers[1]));
            let span_mask = match (from, to) {
                (Some(a), Some(b)) => (a.min(b)..=a.max(b)).fold(0, |m, i| m | 1 << i),
                _ => 0,
            };
            let at = Point::from_m(v.at);
            board.insert(Item {
                kind: Kind::Via,
                net: v.net,
                layers: span_mask,
                shape: Shape::Capsule { a: at, b: at, r: nm(v.size / 2.0) },
                hole: Some((at, nm(v.drill / 2.0))),
                locked: v.locked,
                span: Some(v.span),
            });
        }
        for (z, zone) in copper.zones.iter().enumerate() {
            // A soft via keepout forbids nothing; the router only avoids putting vias in it.
            if zone.name.to_ascii_lowercase().starts_with(SOFT_NO_VIA) {
                if zone.outline.len() >= 3 {
                    board.via_avoid.push(Shape::Polygon(zone.outline.iter().copied().map(Point::from_m).collect()));
                }
                continue;
            }
            if let Some(keepout) = zone.keepout {
                if zone.outline.len() >= 3 {
                    board.insert(Item {
                        kind: Kind::Keepout { forbids: keepout, footprint: zone.footprint },
                        net: 0,
                        layers: mask(&zone.layers),
                        shape: Shape::Polygon(zone.outline.iter().copied().map(Point::from_m).collect()),
                        hole: None,
                        locked: true,
                        span: Some(zone.span),
                    });
                }
                continue;
            }
            for (layer, poly) in &zone.filled {
                if poly.len() >= 3 {
                    board.insert(Item {
                        kind: Kind::Pour { zone: z, kept: blocks.iter().any(|b| b.zones.contains(&z)) },
                        net: zone.net,
                        layers: one(layer),
                        shape: Shape::Polygon(poly.iter().copied().map(Point::from_m).collect()),
                        hole: None,
                        locked: true,
                        span: None,
                    });
                }
            }
        }
        let bare = |front: bool, back: bool| (if front { one("F.Cu") } else { 0 }) | (if back { one("B.Cu") } else { 0 });
        board.via_openings = (bare(copper.vias_bare[0], copper.vias_bare[1]), nm(copper.pad_to_mask_clearance.max(0.0)));
        for g in &copper.mask_graphics {
            let layer = bare(g.layer == "F.Mask", g.layer == "B.Mask");
            let shapes: Vec<Shape> = g.polygons.iter().map(|poly| Shape::Polygon(poly.iter().copied().map(Point::from_m).collect())).collect();
            apertures.push((shapes, layer, None));
        }
        for (shapes, layer, net) in apertures {
            if layer == 0 {
                continue;
            }
            let l = layer.trailing_zeros() as usize;
            // As KiCad judges an opening that belongs to no copper: by the nets of the pads it
            // bares, over the whole of it (its outline's stroke is drawn in pieces), a pad being
            // bared when its copper there touches the opening. One net may run there; with none
            // or several, none may.
            let net = net.unwrap_or_else(|| {
                let mut nets: Vec<u32> = Vec::new();
                for shape in &shapes {
                    for id in board.near(l, shape.bbox()) {
                        let Some(it) = board.item(id) else { continue };
                        if matches!(it.kind, Kind::Pad { .. }) && it.on(l) && shape.gap_within(&it.shape, 1.0) == 0.0 {
                            nets.push(it.net);
                        }
                    }
                }
                nets.sort_unstable();
                nets.dedup();
                if let [n] = nets[..] { n } else { 0 }
            });
            board.mask_areas.extend(shapes.into_iter().map(|shape| (shape, layer, net)));
        }
        for g in &copper.graphics {
            for poly in &g.polygons {
                board.insert(Item {
                    kind: Kind::Pour { zone: DRAWN, kept: true },
                    net: g.net,
                    layers: one(&g.layer),
                    shape: Shape::Polygon(poly.iter().copied().map(Point::from_m).collect()),
                    hole: None,
                    locked: true,
                    span: None,
                });
            }
        }
        board
    }

    /// Inner copper layers that are planes in all but name: one net's pours cover at least
    /// `share` of the board there. The layer index and the net, for each.
    pub fn plane_layers(&self, share: f64) -> Vec<(usize, u32)> {
        let area = |p: &[Point]| {
            (0..p.len()).map(|i| (p[i].x as f64) * (p[(i + 1) % p.len()].y as f64) - (p[(i + 1) % p.len()].x as f64) * (p[i].y as f64)).sum::<f64>().abs() / 2.0
        };
        let Some(outer) = self.outline.first() else { return Vec::new() };
        let board_area = area(outer) - self.outline.iter().skip(1).map(|h| area(h)).sum::<f64>();
        let mut out = Vec::new();
        for l in 1..self.layers.len().saturating_sub(1) {
            let mut by_net: HashMap<u32, f64> = HashMap::new();
            for (_, it) in self.items() {
                if let (Kind::Pour { .. }, Shape::Polygon(p)) = (&it.kind, &it.shape) {
                    if it.net != 0 && it.on(l) && !it.drawn() {
                        *by_net.entry(it.net).or_default() += area(p);
                    }
                }
            }
            // Equal areas go to the lower net, not to the hash map's order.
            if let Some((&net, &a)) = by_net.iter().max_by(|x, y| x.1.total_cmp(y.1).then(y.0.cmp(x.0))) {
                if board_area > 0.0 && a >= share * board_area {
                    out.push((l, net));
                }
            }
        }
        out
    }

    /// Whether a via of radius `r` at `p` would reach into a via-avoid area.
    pub fn in_via_avoid(&self, p: Point, r: Nm) -> bool {
        let via = Shape::Capsule { a: p, b: p, r };
        self.via_avoid.iter().any(|area| area.gap_within(&via, 1.0) <= 0.0)
    }

    pub fn item(&self, id: ItemId) -> Option<&Item> {
        self.items.get(id).and_then(Option::as_ref)
    }

    pub fn bbox(&self, id: ItemId) -> Option<BBox> {
        self.items.get(id).and_then(Option::as_ref).map(|_| self.shape_bboxes[id])
    }

    pub fn items(&self) -> impl Iterator<Item = (ItemId, &Item)> {
        self.items.iter().enumerate().filter_map(|(i, it)| Some((i, it.as_ref()?)))
    }

    fn slots(&self, item: &Item) -> Vec<usize> {
        let mut slots: Vec<usize> = (0..self.layers.len()).filter(|l| item.on(*l)).collect();
        if item.hole.is_some() {
            slots.push(self.layers.len());
        }
        slots
    }

    fn cells_of(b: BBox) -> impl Iterator<Item = (Nm, Nm)> {
        let (x0, x1) = (b.min.x.div_euclid(CELL), b.max.x.div_euclid(CELL));
        let (y0, y1) = (b.min.y.div_euclid(CELL), b.max.y.div_euclid(CELL));
        (x0..=x1).flat_map(move |x| (y0..=y1).map(move |y| (x, y)))
    }

    pub fn insert(&mut self, item: Item) -> ItemId {
        let id = self.items.len();
        let sb = item.shape.bbox();
        let b = match item.hole {
            Some((c, r)) => sb.union(BBox::of([c]).grow(r)),
            None => sb,
        };
        for slot in self.slots(&item) {
            for (x, y) in Board::cells_of(b) {
                self.cells.entry((slot, x, y)).or_default().push(id);
            }
        }
        self.items.push(Some(item));
        self.shape_bboxes.push(sb);
        self.bboxes.push(b);
        id
    }

    /// Removes an item tried and not kept, and its slot too if it is the newest, so the trial
    /// inserts a search makes by the million do not grow the board. Only for an item nothing else
    /// was inserted after, or whose later items are gone the same way: its id may be given out
    /// again.
    pub fn discard(&mut self, id: ItemId) -> Option<Item> {
        let item = self.remove(id);
        if id + 1 == self.items.len() {
            self.items.pop();
            self.shape_bboxes.pop();
            self.bboxes.pop();
        }
        item
    }

    /// Puts a removed item back under the id it had.
    pub fn restore(&mut self, id: ItemId, item: Item) {
        let sb = item.shape.bbox();
        let b = match item.hole {
            Some((c, r)) => sb.union(BBox::of([c]).grow(r)),
            None => sb,
        };
        for slot in self.slots(&item) {
            for (x, y) in Board::cells_of(b) {
                self.cells.entry((slot, x, y)).or_default().push(id);
            }
        }
        if id >= self.items.len() {
            self.items.resize(id + 1, None);
            self.shape_bboxes.resize(id + 1, BBox { min: Point::new(0, 0), max: Point::new(0, 0) });
            self.bboxes.resize(id + 1, BBox { min: Point::new(0, 0), max: Point::new(0, 0) });
        }
        self.items[id] = Some(item);
        self.shape_bboxes[id] = sb;
        self.bboxes[id] = b;
    }

    pub fn remove(&mut self, id: ItemId) -> Option<Item> {
        let item = self.items.get_mut(id)?.take()?;
        let b = self.bboxes[id];
        for slot in self.slots(&item) {
            for (x, y) in Board::cells_of(b) {
                if let Some(ids) = self.cells.get_mut(&(slot, x, y)) {
                    ids.retain(|i| *i != id);
                }
            }
        }
        Some(item)
    }

    /// Items on `layer` whose extent may reach into `b`, each once. `layer == layers.len()`
    /// asks for holes.
    pub fn near(&self, layer: usize, b: BBox) -> Vec<ItemId> {
        let (x0, x1) = (b.min.x.div_euclid(CELL), b.max.x.div_euclid(CELL));
        let (y0, y1) = (b.min.y.div_euclid(CELL), b.max.y.div_euclid(CELL));
        if x0 == x1 && y0 == y1 {
            let mut out: Vec<ItemId> = self
                .cells
                .get(&(layer, x0, y0))
                .into_iter()
                .flatten()
                .copied()
                .filter(|&id| self.items.get(id).is_some_and(Option::is_some) && self.bboxes[id].overlaps(b))
                .collect();
            out.sort_unstable();
            return out;
        }
        let mut out: Vec<ItemId> = Board::cells_of(b)
            .filter_map(|(x, y)| self.cells.get(&(layer, x, y)))
            .flatten()
            .copied()
            .filter(|&id| self.items.get(id).is_some_and(Option::is_some) && self.bboxes[id].overlaps(b))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Moves a footprint's pads and rule areas by `by`. Unlocked tracks and vias that touched its
    /// pads are taken out, to be routed again, and returned.
    pub fn move_footprint(&mut self, footprint: usize, by: Point) -> Vec<Item> {
        let members: Vec<ItemId> = self
            .items()
            .filter(|(_, it)| match &it.kind {
                Kind::Pad { footprint: f, .. } => *f == footprint,
                Kind::Keepout { footprint: f, .. } => *f == Some(footprint),
                _ => false,
            })
            .map(|(id, _)| id)
            .collect();
        let mut attached = Vec::new();
        for &m in &members {
            let Some(pad) = self.item(m) else { continue };
            if !matches!(pad.kind, Kind::Pad { .. }) {
                continue;
            }
            for layer in (0..self.layers.len()).filter(|l| pad.on(*l)) {
                for other in self.near(layer, pad.shape.bbox()) {
                    let Some(o) = self.item(other) else { continue };
                    if matches!(o.kind, Kind::Track | Kind::Via) && !o.locked && o.on(layer) && o.shape.gap_within(&pad.shape, 0.0) == 0.0 {
                        attached.push(other);
                    }
                }
            }
        }
        attached.sort_unstable();
        attached.dedup();
        let ripped: Vec<Item> = attached.into_iter().filter_map(|id| self.remove(id)).collect();
        self.shift_footprint(footprint, by);
        ripped
    }

    /// Moves a footprint's pads, rule areas and courtyard by `by`, leaving all copper where it is.
    pub fn shift_footprint(&mut self, footprint: usize, by: Point) {
        let members: Vec<ItemId> = self
            .items()
            .filter(|(_, it)| match &it.kind {
                Kind::Pad { footprint: f, .. } => *f == footprint,
                Kind::Keepout { footprint: f, .. } => *f == Some(footprint),
                _ => false,
            })
            .map(|(id, _)| id)
            .collect();
        for m in members {
            if let Some(mut item) = self.remove(m) {
                item.shape = item.shape.translated(by);
                item.hole = item.hole.map(|(c, r)| (Point::new(c.x + by.x, c.y + by.y), r));
                self.restore(m, item);
            }
        }
        if let Some(f) = self.footprints.get_mut(footprint) {
            f.at = Point::new(f.at.x + by.x, f.at.y + by.y);
            if let Some(yard) = f.courtyard.as_mut() {
                for p in yard.iter_mut() {
                    *p = Point::new(p.x + by.x, p.y + by.y);
                }
            }
        }
    }

    pub fn net_name(&self, net: u32) -> &str {
        self.nets.get(net as usize).map_or("", String::as_str)
    }

    /// Makes the layers in `mask` planes: a rule area over the whole board keeps every track off
    /// them, vias still passing, and their pours join their nets (see [`crate::connectivity_without_pours`]).
    pub fn set_planes(&mut self, mask: u64) {
        self.planes |= mask;
        let Some(outer) = self.outline.first().cloned() else { return };
        for layer in (0..self.layers.len()).filter(|l| mask & (1 << l) != 0) {
            self.insert(Item {
                kind: Kind::Keepout { forbids: Keepout { tracks: true, ..Keepout::default() }, footprint: None },
                net: 0,
                layers: 1 << layer,
                shape: Shape::Polygon(outer.clone()),
                hole: None,
                locked: true,
                span: None,
            });
        }
    }

    /// Whether an item is a plane's pour.
    pub fn is_plane(&self, item: &Item) -> bool {
        matches!(item.kind, Kind::Pour { .. }) && item.layers & self.planes != 0
    }

    /// The clearance the rules ask between two nets' copper, in nanometres.
    pub fn clearance(&self, a: u32, b: u32) -> Nm {
        let ca = self.net_clearances.get(a as usize).copied().unwrap_or(self.min_clearance_nm);
        let cb = self.net_clearances.get(b as usize).copied().unwrap_or(self.min_clearance_nm);
        ca.max(cb)
    }
}

fn arc_chords(s: [f64; 2], m: [f64; 2], e: [f64; 2], half_width: f64) -> Vec<Point> {
    let Some(c) = kicad_pcb::circumcenter(s, m, e) else {
        return vec![Point::from_m(s), Point::from_m(e)];
    };
    let r = (s[0] - c[0]).hypot(s[1] - c[1]);
    let angle = |p: [f64; 2]| (p[1] - c[1]).atan2(p[0] - c[0]);
    let wrap = |a: f64| a.rem_euclid(std::f64::consts::TAU);
    let (a0, am, a1) = (angle(s), angle(m), angle(e));
    let sweep = if wrap(am - a0) < wrap(a1 - a0) { wrap(a1 - a0) } else { -wrap(a0 - a1) };
    let per = 2.0 * (1.0 - (MAX_ERROR_M / (r + half_width)).min(1.0)).acos();
    let n = ((sweep.abs() / per.max(1e-6)).ceil() as usize).max(1);
    (0..=n)
        .map(|k| {
            let a = a0 + sweep * k as f64 / n as f64;
            Point::from_m([c[0] + r * a.cos(), c[1] + r * a.sin()])
        })
        .collect()
}
