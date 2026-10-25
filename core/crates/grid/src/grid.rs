//! A conservative routing grid. A cell centre is free for a trace of a net only if a trace
//! centred anywhere on a step from it to a neighbouring free centre keeps its clearance to every
//! other net's copper, so any path the grid accepts is legal; it may refuse legal paths, never
//! the other way round. One obstacle map serves every net of a class: each cell records whose
//! copper claims it.

use board::{nm, point_segment, Board, ItemId, Kind, Nm, Point, Shape};

/// How far a step between neighbouring centres strays from the nearer one, in cells.
const STEP_REACH: f64 = std::f64::consts::FRAC_1_SQRT_2;

pub const FREE: i32 = 0;
/// More than one net, or copper of no net: nobody may route here.
pub const SEVERAL: i32 = -1;

/// The sizes a net class routes with, in nanometres.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub class: String,
    pub half_width: Nm,
    pub clearance: Nm,
    pub via_radius: Nm,
    pub drill_radius: Nm,
}

impl Profile {
    pub fn for_net(board: &Board, net: u32) -> Profile {
        let rules = &board.rules;
        let class = rules.class_of(board.net_name(net));
        let take = |v: Option<f64>, floor: f64| nm(v.unwrap_or(floor).max(floor));
        Profile {
            class: class.map_or_else(String::new, |c| c.name.clone()),
            half_width: take(class.map(|c| c.track_width), rules.min_track_width) / 2,
            clearance: take(class.map(|c| c.clearance), rules.min_clearance),
            via_radius: take(class.map(|c| c.via_diameter), rules.min_via_diameter) / 2,
            drill_radius: take(class.map(|c| c.via_drill), rules.min_via_drill) / 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ViaSpec {
    pub radius: Nm,
    pub drill_radius: Nm,
}

#[derive(Debug, Clone)]
pub struct RouteGrid {
    pub net: u32,
    pub x0: Nm,
    pub y0: Nm,
    pub h: Nm,
    pub nx: usize,
    pub ny: usize,
    /// Board layer of each routing layer.
    pub layers: Vec<usize>,
    pub half_width: Nm,
    pub via: Option<ViaSpec>,
    /// Per routing layer and cell: a trace of the net may not pass this centre.
    pub blocked: Vec<bool>,
    /// Per cell: a via of the net may not stand here.
    pub via_blocked: Vec<bool>,
    /// Per routing layer and cell: extra cost of entering it; empty for none.
    pub penalty: Vec<u32>,
}

impl RouteGrid {
    pub fn penalty_at(&self, index: usize) -> u32 {
        self.penalty.get(index).copied().unwrap_or(0)
    }

    pub fn cell_count(&self) -> usize {
        self.nx * self.ny
    }

    pub fn index(&self, layer: usize, i: usize, j: usize) -> usize {
        (layer * self.ny + j) * self.nx + i
    }

    pub fn centre(&self, i: usize, j: usize) -> Point {
        Point::new(self.x0 + (2 * i as Nm + 1) * self.h / 2, self.y0 + (2 * j as Nm + 1) * self.h / 2)
    }

    /// The cell whose centre is nearest `p`, if on the grid.
    pub fn cell_at(&self, p: Point) -> Option<(usize, usize)> {
        let i = (p.x - self.x0).div_euclid(self.h);
        let j = (p.y - self.y0).div_euclid(self.h);
        (i >= 0 && j >= 0 && (i as usize) < self.nx && (j as usize) < self.ny).then_some((i as usize, j as usize))
    }
}

#[derive(Debug, Clone)]
pub struct ObstacleMap {
    pours_block: bool,
    pub profile: Profile,
    pub x0: Nm,
    pub y0: Nm,
    pub h: Nm,
    pub nx: usize,
    pub ny: usize,
    layers: usize,
    owner: Vec<i32>,
    via_owner: Vec<i32>,
}

impl ObstacleMap {
    /// With `pours_block`, poured copper is kept like any other copper; otherwise it claims
    /// nothing, to be refilled around whatever is routed through it.
    pub fn new(board: &Board, profile: Profile, h: Nm, pours_block: bool) -> ObstacleMap {
        let b = board.outline.iter().flatten().copied().fold(None, |b: Option<board::BBox>, p| {
            Some(b.map_or(board::BBox::of([p]), |b| b.union(board::BBox::of([p]))))
        });
        let b = b.unwrap_or_else(|| board::BBox::of(board.items().map(|(_, it)| it.shape.bbox().min)));
        let (x0, y0) = (b.min.x, b.min.y);
        let nx = ((b.max.x - x0) / h).max(1) as usize;
        let ny = ((b.max.y - y0) / h).max(1) as usize;
        let layers = board.layers.len();
        let mut map = ObstacleMap {
            pours_block,
            profile,
            x0,
            y0,
            h,
            nx,
            ny,
            layers,
            owner: vec![FREE; layers * nx * ny],
            via_owner: vec![FREE; nx * ny],
        };
        map.stamp_outline(board);
        let ids: Vec<ItemId> = board.items().map(|(id, _)| id).collect();
        for id in ids {
            map.stamp(board, id);
        }
        map
    }

    fn reach(&self) -> Nm {
        (self.h as f64 * STEP_REACH).ceil() as Nm
    }

    fn centre(&self, i: usize, j: usize) -> Point {
        Point::new(self.x0 + (2 * i as Nm + 1) * self.h / 2, self.y0 + (2 * j as Nm + 1) * self.h / 2)
    }

    fn claim(cell: &mut i32, net: u32) {
        let net = if net == 0 { SEVERAL } else { net as i32 };
        *cell = match *cell {
            FREE => net,
            c if c == net => c,
            _ => SEVERAL,
        };
    }

    /// Calls `f` for every cell whose centre lies closer than `margin` to `shape`.
    fn cells_near(&self, shape: &Shape, margin: Nm, f: &mut dyn FnMut(usize)) {
        let range = |lo: Nm, hi: Nm, origin: Nm, n: usize| {
            let a = ((lo - origin) as f64 / self.h as f64 - 0.5).floor().max(0.0) as usize;
            let b = (((hi - origin) as f64 / self.h as f64 - 0.5).ceil().max(0.0) as usize).min(n.saturating_sub(1));
            a..=b
        };
        match shape {
            Shape::Capsule { a, b, r } => {
                let bb = shape.bbox().grow(margin);
                for j in range(bb.min.y, bb.max.y, self.y0, self.ny) {
                    for i in range(bb.min.x, bb.max.x, self.x0, self.nx) {
                        if point_segment(self.centre(i, j), *a, *b) < (*r + margin) as f64 {
                            f(j * self.nx + i);
                        }
                    }
                }
            }
            Shape::Polygon(poly) => {
                let bb = shape.bbox();
                // The inside by scanline, then a band along every edge.
                for j in range(bb.min.y, bb.max.y, self.y0, self.ny) {
                    let y = self.centre(0, j).y;
                    let mut xs: Vec<f64> = Vec::new();
                    for k in 0..poly.len() {
                        let (p, q) = (poly[k], poly[(k + 1) % poly.len()]);
                        if (p.y > y) != (q.y > y) {
                            xs.push(p.x as f64 + (y - p.y) as f64 * (q.x - p.x) as f64 / (q.y - p.y) as f64);
                        }
                    }
                    xs.sort_by(f64::total_cmp);
                    for span in xs.chunks_exact(2) {
                        for i in range(span[0].floor() as Nm, span[1].ceil() as Nm, self.x0, self.nx) {
                            let x = self.centre(i, j).x as f64;
                            if x > span[0] && x < span[1] {
                                f(j * self.nx + i);
                            }
                        }
                    }
                }
                if margin > 0 {
                    for k in 0..poly.len() {
                        let (p, q) = (poly[k], poly[(k + 1) % poly.len()]);
                        self.cells_near(&Shape::Capsule { a: p, b: q, r: 0 }, margin, f);
                    }
                }
            }
        }
    }

    /// Everything outside the board, and a band inside its edge, is nobody's.
    fn stamp_outline(&mut self, board: &Board) {
        let rules = &board.rules;
        let edge = nm(rules.min_copper_edge_clearance);
        let Some(outer) = board.outline.first() else { return };
        let cells = self.nx * self.ny;
        let mut inside = vec![false; cells];
        self.cells_near(&Shape::Polygon(outer.clone()), 0, &mut |c| inside[c] = true);
        for hole in board.outline.iter().skip(1) {
            self.cells_near(&Shape::Polygon(hole.clone()), 0, &mut |c| inside[c] = false);
        }
        for c in (0..cells).filter(|c| !inside[*c]) {
            self.via_owner[c] = SEVERAL;
            for l in 0..self.layers {
                self.owner[l * cells + c] = SEVERAL;
            }
        }
        let trace_margin = self.profile.half_width + edge + self.reach();
        let via_margin = self.profile.via_radius + edge;
        for poly in &board.outline {
            for k in 0..poly.len() {
                let edge_shape = Shape::Capsule { a: poly[k], b: poly[(k + 1) % poly.len()], r: 0 };
                let mut hit = Vec::new();
                self.cells_near(&edge_shape, trace_margin, &mut |c| hit.push(c));
                for c in hit {
                    for l in 0..self.layers {
                        self.owner[l * cells + c] = SEVERAL;
                    }
                }
                let mut hit = Vec::new();
                self.cells_near(&edge_shape, via_margin, &mut |c| hit.push(c));
                for c in hit {
                    self.via_owner[c] = SEVERAL;
                }
            }
        }
    }

    /// The clearance between this class's copper and an item of `net`.
    fn clearance_to(&self, board: &Board, net: u32) -> Nm {
        let other = board.rules.class_of(board.net_name(net)).map_or(0.0, |c| c.clearance);
        self.profile.clearance.max(nm(other)).max(nm(board.rules.min_clearance))
    }

    /// Records what `id` claims.
    pub fn stamp(&mut self, board: &Board, id: ItemId) {
        let Some(item) = board.item(id) else { return };
        let cells = self.nx * self.ny;
        let p = self.profile.clone();
        let reach = self.reach();
        let rules = &board.rules;
        if let Kind::Keepout { forbids, .. } = &item.kind {
            if forbids.tracks {
                for l in (0..self.layers).filter(|l| item.on(*l)) {
                    let mut hit = Vec::new();
                    self.cells_near(&item.shape, p.half_width + reach, &mut |c| hit.push(c));
                    for c in hit {
                        self.owner[l * cells + c] = SEVERAL;
                    }
                }
            }
            if forbids.vias && item.layers != 0 {
                let mut hit = Vec::new();
                self.cells_near(&item.shape, p.via_radius, &mut |c| hit.push(c));
                for c in hit {
                    self.via_owner[c] = SEVERAL;
                }
            }
            return;
        }
        if item.is_copper() && (self.pours_block || !matches!(item.kind, Kind::Pour { .. })) {
            let clearance = self.clearance_to(board, item.net);
            let mut hit = Vec::new();
            self.cells_near(&item.shape, p.half_width + clearance + reach, &mut |c| hit.push(c));
            for l in (0..self.layers).filter(|l| item.on(*l)) {
                for &c in &hit {
                    ObstacleMap::claim(&mut self.owner[l * cells + c], item.net);
                }
            }
            let mut hit = Vec::new();
            self.cells_near(&item.shape, p.via_radius + clearance, &mut |c| hit.push(c));
            for c in hit {
                ObstacleMap::claim(&mut self.via_owner[c], item.net);
            }
        }
        if let Some((centre, r)) = item.hole {
            let hole = Shape::Capsule { a: centre, b: centre, r };
            let margin = (p.drill_radius + nm(rules.min_hole_to_hole)).max(p.via_radius + nm(rules.min_hole_clearance));
            let mut hit = Vec::new();
            self.cells_near(&hole, margin, &mut |c| hit.push(c));
            for c in hit {
                // A via of the same net may stand on its own via, but no via may share a hole.
                self.via_owner[c] = SEVERAL;
            }
            // Tracks keep the hole clearance from every hole, unplated ones too.
            let mut hit = Vec::new();
            self.cells_near(&hole, p.half_width + nm(rules.min_hole_clearance) + reach, &mut |c| hit.push(c));
            for l in 0..self.layers {
                for &c in &hit {
                    ObstacleMap::claim(&mut self.owner[l * cells + c], if item.plated_hole() { item.net } else { 0 });
                }
            }
        }
    }

    /// The grid a net sees: a cell is blocked unless nothing claims it or only this net does.
    pub fn view(&self, net: u32) -> RouteGrid {
        let free = |o: i32| o == FREE || o == net as i32;
        RouteGrid {
            net,
            x0: self.x0,
            y0: self.y0,
            h: self.h,
            nx: self.nx,
            ny: self.ny,
            layers: (0..self.layers).collect(),
            half_width: self.profile.half_width,
            via: Some(ViaSpec { radius: self.profile.via_radius, drill_radius: self.profile.drill_radius }),
            blocked: self.owner.iter().map(|&o| !free(o)).collect(),
            via_blocked: self.via_owner.iter().map(|&o| !free(o)).collect(),
            penalty: Vec::new(),
        }
    }
}
