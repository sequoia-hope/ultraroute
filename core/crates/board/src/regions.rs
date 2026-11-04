//! Routing regions: areas of the board with rules for which nets may run there, as a planner
//! (circuit-composer's constraint compiler) hands them to the router beside the board. A rule
//! names the nets it holds for and says `never` (keep out), `must` (stay within, on the region's
//! layers), `avoid` or `prefer` (a cost per length inside, or outside), for tracks or for vias.
//! The board holds them resolved to net numbers; the grid applies them to each net's view.

use std::collections::HashSet;

use crate::geom::{Nm, Point, Shape};
use crate::model::{Board, Item, ItemId, Kind};

#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    /// The planner's name for it, said wherever a rule of it blocks or prices copper.
    pub name: String,
    pub shape: Shape,
    /// The copper layers it holds on, one bit per index into `Board::layers`.
    pub layers: u64,
    pub rules: Vec<RegionRule>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegionRule {
    /// The nets it holds for; every net when `None`.
    pub nets: Option<HashSet<u32>>,
    pub kind: RegionKind,
    /// A rule for vias rather than tracks.
    pub vias: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RegionKind {
    /// No copper of the nets in the region on its layers.
    Never,
    /// The nets' copper on the region's layers stays within it.
    Must,
    /// A track pays `weight` times its length inside the region on top of the length itself; a
    /// via there pays `weight` millimetres of track.
    Avoid(f64),
    /// As `Avoid`, outside the region.
    Prefer(f64),
}

impl RegionRule {
    pub fn holds_for(&self, net: u32) -> bool {
        self.nets.as_ref().is_none_or(|n| n.contains(&net))
    }
}

/// How far copper may reach over a region's edge and still count as outside it (or within it),
/// as KiCad's geometry allows, in nanometres.
pub const TOLERANCE: f64 = 500.0;

impl Region {
    /// Whether `shape` reaches into the region: a capsule when its centre line comes nearer
    /// than its radius by more than `TOLERANCE` (touching the edge is outside), a polygon when
    /// it meets it at all.
    pub fn touches(&self, shape: &Shape) -> bool {
        match shape {
            Shape::Capsule { a, b, r } => Shape::Capsule { a: *a, b: *b, r: 0 }.gap_within(&self.shape, *r as f64) + TOLERANCE < *r as f64,
            Shape::Polygon(_) => shape.gap_within(&self.shape, 0.0) == 0.0,
        }
    }

    /// Whether `shape` lies wholly inside the region: a capsule whose two ends lie its radius
    /// deep, less `TOLERANCE`. A polygon never does.
    pub fn holds(&self, shape: &Shape) -> bool {
        match shape {
            Shape::Capsule { a, b, r } => self.shape.depth(*a) + TOLERANCE >= *r as f64 && self.shape.depth(*b) + TOLERANCE >= *r as f64,
            Shape::Polygon(_) => false,
        }
    }

    /// Whether `rule` (one of the region's) holds for `item`: for its net, its kind (a via or
    /// not), on one of its layers.
    pub fn applies(&self, rule: &RegionRule, item: &Item) -> bool {
        item.layers & self.layers != 0 && rule.vias == (item.kind == Kind::Via) && rule.holds_for(item.net)
    }

    fn rules_for<'a>(&'a self, item: &'a Item) -> impl Iterator<Item = &'a RegionRule> + 'a {
        self.rules.iter().filter(move |r| self.applies(r, item))
    }

    /// Whether `rule` (one of the region's) holds for `item` and it breaks it: a `never` net's
    /// copper in the region, or a `must` net's outside it. What `--regions` reports and length
    /// tuning refuses.
    pub fn breaks(&self, rule: &RegionRule, item: &Item) -> bool {
        self.applies(rule, item)
            && match rule.kind {
                RegionKind::Never => self.touches(&item.shape),
                RegionKind::Must => !self.holds(&item.shape),
                _ => false,
            }
    }

    /// What `item` pays under the region's soft rules: the weight of each `avoid` rule whose
    /// region it reaches into and each `prefer` rule whose region it strays out of.
    pub fn price(&self, item: &Item) -> f64 {
        self.rules_for(item)
            .map(|r| match r.kind {
                RegionKind::Avoid(w) if self.touches(&item.shape) => w,
                RegionKind::Prefer(w) if !self.holds(&item.shape) => w,
                _ => 0.0,
            })
            .sum()
    }
}

/// The first of the board's regions with a hard rule `item` breaks.
pub fn broken_region<'a>(board: &'a Board, item: &Item) -> Option<&'a Region> {
    board.regions.iter().find(|r| r.rules.iter().any(|rule| r.breaks(rule, item)))
}

/// What `item` pays under every region's soft rules (`Region::price`).
pub fn region_price(board: &Board, item: &Item) -> f64 {
    board.regions.iter().map(|r| r.price(item)).sum()
}

/// A rule that keeps a ground-referenced net's tracks clear of the holes other nets' vias and
/// plated holes cut in a plane: no via or plated hole of a net outside `nets` may have its
/// antipad on a layer of `planes` (see [`Board::antipads`]) within `margin` of a track of `nets`
/// on `layers`. It holds both ways, whichever of the two is laid first.
#[derive(Debug, Clone, PartialEq)]
pub struct ClearUnder {
    pub nets: HashSet<u32>,
    /// The tracks' layers, one bit per index into `Board::layers`.
    pub layers: u64,
    /// The planes under them.
    pub planes: u64,
    pub margin: Nm,
}

/// A copper zone (not a rule area) as its refill sees it, for working out what other nets' holes
/// cut out of it.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneArea {
    pub net: u32,
    pub layers: u64,
    pub outline: Shape,
    pub cutouts: Vec<Shape>,
    /// The zone's own clearance (`connect_pads (clearance …)`), 0 when unset.
    pub clearance: Nm,
    /// Its minimum width: copper narrower is dropped on a refill.
    pub min_thickness: Nm,
}

impl ZoneArea {
    pub fn covers(&self, p: Point) -> bool {
        self.outline.contains(p) && !self.cutouts.iter().any(|c| c.contains(p))
    }
}

/// The hole a via or plated hole cuts in a zone of another net on one layer.
#[derive(Debug, Clone, PartialEq)]
pub struct Antipad {
    pub layer: usize,
    pub shape: Shape,
    /// The zone's net.
    pub net: u32,
    /// Half the zone's minimum width: a track narrower than that does not keep the plane under
    /// it whole on its own, as the refill drops a sliver that thin beside the hole.
    pub thin: Nm,
}

/// Shortfalls this small are not breaks, as the checks' slack: KiCad's 0.5 um.
const SLACK: Nm = 500;

/// How far past its true circle KiCad's refill cuts an antipad: it draws the arc as chords
/// outside it, to its maximum error (`max_error`, 5 um by default). A track 2 um clear of the
/// circle was 0.00001 mm2 over the refilled hole (rp2350-battery, seed 1, 2026-11-02).
pub const ARC_ERROR: Nm = 5_000;

impl Board {
    /// The holes `item` (a via or a plated hole) cuts on the layers in `planes` in the zones of
    /// other nets that cover its centre, as KiCad's refill cuts them: its copper on the layer
    /// grown by the zone's clearance to it (the zone's own or the nets', the wider), and its
    /// drill grown by that or the hole clearance, the wider, and both by the refill's arc error.
    /// None where no zone of another net is there.
    pub fn antipads(&self, item: &Item, planes: u64) -> Vec<Antipad> {
        let mut out = Vec::new();
        let Some((centre, hole)) = item.hole else { return out };
        if !item.plated_hole() {
            return out;
        }
        let hole_gap = crate::geom::nm(self.rules.min_hole_clearance);
        for zone in self.zone_areas.iter().filter(|z| z.layers & planes & item.layers != 0 && z.net != item.net && z.net != 0 && z.covers(centre)) {
            let c = zone.clearance.max(self.clearance(zone.net, item.net));
            let drill = hole + hole_gap.max(c) + ARC_ERROR;
            let c = c + ARC_ERROR;
            let thin = zone.min_thickness / 2;
            for layer in (0..self.layers.len()).filter(|l| zone.layers & planes & item.layers & (1 << l) != 0) {
                match &item.shape {
                    Shape::Capsule { a, b, r } if a == b && *a == centre => {
                        out.push(Antipad { layer, shape: Shape::Capsule { a: *a, b: *a, r: (r + c).max(drill) }, net: zone.net, thin });
                    }
                    shape => {
                        out.push(Antipad { layer, shape: crate::model::grown(shape, c), net: zone.net, thin });
                        out.push(Antipad { layer, shape: Shape::Capsule { a: centre, b: centre, r: drill }, net: zone.net, thin });
                    }
                }
            }
        }
        out
    }

    /// How far an antipad and a zone's thinnest copper reach beyond a hole's copper, at most,
    /// less the clearances the checks already look as far as: for finding the holes near a track.
    fn antipad_reach(&self) -> Nm {
        self.zone_areas.iter().map(|z| z.clearance + z.min_thickness).max().unwrap_or(0) + crate::geom::nm(self.rules.min_hole_clearance) + ARC_ERROR
    }

    /// What breaks the clear-under rules with `id`: each the rule's index, the other item, the
    /// plane and the gap between the track and the antipad. A track of a rule's nets is judged
    /// against the holes near it; with `both`, a via or plated hole against the tracks near it
    /// too (the whole-board check takes each pair once, from the track).
    pub fn clear_under_breaks(&self, id: ItemId, both: bool) -> Vec<(usize, ItemId, usize, f64)> {
        let mut out = Vec::new();
        let Some(item) = self.item(id) else { return out };
        let bbox = self.bbox(id).unwrap_or_else(|| item.shape.bbox());
        let on = |mask: u64| (0..self.layers.len()).filter(move |l| mask & (1 << l) != 0);
        for (k, rule) in self.clear_under.iter().enumerate() {
            if item.kind == Kind::Track && rule.nets.contains(&item.net) && item.layers & rule.layers != 0 {
                let reach = bbox.grow(rule.margin + self.antipad_reach() + self.limits.max_clearance);
                let mut near: Vec<ItemId> = on(rule.planes).flat_map(|l| self.near(l, reach)).collect();
                near.sort_unstable();
                near.dedup();
                for other in near {
                    let Some(o) = self.item(other) else { continue };
                    if rule.nets.contains(&o.net) || !o.plated_hole() {
                        continue;
                    }
                    for pad in self.antipads(o, rule.planes) {
                        if let Some(gap) = breaks(item, &pad, rule.margin) {
                            out.push((k, other, pad.layer, gap));
                        }
                    }
                }
            } else if both && item.plated_hole() && !rule.nets.contains(&item.net) {
                for pad in self.antipads(item, rule.planes) {
                    let reach = pad.shape.bbox().grow(rule.margin + pad.thin + self.limits.max_clearance);
                    let mut near: Vec<ItemId> = on(rule.layers).flat_map(|l| self.near(l, reach)).collect();
                    near.sort_unstable();
                    near.dedup();
                    for other in near {
                        let Some(t) = self.item(other) else { continue };
                        if t.kind != Kind::Track || !rule.nets.contains(&t.net) || t.layers & rule.layers == 0 {
                            continue;
                        }
                        if let Some(gap) = breaks(t, &pad, rule.margin) {
                            out.push((k, other, pad.layer, gap));
                        }
                    }
                }
            }
        }
        out
    }
}

/// Whether `track` comes within `margin` of `pad` by more than the slack, taken as wide as the
/// zone's minimum width where narrower (so the plane under it is wide enough to keep): the gap
/// between them if so.
fn breaks(track: &Item, pad: &Antipad, margin: Nm) -> Option<f64> {
    let Shape::Capsule { a, b, r } = track.shape else { return None };
    let wide = Shape::Capsule { a, b, r: r.max(pad.thin) + margin - SLACK };
    (wide.gap_within(&pad.shape, 1.0) == 0.0).then(|| track.shape.gap(&pad.shape))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::{check, check_item, Rule};

    /// A four-layer board's GND plane on In1.Cu with a 0.3 mm clearance and 0.2 mm minimum
    /// width; USB's track on F.Cu along y = 10 mm, 0.2 mm wide.
    fn board() -> (Board, ItemId) {
        let (pcb, copper) = kicad_pcb::parse_board(
            r#"(kicad_pcb (version 20240101) (general (thickness 1.6))
            (layers (0 "F.Cu" signal) (1 "In1.Cu" signal) (2 "In2.Cu" signal) (31 "B.Cu" signal))
            (net 0 "") (net 1 "GND") (net 2 "USB") (net 3 "SIG")
            (gr_rect (start 0 0) (end 20 20) (layer "Edge.Cuts") (width 0.1))
            (zone (net 1) (net_name "GND") (layer "In1.Cu") (uuid "z") (connect_pads (clearance 0.3))
              (min_thickness 0.2) (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20)))))"#,
        )
        .unwrap();
        let mut b = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let p = |x: i64, y: i64| Point::new(x * 1_000, y * 1_000);
        let track = b.insert(Item { kind: Kind::Track, net: 2, layers: 1, shape: Shape::Capsule { a: p(2_000, 10_000), b: p(18_000, 10_000), r: 100_000 }, hole: None, locked: false, span: None });
        b.clear_under = vec![ClearUnder { nets: [2].into(), layers: 1, planes: 2, margin: 0 }];
        (b, track)
    }

    /// A through via 0.6 mm across, drilled 0.3 mm, at (10, `y`) mm.
    fn via(b: &mut Board, net: u32, y: i64) -> ItemId {
        let c = Point::new(10_000_000, y * 1_000);
        b.insert(Item { kind: Kind::Via, net, layers: 0b1111, shape: Shape::Capsule { a: c, b: c, r: 300_000 }, hole: Some((c, 150_000)), locked: false, span: None })
    }

    fn breaks(b: &Board, id: ItemId) -> Vec<Option<ItemId>> {
        check_item(b, id).into_iter().filter(|v| v.rule == Rule::Keepout).map(|v| v.b).collect()
    }

    #[test]
    fn another_nets_antipad_may_not_come_under_the_track_either_way() {
        // The antipad reaches 0.3 + 0.3 mm from the via's centre, the track 0.1 mm from its own:
        // 0.65 mm apart they overlap, 0.75 mm apart they do not.
        let (mut b, track) = board();
        let near = via(&mut b, 3, 10_650);
        assert_eq!(breaks(&b, near), vec![Some(track)], "the via's antipad comes under USB");
        assert_eq!(breaks(&b, track), vec![Some(near)], "and USB over it");
        assert_eq!(check(&b).iter().filter(|v| v.rule == Rule::Keepout).count(), 1, "the whole board's check says it once");
        b.remove(near);
        let far = via(&mut b, 3, 10_750);
        assert!(breaks(&b, far).is_empty() && breaks(&b, track).is_empty());
        // A margin keeps the antipad that much further off.
        b.clear_under[0].margin = 200_000;
        assert_eq!(breaks(&b, far), vec![Some(track)]);
    }

    #[test]
    fn no_antipad_where_the_via_is_the_planes_net_or_no_zone_is_there() {
        let (mut b, track) = board();
        let ground = via(&mut b, 1, 10_400);
        assert!(breaks(&b, ground).is_empty(), "a GND via joins the plane, it cuts no hole");
        let own = via(&mut b, 2, 10_000);
        assert!(breaks(&b, own).is_empty(), "nor is the rule's own net's via held to it");
        b.remove(ground);
        b.remove(own);
        let sig = via(&mut b, 3, 10_400);
        b.clear_under[0].planes = 0b100;
        assert!(breaks(&b, sig).is_empty() && breaks(&b, track).is_empty(), "In2.Cu has no zone, so no antipad");
    }

    #[test]
    fn a_track_thinner_than_the_planes_minimum_width_is_taken_as_wide() {
        // A 0.1 mm track keeps 0.1 mm of plane under it only if the refill keeps a 0.2 mm sliver.
        let (mut b, track) = board();
        if let Some(mut t) = b.remove(track) {
            t.shape = Shape::Capsule { a: Point::new(2_000_000, 10_000_000), b: Point::new(18_000_000, 10_000_000), r: 50_000 };
            b.restore(track, t);
        }
        let v = via(&mut b, 3, 10_680);
        assert_eq!(breaks(&b, v), vec![Some(track)], "0.03 mm of plane between the hole and the track's edge is dropped");
    }
}

#[cfg(test)]
mod rule_tests {
    use super::*;
    use crate::check::{check, check_item, Rule};
    use crate::geom::Point;

    fn board() -> Board {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal)) (net 0 \"\") (net 1 \"A\") (net 2 \"B\"))",
        )
        .unwrap();
        let mut b = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let p = |x: i64, y: i64| Point::new(x * 1_000_000, y * 1_000_000);
        let rule = |net: u32, kind, vias| RegionRule { nets: Some([net].into()), kind, vias };
        b.regions = vec![
            Region { name: "keep".into(), shape: Shape::Polygon(vec![p(0, 0), p(4, 0), p(4, 4), p(0, 4)]), layers: 1, rules: vec![rule(1, RegionKind::Never, false), rule(2, RegionKind::Never, true)] },
            Region { name: "lane".into(), shape: Shape::Polygon(vec![p(10, 0), p(20, 0), p(20, 1), p(10, 1)]), layers: 1, rules: vec![rule(2, RegionKind::Must, false), rule(1, RegionKind::Avoid(2.0), false)] },
        ];
        b
    }

    fn track(net: u32, a: (i64, i64), b: (i64, i64), layers: u64) -> Item {
        let p = |(x, y): (i64, i64)| Point::new(x, y);
        Item { kind: Kind::Track, net, layers, shape: Shape::Capsule { a: p(a), b: p(b), r: 100_000 }, hole: None, locked: false, span: None }
    }

    /// The router's checks refuse what breaks a region's hard rule, for the rule's nets, kind and
    /// layers only, as `breaks` says; the whole-board check, KiCad's rules alone, does not.
    #[test]
    fn the_checker_refuses_what_breaks_a_hard_rule() {
        let mut b = board();
        let mut refused = |it: Item| {
            let id = b.insert(it);
            let r = check_item(&b, id).iter().any(|v| v.rule == Rule::Region);
            assert_eq!(r, broken_region(&b, b.item(id).unwrap()).is_some());
            assert!(check(&b).iter().all(|v| v.rule != Rule::Region));
            b.remove(id);
            r
        };
        assert!(refused(track(1, (-1_000_000, 2_000_000), (5_000_000, 2_000_000), 1)), "A crosses its never region");
        assert!(!refused(track(1, (-1_000_000, 2_000_000), (5_000_000, 2_000_000), 2)), "on B.Cu, not the region's layer");
        assert!(!refused(track(2, (-1_000_000, 2_000_000), (5_000_000, 2_000_000), 2)), "B's rule there is for vias, and its lane is on F.Cu");
        assert!(!refused(track(1, (-1_000_000, 4_100_000), (5_000_000, 4_100_000), 1)), "touching the edge is outside");
        assert!(refused(track(1, (-1_000_000, 4_099_000), (5_000_000, 4_099_000), 1)), "a micron in is in");
        let via = Item { kind: Kind::Via, net: 2, layers: 3, shape: Shape::Capsule { a: Point::new(2_000_000, 2_000_000), b: Point::new(2_000_000, 2_000_000), r: 300_000 }, hole: None, locked: false, span: None };
        assert!(refused(via), "B's vias keep out");
        assert!(!refused(track(2, (11_000_000, 500_000), (19_000_000, 500_000), 1)), "B within its lane");
        assert!(!refused(track(2, (11_000_000, 100_000), (19_000_000, 100_000), 1)), "its edge on the lane's");
        assert!(refused(track(2, (11_000_000, 500_000), (21_000_000, 500_000), 1)), "B out of its lane");
        assert!(!refused(track(1, (11_000_000, 500_000), (19_000_000, 500_000), 1)), "A only pays there");
    }

    #[test]
    fn soft_rules_price_what_they_hold_for() {
        let b = board();
        assert_eq!(region_price(&b, &track(1, (11_000_000, 500_000), (19_000_000, 500_000), 1)), 2.0);
        assert_eq!(region_price(&b, &track(1, (11_000_000, 3_000_000), (19_000_000, 3_000_000), 1)), 0.0);
        assert_eq!(region_price(&b, &track(2, (11_000_000, 500_000), (19_000_000, 500_000), 1)), 0.0);
    }
}
