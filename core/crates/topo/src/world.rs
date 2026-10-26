//! The obstacles, per layer: their outlines as triangulation samples, and an index of their
//! polygons for the exact questions the samples can't answer. Copper this router laid remembers
//! the connection it belongs to, so a search may cross it at a price and have it ripped.

use std::collections::{BTreeMap, HashMap};

use board::{Board, ItemId, Kind, Shape};
use spade::handles::{FixedFaceHandle, InnerTag};
use spade::{DelaunayTriangulation, Point2, PositionInTriangulation, Triangulation};

use crate::funnel::{Gate, Pt};
use crate::outline::{pt, Outline};
use crate::WireRules;

pub type Face = FixedFaceHandle<InnerTag>;

/// Which connection each piece of router copper was laid for.
pub type Conns = HashMap<ItemId, u32>;

/// What an obstacle is, which decides how near a wire or a via may come to it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rule {
    Copper,
    /// A drilled hole, kept apart from other holes and, unless it is plated and of the wire's own
    /// net, from copper.
    Hole { plated: bool },
    /// One edge of the board outline.
    Edge,
    Keepout { tracks: bool, vias: bool },
}

#[derive(Clone)]
struct Obstacle {
    net: u32,
    /// The connection this copper was laid for, when a rip-up may remove it.
    conn: Option<u32>,
    rule: Rule,
    layers: u64,
    outline: Outline,
}

impl Obstacle {
    fn on(&self, layer: usize) -> bool {
        self.layers & (1 << layer) != 0
    }
}

#[derive(Clone)]
struct Layer {
    cdt: DelaunayTriangulation<Point2<f64>>,
    /// The obstacles each vertex samples, by vertex index.
    owners: Vec<Vec<u32>>,
}

#[derive(Clone)]
pub struct World {
    obstacles: Vec<Obstacle>,
    layers: BTreeMap<usize, Layer>,
    cells: HashMap<(usize, i64, i64), Vec<u32>>,
    sample: f64,
    pours_block: bool,
    copper_layers: usize,
}

/// Side of an index cell, in nanometres.
const CELL: f64 = 1_000_000.0;
/// How many bends `clear_path` may add to one run.
const MAX_BENDS: usize = 200;
const BEND_SLACK: f64 = 1.02;

/// How near a wire's centre may come to a vertex: with everything kept, and with the copper of
/// `soft`, the connections that keep it nearer, ripped.
struct Reach {
    hard: f64,
    all: f64,
    soft: Vec<u32>,
}

impl World {
    /// With `pours_block` false, poured copper is no obstacle: it will be refilled around
    /// whatever is routed through it.
    pub fn build(board: &Board, sample: f64, pours_block: bool, conns: &Conns) -> World {
        let mut bounds = board::BBox::of(board.outline.iter().flatten().copied());
        for (_, item) in board.items() {
            bounds = bounds.union(item.shape.bbox());
        }
        let pad = 10.0 * sample;
        let (x0, y0, x1, y1) = (bounds.min.x as f64 - pad, bounds.min.y as f64 - pad, bounds.max.x as f64 + pad, bounds.max.y as f64 + pad);
        let mut layers = BTreeMap::new();
        for layer in 0..board.layers.len() {
            let mut l = Layer { cdt: DelaunayTriangulation::new(), owners: Vec::new() };
            for p in [Pt::new(x0, y0), Pt::new(x1, y0), Pt::new(x1, y1), Pt::new(x0, y1)] {
                l.insert(p, None);
            }
            layers.insert(layer, l);
        }
        let mut world =
            World { obstacles: Vec::new(), layers, cells: HashMap::new(), sample, pours_block, copper_layers: board.layers.len() };
        let every_layer = (1u64 << board.layers.len()) - 1;
        for poly in &board.outline {
            for k in 0..poly.len() {
                let (a, b) = (poly[k], poly[(k + 1) % poly.len()]);
                world.add(Obstacle { net: 0, conn: None, rule: Rule::Edge, layers: every_layer, outline: Outline::new(vec![pt(a), pt(b)], false) });
            }
        }
        let ids: Vec<ItemId> = board.items().map(|(id, _)| id).collect();
        world.add_items(board, &ids, conns);
        world
    }

    pub fn add_items(&mut self, board: &Board, ids: &[ItemId], conns: &Conns) {
        let every_layer = (1u64 << board.layers.len()) - 1;
        for &id in ids {
            let Some(item) = board.item(id) else { continue };
            let conn = conns.get(&id).copied();
            let rule = match &item.kind {
                Kind::Keepout { forbids, .. } if forbids.tracks || forbids.vias => Some(Rule::Keepout { tracks: forbids.tracks, vias: forbids.vias }),
                Kind::Keepout { .. } => None,
                Kind::Pour { .. } if !self.pours_block && item.refilled() => None,
                _ if item.is_copper() => Some(Rule::Copper),
                _ => None,
            };
            if let Some(rule) = rule {
                let outline = Outline::of(&item.shape);
                if !outline.points.is_empty() {
                    self.add(Obstacle { net: item.net, conn, rule, layers: item.layers, outline });
                }
            }
            if let Some((centre, r)) = item.hole {
                let outline = Outline::of(&Shape::Capsule { a: centre, b: centre, r });
                let rule = Rule::Hole { plated: item.plated_hole() };
                self.add(Obstacle { net: item.net, conn, rule, layers: every_layer, outline });
            }
        }
    }

    fn add(&mut self, o: Obstacle) {
        let at = self.obstacles.len() as u32;
        let (lo, hi) = o.outline.extent();
        for layer in (0..self.copper_layers).filter(|l| o.on(*l)) {
            for cx in (lo.x / CELL).floor() as i64..=(hi.x / CELL).floor() as i64 {
                for cy in (lo.y / CELL).floor() as i64..=(hi.y / CELL).floor() as i64 {
                    self.cells.entry((layer, cx, cy)).or_default().push(at);
                }
            }
            // Only what keeps wires away needs to be a wall in the triangulation.
            if !matches!(o.rule, Rule::Keepout { tracks: false, .. }) {
                if let Some(l) = self.layers.get_mut(&layer) {
                    for p in o.outline.samples(self.sample) {
                        l.insert(p, Some(at));
                    }
                }
            }
        }
        self.obstacles.push(o);
    }

    pub fn face_at(&self, layer: usize, p: Pt) -> Option<Face> {
        let cdt = &self.layers.get(&layer)?.cdt;
        match cdt.locate(Point2::new(p.x, p.y)) {
            PositionInTriangulation::OnFace(face) => Some(face),
            PositionInTriangulation::OnEdge(edge) => {
                let edge = cdt.directed_edge(edge);
                edge.face().as_inner().or_else(|| edge.rev().face().as_inner()).map(|f| f.fix())
            }
            PositionInTriangulation::OnVertex(vertex) => cdt.vertex(vertex).out_edges().find_map(|e| e.face().as_inner()).map(|f| f.fix()),
            _ => None,
        }
    }

    /// The reach of the obstacles a vertex samples that keep the wire away, 0 when none does.
    fn reach(&self, wire: &WireRules, owners: &[u32]) -> Reach {
        let mut out = Reach { hard: 0.0, all: 0.0, soft: Vec::new() };
        let mut soft = Vec::new();
        for o in owners.iter().map(|&o| &self.obstacles[o as usize]) {
            let Some(reach) = wire.trace_reach(o.rule, o.net) else { continue };
            out.all = out.all.max(reach);
            match o.conn {
                Some(c) => soft.push((c, reach)),
                None => out.hard = out.hard.max(reach),
            }
        }
        out.soft = soft.into_iter().filter(|&(_, r)| r > out.hard).map(|(c, _)| c).collect();
        out
    }

    fn near(&self, layer: usize, p: Pt, radius: f64) -> impl Iterator<Item = &Obstacle> {
        let (x0, x1) = (((p.x - radius) / CELL).floor() as i64, ((p.x + radius) / CELL).floor() as i64);
        let (y0, y1) = (((p.y - radius) / CELL).floor() as i64, ((p.y + radius) / CELL).floor() as i64);
        let mut ids: Vec<u32> = Vec::new();
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                if let Some(found) = self.cells.get(&(layer, cx, cy)) {
                    ids.extend(found);
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        ids.into_iter().map(|o| &self.obstacles[o as usize]).filter(move |o| o.on(layer))
    }

    /// The faces a wire may step to from `face`, each with the stretch of the shared edge it may
    /// cross: the edge less each end's reach, if what is left is free copper. With `rip`, an edge
    /// also gives the wider stretch it would have with other connections' copper ripped, and the
    /// connections that would be.
    pub fn gates(&self, wire: &WireRules, layer: usize, face: Face, rip: bool) -> Vec<(Face, Gate, Vec<u32>)> {
        let Some(l) = self.layers.get(&layer) else { return Vec::new() };
        let face = l.cdt.face(face);
        let mut out = Vec::new();
        for edge in face.adjacent_edges() {
            let Some(next) = edge.rev().face().as_inner() else { continue };
            let (u, v) = (edge.from(), edge.to());
            let (pu, pv) = (u.position(), v.position());
            let reach_u = self.reach(wire, l.owners_of(u.fix().index()));
            let reach_v = self.reach(wire, l.owners_of(v.fix().index()));
            let len = (pv.x - pu.x).hypot(pv.y - pu.y);
            let mut kinds = vec![(reach_u.all, reach_v.all, false)];
            if rip {
                kinds.push((reach_u.hard, reach_v.hard, true));
            }
            for (ru, rv, ripping) in kinds {
                if len <= ru + rv {
                    continue;
                }
                let (dx, dy) = ((pv.x - pu.x) / len, (pv.y - pu.y) / len);
                let right = Pt::new(pu.x + dx * ru, pu.y + dy * ru);
                let left = Pt::new(pv.x - dx * rv, pv.y - dy * rv);
                let mid = Pt::new((left.x + right.x) / 2.0, (left.y + right.y) / 2.0);
                let Some(mut victims) = self.blockers(wire, layer, mid, ripping) else { continue };
                if ripping {
                    victims.extend(&reach_u.soft);
                    victims.extend(&reach_v.soft);
                    victims.sort_unstable();
                    victims.dedup();
                    if victims.is_empty() {
                        continue;
                    }
                }
                let gate = Gate {
                    left,
                    right,
                    left_wrap: (rv > 0.0).then_some((Pt::new(pv.x, pv.y), rv)),
                    right_wrap: (ru > 0.0).then_some((Pt::new(pu.x, pu.y), ru)),
                };
                out.push((next.fix(), gate, victims));
            }
        }
        out
    }

    /// Bends `points` out round the deepest cut into any obstacle's reach, until nothing is cut
    /// or `MAX_BENDS` bends are added; the ends stay put. The samples keep the channel clear only
    /// at the samples themselves; between them, and round a corner the funnel cut, this is what
    /// keeps the wire off the copper. At a pad end (`pad_start` or `pad_end`), the first or last
    /// segment uses `necked` so a wide net or margin does not fight neighbouring fine-pitch pads.
    pub fn clear_path(&self, wire: &WireRules, necked: &WireRules, layer: usize, mut points: Vec<Pt>, pad_start: bool, pad_end: bool) -> Vec<Pt> {
        for _ in 0..MAX_BENDS {
            let mut worst: Option<(usize, Pt, f64)> = None;
            let segs = points.len().saturating_sub(1);
            for k in 0..segs {
                let (a, b) = (points[k], points[k + 1]);
                let active = if (pad_start && k == 0) || (pad_end && k + 1 == segs) { necked } else { wire };
                let mid = Pt::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
                let span = (a.x - b.x).abs().max((a.y - b.y).abs()) / 2.0 + CELL;
                for o in self.near(layer, mid, span) {
                    let Some(reach) = active.trace_reach(o.rule, o.net) else { continue };
                    let (d, on_segment, on_obstacle) = o.outline.segment_within(a, b, reach);
                    let cut = reach - d;
                    if cut <= reach * 1e-3 || worst.as_ref().is_some_and(|w| cut <= w.2) {
                        continue;
                    }
                    let away = if d > 0.0 {
                        ((on_segment.x - on_obstacle.x) / d, (on_segment.y - on_obstacle.y) / d)
                    } else {
                        let c = o.outline.centroid();
                        let len = a.distance(b).max(1e-9);
                        let n = (-(b.y - a.y) / len, (b.x - a.x) / len);
                        if n.0 * (on_segment.x - c.x) + n.1 * (on_segment.y - c.y) >= 0.0 {
                            n
                        } else {
                            (-n.0, -n.1)
                        }
                    };
                    let bend = Pt::new(on_obstacle.x + away.0 * reach * BEND_SLACK, on_obstacle.y + away.1 * reach * BEND_SLACK);
                    worst = Some((k, bend, cut));
                }
            }
            let Some((k, bend, _)) = worst else { break };
            points.insert(k + 1, bend);
        }
        points
    }

    /// `None` when `p` is inside copper that keeps the wire out, else the connections whose
    /// copper it is inside, which only `rip` lets it be.
    fn blockers(&self, wire: &WireRules, layer: usize, p: Pt, rip: bool) -> Option<Vec<u32>> {
        let mut victims = Vec::new();
        for o in self.near(layer, p, 0.0) {
            if wire.trace_reach(o.rule, o.net).is_none() || !o.outline.inside(p) {
                continue;
            }
            match o.conn {
                Some(c) if rip => victims.push(c),
                _ => return None,
            }
        }
        Some(victims)
    }

    /// The centre of `face` when a via of the wire's net fits there on every copper layer, with
    /// the connections whose copper must be ripped for it, which only `rip` allows.
    pub fn via_site(&self, wire: &WireRules, layer: usize, face: Face, rip: bool) -> Option<(Pt, Vec<u32>)> {
        wire.via?;
        let l = self.layers.get(&layer)?;
        let centre = l.cdt.face(face).center();
        let c = Pt::new(centre.x, centre.y);
        let mut victims = Vec::new();
        for copper in 0..self.copper_layers {
            for o in self.near(copper, c, wire.max_via_reach) {
                let Some(need) = wire.via_reach(o.rule, o.net) else { continue };
                if o.outline.distance_within(c, need) < need {
                    match o.conn {
                        Some(conn) if rip => victims.push(conn),
                        _ => return None,
                    }
                }
            }
        }
        victims.sort_unstable();
        victims.dedup();
        Some((c, victims))
    }
}

impl Layer {
    fn insert(&mut self, p: Pt, owner: Option<u32>) {
        let Ok(handle) = self.cdt.insert(Point2::new(p.x, p.y)) else { return };
        let index = handle.index();
        if self.owners.len() <= index {
            self.owners.resize(index + 1, Vec::new());
        }
        if let Some(o) = owner {
            if !self.owners[index].contains(&o) {
                self.owners[index].push(o);
            }
        }
    }

    fn owners_of(&self, index: usize) -> &[u32] {
        self.owners.get(index).map_or(&[], Vec::as_slice)
    }
}
