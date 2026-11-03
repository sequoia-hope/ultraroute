//! Routing regions: areas of the board with rules for which nets may run there, as a planner
//! (circuit-composer's constraint compiler) hands them to the router beside the board. A rule
//! names the nets it holds for and says `never` (keep out), `must` (stay within, on the region's
//! layers), `avoid` or `prefer` (a cost per length inside, or outside), for tracks or for vias.
//! The board holds them resolved to net numbers; the grid applies them to each net's view.

use std::collections::HashSet;

use crate::geom::Shape;
use crate::model::{Board, Item, Kind};

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

#[cfg(test)]
mod tests {
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
