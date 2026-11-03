//! A conservative routing grid. A cell centre is free for a trace of a net only if a trace
//! centred anywhere on a step from it to a neighbouring free centre keeps its clearance to every
//! other net's copper, so any path the grid accepts is legal; it may refuse legal paths, never
//! the other way round. One obstacle map serves every net of a class: each cell records whose
//! copper claims it.

use board::{nm, Board, ItemId, Kind, Nm, Point, Shape};

use crate::clock::Deadline;

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
    /// The narrowest a leg or escape of the net may neck to near a pad: the board's minimum
    /// track, or the net's own from a custom rule; `half_width` itself for a net of a class wider
    /// than the default, which necks only as [`Profile::neck_into`] allows.
    pub narrow_half_width: Nm,
    /// The board's minimum track, or the net's own from a custom rule: no neck goes below it.
    pub min_half_width: Nm,
    /// The narrowest track the rules allow for any net, the floor of a forced neck
    /// ([`Profile::forced_necks`]): the board's minimum unless a custom rule for every net sets
    /// another.
    pub board_min_half_width: Nm,
    pub clearance: Nm,
    pub via_radius: Nm,
    pub drill_radius: Nm,
    /// The net, where the board's custom rules tell nets apart by name; else the map serves the
    /// whole class and the rules are asked about a net of it.
    pub net: Option<u32>,
}

impl Profile {
    /// How narrow a leg into `pad` may neck. A signal necks to its narrowest; a net that carries
    /// current only to the pad's own width, as a designer would draw it, where the pad is
    /// narrower than the net's track: never thinner than the copper it feeds.
    pub fn neck_into(&self, pad: Option<&board::Item>) -> Nm {
        if self.narrow_half_width < self.half_width {
            return self.narrow_half_width;
        }
        let Some(pad) = pad.filter(|p| matches!(p.kind, board::Kind::Pad { .. })) else { return self.half_width };
        // Not `clamp`: with no track width in the rules the floor is above the zero width.
        pad_half_side(pad).max(self.min_half_width).min(self.half_width)
    }

    /// Narrower legs into `pad` than [`Profile::neck_into`] allows, widest first, for a pad no
    /// leg the rules allow can leave: the pin's own width, as the current already passes through
    /// it, then the board's minimum track. They break the net's own width rule, or leave it
    /// thinner than its pin, so the run reports each one ([`crate::necks`]).
    pub fn forced_necks(&self, pad: Option<&board::Item>) -> Vec<Nm> {
        let Some(pad) = pad.filter(|p| matches!(p.kind, board::Kind::Pad { .. })) else { return Vec::new() };
        let floor = self.board_min_half_width;
        let allowed = self.neck_into(Some(pad)).min(self.half_width);
        let mut widths: Vec<Nm> = vec![pad_half_side(pad).max(floor), floor];
        widths.retain(|&w| w < allowed);
        widths.dedup();
        widths
    }

    pub fn for_net(board: &Board, net: u32) -> Profile {
        let rules = &board.rules;
        let class = rules.class_of(board.net_name(net));
        let class = class.as_ref();
        let take = |v: Option<f64>, floor: f64| nm(v.unwrap_or(floor).max(floor));
        let min_width = board.min_track_width_of(net) as f64 / 1e9;
        let half_width = take(class.map(|c| c.track_width), min_width) / 2;
        // A net whose class is wider than the board's default carries current: it keeps its
        // width all the way into its pads, however tight; only signals neck to the minimum.
        let signal = rules.classes.iter().find(|c| c.name == "Default").map_or(0.0, |c| c.track_width).max(rules.min_track_width);
        let power = class.is_some_and(|c| c.track_width > signal + 1e-9);
        // A class's via may leave a thinner ring round its drill than the board allows; the via
        // grows to keep the ring, the drill as the class has it. Custom rules on vias of the net
        // (hole_size, via_diameter, annular_width) may ask more of either; never less.
        let drill = take(class.map(|c| c.via_drill), rules.min_via_drill);
        let diameter = take(class.map(|c| c.via_diameter), rules.min_via_diameter);
        let (drill, diameter, ring) = if ["hole_size", "via_diameter", "annular_width"].iter().any(|k| board.custom.has(k)) {
            let via = board::Thing::New { board, via: true, net: Some(net), class: board.class_name(net), width: diameter, drill, layer: None, area: board::custom::Area::Unknown };
            let rule = |kind: &str, floor: Nm| (0..board.layers.len()).map(|l| board.rule_min(kind, &via, None, l, floor)).max().unwrap_or(floor).max(floor);
            (rule("hole_size", drill), rule("via_diameter", diameter), rule("annular_width", nm(rules.min_via_annular_width)))
        } else {
            (drill, diameter, nm(rules.min_via_annular_width))
        };
        Profile {
            class: class.map_or_else(String::new, |c| c.name.clone()),
            half_width,
            narrow_half_width: if power { half_width } else { (nm(min_width) / 2).clamp(1, half_width.max(1)) },
            min_half_width: (nm(min_width) / 2).clamp(1, half_width.max(1)),
            // The rules' floor for every net: the board's minimum, or a custom rule's that holds
            // whatever the net (a fab's 0.15 mm over a board minimum of 0.12).
            board_min_half_width: (board.min_track_width_any() / 2).clamp(1, half_width.max(1)),
            clearance: take(class.map(|c| c.clearance), rules.min_clearance),
            via_radius: (diameter / 2).max(drill / 2 + ring),
            drill_radius: drill / 2,
            net: board.rules_name_nets().then_some(net),
        }
    }
}

/// Half the shorter side of a pad's bounding box: half the width of the pin a leg enters.
pub(crate) fn pad_half_side(pad: &board::Item) -> Nm {
    let b = pad.shape.bbox();
    (b.max.x - b.min.x).min(b.max.y - b.min.y) / 2
}

/// What a board's map covers: its outline, or with none its items' extent and room round it, not
/// only their corners nearest the origin, which left a map too small to route round anything.
/// Tracks and vias the router may lay or take up are left out, so every map of one run is the
/// same size (a map made mid-run once came out larger than those whose cells it was read with).
pub(crate) fn map_extent(board: &Board) -> board::BBox {
    let b = board.outline.iter().flatten().copied().fold(None, |b: Option<board::BBox>, p| {
        Some(b.map_or(board::BBox::of([p]), |b| b.union(board::BBox::of([p]))))
    });
    b.unwrap_or_else(|| {
        let fixed = board.items().filter(|(_, it)| it.locked || !matches!(it.kind, Kind::Track | Kind::Via));
        let items = fixed.map(|(_, it)| it.shape.bbox()).reduce(board::BBox::union);
        items.unwrap_or(board::BBox::of([Point::new(0, 0)])).grow(2_000_000)
    })
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
    /// Per cell: extra cost of a via standing there, in a via-avoid area, plus negotiation's price
    /// for vias crowding there when it prices them; empty for none.
    pub via_penalty: Vec<u32>,
    /// Per routing layer and cell: what the board's routing regions charge for entering it, on
    /// top of `penalty`, which the stages set for their own ends; empty for none.
    pub region_cost: Vec<u32>,
    /// Pours are planes to keep clear of, not copper refilled around the route.
    pub pours_block: bool,
    /// Steps along a layer's own direction cost less than across it: horizontal on even
    /// routing layers, vertical on odd ones (see [`crate::search`]). For two-layer boards, where
    /// crossing nets otherwise both take the first layer and pay vias to pass each other.
    pub directional: bool,
    /// When a search on the grid gives up; the stage searching sets it from its own deadline.
    pub deadline: crate::clock::Deadline,
    /// What other nets' thermal spokes charge, shared with the map's other views; none for none.
    pub spokes: Option<std::sync::Arc<crate::spokes::SpokePrices>>,
}

impl RouteGrid {
    /// What the item breaks, as this grid's routes are judged.
    pub fn violations(&self, board: &Board, id: ItemId) -> Vec<board::Violation> {
        if self.pours_block { board::check_item_with_pours(board, id) } else { board::check_item(board, id) }
    }
}

impl RouteGrid {
    #[inline]
    pub fn penalty_at(&self, index: usize) -> u32 {
        let spokes = self.spokes.as_ref().map_or(0, |s| s.track(index, self.net));
        self.penalty.get(index).copied().unwrap_or(0) + self.region_cost.get(index).copied().unwrap_or(0) + spokes
    }

    #[inline]
    pub fn via_penalty_at(&self, cell: usize) -> u32 {
        self.via_penalty.get(cell).copied().unwrap_or(0) + self.spokes.as_ref().map_or(0, |s| s.via(cell, self.net))
    }

    #[inline]
    pub fn cell_count(&self) -> usize {
        self.nx * self.ny
    }

    #[inline]
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
    pub(crate) layers: usize,
    pub(crate) owner: Vec<i32>,
    pub(crate) via_owner: Vec<i32>,
    /// Per cell: the extra cost of a via there, in the board's via-avoid areas; empty for none.
    pub(crate) via_avoid: Vec<u32>,
    /// The board's routing regions on these cells, applied to each net's view.
    pub(crate) regions: Vec<crate::regions::RegionRaster>,
    /// What the thermal spokes pads can spare charge on these cells, shared by the views.
    pub(crate) spokes: Option<std::sync::Arc<crate::spokes::SpokePrices>>,
    /// The board's clear-under rules on these cells; empty without any.
    pub(crate) under: Vec<crate::under::UnderRaster>,
}


/// Building a map looks at its deadline once in this many items stamped, and once in this many
/// mask openings.
const STAMPS_PER_CHECK: usize = 256;
const MASKS_PER_CHECK: usize = 16;

impl ObstacleMap {
    /// With `pours_block`, poured copper is kept like any other copper; otherwise it claims
    /// nothing, to be refilled around whatever is routed through it.
    pub fn new(board: &Board, profile: Profile, h: Nm, pours_block: bool) -> ObstacleMap {
        ObstacleMap::without(board, profile, h, pours_block, &|_| false)
    }

    /// A map covering only `bounds`, for fine-grid work such as escaping a single package without
    /// paying for a fine grid over the whole board.
    pub fn window(board: &Board, profile: Profile, h: Nm, bounds: board::BBox, pours_block: bool) -> ObstacleMap {
        ObstacleMap::window_without(board, profile, h, bounds, pours_block, &|_| false)
    }

    /// [`ObstacleMap::window`], leaving out the items `skip` names, as [`ObstacleMap::without`].
    pub fn window_without(board: &Board, profile: Profile, h: Nm, bounds: board::BBox, pours_block: bool, skip: &dyn Fn(ItemId) -> bool) -> ObstacleMap {
        crate::work::add(&crate::work::MAPS, 1);
        let (x0, y0) = (bounds.min.x, bounds.min.y);
        let nx = ((bounds.max.x - x0) / h).max(1) as usize;
        let ny = ((bounds.max.y - y0) / h).max(1) as usize;
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
            via_avoid: Vec::new(),
            regions: Vec::new(),
            spokes: None,
            under: Vec::new(),
        };
        map.stamp_via_avoid(board);
        map.regions = map.region_rasters(board);
        // Before the copper, which marks it as it is stamped.
        map.under = map.under_rasters(board);
        map.spokes = map.spoke_prices(board);
        let max_clr = if board.nets.is_empty() {
            map.profile.clearance
        } else {
            map.profile.clearance.max(board.max_net_clearance().unwrap_or(0))
        };
        let hole_margin = (map.profile.drill_radius + nm(board.rules.min_hole_to_hole))
            .max(map.profile.via_radius + nm(board.rules.min_hole_clearance))
            .max(map.profile.half_width + nm(board.rules.min_hole_clearance) + map.reach());
        let copper_margin = (map.profile.half_width + max_clr + map.reach()).max(map.profile.via_radius + max_clr);
        let reach_box = board::BBox {
            min: Point::new(x0, y0),
            max: Point::new(x0 + nx as Nm * h, y0 + ny as Nm * h),
        }
        .grow(copper_margin.max(hole_margin) + h);
        // Fixed copper in a window (outline, pads, pours, keepouts, mask openings) stays put across
        // trial rip-ups just as on a whole-board map; caching it in its own window pool avoids
        // restamping fine-grid polygons and pads on every reroute try without evicting whole maps.
        let (mut fixed, mut moving) = (Vec::new(), Vec::new());
        for id in board.overlapping(reach_box) {
            if skip(id) {
                continue;
            }
            let Some(it) = board.item(id) else { continue };
            if matches!(it.kind, Kind::Track | Kind::Via) { &mut moving } else { &mut fixed }.push(id);
        }
        map.stamp_fixed_cached(board, &fixed, &Deadline::default(), true);
        let gkey = map.grid_stamp_key(board);
        for id in moving {
            map.stamp_with_key(board, id, gkey.as_ref());
        }
        map
    }

    /// This map's cells within `bounds`, as [`ObstacleMap::window`] would make them over a map
    /// of the fixed copper alone (a map that left out every unlocked track and via): the cells
    /// are copied, and only the unlocked tracks and vias in reach are stamped, not the pads and
    /// pours again.
    pub fn subwindow(&self, board: &Board, bounds: board::BBox) -> ObstacleMap {
        crate::work::add(&crate::work::MAPS, 1);
        let h = self.h;
        let i0 = (bounds.min.x - self.x0).div_euclid(h).clamp(0, self.nx.saturating_sub(1) as Nm) as usize;
        let j0 = (bounds.min.y - self.y0).div_euclid(h).clamp(0, self.ny.saturating_sub(1) as Nm) as usize;
        let i1 = (bounds.max.x - self.x0 + h - 1).div_euclid(h).clamp((i0 + 1) as Nm, self.nx as Nm) as usize;
        let j1 = (bounds.max.y - self.y0 + h - 1).div_euclid(h).clamp((j0 + 1) as Nm, self.ny as Nm) as usize;
        let (nx, ny) = (i1 - i0, j1 - j0);
        let (x0, y0) = (self.x0 + i0 as Nm * h, self.y0 + j0 as Nm * h);
        let rows = |cells: &[i32], planes: usize, full: usize| -> Vec<i32> {
            let mut out = Vec::with_capacity(planes * nx * ny);
            for l in 0..planes {
                for j in j0..j1 {
                    let at = l * full + j * self.nx + i0;
                    out.extend_from_slice(&cells[at..at + nx]);
                }
            }
            out
        };
        let full = self.nx * self.ny;
        let mut map = ObstacleMap {
            pours_block: self.pours_block,
            profile: self.profile.clone(),
            x0,
            y0,
            h,
            nx,
            ny,
            layers: self.layers,
            owner: rows(&self.owner, self.layers, full),
            via_owner: rows(&self.via_owner, 1, full),
            via_avoid: Vec::new(),
            regions: Vec::new(),
            spokes: None,
            under: self.under_window(i0, j0, nx, ny),
        };
        map.regions = map.region_rasters(board);
        map.spokes = map.spoke_prices(board);
        if !self.via_avoid.is_empty() {
            map.via_avoid = (j0..j1).flat_map(|j| self.via_avoid[j * self.nx + i0..j * self.nx + i0 + nx].iter().copied()).collect();
        }
        // As far as `window` looks for copper whose claims reach into it.
        let max_clr = if board.nets.is_empty() {
            map.profile.clearance
        } else {
            map.profile.clearance.max(board.max_net_clearance().unwrap_or(0))
        };
        let p = &map.profile;
        let hole_margin = (p.drill_radius + nm(board.rules.min_hole_to_hole))
            .max(p.via_radius + nm(board.rules.min_hole_clearance))
            .max(p.half_width + nm(board.rules.min_hole_clearance) + map.reach());
        let copper_margin = (p.half_width + max_clr + map.reach()).max(p.via_radius + max_clr);
        let reach = board::BBox { min: Point::new(x0, y0), max: Point::new(x0 + nx as Nm * h, y0 + ny as Nm * h) }.grow(copper_margin.max(hole_margin) + h);
        let mut ids: Vec<ItemId> = (0..=board.layers.len()).flat_map(|slot| board.near(slot, reach)).collect();
        ids.sort_unstable();
        ids.dedup();
        let gkey = map.grid_stamp_key(board);
        for id in ids {
            if board.item(id).is_some_and(|it| !it.locked && matches!(it.kind, Kind::Track | Kind::Via)) {
                map.stamp_with_key(board, id, gkey.as_ref());
            }
        }
        map
    }

    /// The map with the items `skip` names left out, as if they were not on the board.
    pub fn without(board: &Board, profile: Profile, h: Nm, pours_block: bool, skip: &dyn Fn(ItemId) -> bool) -> ObstacleMap {
        ObstacleMap::without_by(board, profile, h, pours_block, skip, &Deadline::default()).expect("a map with no deadline is finished")
    }

    /// [`ObstacleMap::new`], or `None` if `deadline` passes first: a board with tens of thousands
    /// of drawn copper shapes takes tens of seconds to map.
    pub fn new_by(board: &Board, profile: Profile, h: Nm, pours_block: bool, deadline: &Deadline) -> Option<ObstacleMap> {
        ObstacleMap::without_by(board, profile, h, pours_block, &|_| false, deadline)
    }

    /// [`ObstacleMap::without`], or `None` if `deadline` passes first.
    pub fn without_by(board: &Board, profile: Profile, h: Nm, pours_block: bool, skip: &dyn Fn(ItemId) -> bool, deadline: &Deadline) -> Option<ObstacleMap> {
        crate::work::add(&crate::work::MAPS, 1);
        let b = map_extent(board);
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
            via_avoid: Vec::new(),
            regions: Vec::new(),
            spokes: None,
            under: Vec::new(),
        };
        map.stamp_via_avoid(board);
        // What does not move while routing (outline, pads, pours, keepouts, mask openings) is
        // stamped once and copied after: plane pours and hundreds of pads cost far more to stamp
        // than the copy, and a try of the shove builds maps over and over. Claims commute (a cell
        // is free, one net's, or several nets'), so stamping them before the tracks gives the same map.
        let (mut fixed, mut moving) = (Vec::new(), Vec::new());
        for (id, it) in board.items().filter(|(id, _)| !skip(*id)) {
            if matches!(it.kind, Kind::Track | Kind::Via) { &mut moving } else { &mut fixed }.push(id);
        }
        if !map.stamp_fixed_cached(board, &fixed, deadline, false) {
            return None;
        }
        map.regions = map.region_rasters(board);
        // The fixed copper's part in the clear-under rules, which the cache does not keep.
        map.under = map.under_rasters(board);
        for &id in &fixed {
            map.stamp_under(board, id);
        }
        map.spokes = map.spoke_prices(board);
        let gkey = map.grid_stamp_key(board);
        for (k, id) in moving.into_iter().enumerate() {
            if k % STAMPS_PER_CHECK == STAMPS_PER_CHECK - 1 && deadline.expired() {
                return None;
            }
            map.stamp_with_key(board, id, gkey.as_ref());
        }
        Some(map)
    }

    /// The outline, `fixed`, and mask openings, from the last maps' if one had the same; false,
    /// and nothing kept, if `deadline` passes first. Whole-board and window maps keep separate
    /// LRU pools so fine-grid windows never evict whole-board maps.
    fn stamp_fixed_cached(&mut self, board: &Board, fixed: &[ItemId], deadline: &Deadline, is_window: bool) -> bool {
        const WHOLE_KEPT: usize = 16;
        const WIN_KEPT: usize = 48;
        const WIN_RUN_CAP: usize = 4_000_000;
        type Kept = Vec<(u128, Vec<(i32, u32)>, Vec<(i32, u32)>)>;
        thread_local! {
            static WHOLE_CACHE: std::cell::RefCell<Kept> = const { std::cell::RefCell::new(Vec::new()) };
            static WIN_CACHE: std::cell::RefCell<Kept> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let key = self.fixed_key(board, fixed);
        let lookup = |cache: &std::cell::RefCell<Kept>, owner_out: &mut [i32], via_out: &mut [i32]| -> bool {
            let mut cache = cache.borrow_mut();
            let Some(pos) = cache.iter().rposition(|(k, ..)| *k == key) else { return false };
            unrun(&cache[pos].1, owner_out);
            unrun(&cache[pos].2, via_out);
            if pos + 1 < cache.len() {
                let entry = cache.remove(pos);
                cache.push(entry);
            }
            true
        };
        let hit = if is_window {
            WIN_CACHE.with(|c| lookup(c, &mut self.owner, &mut self.via_owner))
        } else {
            WHOLE_CACHE.with(|c| lookup(c, &mut self.owner, &mut self.via_owner))
        };
        if hit {
            return true;
        }
        self.stamp_outline(board);
        for (k, &id) in fixed.iter().enumerate() {
            if k % STAMPS_PER_CHECK == STAMPS_PER_CHECK - 1 && deadline.expired() {
                return false;
            }
            self.stamp(board, id);
        }
        if !self.stamp_mask_areas(board, deadline) {
            return false;
        }
        let entry = (key, runs(&self.owner), runs(&self.via_owner));
        if is_window {
            WIN_CACHE.with(|cache| {
                let mut cache = cache.borrow_mut();
                while cache.len() >= WIN_KEPT
                    || cache.iter().map(|(_, o, v)| o.len() + v.len()).sum::<usize>() + entry.1.len() + entry.2.len() > WIN_RUN_CAP && !cache.is_empty()
                {
                    cache.remove(0);
                }
                cache.push(entry);
            });
        } else {
            WHOLE_CACHE.with(|cache| {
                let mut cache = cache.borrow_mut();
                if cache.len() >= WHOLE_KEPT {
                    cache.remove(0);
                }
                cache.push(entry);
            });
        }
        true
    }

    /// Everything [`ObstacleMap::stamp_fixed_cached`] stamps from: the grid, the profile, the
    /// rules, the outline, mask openings and every fixed item as it stands, hashed twice over for
    /// 128 bits.
    fn fixed_key(&self, board: &Board, fixed: &[ItemId]) -> u128 {
        use std::hash::Hash;
        let hash_shape = |shape: &Shape, h: &mut crate::fx::FxHasher| match shape {
            Shape::Capsule { a, b, r } => (0u8, a, b, r).hash(h),
            Shape::Polygon(pts) => (1u8, pts).hash(h),
        };
        crate::fx::hash128(|h| {
            (self.x0, self.y0, self.h, self.nx, self.ny, self.layers, self.pours_block).hash(h);
            let p = &self.profile;
            (p.half_width, p.via_radius, p.drill_radius, p.clearance, p.net).hash(h);
            let r = &board.rules;
            if r.custom.is_some() {
                p.class.hash(h);
            }
            [r.min_copper_edge_clearance, r.min_clearance, r.min_hole_clearance, r.min_hole_to_hole].map(f64::to_bits).hash(h);
            r.custom.hash(h);
            board.outline.hash(h);
            board.outline_edges.len().hash(h);
            for &(a, b, _, c) in &board.outline_edges {
                (a, b, c).hash(h);
            }
            board.areas.len().hash(h);
            for (name, shape, layers) in &board.areas {
                (name, layers).hash(h);
                hash_shape(shape, h);
            }
            (board.via_openings, board.mask_web, board.mask_margin()).hash(h);
            let masks = &board.mask_areas;
            masks.len().hash(h);
            if masks.len() <= 64 {
                for (s, l, n) in masks {
                    (l, n).hash(h);
                    hash_shape(s, h);
                }
            } else {
                for (s, l, n) in masks[..16].iter().chain(&masks[masks.len() - 16..]) {
                    (l, n).hash(h);
                    hash_shape(s, h);
                }
            }
            fixed.len().hash(h);
            for &id in fixed {
                let Some(it) = board.item(id) else { continue };
                (id, it.net, it.layers, it.refilled(), it.hole, self.clearance_to(board, it.net)).hash(h);
                (board.pad_clearances.get(&id), board.mask_openings.get(&id), board.hole_ends.get(&id)).hash(h);
                if let Some(holes) = board.keepout_holes.get(&id) {
                    1u8.hash(h);
                    holes.len().hash(h);
                    for hs in holes {
                        hash_shape(hs, h);
                    }
                } else {
                    0u8.hash(h);
                }
                match &it.kind {
                    Kind::Pad { plated, .. } => (1u8, *plated).hash(h),
                    Kind::Pour { kept, .. } => (2u8, *kept).hash(h),
                    Kind::Keepout { forbids, .. } => (3u8, forbids.tracks, forbids.vias).hash(h),
                    Kind::Track | Kind::Via => 4u8.hash(h),
                    Kind::Spoke { clearance, .. } => (5u8, *clearance).hash(h),
                }
                hash_shape(&it.shape, h);
            }
        })
    }

    /// Openings drawn in the mask: only the net they bare may run there; none where they bare
    /// none or several. False if `deadline` passes first: a board can draw thousands.
    fn stamp_mask_areas(&mut self, board: &Board, deadline: &Deadline) -> bool {
        let cells = self.nx * self.ny;
        let (track, via) = (self.profile.half_width + board.mask_margin() + self.reach(), self.profile.via_radius + board.mask_margin());
        let mut spans = Vec::new();
        for (k, (shape, layers, net)) in board.mask_areas.iter().enumerate() {
            if k % MASKS_PER_CHECK == MASKS_PER_CHECK - 1 && deadline.expired() {
                return false;
            }
            spans.clear();
            self.cells_near_in(shape, track, |c| push_span(&mut spans, c));
            self.apply_owner_spans(cells, *layers, *net, &spans);
            spans.clear();
            // A bare via's own opening keeps clear of the area too, by the mask's narrowest web.
            let bare = if board.via_openings.0 & layers != 0 { board.via_openings.1 + board.mask_web } else { 0 };
            self.cells_near_in(shape, via + bare, |c| push_span(&mut spans, c));
            self.apply_via_spans(*net, &spans);
        }
        true
    }

    /// Marks the cells where a via would reach into one of the board's via-avoid areas.
    fn stamp_via_avoid(&mut self, board: &Board) {
        if board.via_avoid.is_empty() {
            return;
        }
        self.via_avoid = vec![0; self.nx * self.ny];
        let mut hit = Vec::new();
        for area in &board.via_avoid {
            self.cells_near(area, self.profile.via_radius, &mut |c| hit.push(c));
        }
        for c in hit {
            self.via_avoid[c] = board.via_avoid_cost;
        }
    }

    /// One more via-avoid area on a map already built, as if it had been among the board's.
    pub(crate) fn add_via_avoid(&mut self, board: &Board, area: &Shape) {
        if self.via_avoid.is_empty() {
            self.via_avoid = vec![0; self.nx * self.ny];
        }
        let mut hit = Vec::new();
        self.cells_near(area, self.profile.via_radius, &mut |c| hit.push(c));
        for c in hit {
            self.via_avoid[c] = board.via_avoid_cost;
        }
    }

    pub(crate) fn reach(&self) -> Nm {
        (self.h as f64 * STEP_REACH).ceil() as Nm
    }

    pub(crate) fn centre(&self, i: usize, j: usize) -> Point {
        Point::new(self.x0 + (2 * i as Nm + 1) * self.h / 2, self.y0 + (2 * j as Nm + 1) * self.h / 2)
    }

    fn apply_owner_spans(&mut self, cells: usize, layers_mask: u64, net: u32, spans: &[(u32, u16)]) {
        if spans.is_empty() {
            return;
        }
        let mut mask = if self.layers >= 64 { layers_mask } else { layers_mask & ((1u64 << self.layers) - 1) };
        while mask != 0 {
            let l = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            let plane = &mut self.owner[l * cells..(l + 1) * cells];
            for &(start, len) in spans {
                let s = start as usize;
                claim_slice(&mut plane[s..s + len as usize], net);
            }
        }
    }

    fn apply_via_spans(&mut self, net: u32, spans: &[(u32, u16)]) {
        for &(start, len) in spans {
            let s = start as usize;
            claim_slice(&mut self.via_owner[s..s + len as usize], net);
        }
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn stamp_owner_near(&mut self, shape: &Shape, margin: Nm, cells: usize, layers_mask: u64, net: u32, rec: Option<&mut CachedStamp>, spans: &mut SpanList) {
        let mask = if self.layers >= 64 { layers_mask } else { layers_mask & ((1u64 << self.layers) - 1) };
        if mask == 0 {
            return;
        }
        if let Some(r) = rec {
            spans.clear();
            self.cells_near_in(shape, margin, |c| push_span(spans, c));
            if !spans.is_empty() {
                self.apply_owner_spans(cells, mask, net, spans);
                r.owner_ops.push((layers_mask, net, spans.clone()));
            }
        } else if mask.is_power_of_two() {
            let l = mask.trailing_zeros() as usize;
            let base = l * cells;
            let mut owner = std::mem::take(&mut self.owner);
            let plane = &mut owner[base..base + cells];
            if net == 0 {
                self.cells_near_in(shape, margin, |c| plane[c] = SEVERAL);
            } else {
                let n = net as i32;
                self.cells_near_in(shape, margin, |c| {
                    let v = plane[c];
                    plane[c] = if v == FREE || v == n { n } else { SEVERAL };
                });
            }
            self.owner = owner;
        } else {
            let mut owner = std::mem::take(&mut self.owner);
            if net == 0 {
                self.cells_near_in(shape, margin, |c| {
                    let mut m = mask;
                    while m != 0 {
                        let l = m.trailing_zeros() as usize;
                        m &= m - 1;
                        owner[l * cells + c] = SEVERAL;
                    }
                });
            } else {
                let n = net as i32;
                self.cells_near_in(shape, margin, |c| {
                    let mut m = mask;
                    while m != 0 {
                        let l = m.trailing_zeros() as usize;
                        m &= m - 1;
                        let cell = &mut owner[l * cells + c];
                        let v = *cell;
                        *cell = if v == FREE || v == n { n } else { SEVERAL };
                    }
                });
            }
            self.owner = owner;
        }
    }

    #[inline]
    fn stamp_via_near(&mut self, shape: &Shape, margin: Nm, net: u32, rec: Option<&mut CachedStamp>, spans: &mut Vec<(u32, u16)>) {
        if let Some(r) = rec {
            spans.clear();
            self.cells_near_in(shape, margin, |c| push_span(spans, c));
            if !spans.is_empty() {
                self.apply_via_spans(net, spans);
                r.via_ops.push((net, spans.clone()));
            }
        } else {
            let mut via_owner = std::mem::take(&mut self.via_owner);
            if net == 0 {
                self.cells_near_in(shape, margin, |c| via_owner[c] = SEVERAL);
            } else {
                let n = net as i32;
                self.cells_near_in(shape, margin, |c| {
                    let v = via_owner[c];
                    via_owner[c] = if v == FREE || v == n { n } else { SEVERAL };
                });
            }
            self.via_owner = via_owner;
        }
    }

    fn apply_cached_stamp(&mut self, cells: usize, cached: &CachedStamp) {
        for &(layers, net, ref spans) in &cached.owner_ops {
            self.apply_owner_spans(cells, layers, net, spans);
        }
        for &(net, ref spans) in &cached.via_ops {
            self.apply_via_spans(net, spans);
        }
    }

    fn grid_stamp_key(&self, board: &Board) -> Option<GridStampKey> {
        if board.custom.has("clearance")
            || board.custom.has("hole_clearance")
            || board.custom.has("hole_to_hole")
            || board.custom.has("physical_hole_clearance")
        {
            return None;
        }
        Some(GridStampKey {
            x0: self.x0,
            y0: self.y0,
            h: self.h,
            nx: self.nx,
            ny: self.ny,
            layers: self.layers,
            pours_block: self.pours_block,
            half_width: self.profile.half_width,
            via_radius: self.profile.via_radius,
            drill_radius: self.profile.drill_radius,
            clearance: self.profile.clearance,
            min_hole_clearance: nm(board.rules.min_hole_clearance),
            min_hole_to_hole: nm(board.rules.min_hole_to_hole),
            via_open_layers: board.via_openings.0,
            via_open_by: board.via_openings.1,
            mask_web: board.mask_web,
            mask_margin: board.mask_margin(),
        })
    }

    /// Calls `f` for every cell whose centre lies closer than `margin` to `shape`.
    pub(crate) fn cells_near(&self, shape: &Shape, margin: Nm, f: &mut dyn FnMut(usize)) {
        self.cells_near_in(shape, margin, f);
    }

    #[inline]
    fn cells_near_in<F: FnMut(usize)>(&self, shape: &Shape, margin: Nm, mut f: F) {
        if self.nx == 0 || self.ny == 0 {
            return;
        }
        let sb = shape.bbox();
        let (x_end, y_end) = (self.x0 + self.nx as Nm * self.h, self.y0 + self.ny as Nm * self.h);
        if sb.max.x + margin < self.x0 || sb.min.x - margin > x_end || sb.max.y + margin < self.y0 || sb.min.y - margin > y_end {
            return;
        }
        match shape {
            Shape::Capsule { a, b, r } => self.cells_near_capsule(*a, *b, *r, margin, &mut f),
            Shape::Polygon(poly) => self.cells_near_poly(poly, margin, &mut f),
        }
    }

    #[inline]
    fn cells_near_capsule<F: FnMut(usize)>(&self, a: Point, b: Point, r: Nm, margin: Nm, f: &mut F) {
        if self.nx == 0 || self.ny == 0 {
            return;
        }
        let (min_x, max_x) = (a.x.min(b.x) - r - margin, a.x.max(b.x) + r + margin);
        let (min_y, max_y) = (a.y.min(b.y) - r - margin, a.y.max(b.y) + r + margin);
        let (x_end, y_end) = (self.x0 + self.nx as Nm * self.h, self.y0 + self.ny as Nm * self.h);
        if max_x < self.x0 || min_x > x_end || max_y < self.y0 || min_y > y_end {
            return;
        }
        let range = |lo: Nm, hi: Nm, origin: Nm, n: usize| {
            if n == 0 || hi < origin {
                return 0..0;
            }
            let ia = ((lo - origin) as f64 / self.h as f64 - 0.5).floor().max(0.0) as usize;
            let ib = (((hi - origin) as f64 / self.h as f64 - 0.5).ceil().max(0.0) as usize).min(n - 1);
            ia..ib + 1
        };
        let (x_half, y_half, h) = (self.x0 + self.h / 2, self.y0 + self.h / 2, self.h);
        let limit = (r + margin) as f64;
        let limit2 = limit * limit;
        if a == b {
            for j in range(min_y, max_y, self.y0, self.ny) {
                let dy = (y_half + j as Nm * h - a.y) as f64;
                let dy2 = dy * dy;
                if dy2 >= limit2 {
                    continue;
                }
                let row = j * self.nx;
                for i in range(min_x, max_x, self.x0, self.nx) {
                    let dx = (x_half + i as Nm * h - a.x) as f64;
                    if dx * dx + dy2 < limit2 {
                        f(row + i);
                    }
                }
            }
        } else {
            let (ax, ay, bx, by) = (a.x as f64, a.y as f64, b.x as f64, b.y as f64);
            let (dx, dy) = (bx - ax, by - ay);
            let len2 = dx * dx + dy * dy;
            if a.y == b.y {
                let (x_lo, x_hi) = (a.x.min(b.x) as f64 + 2.0, a.x.max(b.x) as f64 - 2.0);
                for j in range(min_y, max_y, self.y0, self.ny) {
                    let y = (y_half + j as Nm * h) as f64;
                    if (y - ay).abs() >= limit {
                        continue;
                    }
                    let row = j * self.nx;
                    for i in range(min_x, max_x, self.x0, self.nx) {
                        let x = (x_half + i as Nm * h) as f64;
                        if x >= x_lo && x <= x_hi {
                            f(row + i);
                        } else {
                            let t = (((x - ax) * dx + (y - ay) * dy) / len2).clamp(0.0, 1.0);
                            let (qx, qy) = (ax + t * dx, ay + t * dy);
                            let d2 = (x - qx) * (x - qx) + (y - qy) * (y - qy);
                            if d2 < limit2 && (d2 < limit2 * 0.999999 || (x - qx).hypot(y - qy) < limit) {
                                f(row + i);
                            }
                        }
                    }
                }
            } else if a.x == b.x {
                let cols = range(min_x, max_x, self.x0, self.nx);
                let mut c_first = usize::MAX;
                let mut c_last = 0;
                for i in cols {
                    if ((x_half + i as Nm * h) as f64 - ax).abs() < limit {
                        if c_first == usize::MAX {
                            c_first = i;
                        }
                        c_last = i;
                    }
                }
                if c_first != usize::MAX {
                    let (y_lo, y_hi) = (a.y.min(b.y) as f64 + 2.0, a.y.max(b.y) as f64 - 2.0);
                    for j in range(min_y, max_y, self.y0, self.ny) {
                        let y = (y_half + j as Nm * h) as f64;
                        let row = j * self.nx;
                        if y >= y_lo && y <= y_hi {
                            for i in c_first..=c_last {
                                f(row + i);
                            }
                        } else {
                            for i in c_first..=c_last {
                                let x = (x_half + i as Nm * h) as f64;
                                let t = (((x - ax) * dx + (y - ay) * dy) / len2).clamp(0.0, 1.0);
                                let (qx, qy) = (ax + t * dx, ay + t * dy);
                                let d2 = (x - qx) * (x - qx) + (y - qy) * (y - qy);
                                if d2 < limit2 && (d2 < limit2 * 0.999999 || (x - qx).hypot(y - qy) < limit) {
                                    f(row + i);
                                }
                            }
                        }
                    }
                }
            } else {
                for j in range(min_y, max_y, self.y0, self.ny) {
                    let y = (y_half + j as Nm * h) as f64;
                    let (row_min_x, row_max_x) = if dy.abs() > 1.0 {
                        let t0 = ((y - limit - ay) / dy).clamp(0.0, 1.0);
                        let t1 = ((y + limit - ay) / dy).clamp(0.0, 1.0);
                        let x0 = ax + t0 * dx;
                        let x1 = ax + t1 * dx;
                        (
                            ((x0.min(x1) - limit - 2.0).floor() as Nm).max(min_x),
                            ((x0.max(x1) + limit + 2.0).ceil() as Nm).min(max_x),
                        )
                    } else {
                        (min_x, max_x)
                    };
                    let row = j * self.nx;
                    for i in range(row_min_x, row_max_x, self.x0, self.nx) {
                        let x = (x_half + i as Nm * h) as f64;
                        let t = (((x - ax) * dx + (y - ay) * dy) / len2).clamp(0.0, 1.0);
                        let (qx, qy) = (ax + t * dx, ay + t * dy);
                        let d2 = (x - qx) * (x - qx) + (y - qy) * (y - qy);
                        if d2 < limit2 && (d2 < limit2 * 0.999999 || (x - qx).hypot(y - qy) < limit) {
                            f(row + i);
                        }
                    }
                }
            }
        }
    }

    fn cells_near_poly<F: FnMut(usize)>(&self, poly: &[Point], margin: Nm, f: &mut F) {
        if poly.is_empty() || self.nx == 0 || self.ny == 0 {
            return;
        }
        let bb = board::BBox::of(poly.iter().copied());
        let (x_end, y_end) = (self.x0 + self.nx as Nm * self.h, self.y0 + self.ny as Nm * self.h);
        if bb.max.x + margin < self.x0 || bb.min.x - margin > x_end || bb.max.y + margin < self.y0 || bb.min.y - margin > y_end {
            return;
        }
        let range = |lo: Nm, hi: Nm, origin: Nm, n: usize| {
            if n == 0 || hi < origin {
                return 0..0;
            }
            let a = ((lo - origin) as f64 / self.h as f64 - 0.5).floor().max(0.0) as usize;
            let b = (((hi - origin) as f64 / self.h as f64 - 0.5).ceil().max(0.0) as usize).min(n - 1);
            a..b + 1
        };
        let (x_half, y_half, h) = (self.x0 + self.h / 2, self.y0 + self.h / 2, self.h);
        let mut xs: Vec<f64> = Vec::new();
        // The inside by scanline, then a band along every edge.
        for j in range(bb.min.y, bb.max.y, self.y0, self.ny) {
            let y = y_half + j as Nm * h;
            xs.clear();
            for k in 0..poly.len() {
                let (p, q) = (poly[k], poly[(k + 1) % poly.len()]);
                if (p.y > y) != (q.y > y) {
                    xs.push(p.x as f64 + (y - p.y) as f64 * (q.x - p.x) as f64 / (q.y - p.y) as f64);
                }
            }
            xs.sort_by(f64::total_cmp);
            let row = j * self.nx;
            for span in xs.chunks_exact(2) {
                for i in range(span[0].floor() as Nm, span[1].ceil() as Nm, self.x0, self.nx) {
                    let x = (x_half + i as Nm * h) as f64;
                    if x > span[0] && x < span[1] {
                        f(row + i);
                    }
                }
            }
        }
        if margin > 0 {
            for k in 0..poly.len() {
                let (p, q) = (poly[k], poly[(k + 1) % poly.len()]);
                self.cells_near_capsule(p, q, 0, margin, f);
            }
        }
    }

    /// Everything outside the board, and a band inside its edge, is nobody's; and wherever the
    /// custom rules forbid this map's tracks or vias.
    fn stamp_outline(&mut self, board: &Board) {
        self.stamp_edges(board);
        if board.custom.has("disallow") {
            self.stamp_disallowed(board);
        }
        // A net a custom rule allows no vias gets none.
        if self.profile.net.is_some_and(|n| board.no_vias(n)) {
            self.via_owner.iter_mut().for_each(|c| *c = SEVERAL);
        }
    }

    /// Where a custom rule's `disallow` forbids this map's tracks or vias: a whole layer, or a
    /// named area of it. A rule that may hold (on something the router cannot answer) forbids.
    fn stamp_disallowed(&mut self, board: &Board) {
        use board::custom::Area;
        use kicad_pcb::dru::Tri;
        let cells = self.nx * self.ny;
        let p = self.profile.clone();
        let class = p.net.map_or(p.class.as_str(), |n| board.class_name(n)).to_string();
        let thing = |via: bool, layer: usize, area: Area| board::Thing::New {
            board,
            via,
            net: p.net,
            class: &class,
            width: if via { 2 * p.via_radius } else { 2 * p.half_width },
            drill: if via { 2 * p.drill_radius } else { 0 },
            layer: Some(layer),
            area,
        };
        let forbids = |word: &str, via: bool, layer: usize, area: Area| board.disallowed(word, &thing(via, layer, area), layer) != Tri::False;
        let reach = self.reach();
        // A via stands on every layer: any layer that forbids it forbids it.
        let via_anywhere = (0..self.layers).any(|l| forbids("via", true, l, Area::Outside));
        if via_anywhere {
            self.via_owner.iter_mut().for_each(|c| *c = SEVERAL);
        }
        let mut spans = Vec::new();
        for l in 0..self.layers {
            if forbids("track", false, l, Area::Outside) {
                self.owner[l * cells..(l + 1) * cells].iter_mut().for_each(|c| *c = SEVERAL);
                continue;
            }
            for (k, (_, shape, layers)) in board.areas.iter().enumerate() {
                if layers & (1 << l) == 0 {
                    continue;
                }
                if forbids("track", false, l, Area::In(k)) {
                    spans.clear();
                    self.cells_near_in(shape, p.half_width + reach, |c| push_span(&mut spans, c));
                    self.apply_owner_spans(cells, 1u64 << l, 0, &spans);
                }
                if !via_anywhere && forbids("via", true, l, Area::In(k)) {
                    spans.clear();
                    self.cells_near_in(shape, p.via_radius, |c| push_span(&mut spans, c));
                    self.apply_via_spans(0, &spans);
                }
            }
        }
    }

    /// Everything outside the board, and a band inside its edge, is nobody's.
    fn stamp_edges(&mut self, board: &Board) {
        let rules = &board.rules;
        let edge = nm(rules.min_copper_edge_clearance);
        if board.outline.is_empty() && board.outline_edges.is_empty() {
            return;
        }
        let outer = board.outline.first();
        let trace_margin = self.profile.half_width + edge + self.reach();
        let via_margin = self.profile.via_radius + edge;
        type OutlineKey = (Nm, Nm, Nm, usize, usize, usize, Nm, Nm, usize, Option<Point>, Option<Point>);
        let key: OutlineKey = (
            self.x0,
            self.y0,
            self.h,
            self.nx,
            self.ny,
            self.layers,
            trace_margin,
            via_margin,
            board.outline.iter().map(Vec::len).sum::<usize>() + board.outline_edges.len() + board.outline_edges.iter().map(|e| e.3 as usize).sum::<usize>(),
            outer.and_then(|o| o.first().copied()),
            outer.and_then(|o| o.last().copied()),
        );
        const KEPT: usize = 8;
        type OutlineCache = Vec<(OutlineKey, Vec<(i32, u32)>, Vec<(i32, u32)>)>;
        thread_local! {
            static CACHE: std::cell::RefCell<OutlineCache> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let hit = CACHE.with(|cache| {
            let mut borrow = cache.borrow_mut();
            let Some(pos) = borrow.iter().rposition(|(k, ..)| *k == key) else { return false };
            unrun(&borrow[pos].1, &mut self.owner);
            unrun(&borrow[pos].2, &mut self.via_owner);
            if pos + 1 < borrow.len() {
                let entry = borrow.remove(pos);
                borrow.push(entry);
            }
            true
        });
        if hit {
            return;
        }
        let cells = self.nx * self.ny;
        // Without a closed outline there is no inside to keep to, only the edges.
        let mut inside = vec![outer.is_none(); cells];
        if let Some(outer) = outer {
            self.cells_near_poly(outer, 0, &mut |c| inside[c] = true);
        }
        for hole in board.outline.iter().skip(1) {
            self.cells_near_poly(hole, 0, &mut |c| inside[c] = false);
        }
        for c in (0..cells).filter(|c| !inside[*c]) {
            self.via_owner[c] = SEVERAL;
            for l in 0..self.layers {
                self.owner[l * cells + c] = SEVERAL;
            }
        }
        let all_layers = if self.layers >= 64 { u64::MAX } else { (1u64 << self.layers) - 1 };
        let mut spans = Vec::new();
        for &(a, b, _, clearance) in &board.outline_edges {
            // An edge a custom rule holds copper further from keeps its own margins.
            let (trace_margin, via_margin) = (trace_margin - edge + clearance, via_margin - edge + clearance);
            spans.clear();
            self.cells_near_capsule(a, b, 0, trace_margin, &mut |c| push_span(&mut spans, c));
            self.apply_owner_spans(cells, all_layers, 0, &spans);
            spans.clear();
            self.cells_near_capsule(a, b, 0, via_margin, &mut |c| push_span(&mut spans, c));
            self.apply_via_spans(0, &spans);
        }
        let entry = (key, runs(&self.owner), runs(&self.via_owner));
        CACHE.with(|cache| {
            let mut borrow = cache.borrow_mut();
            if borrow.len() >= KEPT {
                borrow.remove(0);
            }
            borrow.push(entry);
        });
    }

    /// The clearance between this class's copper and an item of `net`.
    pub(crate) fn clearance_to(&self, board: &Board, net: u32) -> Nm {
        self.profile.clearance.max(board.clearance(net, net))
    }

    /// A track (or a via) of this map's nets on `layer`, as the custom rules are asked about it.
    pub(crate) fn new_copper<'b>(&'b self, board: &'b Board, via: bool, layer: usize) -> board::Thing<'b> {
        let p = &self.profile;
        let (width, drill) = if via { (2 * p.via_radius, 2 * p.drill_radius) } else { (2 * p.half_width, 0) };
        let class = p.net.map_or(p.class.as_str(), |n| board.class_name(n));
        board::Thing::New { board, via, net: p.net, class, width, drill, layer: Some(layer), area: board::custom::Area::Unknown }
    }

    /// A custom rule's minimum `kind` between this map's track (or via) and item `id`, on the
    /// layers they share, where `implicit` stands without one.
    pub(crate) fn rule_to(&self, board: &Board, kind: &str, id: ItemId, item: &board::Item, via: bool, implicit: Nm) -> Nm {
        if !board.custom.has(kind) {
            return implicit;
        }
        let it = board::Thing::Item(board, id, item);
        let layers = if item.layers == 0 { (1u64 << self.layers) - 1 } else { item.layers };
        (0..self.layers).filter(|l| layers & (1 << l) != 0).map(|l| board.rule_min(kind, &it, Some(&self.new_copper(board, via, l)), l, implicit)).max().unwrap_or(implicit)
    }

    /// Records what `id` claims.
    pub fn stamp(&mut self, board: &Board, id: ItemId) {
        let gkey = self.grid_stamp_key(board);
        self.stamp_with_key(board, id, gkey.as_ref());
    }

    fn stamp_with_key(&mut self, board: &Board, id: ItemId, gkey: Option<&GridStampKey>) {
        self.stamp_under(board, id);
        let Some(item) = board.item(id) else { return };
        if let (Some(&gk), Kind::Track | Kind::Via, Shape::Capsule { a, b, r }) = (gkey, &item.kind, &item.shape) {
            if !board.pad_clearances.contains_key(&id) && !board.hole_ends.contains_key(&id) {
                const STAMP_CACHE_CAP: usize = 16_384;
                thread_local! {
                    static STAMP_CACHE: std::cell::RefCell<crate::fx::FxHashMap<(GridStampKey, ItemStampKey), CachedStamp>> =
                        std::cell::RefCell::new(crate::fx::FxHashMap::default());
                }
                let ik = ItemStampKey {
                    is_via: matches!(item.kind, Kind::Via),
                    net: item.net,
                    layers: item.layers,
                    a: *a,
                    b: *b,
                    r: *r,
                    hole: item.hole,
                    clearance: self.clearance_to(board, item.net),
                };
                let key = (gk, ik);
                let cells = self.nx * self.ny;
                let hit = STAMP_CACHE.with(|cache| {
                    let borrow = cache.borrow();
                    if let Some(cs) = borrow.get(&key) {
                        self.apply_cached_stamp(cells, cs);
                        true
                    } else {
                        false
                    }
                });
                if hit {
                    return;
                }
                let mut rec = CachedStamp::default();
                self.stamp_uncached(board, id, item, Some(&mut rec));
                STAMP_CACHE.with(|cache| {
                    let mut borrow = cache.borrow_mut();
                    if borrow.len() >= STAMP_CACHE_CAP {
                        borrow.clear();
                    }
                    borrow.insert(key, rec);
                });
                return;
            }
        }
        self.stamp_uncached(board, id, item, None);
    }

    fn stamp_uncached(&mut self, board: &Board, id: ItemId, item: &board::Item, mut rec: Option<&mut CachedStamp>) {
        let cells = self.nx * self.ny;
        let half_width = self.profile.half_width;
        let via_radius = self.profile.via_radius;
        let drill_radius = self.profile.drill_radius;
        let reach = self.reach();
        let rules = &board.rules;
        if let Kind::Keepout { forbids, .. } = &item.kind {
            // Its cutouts forbid nothing: a cell well inside one is left as it is.
            let holes = board.keepout_holes.get(&id);
            let (x0, y0, nx, h) = (self.x0, self.y0, self.nx, self.h);
            let open = |c: usize, margin: Nm| {
                let centre = Point::new(x0 + (2 * (c % nx) as Nm + 1) * h / 2, y0 + (2 * (c / nx) as Nm + 1) * h / 2);
                holes.is_some_and(|hs| hs.iter().any(|hole| board::within(hole, &Shape::Capsule { a: centre, b: centre, r: 0 }, margin)))
            };
            if forbids.tracks {
                let mut spans = Vec::new();
                self.cells_near_in(&item.shape, half_width + reach, |c| {
                    if !open(c, half_width + reach) {
                        push_span(&mut spans, c);
                    }
                });
                self.apply_owner_spans(cells, item.layers, 0, &spans);
            }
            if forbids.vias && item.layers != 0 {
                let mut spans = Vec::new();
                self.cells_near_in(&item.shape, via_radius, |c| {
                    if !open(c, via_radius + reach) {
                        push_span(&mut spans, c);
                    }
                });
                self.apply_via_spans(0, &spans);
            }
            return;
        }
        // Room for a pour's thermal spoke its pad cannot spare: other nets keep their clearance
        // from it, and the zone's where wider, as from the pour's own copper. One it can spare
        // is priced instead (`region_rasters`), and the checker refuses the cut too many.
        if let Kind::Spoke { clearance: zone, spare, .. } = item.kind {
            if spare > 0 {
                return;
            }
            let implicit = self.clearance_to(board, item.net);
            let clearance = self.rule_to(board, "clearance", id, item, false, implicit).max(zone);
            let via_clearance = self.rule_to(board, "clearance", id, item, true, implicit).max(zone);
            let mut spans = Vec::new();
            self.stamp_owner_near(&item.shape, half_width + clearance + reach, cells, item.layers, item.net, None, &mut spans);
            self.stamp_via_near(&item.shape, via_radius + via_clearance, item.net, None, &mut spans);
            return;
        }
        let mut spans = Vec::new();
        if item.is_copper() && (self.pours_block || !item.refilled()) {
            // A pad's own clearance stands in for the classes and the custom rules in KiCad's
            // judgement (`Board::clearance_between`), lower too; the router keeps the wider of the
            // two, which KiCad always accepts: on PCBench, using a pad's narrower clearance let
            // other nets box a pad in more often than it opened a way (2026-10-30).
            let own = board.pad_clearances.get(&id).copied().unwrap_or(0);
            let implicit = self.clearance_to(board, item.net);
            let clearance = self.rule_to(board, "clearance", id, item, false, implicit).max(own);
            let via_clearance = self.rule_to(board, "clearance", id, item, true, implicit).max(own);
            self.stamp_owner_near(&item.shape, half_width + clearance + reach, cells, item.layers, item.net, rec.as_deref_mut(), &mut spans);
            self.stamp_via_near(&item.shape, via_radius + via_clearance, item.net, rec.as_deref_mut(), &mut spans);
            // Where the mask leaves vias bare, a via of this net may not open over this copper:
            // it keeps the opening's reach from it.
            let (bare, by) = board.via_openings;
            if bare & item.layers != 0 && by + board.mask_margin() > via_clearance {
                self.stamp_via_near(&item.shape, via_radius + by + board.mask_margin(), item.net, rec.as_deref_mut(), &mut spans);
            }
            // A pad's solder mask opening, or a bare via's, where wider than the clearance:
            // other nets' tracks and vias stay out of it on the layers it opens.
            let opening = match item.kind {
                Kind::Pad { .. } => board.mask_openings.get(&id).copied(),
                Kind::Via if bare != 0 => Some((bare, by)),
                _ => None,
            };
            if let Some((layers, by)) = opening {
                let keep = by + board.mask_margin();
                // A bare via's own opening keeps the mask's narrowest web from this one too.
                let via_keep = if layers & bare != 0 { keep + board.via_openings.1 + board.mask_web } else { keep };
                if keep > clearance {
                    self.stamp_owner_near(&item.shape, half_width + keep + reach, cells, layers, item.net, rec.as_deref_mut(), &mut spans);
                }
                if via_keep > via_clearance {
                    self.stamp_via_near(&item.shape, via_radius + via_keep, item.net, rec.as_deref_mut(), &mut spans);
                }
            }
        }
        // A via's hole keeps the hole clearance from this copper, the board's or a custom rule's
        // (a fab's for through-hole pads, say): where that reaches further than the via's copper
        // and its clearance (a board whose rules ask more of holes than of copper), vias of other
        // nets keep it.
        let plain_hole = nm(rules.min_hole_clearance);
        if item.is_copper() && (self.pours_block || !item.refilled()) {
            let via_hole = if board.custom.has("hole_clearance") { self.rule_to(board, "hole_clearance", id, item, true, plain_hole) } else { plain_hole };
            let own = board.pad_clearances.get(&id).copied().unwrap_or(0);
            let via_clearance = self.rule_to(board, "clearance", id, item, true, self.clearance_to(board, item.net)).max(own);
            if drill_radius + via_hole > via_radius + via_clearance {
                self.stamp_via_near(&item.shape, drill_radius + via_hole, item.net, rec.as_deref_mut(), &mut spans);
            }
        }
        // A physical hole clearance to a pad holds for vias of the pad's own net too (KiCad flags
        // a via drilled in its own SMD pad): no via's drill comes that close.
        if matches!(item.kind, Kind::Pad { .. }) && item.hole.is_none() && board.custom.has("physical_hole_clearance") {
            let physical = self.rule_to(board, "physical_hole_clearance", id, item, true, 0);
            if physical > 0 {
                self.stamp_via_near(&item.shape, drill_radius + physical, 0, None, &mut spans);
            }
        }
        if let Some(hole) = board.hole_shape(id, item) {
            let hole_clearance = self.rule_to(board, "hole_clearance", id, item, true, plain_hole);
            let track_hole = self.rule_to(board, "hole_clearance", id, item, false, plain_hole);
            // An unplated hole's pad may carry a clearance of its own, wider than the rule's.
            let track_hole = if item.plated_hole() { track_hole } else { track_hole.max(board.pad_clearances.get(&id).copied().unwrap_or(0)) };
            let hole_clearance = if item.plated_hole() { hole_clearance } else { hole_clearance.max(board.pad_clearances.get(&id).copied().unwrap_or(0)) };
            let to_hole = self.rule_to(board, "hole_to_hole", id, item, true, nm(rules.min_hole_to_hole));
            // A physical hole clearance holds whatever the nets: no via of any net comes closer.
            let physical = self.rule_to(board, "physical_hole_clearance", id, item, true, 0);
            let margin = (drill_radius + to_hole).max(via_radius + hole_clearance).max(via_radius + physical);
            // A via of the same net may stand on its own via, but no via may share a hole.
            self.stamp_via_near(&hole, margin, 0, rec.as_deref_mut(), &mut spans);
            // Tracks keep the hole clearance from every hole, unplated ones too.
            let all_layers = if self.layers >= 64 { u64::MAX } else { (1u64 << self.layers) - 1 };
            let hole_net = if item.plated_hole() { item.net } else { 0 };
            self.stamp_owner_near(&hole, half_width + track_hole + reach, cells, all_layers, hole_net, rec, &mut spans);
        }
    }

    /// Checks if any cell in the map is claimed by an item other than `net`.
    pub fn blocked_at(&self, net: u32, index: usize) -> bool {
        let o = self.owner[index];
        o != FREE && o != net as i32
    }

    /// Whether a via of `net` at the 2D `cell` is claimed by another net's item.
    pub fn via_blocked_at(&self, net: u32, cell: usize) -> bool {
        let o = self.via_owner[cell];
        o != FREE && o != net as i32
    }

    /// Sets `grid`'s geometry and net profile without copying the blocked or penalty arrays, for
    /// [`crate::route::lay`] when only cell coordinates, widths, vias, and rules are needed.
    pub(crate) fn header_into(&self, net: u32, grid: &mut RouteGrid) {
        grid.net = net;
        (grid.x0, grid.y0, grid.h, grid.nx, grid.ny) = (self.x0, self.y0, self.h, self.nx, self.ny);
        grid.layers.clear();
        grid.layers.extend(0..self.layers);
        grid.half_width = self.profile.half_width;
        grid.via = Some(ViaSpec { radius: self.profile.via_radius, drill_radius: self.profile.drill_radius });
        grid.pours_block = self.pours_block;
    }

    /// The grid a net sees: a cell is blocked unless nothing claims it or only this net does.
    /// `grid` made over as this map's view for `net`, keeping its buffers.
    pub fn view_into(&self, net: u32, grid: &mut RouteGrid) {
        let net_i = net as i32;
        self.header_into(net, grid);
        grid.blocked.resize(self.owner.len(), false);
        for (dst, &o) in grid.blocked.iter_mut().zip(&self.owner) {
            *dst = o != FREE && o != net_i;
        }
        grid.via_blocked.resize(self.via_owner.len(), false);
        for (dst, &o) in grid.via_blocked.iter_mut().zip(&self.via_owner) {
            *dst = o != FREE && o != net_i;
        }
        grid.penalty.clear();
        grid.via_penalty.clone_from(&self.via_avoid);
        grid.region_cost.clear();
        grid.directional = false;
        grid.spokes.clone_from(&self.spokes);
        self.apply_regions(net, grid);
        self.apply_under(net, grid);
    }

    pub fn view(&self, net: u32) -> RouteGrid {
        let net_i = net as i32;
        let mut grid = RouteGrid {
            net,
            x0: self.x0,
            y0: self.y0,
            h: self.h,
            nx: self.nx,
            ny: self.ny,
            layers: (0..self.layers).collect(),
            half_width: self.profile.half_width,
            via: Some(ViaSpec { radius: self.profile.via_radius, drill_radius: self.profile.drill_radius }),
            blocked: self.owner.iter().map(|&o| o != FREE && o != net_i).collect(),
            via_blocked: self.via_owner.iter().map(|&o| o != FREE && o != net_i).collect(),
            penalty: Vec::new(),
            via_penalty: self.via_avoid.clone(),
            region_cost: Vec::new(),
            pours_block: self.pours_block,
            directional: false,
            deadline: Default::default(),
            spokes: self.spokes.clone(),
        };
        self.apply_regions(net, &mut grid);
        self.apply_under(net, &mut grid);
        grid
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GridStampKey {
    x0: Nm,
    y0: Nm,
    h: Nm,
    nx: usize,
    ny: usize,
    layers: usize,
    pours_block: bool,
    half_width: Nm,
    via_radius: Nm,
    drill_radius: Nm,
    clearance: Nm,
    min_hole_clearance: Nm,
    min_hole_to_hole: Nm,
    via_open_layers: u64,
    via_open_by: Nm,
    mask_web: Nm,
    mask_margin: Nm,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ItemStampKey {
    is_via: bool,
    net: u32,
    layers: u64,
    a: Point,
    b: Point,
    r: Nm,
    hole: Option<(Point, Nm)>,
    clearance: Nm,
}

type SpanList = Vec<(u32, u16)>;

#[derive(Default)]
struct CachedStamp {
    owner_ops: Vec<(u64, u32, SpanList)>,
    via_ops: Vec<(u32, SpanList)>,
}

#[inline]
fn push_span(spans: &mut Vec<(u32, u16)>, c: usize) {
    debug_assert!(c <= u32::MAX as usize);
    let c32 = c as u32;
    if let Some((start, len)) = spans.last_mut() {
        if *start + u32::from(*len) == c32 && *len < u16::MAX {
            *len += 1;
            return;
        }
    }
    spans.push((c32, 1));
}

#[inline]
fn claim_slice(slice: &mut [i32], net: u32) {
    if net == 0 {
        slice.fill(SEVERAL);
    } else {
        let n = net as i32;
        for cell in slice {
            let c = *cell;
            *cell = if c == FREE || c == n { n } else { SEVERAL };
        }
    }
}

/// `cells` as runs of one claim: the claim and how many cells in a row have it.
fn runs(cells: &[i32]) -> Vec<(i32, u32)> {
    let mut out: Vec<(i32, u32)> = Vec::new();
    let mut i = 0;
    while i < cells.len() {
        let v = cells[i];
        let mut j = i + 1;
        while j < cells.len() && cells[j] == v {
            j += 1;
        }
        let mut count = j - i;
        while count > u32::MAX as usize {
            out.push((v, u32::MAX));
            count -= u32::MAX as usize;
        }
        out.push((v, count as u32));
        i = j;
    }
    out.shrink_to_fit();
    out
}

fn unrun(runs: &[(i32, u32)], cells: &mut [i32]) {
    let mut at = 0;
    for &(o, n) in runs {
        cells[at..at + n as usize].fill(o);
        at += n as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_board_with_no_outline_keeps_its_maps_size_whatever_the_router_lays() {
        let (pcb, copper) = kicad_pcb::parse_board(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal)) (net 0 \"\") (net 1 \"A\")
             (footprint \"R\" (layer \"F.Cu\") (at 10 10) (property \"Reference\" \"R1\")
               (pad \"1\" smd rect (at -1 0) (size 1 1) (layers \"F.Cu\") (net 1 \"A\"))
               (pad \"2\" smd rect (at 1 0) (size 1 1) (layers \"F.Cu\") (net 1 \"A\"))))",
        )
        .unwrap();
        let mut board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        assert!(board.outline.is_empty());
        let size = |b: &Board| {
            let m = ObstacleMap::new(b, Profile::for_net(b, 1), 50_000, false);
            (m.x0, m.y0, m.nx, m.ny)
        };
        let before = size(&board);
        // A route of the router's swinging out into the room round the parts.
        board.insert(board::Item {
            kind: Kind::Track,
            net: 1,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(9_000_000, 10_000_000), b: Point::new(9_000_000, 12_500_000), r: 100_000 },
            hole: None,
            locked: false,
            span: None,
        });
        assert_eq!(size(&board), before);
    }

    #[test]
    fn a_window_map_agrees_cell_for_cell_with_a_full_board_map() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pcb");
        let board = Board::load(&path).unwrap().0;
        let profile = Profile::for_net(&board, 1);
        let h = 50_000;
        for pours_block in [false, true] {
            let full = ObstacleMap::new(&board, profile.clone(), h, pours_block);
            let (i0, j0, wn, hn) = (full.nx / 3, full.ny / 3, 80.min(full.nx / 2), 80.min(full.ny / 2));
            let bounds = board::BBox {
                min: Point::new(full.x0 + i0 as Nm * h, full.y0 + j0 as Nm * h),
                max: Point::new(full.x0 + (i0 + wn) as Nm * h, full.y0 + (j0 + hn) as Nm * h),
            };
            let win = ObstacleMap::window(&board, profile.clone(), h, bounds, pours_block);
            assert_eq!((win.nx, win.ny), (wn, hn));
            for j in 0..win.ny {
                for i in 0..win.nx {
                    let wc = j * win.nx + i;
                    let fc = (j0 + j) * full.nx + (i0 + i);
                    assert_eq!(win.via_owner[wc], full.via_owner[fc], "via_owner mismatch at ({i}, {j}) pours={pours_block}");
                    for l in 0..full.layers {
                        assert_eq!(
                            win.owner[l * win.nx * win.ny + wc],
                            full.owner[l * full.nx * full.ny + fc],
                            "owner mismatch on layer {l} at ({i}, {j}) pours={pours_block}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn fixed_copper_from_the_cache_gives_the_map_stamped_in_board_order() {
        let h = 50_000;
        // Every item in board order, the outline first, as maps were built before the cache.
        let direct = |board: &Board, like: &ObstacleMap| {
            let mut map = ObstacleMap {
                pours_block: like.pours_block,
                profile: like.profile.clone(),
                x0: like.x0,
                y0: like.y0,
                h,
                nx: like.nx,
                ny: like.ny,
                layers: like.layers,
                owner: vec![FREE; like.owner.len()],
                via_owner: vec![FREE; like.via_owner.len()],
                via_avoid: Vec::new(),
                regions: Vec::new(),
                spokes: None,
                under: Vec::new(),
            };
            map.stamp_outline(board);
            map.stamp_mask_areas(board, &Deadline::default());
            let ids: Vec<ItemId> = board.items().map(|(id, _)| id).collect();
            for id in ids {
                map.stamp(board, id);
            }
            map
        };
        let same = |a: &ObstacleMap, b: &ObstacleMap| a.owner == b.owner && a.via_owner == b.via_owner;
        for name in ["earring.kicad_pcb", "bluebuzz-rm2.kicad_pcb"] {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name);
            let mut board = Board::load(&path).unwrap().0;
            for net in [1, 2] {
                for pours_block in [false, true] {
                    let profile = Profile::for_net(&board, net);
                    let first = ObstacleMap::new(&board, profile.clone(), h, pours_block);
                    let again = ObstacleMap::new(&board, profile, h, pours_block);
                    assert!(same(&first, &direct(&board, &first)), "{name} net {net} pours {pours_block}");
                    assert!(same(&again, &first), "{name} cached, net {net} pours {pours_block}");
                }
            }
            // A track laid since: the fixed copper comes from the cache, the track is stamped on it.
            let at = ObstacleMap::new(&board, Profile::for_net(&board, 1), h, false);
            let mid = Point::new(at.x0 + at.nx as Nm * h / 2, at.y0 + at.ny as Nm * h / 2);
            board.insert(board::Item {
                kind: Kind::Track,
                net: 1,
                layers: 1,
                shape: Shape::Capsule { a: mid, b: Point::new(mid.x + 1_500_000, mid.y), r: 75_000 },
                hole: None,
                locked: false,
                span: None,
            });
            let cached = ObstacleMap::new(&board, at.profile.clone(), h, false);
            assert!(same(&cached, &direct(&board, &cached)) && !same(&cached, &at), "{name}");
        }
    }

    #[test]
    fn a_subwindow_of_the_fixed_copper_agrees_cell_for_cell_with_a_window() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pcb");
        let mut board = Board::load(&path).unwrap().0;
        let at = board.footprints[0].at;
        let mm = 1_000_000;
        board.via_avoid.push(Shape::Polygon(vec![
            Point::new(at.x - mm, at.y - mm),
            Point::new(at.x + mm, at.y - mm),
            Point::new(at.x + mm, at.y + mm),
            Point::new(at.x - mm, at.y + mm),
        ]));
        let profile = Profile::for_net(&board, 1);
        let h = 50_000;
        for pours_block in [false, true] {
            let moving = |b: &Board, id: ItemId| b.item(id).is_some_and(|it| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked);
            let fixed = ObstacleMap::without(&board, profile.clone(), h, pours_block, &|id| moving(&board, id));
            let added = board.insert(board::Item {
                kind: Kind::Track,
                net: 2,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(fixed.x0 + 15 * h, fixed.y0 + 15 * h), b: Point::new(fixed.x0 + 45 * h, fixed.y0 + 35 * h), r: 100_000 },
                hole: None,
                locked: false,
                span: None,
            });
            for (fx, fy) in [(0, 0), (fixed.nx / 4, fixed.ny / 3), (fixed.nx / 2, fixed.ny / 2)] {
                let (wn, hn) = (90.min(fixed.nx - fx), 90.min(fixed.ny - fy));
                let bounds = board::BBox {
                    min: Point::new(fixed.x0 + fx as Nm * h, fixed.y0 + fy as Nm * h),
                    max: Point::new(fixed.x0 + (fx + wn) as Nm * h, fixed.y0 + (fy + hn) as Nm * h),
                };
                let sub = fixed.subwindow(&board, bounds);
                let win = ObstacleMap::window(&board, profile.clone(), h, bounds, pours_block);
                assert_eq!((sub.x0, sub.y0, sub.nx, sub.ny), (win.x0, win.y0, win.nx, win.ny));
                assert!(sub.owner == win.owner && sub.via_owner == win.via_owner && sub.via_avoid == win.via_avoid, "at ({fx}, {fy}) pours {pours_block}");
            }
            board.discard(added);
        }
    }

    fn cells_near_reference(map: &ObstacleMap, shape: &Shape, margin: Nm, f: &mut dyn FnMut(usize)) {
        let range = |lo: Nm, hi: Nm, origin: Nm, n: usize| {
            if n == 0 {
                return 0..0;
            }
            let a = ((lo - origin) as f64 / map.h as f64 - 0.5).floor().max(0.0) as usize;
            let b = (((hi - origin) as f64 / map.h as f64 - 0.5).ceil().max(0.0) as usize).min(n - 1);
            a..b + 1
        };
        match shape {
            Shape::Capsule { a, b, r } => {
                let (min_x, max_x) = (a.x.min(b.x) - r - margin, a.x.max(b.x) + r + margin);
                let (min_y, max_y) = (a.y.min(b.y) - r - margin, a.y.max(b.y) + r + margin);
                let limit = (r + margin) as f64;
                let limit2 = limit * limit;
                if a == b {
                    for j in range(min_y, max_y, map.y0, map.ny) {
                        let dy = (map.centre(0, j).y - a.y) as f64;
                        if dy * dy >= limit2 {
                            continue;
                        }
                        for i in range(min_x, max_x, map.x0, map.nx) {
                            let dx = (map.centre(i, j).x - a.x) as f64;
                            if dx * dx + dy * dy < limit2 {
                                f(j * map.nx + i);
                            }
                        }
                    }
                } else {
                    let (ax, ay, bx, by) = (a.x as f64, a.y as f64, b.x as f64, b.y as f64);
                    let (dx, dy) = (bx - ax, by - ay);
                    let len2 = dx * dx + dy * dy;
                    for j in range(min_y, max_y, map.y0, map.ny) {
                        let y = map.centre(0, j).y as f64;
                        let (row_min_x, row_max_x) = if dy.abs() > 1.0 {
                            let t0 = ((y - limit - ay) / dy).clamp(0.0, 1.0);
                            let t1 = ((y + limit - ay) / dy).clamp(0.0, 1.0);
                            let x0 = ax + t0 * dx;
                            let x1 = ax + t1 * dx;
                            (((x0.min(x1) - limit - 2.0).floor() as Nm).max(min_x), ((x0.max(x1) + limit + 2.0).ceil() as Nm).min(max_x))
                        } else {
                            (min_x, max_x)
                        };
                        for i in range(row_min_x, row_max_x, map.x0, map.nx) {
                            let x = map.centre(i, j).x as f64;
                            let t = (((x - ax) * dx + (y - ay) * dy) / len2).clamp(0.0, 1.0);
                            let (qx, qy) = (ax + t * dx, ay + t * dy);
                            let d2 = (x - qx) * (x - qx) + (y - qy) * (y - qy);
                            if d2 < limit2 && (d2 < limit2 * 0.999999 || (x - qx).hypot(y - qy) < limit) {
                                f(j * map.nx + i);
                            }
                        }
                    }
                }
            }
            Shape::Polygon(poly) => {
                if poly.is_empty() {
                    return;
                }
                let bb = board::BBox::of(poly.iter().copied());
                let mut xs: Vec<f64> = Vec::new();
                for j in range(bb.min.y, bb.max.y, map.y0, map.ny) {
                    let y = map.centre(0, j).y;
                    xs.clear();
                    for k in 0..poly.len() {
                        let (p, q) = (poly[k], poly[(k + 1) % poly.len()]);
                        if (p.y > y) != (q.y > y) {
                            xs.push(p.x as f64 + (y - p.y) as f64 * (q.x - p.x) as f64 / (q.y - p.y) as f64);
                        }
                    }
                    xs.sort_by(f64::total_cmp);
                    for span in xs.chunks_exact(2) {
                        for i in range(span[0].floor() as Nm, span[1].ceil() as Nm, map.x0, map.nx) {
                            let x = map.centre(i, j).x as f64;
                            if x > span[0] && x < span[1] {
                                f(j * map.nx + i);
                            }
                        }
                    }
                }
                if margin > 0 {
                    for k in 0..poly.len() {
                        let (p, q) = (poly[k], poly[(k + 1) % poly.len()]);
                        cells_near_reference(map, &Shape::Capsule { a: p, b: q, r: 0 }, margin, f);
                    }
                }
            }
        }
    }

    #[test]
    fn cells_near_matches_reference_across_shapes_and_windows() {
        let map = ObstacleMap {
            pours_block: false,
            profile: Profile {
                class: String::new(),
                half_width: 100_000,
                narrow_half_width: 100_000,
                min_half_width: 100_000,
                board_min_half_width: 100_000,
                clearance: 100_000,
                via_radius: 200_000,
                drill_radius: 100_000,
                net: Some(1),
            },
            x0: 1_000_000,
            y0: 2_000_000,
            h: 25_000,
            nx: 64,
            ny: 52,
            layers: 2,
            owner: Vec::new(),
            via_owner: Vec::new(),
            via_avoid: Vec::new(),
            regions: Vec::new(),
            spokes: None,
        };
        let mut seed: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            seed
        };
        let mut shapes = vec![
            // Point, horizontal, vertical, 1-nm shallow, 45-degree, and out-of-window shapes.
            (Shape::Capsule { a: Point::new(1_500_000, 2_500_000), b: Point::new(1_500_000, 2_500_000), r: 125_000 }, 50_000),
            (Shape::Capsule { a: Point::new(1_112_500, 2_312_500), b: Point::new(2_412_500, 2_312_500), r: 75_000 }, 100_000),
            (Shape::Capsule { a: Point::new(2_412_500, 2_312_500), b: Point::new(1_112_500, 2_312_500), r: 75_000 }, 100_000),
            (Shape::Capsule { a: Point::new(1_612_500, 2_050_000), b: Point::new(1_612_500, 3_150_000), r: 80_000 }, 95_000),
            (Shape::Capsule { a: Point::new(1_612_500, 3_150_000), b: Point::new(1_612_500, 2_050_000), r: 80_000 }, 95_000),
            (Shape::Capsule { a: Point::new(1_200_000, 2_400_000), b: Point::new(2_200_000, 2_400_001), r: 60_000 }, 65_000),
            (Shape::Capsule { a: Point::new(200_000, 500_000), b: Point::new(600_000, 900_000), r: 50_000 }, 50_000),
            (Shape::Capsule { a: Point::new(500_000, 2_500_000), b: Point::new(1_200_000, 2_500_000), r: 100_000 }, 75_000),
            (
                Shape::Polygon(vec![
                    Point::new(1_200_000, 2_200_000),
                    Point::new(2_100_000, 2_250_000),
                    Point::new(1_900_000, 2_600_000),
                    Point::new(2_200_000, 3_050_000),
                    Point::new(1_300_000, 2_950_000),
                ]),
                0,
            ),
            (
                Shape::Polygon(vec![
                    Point::new(1_200_000, 2_200_000),
                    Point::new(2_100_000, 2_250_000),
                    Point::new(1_900_000, 2_600_000),
                    Point::new(2_200_000, 3_050_000),
                    Point::new(1_300_000, 2_950_000),
                ]),
                85_000,
            ),
        ];
        for k in 0..300 {
            let ax = 700_000 + (next() % 2_400_000) as Nm;
            let ay = 1_700_000 + (next() % 2_000_000) as Nm;
            let (bx, by) = match k % 5 {
                0 => (ax, ay),
                1 => (700_000 + (next() % 2_400_000) as Nm, ay),
                2 => (ax, 1_700_000 + (next() % 2_000_000) as Nm),
                3 => (700_000 + (next() % 2_400_000) as Nm, ay + (next() % 3) as Nm - 1),
                _ => (700_000 + (next() % 2_400_000) as Nm, 1_700_000 + (next() % 2_000_000) as Nm),
            };
            let r = (next() % 180_000) as Nm;
            let margin = (next() % 180_000) as Nm;
            shapes.push((Shape::Capsule { a: Point::new(ax, ay), b: Point::new(bx, by), r }, margin));
        }
        for (idx, (shape, margin)) in shapes.iter().enumerate() {
            let (mut got, mut want) = (Vec::new(), Vec::new());
            map.cells_near(shape, *margin, &mut |c| got.push(c));
            cells_near_reference(&map, shape, *margin, &mut |c| want.push(c));
            assert_eq!(got, want, "shape {idx}: {shape:?} margin={margin}");
        }
    }

    #[test]
    fn window_without_and_stamp_cache_match_uncached_board_order() {
        let raw_window = |board: &Board, profile: Profile, h: Nm, bounds: board::BBox, pours_block: bool, skip: &dyn Fn(ItemId) -> bool| {
            let (x0, y0) = (bounds.min.x, bounds.min.y);
            let nx = ((bounds.max.x - x0) / h).max(1) as usize;
            let ny = ((bounds.max.y - y0) / h).max(1) as usize;
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
                via_avoid: Vec::new(),
                regions: Vec::new(),
                spokes: None,
            };
            map.stamp_outline(board);
            map.stamp_via_avoid(board);
            map.stamp_mask_areas(board, &Deadline::default());
            let max_clr = (0..board.nets.len() as u32).map(|n| map.clearance_to(board, n)).max().unwrap_or(map.profile.clearance);
            let hole_margin = (map.profile.drill_radius + nm(board.rules.min_hole_to_hole))
                .max(map.profile.via_radius + nm(board.rules.min_hole_clearance))
                .max(map.profile.half_width + nm(board.rules.min_hole_clearance) + map.reach());
            let copper_margin = (map.profile.half_width + max_clr + map.reach()).max(map.profile.via_radius + max_clr);
            let reach_box = board::BBox { min: Point::new(x0, y0), max: Point::new(x0 + nx as Nm * h, y0 + ny as Nm * h) }.grow(copper_margin.max(hole_margin) + h);
            for (id, it) in board.items() {
                if !skip(id) && board.bbox(id).is_some_and(|b| b.overlaps(reach_box)) {
                    map.stamp_uncached(board, id, it, None);
                }
            }
            map
        };
        for name in ["earring.kicad_pcb", "bluebuzz-rm2.kicad_pcb"] {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name);
            let mut board = Board::load(&path).unwrap().0;
            let base = ObstacleMap::new(&board, Profile::for_net(&board, 1), 50_000, false);
            let (mx, my) = (base.x0 + base.nx as Nm * 25_000, base.y0 + base.ny as Nm * 25_000);
            let t1 = board.insert(board::Item {
                kind: Kind::Track,
                net: 1,
                layers: 1,
                shape: Shape::Capsule { a: Point::new(mx - 800_000, my), b: Point::new(mx + 800_000, my), r: 100_000 },
                hole: None,
                locked: false,
                span: None,
            });
            let t2 = board.insert(board::Item {
                kind: Kind::Track,
                net: 2,
                layers: 2.min((1u64 << board.layers.len()) - 1),
                shape: Shape::Capsule { a: Point::new(mx, my - 900_000), b: Point::new(mx, my + 900_000), r: 100_000 },
                hole: None,
                locked: false,
                span: None,
            });
            let v1 = board.insert(board::Item {
                kind: Kind::Via,
                net: 2,
                layers: (1u64 << board.layers.len()) - 1,
                shape: Shape::Capsule { a: Point::new(mx + 200_000, my + 150_000), b: Point::new(mx + 200_000, my + 150_000), r: 225_000 },
                hole: Some((Point::new(mx + 200_000, my + 150_000), 125_000)),
                locked: false,
                span: None,
            });
            let bounds = board::BBox { min: Point::new(mx - 1_500_000, my - 1_400_000), max: Point::new(mx + 1_600_000, my + 1_500_000) };
            for h in [50_000, 25_000] {
                for skipped in [usize::MAX, t1, t2, v1] {
                    let profile = Profile::for_net(&board, 1);
                    let got = ObstacleMap::window_without(&board, profile.clone(), h, bounds, false, &|id| id == skipped);
                    let got_again = ObstacleMap::window_without(&board, profile.clone(), h, bounds, false, &|id| id == skipped);
                    let want = raw_window(&board, profile, h, bounds, false, &|id| id == skipped);
                    assert_eq!(got.owner, want.owner, "{name} h={h} skip={skipped} owner");
                    assert_eq!(got.via_owner, want.via_owner, "{name} h={h} skip={skipped} via_owner");
                    assert_eq!(got_again.owner, want.owner, "{name} h={h} skip={skipped} cached owner");
                    assert_eq!(got_again.via_owner, want.via_owner, "{name} h={h} skip={skipped} cached via_owner");
                }
            }
            board.discard(v1);
            board.discard(t2);
            board.discard(t1);
        }
    }
}
