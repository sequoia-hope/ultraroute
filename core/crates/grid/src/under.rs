//! Clear-under rules on the grid (see [`board::ClearUnder`]): per rule and obstacle map, the
//! cells where a track of the rule's nets would come over an antipad another net's via or plated
//! hole cuts in a plane, and the cells where a new via would cut one under such a track. Both
//! follow the copper as it is stamped, so whichever is laid first keeps the other off: a via
//! placed before a rule's net is routed walls it off, and the net's track laid first keeps later
//! vias away. The checker holds the rule exactly ([`board::Board::clear_under_breaks`]); these
//! are what make the searches go round.

use std::collections::HashSet;

use board::{Board, ItemId, Kind, Nm};

use crate::grid::{ObstacleMap, RouteGrid};

/// One clear-under rule on one map's cells.
#[derive(Debug, Clone)]
pub(crate) struct UnderRaster {
    rule: usize,
    nets: HashSet<u32>,
    layers: u64,
    /// Per cell: the net of the zone on one of the rule's planes that a via here would cut; 0
    /// where none is.
    plane: Vec<u32>,
    /// Per cell: a track of the rule's nets here would come over an antipad.
    tracks: Vec<bool>,
    /// Per cell: a via here, of a net neither the rule's nor the plane's, would cut an antipad
    /// under a track of the rule's nets.
    vias: Vec<bool>,
    /// How far from a track's centre line a via of this map's class may not stand: its antipad's
    /// radius, the margin, and half the zones' thinnest copper where wider than the track.
    via_keep: Nm,
    /// Half the zones' thinnest copper.
    thin: Nm,
}

impl ObstacleMap {
    /// The board's clear-under rules as empty rasters on this map's cells, with the planes'
    /// zones filled in; [`ObstacleMap::stamp_under`] marks them.
    pub(crate) fn under_rasters(&self, board: &Board) -> Vec<UnderRaster> {
        let cells = self.nx * self.ny;
        let p = &self.profile;
        let hole_gap = board::nm(board.rules.min_hole_clearance);
        let mut out = Vec::new();
        for (k, rule) in board.clear_under.iter().enumerate() {
            let mut plane = vec![0u32; cells];
            let (mut reach, mut thin) = (0, 0);
            for zone in board.zone_areas.iter().filter(|z| z.layers & rule.planes != 0 && z.net != 0) {
                self.cells_near(&zone.outline, 0, &mut |c| {
                    if plane[c] == 0 {
                        plane[c] = zone.net;
                    }
                });
                let c = zone.clearance.max(self.clearance_to(board, zone.net));
                reach = reach.max(p.via_radius + c).max(p.drill_radius + hole_gap.max(c));
                thin = thin.max(zone.min_thickness / 2);
            }
            out.push(UnderRaster {
                rule: k,
                nets: rule.nets.clone(),
                layers: rule.layers,
                plane,
                tracks: vec![false; cells],
                vias: vec![false; cells],
                via_keep: reach + board::regions::ARC_ERROR + rule.margin,
                thin,
            });
        }
        out
    }

    /// Marks what `id` keeps off under the clear-under rules: a hole's antipads wall off the
    /// rules' nets' tracks, a rule's net's track walls off other nets' vias.
    pub(crate) fn stamp_under(&mut self, board: &Board, id: ItemId) {
        if self.under.is_empty() {
            return;
        }
        let Some(item) = board.item(id) else { return };
        let (half_width, step) = (self.profile.half_width, self.reach());
        let mut under = std::mem::take(&mut self.under);
        for raster in &mut under {
            let rule = &board.clear_under[raster.rule];
            if item.kind == Kind::Track && rule.nets.contains(&item.net) && item.layers & rule.layers != 0 {
                let board::Shape::Capsule { a, b, r } = item.shape else { continue };
                let wide = board::Shape::Capsule { a, b, r: r.max(raster.thin) };
                self.cells_near(&wide, raster.via_keep, &mut |c| raster.vias[c] = true);
            } else if item.plated_hole() && !rule.nets.contains(&item.net) {
                for pad in board.antipads(item, rule.planes) {
                    let keep = half_width.max(pad.thin) + step + rule.margin;
                    self.cells_near(&pad.shape, keep, &mut |c| raster.tracks[c] = true);
                }
            }
        }
        self.under = under;
    }

    /// Applies the clear-under rules to `net`'s view: its tracks off other nets' antipads on
    /// the rules' layers if it is a rule's net, else its vias out from under the rules' nets'
    /// tracks wherever they would cut a plane of another net.
    pub(crate) fn apply_under(&self, net: u32, grid: &mut RouteGrid) {
        let cells = self.nx * self.ny;
        for raster in &self.under {
            if raster.nets.contains(&net) {
                for l in (0..self.layers).filter(|l| raster.layers & (1 << l) != 0) {
                    for (c, &t) in raster.tracks.iter().enumerate() {
                        grid.blocked[l * cells + c] |= t;
                    }
                }
            } else {
                for (c, &v) in raster.vias.iter().enumerate() {
                    grid.via_blocked[c] |= v && raster.plane[c] != 0 && raster.plane[c] != net;
                }
            }
        }
    }

    /// This map's clear-under rasters cut to the cells from column `i0`, row `j0`, `nx` by `ny`:
    /// for a sub-window of it.
    pub(crate) fn under_window(&self, i0: usize, j0: usize, nx: usize, ny: usize) -> Vec<UnderRaster> {
        let cut = |v: &[bool]| (j0..j0 + ny).flat_map(|j| v[j * self.nx + i0..j * self.nx + i0 + nx].iter().copied()).collect::<Vec<bool>>();
        self.under
            .iter()
            .map(|r| UnderRaster {
                rule: r.rule,
                nets: r.nets.clone(),
                layers: r.layers,
                plane: (j0..j0 + ny).flat_map(|j| r.plane[j * self.nx + i0..j * self.nx + i0 + nx].iter().copied()).collect(),
                tracks: cut(&r.tracks),
                vias: cut(&r.vias),
                via_keep: r.via_keep,
                thin: r.thin,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use board::{ClearUnder, Item, Point, Shape};

    use super::*;
    use crate::grid::Profile;

    /// A GND plane on In1.Cu over the whole board; a rule for net USB's tracks on F.Cu over it.
    fn board() -> Board {
        let (pcb, copper) = kicad_pcb::parse_board(
            r#"(kicad_pcb (version 20240101) (general (thickness 1.6))
            (layers (0 "F.Cu" signal) (1 "In1.Cu" signal) (31 "B.Cu" signal))
            (net 0 "") (net 1 "GND") (net 2 "USB") (net 3 "SIG")
            (gr_rect (start 0 0) (end 20 20) (layer "Edge.Cuts") (width 0.1))
            (zone (net 1) (net_name "GND") (layer "In1.Cu") (uuid "z") (connect_pads (clearance 0.3))
              (min_thickness 0.2) (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20)))))"#,
        )
        .unwrap();
        let mut b = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        b.clear_under = vec![ClearUnder { nets: [2].into(), layers: 1, planes: 2, margin: 0 }];
        b
    }

    fn profile(b: &Board) -> Profile {
        Profile { half_width: 100_000, via_radius: 300_000, drill_radius: 150_000, clearance: 200_000, ..Profile::for_net(b, 3) }
    }

    fn mm(x: f64, y: f64) -> Point {
        Point::new((x * 1e6) as i64, (y * 1e6) as i64)
    }

    #[test]
    fn a_via_walls_the_rules_tracks_off_its_antipad_and_a_track_walls_other_nets_vias_off() {
        let mut b = board();
        let mut map = ObstacleMap::new(&b, profile(&b), 50_000, false);
        let at = |g: &RouteGrid, p: Point| g.cell_at(p).map(|(i, j)| j * g.nx + i).unwrap();
        // A SIG via stamped after the map was built: USB's track may not pass within 0.6 mm (its
        // antipad) and 0.1 mm (its own half width) of the via's centre on F.Cu, SIG's may.
        let c = mm(10.0, 10.0);
        let v = b.insert(Item { kind: Kind::Via, net: 3, layers: 0b111, shape: Shape::Capsule { a: c, b: c, r: 300_000 }, hole: Some((c, 150_000)), locked: false, span: None });
        map.stamp(&b, v);
        // 0.7 mm off is beyond the via's clearance (0.3 + 0.2 + 0.1 mm) but over the antipad.
        let usb = map.view(2);
        let cells = usb.cell_count();
        assert!(usb.blocked[at(&usb, mm(10.0, 10.7))], "0.7 mm off: over the antipad");
        assert!(!usb.blocked[at(&usb, mm(10.0, 11.0))], "1 mm off is clear");
        assert!(!usb.blocked[cells + at(&usb, mm(10.0, 10.7))], "In1.Cu is not the rule's layer");
        let gnd = map.view(1);
        assert!(!gnd.blocked[at(&gnd, mm(10.0, 10.7))], "other nets' tracks are not held to it");
        // USB's track along y = 5 mm: a SIG via may not stand where its 0.6 mm antipad would come
        // under it, a GND via (the plane's own net) may.
        let t = b.insert(Item { kind: Kind::Track, net: 2, layers: 1, shape: Shape::Capsule { a: mm(2.0, 5.0), b: mm(18.0, 5.0), r: 100_000 }, hole: None, locked: false, span: None });
        map.stamp(&b, t);
        let (sig, gnd, usb) = (map.view(3), map.view(1), map.view(2));
        assert!(sig.via_blocked[at(&sig, mm(10.0, 5.65))], "SIG's via 0.65 mm off would cut under USB");
        assert!(!sig.via_blocked[at(&sig, mm(10.0, 5.8))]);
        assert!(!gnd.via_blocked[at(&gnd, mm(10.0, 5.65))], "a GND via cuts no hole in GND's plane");
        assert!(!usb.via_blocked[at(&usb, mm(10.0, 5.65))]);
        // A map built with both already on the board agrees.
        let fresh = ObstacleMap::new(&b, profile(&b), 50_000, false);
        assert_eq!(fresh.view(2).blocked, map.view(2).blocked);
        assert_eq!(fresh.view(3).via_blocked, map.view(3).via_blocked);
    }

    #[test]
    fn a_window_walls_off_a_fixed_pads_antipad_as_the_whole_map_does() {
        let mut b = board();
        let c = mm(10.0, 10.0);
        let pad = Kind::Pad { footprint: 0, number: "1".into(), plated: true };
        b.insert(Item { kind: pad, net: 3, layers: 0b111, shape: Shape::Capsule { a: c, b: c, r: 300_000 }, hole: Some((c, 150_000)), locked: false, span: None });
        let whole = ObstacleMap::new(&b, profile(&b), 50_000, false);
        let bounds = board::BBox { min: mm(5.0, 5.0), max: mm(15.0, 15.0) };
        // Twice: the second window's pads come from the fixed-copper cache.
        for _ in 0..2 {
            let win = ObstacleMap::window(&b, profile(&b), 50_000, bounds, false);
            let usb = win.view(2);
            let at = usb.cell_at(mm(10.0, 10.7)).map(|(i, j)| j * usb.nx + i).unwrap();
            assert!(usb.blocked[at], "0.7 mm off: over the pad's antipad");
            assert_eq!(win.under_window(0, 0, win.nx, win.ny)[0].tracks, whole.under_window(100, 100, win.nx, win.ny)[0].tracks);
        }
    }

    #[test]
    fn a_board_without_the_rule_maps_as_before() {
        let mut b = board();
        let c = mm(10.0, 10.0);
        b.insert(Item { kind: Kind::Via, net: 3, layers: 0b111, shape: Shape::Capsule { a: c, b: c, r: 300_000 }, hole: Some((c, 150_000)), locked: false, span: None });
        let with = ObstacleMap::new(&b, profile(&b), 50_000, false);
        b.clear_under.clear();
        let without = ObstacleMap::new(&b, profile(&b), 50_000, false);
        assert!(without.under.is_empty());
        assert_eq!(with.view(3).blocked, without.view(3).blocked, "SIG sees the same either way");
        assert_ne!(with.view(2).blocked, without.view(2).blocked, "USB does not");
    }
}
