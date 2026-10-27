//! Routing regions on the grid: each rule of each of the board's regions as the cells it
//! touches, worked out once per obstacle map, and applied to the view of every net it holds for.
//! `never` blocks the cells a track (or via) there would reach into the region; `must` blocks
//! every cell but those deep enough inside it; `avoid` and `prefer` price cells inside or
//! outside it. Costs go in `RouteGrid::region_cost` (tracks) and `via_penalty` (vias), which
//! the CPU searches add to every step; the GPU and the browser's worker pool do not see them yet.

use board::{Board, RegionKind};

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
                let mut inside = Vec::new();
                self.cells_near(&region.shape, 0, &mut |c| {
                    let (i, j) = (c % self.nx, c / self.nx);
                    if region.shape.depth(self.centre(i, j)) >= reach as f64 {
                        inside.push(c as u32);
                    }
                });
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
}
