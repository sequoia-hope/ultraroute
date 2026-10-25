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

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Pad { footprint: usize, number: String, plated: bool },
    Track,
    Via,
    /// Copper a zone poured, one item per filled polygon. A kept pour belongs to a block: it is
    /// a plane other nets keep clear of, not copper refilled around them.
    Pour { zone: usize, kept: bool },
    /// A rule area, and the footprint it belongs to, whose own pads it does not keep out.
    Keepout { forbids: Keepout, footprint: Option<usize> },
}

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
            for poly in &board.outline {
                for i in 0..poly.len() {
                    let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
                    board.outline_edges.push((a, b, BBox::of([a, b])));
                }
            }
        }

        for pad in &copper.pads {
            let model = &pcb.footprints[pad.footprint].pads[pad.pad];
            let hole = model.drill.map(|d| (Point::from_m(model.position), nm(d[0].min(d[1]) / 2.0)));
            let plated = !matches!(model.kind, PadKind::NpThruHole);
            let bigger_than_hole = model.drill.is_none_or(|d| model.size[0] > d[0] || model.size[1] > d[1]);
            let layers = if plated || bigger_than_hole { mask(&pad.layers) } else { 0 };
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
            if let Outline::Polygons(polys) = &pad.outline {
                for extra in polys.iter().skip(1) {
                    board.insert(Item {
                        shape: Shape::Polygon(extra.iter().copied().map(Point::from_m).collect()),
                        hole: None,
                        ..base.clone()
                    });
                }
            }
            board.insert(base);
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
        board
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
