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
    pub clearance: Nm,
    pub via_radius: Nm,
    pub drill_radius: Nm,
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
        let b = pad.shape.bbox();
        // Not `clamp`: with no track width in the rules the floor is above the zero width.
        ((b.max.x - b.min.x).min(b.max.y - b.min.y) / 2).max(self.min_half_width).min(self.half_width)
    }

    pub fn for_net(board: &Board, net: u32) -> Profile {
        let rules = &board.rules;
        let class = rules.class_of(board.net_name(net));
        let take = |v: Option<f64>, floor: f64| nm(v.unwrap_or(floor).max(floor));
        let min_width = rules.min_track_width_of(board.net_name(net));
        let half_width = take(class.map(|c| c.track_width), min_width) / 2;
        // A net whose class is wider than the board's default carries current: it keeps its
        // width all the way into its pads, however tight; only signals neck to the minimum.
        let signal = rules.classes.iter().find(|c| c.name == "Default").map_or(0.0, |c| c.track_width).max(rules.min_track_width);
        let power = class.is_some_and(|c| c.track_width > signal + 1e-9);
        Profile {
            class: class.map_or_else(String::new, |c| c.name.clone()),
            half_width,
            narrow_half_width: if power { half_width } else { (nm(min_width) / 2).clamp(1, half_width.max(1)) },
            min_half_width: (nm(min_width) / 2).clamp(1, half_width.max(1)),
            clearance: take(class.map(|c| c.clearance), rules.min_clearance),
            // A class's via may leave a thinner ring round its drill than the board allows; the
            // via grows to keep the ring, the drill as the class has it.
            via_radius: (take(class.map(|c| c.via_diameter), rules.min_via_diameter) / 2)
                .max(take(class.map(|c| c.via_drill), rules.min_via_drill) / 2 + nm(rules.min_via_annular_width)),
            drill_radius: take(class.map(|c| c.via_drill), rules.min_via_drill) / 2,
        }
    }
}

/// What a board's map covers: its outline, or with none its items' extent and room round it, not
/// only their corners nearest the origin, which left a map too small to route round anything.
pub(crate) fn map_extent(board: &Board) -> board::BBox {
    let b = board.outline.iter().flatten().copied().fold(None, |b: Option<board::BBox>, p| {
        Some(b.map_or(board::BBox::of([p]), |b| b.union(board::BBox::of([p]))))
    });
    b.unwrap_or_else(|| {
        let items = board.items().map(|(_, it)| it.shape.bbox()).reduce(board::BBox::union);
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
}

impl RouteGrid {
    /// What the item breaks, as this grid's routes are judged.
    pub fn violations(&self, board: &Board, id: ItemId) -> Vec<board::Violation> {
        if self.pours_block { board::check_item_with_pours(board, id) } else { board::check_item(board, id) }
    }
}

impl RouteGrid {
    pub fn penalty_at(&self, index: usize) -> u32 {
        self.penalty.get(index).copied().unwrap_or(0) + self.region_cost.get(index).copied().unwrap_or(0)
    }

    pub fn via_penalty_at(&self, cell: usize) -> u32 {
        self.via_penalty.get(cell).copied().unwrap_or(0)
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
    pub(crate) layers: usize,
    pub(crate) owner: Vec<i32>,
    pub(crate) via_owner: Vec<i32>,
    /// Per cell: the extra cost of a via there, in the board's via-avoid areas; empty for none.
    pub(crate) via_avoid: Vec<u32>,
    /// The board's routing regions on these cells, applied to each net's view.
    pub(crate) regions: Vec<crate::regions::RegionRaster>,
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
        };
        map.stamp_outline(board);
        map.stamp_via_avoid(board);
        map.regions = map.region_rasters(board);
        map.stamp_mask_areas(board, &Deadline::default());
        let max_clr = (0..board.nets.len() as u32)
            .map(|n| map.clearance_to(board, n))
            .max()
            .unwrap_or(map.profile.clearance);
        let hole_margin = (map.profile.drill_radius + nm(board.rules.min_hole_to_hole))
            .max(map.profile.via_radius + nm(board.rules.min_hole_clearance))
            .max(map.profile.half_width + nm(board.rules.min_hole_clearance) + map.reach());
        let copper_margin = (map.profile.half_width + max_clr + map.reach()).max(map.profile.via_radius + max_clr);
        let reach_box = board::BBox {
            min: Point::new(x0, y0),
            max: Point::new(x0 + nx as Nm * h, y0 + ny as Nm * h),
        }
        .grow(copper_margin.max(hole_margin) + h);
        let ids: Vec<ItemId> = board
            .items()
            .filter_map(|(id, _)| board.bbox(id).filter(|b| b.overlaps(reach_box)).map(|_| id))
            .collect();
        for id in ids {
            map.stamp(board, id);
        }
        map
    }

    /// This map's cells within `bounds`, as [`ObstacleMap::window`] would make them over a map
    /// of the fixed copper alone (a map that left out every unlocked track and via): the cells
    /// are copied, and only the unlocked tracks and vias in reach are stamped, not the pads and
    /// pours again.
    pub fn subwindow(&self, board: &Board, bounds: board::BBox) -> ObstacleMap {
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
        };
        map.regions = map.region_rasters(board);
        if !self.via_avoid.is_empty() {
            map.via_avoid = (j0..j1).flat_map(|j| self.via_avoid[j * self.nx + i0..j * self.nx + i0 + nx].iter().copied()).collect();
        }
        // As far as `window` looks for copper whose claims reach into it.
        let max_clr = (0..board.nets.len() as u32).map(|n| map.clearance_to(board, n)).max().unwrap_or(map.profile.clearance);
        let p = &map.profile;
        let hole_margin = (p.drill_radius + nm(board.rules.min_hole_to_hole))
            .max(p.via_radius + nm(board.rules.min_hole_clearance))
            .max(p.half_width + nm(board.rules.min_hole_clearance) + map.reach());
        let copper_margin = (p.half_width + max_clr + map.reach()).max(p.via_radius + max_clr);
        let reach = board::BBox { min: Point::new(x0, y0), max: Point::new(x0 + nx as Nm * h, y0 + ny as Nm * h) }.grow(copper_margin.max(hole_margin) + h);
        let mut ids: Vec<ItemId> = (0..=board.layers.len()).flat_map(|slot| board.near(slot, reach)).collect();
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            if board.item(id).is_some_and(|it| !it.locked && matches!(it.kind, Kind::Track | Kind::Via)) {
                map.stamp(board, id);
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
        };
        map.stamp_via_avoid(board);
        // What does not move while routing (outline, pads, pours, keepouts) is stamped once and
        // copied after: plane pours and hundreds of pads cost far more to stamp than the copy, and
        // a try of the shove builds maps over and over. Claims commute (a cell is free, one net's,
        // or several nets'), so stamping them before the tracks gives the same map.
        let (mut fixed, mut moving) = (Vec::new(), Vec::new());
        for (id, it) in board.items().filter(|(id, _)| !skip(*id)) {
            if matches!(it.kind, Kind::Track | Kind::Via) { &mut moving } else { &mut fixed }.push(id);
        }
        if !map.stamp_fixed(board, &fixed, deadline) {
            return None;
        }
        map.regions = map.region_rasters(board);
        if !map.stamp_mask_areas(board, deadline) {
            return None;
        }
        for (k, id) in moving.into_iter().enumerate() {
            if k % STAMPS_PER_CHECK == STAMPS_PER_CHECK - 1 && deadline.expired() {
                return None;
            }
            map.stamp(board, id);
        }
        Some(map)
    }

    /// The outline and `fixed`, from the last few maps' if one had the same; false, and nothing
    /// kept, if `deadline` passes first.
    fn stamp_fixed(&mut self, board: &Board, fixed: &[ItemId], deadline: &Deadline) -> bool {
        // A few, for the classes a board's maps are built for. Kept as runs of cells alike,
        // which fixed copper leaves long: a whole map each would cost a phone's tab too dear.
        const KEPT: usize = 4;
        type Kept = Vec<(u128, Vec<(i32, u32)>, Vec<(i32, u32)>)>;
        thread_local! {
            static CACHE: std::cell::RefCell<Kept> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let key = self.fixed_key(board, fixed);
        let hit = CACHE.with(|cache| {
            let cache = cache.borrow();
            let Some((_, owner, via_owner)) = cache.iter().find(|(k, ..)| *k == key) else { return false };
            unrun(owner, &mut self.owner);
            unrun(via_owner, &mut self.via_owner);
            true
        });
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
        CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.len() >= KEPT {
                cache.remove(0);
            }
            cache.push((key, runs(&self.owner), runs(&self.via_owner)));
        });
        true
    }

    /// Everything [`ObstacleMap::stamp_fixed`] stamps from: the grid, the profile, the rules, the
    /// outline and every fixed item as it stands, hashed twice over for 128 bits.
    fn fixed_key(&self, board: &Board, fixed: &[ItemId]) -> u128 {
        use std::hash::{Hash, Hasher};
        let feed = |h: &mut std::collections::hash_map::DefaultHasher| {
            (self.x0, self.y0, self.h, self.nx, self.ny, self.layers, self.pours_block).hash(h);
            let p = &self.profile;
            (p.half_width, p.via_radius, p.drill_radius, p.clearance).hash(h);
            let r = &board.rules;
            [r.min_copper_edge_clearance, r.min_clearance, r.min_hole_clearance, r.pth_hole_clearance, r.min_hole_to_hole].map(f64::to_bits).hash(h);
            board.outline.hash(h);
            board.outline_edges.iter().map(|&(a, b, _)| (a, b)).collect::<Vec<_>>().hash(h);
            (board.via_openings, board.mask_web).hash(h);
            for &id in fixed {
                let Some(it) = board.item(id) else { continue };
                (id, it.net, it.layers, it.refilled(), it.hole, self.clearance_to(board, it.net)).hash(h);
                (board.pad_clearances.get(&id), board.mask_openings.get(&id)).hash(h);
                match &it.kind {
                    Kind::Pad { plated, .. } => (1u8, *plated).hash(h),
                    Kind::Pour { kept, .. } => (2u8, *kept).hash(h),
                    Kind::Keepout { forbids, .. } => (3u8, forbids.tracks, forbids.vias).hash(h),
                    Kind::Track | Kind::Via => 4u8.hash(h),
                }
                match &it.shape {
                    Shape::Capsule { a, b, r } => (a, b, r).hash(h),
                    Shape::Polygon(pts) => pts.hash(h),
                }
            }
        };
        let (mut h1, mut h2) = (std::collections::hash_map::DefaultHasher::new(), std::collections::hash_map::DefaultHasher::new());
        0x9e37_79b9_7f4a_7c15_u64.hash(&mut h2);
        feed(&mut h1);
        feed(&mut h2);
        (u128::from(h1.finish()) << 64) | u128::from(h2.finish())
    }

    /// Openings drawn in the mask: only the net they bare may run there; none where they bare
    /// none or several. False if `deadline` passes first: a board can draw thousands.
    fn stamp_mask_areas(&mut self, board: &Board, deadline: &Deadline) -> bool {
        let cells = self.nx * self.ny;
        let (track, via) = (self.profile.half_width + board::MASK_MARGIN + self.reach(), self.profile.via_radius + board::MASK_MARGIN);
        for (k, (shape, layers, net)) in board.mask_areas.iter().enumerate() {
            if k % MASKS_PER_CHECK == MASKS_PER_CHECK - 1 && deadline.expired() {
                return false;
            }
            let mut hit = Vec::new();
            self.cells_near(shape, track, &mut |c| hit.push(c));
            for l in (0..self.layers).filter(|l| layers & (1 << l) != 0) {
                for &c in &hit {
                    ObstacleMap::claim(&mut self.owner[l * cells + c], *net);
                }
            }
            hit.clear();
            self.cells_near(shape, via, &mut |c| hit.push(c));
            for c in hit {
                ObstacleMap::claim(&mut self.via_owner[c], *net);
            }
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

    fn claim(cell: &mut i32, net: u32) {
        let net = if net == 0 { SEVERAL } else { net as i32 };
        *cell = match *cell {
            FREE => net,
            c if c == net => c,
            _ => SEVERAL,
        };
    }

    /// Calls `f` for every cell whose centre lies closer than `margin` to `shape`.
    pub(crate) fn cells_near(&self, shape: &Shape, margin: Nm, f: &mut dyn FnMut(usize)) {
        let range = |lo: Nm, hi: Nm, origin: Nm, n: usize| {
            let a = ((lo - origin) as f64 / self.h as f64 - 0.5).floor().max(0.0) as usize;
            let b = (((hi - origin) as f64 / self.h as f64 - 0.5).ceil().max(0.0) as usize).min(n.saturating_sub(1));
            a..=b
        };
        match shape {
            Shape::Capsule { a, b, r } => {
                let bb = shape.bbox().grow(margin);
                let limit = (*r + margin) as f64;
                if a == b {
                    let limit2 = limit * limit;
                    for j in range(bb.min.y, bb.max.y, self.y0, self.ny) {
                        let dy = (self.centre(0, j).y - a.y) as f64;
                        let dy2 = dy * dy;
                        if dy2 >= limit2 {
                            continue;
                        }
                        for i in range(bb.min.x, bb.max.x, self.x0, self.nx) {
                            let dx = (self.centre(i, j).x - a.x) as f64;
                            if dx * dx + dy2 < limit2 {
                                f(j * self.nx + i);
                            }
                        }
                    }
                } else {
                    let (ax, ay, bx, by) = (a.x as f64, a.y as f64, b.x as f64, b.y as f64);
                    let (dx, dy) = (bx - ax, by - ay);
                    let len2 = dx * dx + dy * dy;
                    let limit2 = limit * limit;
                    for j in range(bb.min.y, bb.max.y, self.y0, self.ny) {
                        let y = self.centre(0, j).y as f64;
                        let (row_min_x, row_max_x) = if dy.abs() > 1.0 {
                            let t0 = ((y - limit - ay) / dy).clamp(0.0, 1.0);
                            let t1 = ((y + limit - ay) / dy).clamp(0.0, 1.0);
                            let x0 = ax + t0 * dx;
                            let x1 = ax + t1 * dx;
                            (
                                ((x0.min(x1) - limit - 2.0).floor() as Nm).max(bb.min.x),
                                ((x0.max(x1) + limit + 2.0).ceil() as Nm).min(bb.max.x),
                            )
                        } else {
                            (bb.min.x, bb.max.x)
                        };
                        for i in range(row_min_x, row_max_x, self.x0, self.nx) {
                            let x = self.centre(i, j).x as f64;
                            let t = (((x - ax) * dx + (y - ay) * dy) / len2).clamp(0.0, 1.0);
                            let (qx, qy) = (ax + t * dx, ay + t * dy);
                            let d2 = (x - qx) * (x - qx) + (y - qy) * (y - qy);
                            if d2 < limit2 && (d2 < limit2 * 0.999999 || (x - qx).hypot(y - qy) < limit) {
                                f(j * self.nx + i);
                            }
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
            board.outline.iter().map(Vec::len).sum::<usize>() + board.outline_edges.len(),
            outer.and_then(|o| o.first().copied()),
            outer.and_then(|o| o.last().copied()),
        );
        type OutlineCache = Option<(OutlineKey, Vec<i32>, Vec<i32>)>;
        thread_local! {
            static CACHE: std::cell::RefCell<OutlineCache> = const { std::cell::RefCell::new(None) };
        }
        let hit = CACHE.with(|cache| {
            let borrow = cache.borrow();
            if let Some((k, owner, via_owner)) = borrow.as_ref() {
                if *k == key {
                    self.owner.copy_from_slice(owner);
                    self.via_owner.copy_from_slice(via_owner);
                    return true;
                }
            }
            false
        });
        if hit {
            return;
        }
        let cells = self.nx * self.ny;
        // Without a closed outline there is no inside to keep to, only the edges.
        let mut inside = vec![outer.is_none(); cells];
        if let Some(outer) = outer {
            self.cells_near(&Shape::Polygon(outer.clone()), 0, &mut |c| inside[c] = true);
        }
        for hole in board.outline.iter().skip(1) {
            self.cells_near(&Shape::Polygon(hole.clone()), 0, &mut |c| inside[c] = false);
        }
        for c in (0..cells).filter(|c| !inside[*c]) {
            self.via_owner[c] = SEVERAL;
            for l in 0..self.layers {
                self.owner[l * cells + c] = SEVERAL;
            }
        }
        for &(a, b, _) in &board.outline_edges {
            {
                let edge_shape = Shape::Capsule { a, b, r: 0 };
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
        CACHE.with(|cache| {
            *cache.borrow_mut() = Some((key, self.owner.clone(), self.via_owner.clone()));
        });
    }

    /// The clearance between this class's copper and an item of `net`.
    pub(crate) fn clearance_to(&self, board: &Board, net: u32) -> Nm {
        self.profile.clearance.max(board.clearance(net, net))
    }

    /// Records what `id` claims.
    pub fn stamp(&mut self, board: &Board, id: ItemId) {
        let Some(item) = board.item(id) else { return };
        let cells = self.nx * self.ny;
        let half_width = self.profile.half_width;
        let via_radius = self.profile.via_radius;
        let drill_radius = self.profile.drill_radius;
        let reach = self.reach();
        let rules = &board.rules;
        if let Kind::Keepout { forbids, .. } = &item.kind {
            if forbids.tracks {
                let mut hit = Vec::new();
                self.cells_near(&item.shape, half_width + reach, &mut |c| hit.push(c));
                for l in (0..self.layers).filter(|l| item.on(*l)) {
                    for &c in &hit {
                        self.owner[l * cells + c] = SEVERAL;
                    }
                }
            }
            if forbids.vias && item.layers != 0 {
                let mut hit = Vec::new();
                self.cells_near(&item.shape, via_radius, &mut |c| hit.push(c));
                for c in hit {
                    self.via_owner[c] = SEVERAL;
                }
            }
            return;
        }
        if item.is_copper() && (self.pours_block || !item.refilled()) {
            let clearance = self.clearance_to(board, item.net).max(board.pad_clearances.get(&id).copied().unwrap_or(0));
            let mut hit = Vec::new();
            self.cells_near(&item.shape, half_width + clearance + reach, &mut |c| hit.push(c));
            for l in (0..self.layers).filter(|l| item.on(*l)) {
                for &c in &hit {
                    ObstacleMap::claim(&mut self.owner[l * cells + c], item.net);
                }
            }
            hit.clear();
            self.cells_near(&item.shape, via_radius + clearance, &mut |c| hit.push(c));
            for c in hit {
                ObstacleMap::claim(&mut self.via_owner[c], item.net);
            }
            // Where the mask leaves vias bare, a via of this net may not open over this copper:
            // it keeps the opening's reach from it.
            let (bare, by) = board.via_openings;
            if bare & item.layers != 0 && by + board::MASK_MARGIN > clearance {
                let mut hit = Vec::new();
                self.cells_near(&item.shape, via_radius + by + board::MASK_MARGIN, &mut |c| hit.push(c));
                for c in hit {
                    ObstacleMap::claim(&mut self.via_owner[c], item.net);
                }
            }
            // A pad's solder mask opening, or a bare via's, where wider than the clearance:
            // other nets' tracks and vias stay out of it on the layers it opens.
            let opening = match item.kind {
                Kind::Pad { .. } => board.mask_openings.get(&id).copied(),
                Kind::Via if bare != 0 => Some((bare, by)),
                _ => None,
            };
            if let Some((layers, by)) = opening {
                let keep = by + board::MASK_MARGIN;
                // A bare via's own opening keeps the mask's narrowest web from this one too.
                let via_keep = if layers & bare != 0 { keep + board.via_openings.1 + board.mask_web } else { keep };
                if keep > clearance {
                    let mut hit = Vec::new();
                    self.cells_near(&item.shape, half_width + keep + reach, &mut |c| hit.push(c));
                    for l in (0..self.layers).filter(|l| layers & (1 << l) != 0) {
                        for &c in &hit {
                            ObstacleMap::claim(&mut self.owner[l * cells + c], item.net);
                        }
                    }
                }
                if via_keep > clearance {
                    let mut hit = Vec::new();
                    self.cells_near(&item.shape, via_radius + via_keep, &mut |c| hit.push(c));
                    for c in hit {
                        ObstacleMap::claim(&mut self.via_owner[c], item.net);
                    }
                }
            }
        }
        // A through-hole pad may carry a stricter hole clearance from custom rules, both ways
        // round: from its hole to other copper, and from other holes (a via's) to its copper.
        let through_hole = matches!(item.kind, Kind::Pad { plated: true, .. }) && item.hole.is_some();
        let hole_clearance = if through_hole { nm(rules.min_hole_clearance.max(rules.pth_hole_clearance)) } else { nm(rules.min_hole_clearance) };
        if through_hole && rules.pth_hole_clearance > rules.min_hole_clearance {
            let mut hit = Vec::new();
            self.cells_near(&item.shape, drill_radius + hole_clearance, &mut |c| hit.push(c));
            for c in hit {
                self.via_owner[c] = SEVERAL;
            }
        }
        if let Some((centre, r)) = item.hole {
            let hole = Shape::Capsule { a: centre, b: centre, r };
            // An unplated hole's pad may carry a clearance of its own, wider than the rule's.
            let hole_clearance = if item.plated_hole() { hole_clearance } else { hole_clearance.max(board.pad_clearances.get(&id).copied().unwrap_or(0)) };
            let margin = (drill_radius + nm(rules.min_hole_to_hole)).max(via_radius + hole_clearance);
            let mut hit = Vec::new();
            self.cells_near(&hole, margin, &mut |c| hit.push(c));
            for c in hit {
                // A via of the same net may stand on its own via, but no via may share a hole.
                self.via_owner[c] = SEVERAL;
            }
            // Tracks keep the hole clearance from every hole, unplated ones too.
            let mut hit = Vec::new();
            self.cells_near(&hole, half_width + hole_clearance + reach, &mut |c| hit.push(c));
            for l in 0..self.layers {
                for &c in &hit {
                    ObstacleMap::claim(&mut self.owner[l * cells + c], if item.plated_hole() { item.net } else { 0 });
                }
            }
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

    /// The grid a net sees: a cell is blocked unless nothing claims it or only this net does.
    /// `grid` made over as this map's view for `net`, keeping its buffers.
    pub fn view_into(&self, net: u32, grid: &mut RouteGrid) {
        let free = |o: i32| o == FREE || o == net as i32;
        grid.net = net;
        (grid.x0, grid.y0, grid.h, grid.nx, grid.ny) = (self.x0, self.y0, self.h, self.nx, self.ny);
        grid.layers.clear();
        grid.layers.extend(0..self.layers);
        grid.half_width = self.profile.half_width;
        grid.via = Some(ViaSpec { radius: self.profile.via_radius, drill_radius: self.profile.drill_radius });
        grid.blocked.clear();
        grid.blocked.extend(self.owner.iter().map(|&o| !free(o)));
        grid.via_blocked.clear();
        grid.via_blocked.extend(self.via_owner.iter().map(|&o| !free(o)));
        grid.penalty.clear();
        grid.via_penalty.clone_from(&self.via_avoid);
        grid.region_cost.clear();
        grid.pours_block = self.pours_block;
        grid.directional = false;
        self.apply_regions(net, grid);
    }

    pub fn view(&self, net: u32) -> RouteGrid {
        let free = |o: i32| o == FREE || o == net as i32;
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
            blocked: self.owner.iter().map(|&o| !free(o)).collect(),
            via_blocked: self.via_owner.iter().map(|&o| !free(o)).collect(),
            penalty: Vec::new(),
            via_penalty: self.via_avoid.clone(),
            region_cost: Vec::new(),
            pours_block: self.pours_block,
            directional: false,
            deadline: Default::default(),
        };
        self.apply_regions(net, &mut grid);
        grid
    }
}

/// `cells` as runs of one claim: the claim and how many cells in a row have it.
fn runs(cells: &[i32]) -> Vec<(i32, u32)> {
    let mut out: Vec<(i32, u32)> = Vec::new();
    for &c in cells {
        match out.last_mut() {
            Some((o, n)) if *o == c => *n += 1,
            _ => out.push((c, 1)),
        }
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
}
