//! Routing regions on the grid: each rule of each of the board's regions as the cells it
//! touches, worked out once per obstacle map, and applied to the view of every net it holds for.
//! `never` blocks the cells a track (or via) there would reach into the region; `must` blocks
//! every cell but those deep enough inside it; `avoid` and `prefer` price cells inside or
//! outside it. Costs go in `RouteGrid::region_cost` (tracks) and `via_penalty` (vias), which
//! the CPU searches add to every step; the GPU and the browser's worker pool do not see them yet.

use board::{point_segment, Board, Nm, RegionKind, Shape};

use crate::grid::{ObstacleMap, RouteGrid};
use crate::search::STRAIGHT;

/// One rule of one region, on one obstacle map's cells.
#[derive(Debug, Clone)]
pub(crate) struct RegionRaster {
    pub layers: u64,
    pub kind: RegionKind,
    pub vias: bool,
    /// The nets it holds for; every net when `None`.
    pub nets: Option<std::collections::HashSet<u32>>,
    /// A net it never holds for: a pour's own.
    pub except: Option<u32>,
    /// Cells where copper of this kind (a track's or a via's) would reach into the region.
    pub near: Vec<u32>,
    /// Cells where copper of this kind lies wholly inside it.
    pub inside: Vec<u32>,
    /// The cost a cell or via is charged, in search units.
    pub cost: u32,
}

impl ObstacleMap {
    /// The board's regions as rasters on this map's cells.
    pub(crate) fn region_rasters(&self, board: &Board) -> Vec<RegionRaster> {
        let mut out = Vec::new();
        for region in &board.regions {
            for rule in &region.rules {
                let reach = if rule.vias { self.profile.via_radius } else { self.profile.half_width + self.reach() };
                let mut near = Vec::new();
                self.cells_near(&region.shape, reach, &mut |c| near.push(c as u32));
                // Only `must` keeps to the cells inside.
                let mut inside = Vec::new();
                if rule.kind == RegionKind::Must {
                    self.cells_deep(&region.shape, reach, &mut |c| inside.push(c as u32));
                }
                let cost = match rule.kind {
                    RegionKind::Avoid(w) | RegionKind::Prefer(w) if rule.vias => (w * 1e6 / self.h as f64 * STRAIGHT as f64).round() as u32,
                    RegionKind::Avoid(w) | RegionKind::Prefer(w) => (w * STRAIGHT as f64).round() as u32,
                    _ => 0,
                };
                out.push(RegionRaster { layers: region.layers, kind: rule.kind, vias: rule.vias, nets: rule.nets.clone(), except: None, near, inside, cost });
            }
        }
        // Another net's pour on a layer that is not a plane is refilled round what crosses it,
        // and cut into pieces by it: crossing it costs `pour_cost` times the length inside.
        if board.pour_cost > 0.0 {
            let cost = (board.pour_cost * STRAIGHT as f64).round() as u32;
            for (_, it) in board.items().filter(|(_, it)| it.refilled() && it.net != 0 && !board.is_plane(it)) {
                let mut near = Vec::new();
                self.cells_near(&it.shape, self.profile.half_width, &mut |c| near.push(c as u32));
                if !near.is_empty() {
                    out.push(RegionRaster { layers: it.layers, kind: RegionKind::Avoid(board.pour_cost), vias: false, nets: None, except: Some(it.net), near, inside: Vec::new(), cost });
                }
            }
        }
        out
    }

    /// Calls `f` for every cell whose centre lies inside `shape` at least `depth` from its edge
    /// (`Shape::depth`), in the order `cells_near` gives the inside. Asking `depth` of every
    /// cell measures it to every edge, cells times edges: a ground fill's outline of 700 edges
    /// over a board took minutes. So the cells within `depth` of each edge are struck out from
    /// the inside instead, each edge visiting only its own band, measured as `depth` measures.
    pub(crate) fn cells_deep(&self, shape: &Shape, depth: Nm, f: &mut dyn FnMut(usize)) {
        let Shape::Polygon(poly) = shape else {
            self.cells_near(shape, 0, &mut |c| {
                if shape.depth(self.centre(c % self.nx, c / self.nx)) >= depth as f64 {
                    f(c)
                }
            });
            return;
        };
        let mut inside = Vec::new();
        self.cells_near(shape, 0, &mut |c| inside.push(c));
        if inside.is_empty() {
            return;
        }
        let mut shallow = vec![false; self.nx * self.ny];
        let d = depth as f64;
        let cols = |lo: f64, hi: f64| {
            let a = ((lo - self.x0 as f64) / self.h as f64 - 0.5).floor().max(0.0) as usize;
            let b = (((hi - self.x0 as f64) / self.h as f64 - 0.5).ceil().max(0.0) as usize).min(self.nx - 1);
            a..=b
        };
        for k in 0..poly.len() {
            let (p, q) = (poly[k], poly[(k + 1) % poly.len()]);
            let (lo, hi) = (p.y.min(q.y) as f64 - d, p.y.max(q.y) as f64 + d);
            let j0 = ((lo - self.y0 as f64) / self.h as f64 - 0.5).floor().max(0.0) as usize;
            let j1 = (((hi - self.y0 as f64) / self.h as f64 - 0.5).ceil().max(0.0) as usize).min(self.ny - 1);
            let (dx, dy) = ((q.x - p.x) as f64, (q.y - p.y) as f64);
            for j in j0..=j1 {
                let y = self.centre(0, j).y as f64;
                // The edge's x over the rows within `depth`, widened by `depth` and a little more.
                let (x0, x1) = if dy.abs() > 1.0 {
                    let t0 = ((y - d - p.y as f64) / dy).clamp(0.0, 1.0);
                    let t1 = ((y + d - p.y as f64) / dy).clamp(0.0, 1.0);
                    let (a, b) = (p.x as f64 + t0 * dx, p.x as f64 + t1 * dx);
                    (a.min(b), a.max(b))
                } else {
                    ((p.x.min(q.x)) as f64, (p.x.max(q.x)) as f64)
                };
                for i in cols(x0 - d - 2.0, x1 + d + 2.0) {
                    let c = j * self.nx + i;
                    if !shallow[c] && point_segment(self.centre(i, j), p, q) < d {
                        shallow[c] = true;
                    }
                }
            }
        }
        for c in inside {
            if !shallow[c] {
                f(c);
            }
        }
    }

    /// Applies the region rules that hold for `net` to its view.
    pub(crate) fn apply_regions(&self, net: u32, grid: &mut RouteGrid) {
        let cells = self.nx * self.ny;
        for raster in &self.regions {
            if raster.nets.as_ref().is_some_and(|n| !n.contains(&net)) || raster.except == Some(net) {
                continue;
            }
            let on = |l: usize| raster.layers & (1 << l) != 0;
            match (raster.kind, raster.vias) {
                (RegionKind::Never, false) => {
                    for l in (0..self.layers).filter(|l| on(*l)) {
                        for &c in &raster.near {
                            grid.blocked[l * cells + c as usize] = true;
                        }
                    }
                }
                (RegionKind::Never, true) => {
                    for &c in &raster.near {
                        grid.via_blocked[c as usize] = true;
                    }
                }
                (RegionKind::Must, vias) => {
                    let mut keep = vec![false; cells];
                    for &c in &raster.inside {
                        keep[c as usize] = true;
                    }
                    if vias {
                        for (c, k) in keep.iter().enumerate() {
                            grid.via_blocked[c] |= !k;
                        }
                    } else {
                        for l in (0..self.layers).filter(|l| on(*l)) {
                            for (c, k) in keep.iter().enumerate() {
                                grid.blocked[l * cells + c] |= !k;
                            }
                        }
                    }
                }
                (RegionKind::Avoid(_) | RegionKind::Prefer(_), vias) => {
                    // The cells charged: the region's own for avoid, every other for prefer.
                    let charged: Vec<usize> = if matches!(raster.kind, RegionKind::Avoid(_)) {
                        raster.near.iter().map(|&c| c as usize).collect()
                    } else {
                        let mut out = vec![true; cells];
                        for &c in &raster.near {
                            out[c as usize] = false;
                        }
                        (0..cells).filter(|c| out[*c]).collect()
                    };
                    if vias {
                        if grid.via_penalty.len() != cells {
                            grid.via_penalty = vec![0; cells];
                        }
                        for &c in &charged {
                            grid.via_penalty[c] = grid.via_penalty[c].saturating_add(raster.cost);
                        }
                    } else {
                        if grid.region_cost.len() != self.layers * cells {
                            grid.region_cost = vec![0; self.layers * cells];
                        }
                        for l in (0..self.layers).filter(|l| on(*l)) {
                            for &c in &charged {
                                let n = l * cells + c;
                                grid.region_cost[n] = grid.region_cost[n].saturating_add(raster.cost);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use board::{Point, Region, RegionRule, Shape};
    use crate::grid::Profile;

    fn board() -> Board {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240108) (generator \"pcbnew\") (general (thickness 1.6))
             (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))
             (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
             (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1)))",
        )
        .unwrap();
        Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap())
    }

    fn square() -> Shape {
        let p = |x: i64, y: i64| Point::new(x * 1_000_000, y * 1_000_000);
        Shape::Polygon(vec![p(8, 8), p(12, 8), p(12, 12), p(8, 12)])
    }

    #[test]
    fn rules_hold_for_their_nets_on_their_layers() {
        let mut b = board();
        b.regions = vec![Region {
            name: "keep".into(),
            shape: square(),
            layers: 1,
            rules: vec![
                RegionRule { nets: Some([1].into()), kind: RegionKind::Never, vias: false },
                RegionRule { nets: Some([2].into()), kind: RegionKind::Must, vias: false },
                RegionRule { nets: None, kind: RegionKind::Avoid(2.0), vias: true },
            ],
        }];
        let map = ObstacleMap::new(&b, Profile::for_net(&b, 1), 250_000, false);
        let a = map.view(1);
        let centre = a.cell_at(Point::new(10_000_000, 10_000_000)).unwrap();
        let corner = a.cell_at(Point::new(2_000_000, 2_000_000)).unwrap();
        assert!(a.blocked[a.index(0, centre.0, centre.1)], "A may not run inside on F.Cu");
        assert!(!a.blocked[a.index(1, centre.0, centre.1)], "but may on B.Cu");
        assert!(!a.blocked[a.index(0, corner.0, corner.1)]);
        let bnet = map.view(2);
        assert!(!bnet.blocked[bnet.index(0, centre.0, centre.1)], "B runs inside");
        assert!(bnet.blocked[bnet.index(0, corner.0, corner.1)], "and nowhere else on F.Cu");
        assert!(!bnet.blocked[bnet.index(1, corner.0, corner.1)], "B.Cu is not the region's layer");
        let at = |g: &RouteGrid, (i, j): (usize, usize)| g.via_penalty_at(j * g.nx + i);
        assert!(at(&a, centre) > 0 && at(&a, corner) == 0, "every net's vias pay inside");
    }

    /// The polygons the rasters are checked on, in millimetres: a square, a concave comb, a
    /// fractured ring (a hole joined to the outside by a slit, as KiCad's fills are), a slanted
    /// thin sliver, and a ragged 700-vertex blob like a ground fill's outline, part off the board.
    fn polygons() -> Vec<Shape> {
        let mm = |pts: &[(f64, f64)]| Shape::Polygon(pts.iter().map(|&(x, y)| Point::new((x * 1e6) as i64, (y * 1e6) as i64)).collect());
        let mut blob = Vec::new();
        for k in 0..700 {
            let t = k as f64 / 700.0 * std::f64::consts::TAU;
            let r = 7.0 + 1.3 * (7.0 * t).sin() + 0.4 * (53.0 * t).cos() + 0.15 * ((k * 7919) % 13) as f64 / 13.0;
            blob.push((11.0 + r * t.cos(), 10.0 + r * t.sin()));
        }
        vec![
            mm(&[(8.0, 8.0), (12.0, 8.0), (12.0, 12.0), (8.0, 12.0)]),
            mm(&[(2.0, 2.0), (18.0, 2.0), (18.0, 9.0), (15.0, 9.0), (15.0, 4.0), (12.0, 4.0), (12.0, 9.0), (9.0, 9.0), (9.0, 4.3), (6.0, 4.3), (6.0, 9.0), (2.0, 9.0)]),
            mm(&[(3.0, 11.0), (17.0, 11.0), (17.0, 19.0), (3.0, 19.0), (3.0, 15.0), (7.0, 15.0), (7.0, 17.0), (13.0, 17.0), (13.0, 13.0), (7.0, 13.0), (7.0, 15.0), (3.0, 15.0)]),
            mm(&[(1.0, 1.0), (19.3, 17.1), (18.9, 17.6), (1.0, 1.8)]),
            mm(&blob),
        ]
    }

    /// `cells_deep` is `Shape::depth` asked of every cell inside, cell for cell, on maps whose
    /// cells do not line up with the millimetre, and on a window.
    #[test]
    fn deep_cells_are_those_whose_depth_reaches() {
        let b = board();
        let window = board::BBox { min: Point::new(4_300_000, 3_100_000), max: Point::new(13_700_000, 15_900_000) };
        let maps = [
            ObstacleMap::new(&b, Profile::for_net(&b, 1), 97_000, false),
            ObstacleMap::new(&b, Profile::for_net(&b, 1), 35_000, false),
            ObstacleMap::window(&b, Profile::for_net(&b, 1), 41_000, window, false),
        ];
        let mut cases = 0;
        for map in &maps {
            for shape in polygons() {
                for depth in [1, 35_000, 130_000, 260_001, 1_000_000] {
                    let mut old = Vec::new();
                    map.cells_near(&shape, 0, &mut |c| {
                        if shape.depth(map.centre(c % map.nx, c / map.nx)) >= depth as f64 {
                            old.push(c);
                        }
                    });
                    let mut new = Vec::new();
                    map.cells_deep(&shape, depth, &mut |c| new.push(c));
                    assert_eq!(old, new, "cell {} depth {depth}", map.h);
                    cases += !old.is_empty() as usize;
                }
            }
        }
        assert!(cases >= 40, "{cases}");
    }
}
