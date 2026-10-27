//! Pre-fanout of fine-pitch packages before global routing.
//!
//! On a 0.4 mm QFN or 0.5 mm WLCSP, routing connections one after another lets early traces and
//! vias close every legal exit from neighbouring pads before those pads' nets are ever tried.
//! Pre-fanout claims each pad's escape while the fanout zone around the package is still bare:
//! first bridging pads that belong to a local kept pour, exposed thermal pad, or adjacent
//! same-net pad inside the package, then laying an outward escape per remaining pad cluster onto
//! cells that are free on the global routing grid.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use board::{connectivity, connectivity_without_pours, nm, BBox, Board, Connectivity, Item, ItemId, Kind, Nm, Point, Shape};
use kicad_pcb::Span;

use crate::cleanup::{prune_dangling, pull_tight};
use crate::grid::{ObstacleMap, Profile, RouteGrid};
use crate::negotiate::{route_negotiated, NegotiationOptions};
use crate::report::{item_name, tell, Focus, Log};
use crate::route::{anchor, item_terminals, lay, route_open, track, CpuSolver, Options, Terminal};
use crate::search::{route_astar, GridPath};

/// Escape geometry laid for each outward fine-pitch pad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeMode {
    /// A straight surface track along the pad's outward normal to the target ring.
    Stub,
    /// A short surface stub to a staggered through-via onto inner layers (classic dogbone).
    Dogbone,
    /// A fine-grid A* route on the pad's surface layer out to the target ring.
    GridRing,
    /// A fine-grid A* route across layers out to the target ring, preferring inner layers so each
    /// dogbone via also reserves an inner-layer channel clear of its neighbours' vias.
    GridInner,
}

/// Order in which a package's candidate pads claim their escapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeOrder {
    /// Clockwise around each package's perimeter.
    Round,
    /// Inner rows, wide net classes, and pads crowded on both sides before outer or corner pads.
    InnerFirst,
    /// Pads whose nets ended open in a short trial route first, then `InnerFirst`.
    TrappedFirst,
}

#[derive(Debug, Clone)]
pub struct PrefanoutOptions {
    /// Fine local grid step around each package; 10 um resolves 0.4 mm dogbone lanes cleanly.
    pub local_cell: Nm,
    /// Global router grid step whose free cells each escape must reach.
    pub global_cell: Nm,
    /// Distance past a package's SMD pad box where the target ring begins, in nanometres.
    pub ring: Nm,
    pub mode: EscapeMode,
    pub order: EscapeOrder,
    /// Join pads locally to same-net kept pours, exposed pads, and nearby sibling pads first.
    pub inward: bool,
    /// Footprints to fan out, by index into `Board::footprints`.
    pub parts: Vec<usize>,
    /// Nets whose pads are left for routing of their own, such as a differential pair's.
    pub skip_nets: HashSet<u32>,
    pub pours_block: bool,
    pub cancel: Option<Arc<AtomicBool>>,
    pub log: Option<Log>,
}

impl Default for PrefanoutOptions {
    fn default() -> Self {
        PrefanoutOptions {
            local_cell: 10_000,
            global_cell: 50_000,
            ring: 1_100_000,
            mode: EscapeMode::GridInner,
            order: EscapeOrder::InnerFirst,
            inward: true,
            parts: Vec::new(),
            skip_nets: HashSet::new(),
            pours_block: false,
            cancel: None,
            log: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrefanoutStats {
    pub parts: usize,
    pub candidates: usize,
    pub bridged_inward: usize,
    pub escaped: usize,
    pub vias: usize,
    pub skipped_no_net: usize,
    pub skipped_pth: usize,
    pub skipped_exposed: usize,
    pub skipped_escaped: usize,
    /// Pads of nets the options leave out.
    pub skipped_nets: usize,
    pub failed: usize,
    pub laid: Vec<ItemId>,
}

/// Footprint-local coordinates (`u`, `v`) aligned with the package's rotation in KiCad's Y-down
/// board frame (`x = at.x + u cos - (-v sin)`, specifically `x' = x cos + y sin, y' = y cos - x sin`).
#[derive(Debug, Clone)]
struct Frame {
    at: Point,
    cos: f64,
    sin: f64,
    u_min: f64,
    u_max: f64,
    v_min: f64,
    v_max: f64,
    world_bbox: BBox,
}

impl Frame {
    fn of(board: &Board, footprint: usize, smd_pads: &[ItemId]) -> Option<Frame> {
        if smd_pads.is_empty() {
            return None;
        }
        let fp = board.footprints.get(footprint)?;
        let (sin, cos) = fp.rotation_deg.to_radians().sin_cos();
        let to_local = |p: Point| -> (f64, f64) {
            let (dx, dy) = ((p.x - fp.at.x) as f64, (p.y - fp.at.y) as f64);
            (dx * cos - dy * sin, dx * sin + dy * cos)
        };
        let mut u_min = f64::INFINITY;
        let mut u_max = f64::NEG_INFINITY;
        let mut v_min = f64::INFINITY;
        let mut v_max = f64::NEG_INFINITY;
        let mut world: Option<BBox> = None;
        for &id in smd_pads {
            let Some(item) = board.item(id) else { continue };
            let bb = item.shape.bbox();
            world = Some(world.map_or(bb, |w| w.union(bb)));
            match &item.shape {
                Shape::Polygon(pts) => {
                    for &p in pts {
                        let (u, v) = to_local(p);
                        u_min = u_min.min(u);
                        u_max = u_max.max(u);
                        v_min = v_min.min(v);
                        v_max = v_max.max(v);
                    }
                }
                Shape::Capsule { a, b, r } => {
                    let rf = *r as f64;
                    for &p in [a, b] {
                        let (u, v) = to_local(p);
                        u_min = u_min.min(u - rf);
                        u_max = u_max.max(u + rf);
                        v_min = v_min.min(v - rf);
                        v_max = v_max.max(v + rf);
                    }
                }
            }
        }
        Some(Frame { at: fp.at, cos, sin, u_min, u_max, v_min, v_max, world_bbox: world? })
    }

    fn to_local(&self, p: Point) -> (f64, f64) {
        let (dx, dy) = ((p.x - self.at.x) as f64, (p.y - self.at.y) as f64);
        (dx * self.cos - dy * self.sin, dx * self.sin + dy * self.cos)
    }

    fn dir_to_board(&self, du: f64, dv: f64) -> (f64, f64) {
        (du * self.cos + dv * self.sin, dv * self.cos - du * self.sin)
    }

    /// Euclidean distance outside the package's oriented SMD pad rectangle; 0 on or inside it.
    fn ring_depth(&self, p: Point) -> f64 {
        let (u, v) = self.to_local(p);
        let du = (self.u_min - u).max(u - self.u_max).max(0.0);
        let dv = (self.v_min - v).max(v - self.v_max).max(0.0);
        du.hypot(dv)
    }

    fn pad_spans(&self, shape: &Shape) -> (f64, f64, f64, f64) {
        match shape {
            Shape::Polygon(pts) => {
                let (mut u0, mut u1, mut v0, mut v1) = (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
                for &p in pts {
                    let (u, v) = self.to_local(p);
                    u0 = u0.min(u);
                    u1 = u1.max(u);
                    v0 = v0.min(v);
                    v1 = v1.max(v);
                }
                ((u0 + u1) * 0.5, (v0 + v1) * 0.5, (u1 - u0).max(0.0), (v1 - v0).max(0.0))
            }
            Shape::Capsule { a, b, r } => {
                let (ua, va) = self.to_local(*a);
                let (ub, vb) = self.to_local(*b);
                let rf = *r as f64;
                let (u0, u1) = (ua.min(ub) - rf, ua.max(ub) + rf);
                let (v0, v1) = (va.min(vb) - rf, va.max(vb) + rf);
                ((u0 + u1) * 0.5, (v0 + v1) * 0.5, u1 - u0, v1 - v0)
            }
        }
    }

    /// Outward unit direction, tangent unit direction, inward depth from the nearest side of the
    /// SMD pad box, and offset from the middle of that side.
    fn side_info(&self, cu: f64, cv: f64) -> ((f64, f64), (f64, f64), f64, f64) {
        let u_mid = (self.u_min + self.u_max) * 0.5;
        let v_mid = (self.v_min + self.v_max) * 0.5;
        let sides = [
            (cu - self.u_min, (-1.0, 0.0), (0.0, 1.0), (cv - v_mid).abs()),
            (self.u_max - cu, (1.0, 0.0), (0.0, 1.0), (cv - v_mid).abs()),
            (cv - self.v_min, (0.0, -1.0), (1.0, 0.0), (cu - u_mid).abs()),
            (self.v_max - cv, (0.0, 1.0), (1.0, 0.0), (cu - u_mid).abs()),
        ];
        let &(gap, out_l, along_l, off_mid) =
            sides.iter().min_by(|a, b| a.0.total_cmp(&b.0)).expect("four sides");
        (self.dir_to_board(out_l.0, out_l.1), self.dir_to_board(along_l.0, along_l.1), gap.max(0.0), off_mid)
    }
}

#[derive(Debug, Clone)]
struct PadInfo {
    id: ItemId,
    net: u32,
    layer: usize,
    centre: Point,
    angle: f64,
    edge_gap: f64,
    off_mid: f64,
    out: (f64, f64),
    along: (f64, f64),
    crowding: usize,
    mouth_room: usize,
    seq_index: usize,
    profile_reach: Nm,
}

/// Free global-grid cells that a previously laid escape relies on, so no later escape on a
/// neighbouring pad may choke off the last free global cell of an earlier one.
struct EscapedHandoff {
    net: u32,
    class: String,
    cells: Vec<usize>,
}

fn board_connectivity(board: &Board, pours_block: bool) -> Connectivity {
    if pours_block { connectivity(board) } else { connectivity_without_pours(board) }
}

fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// Aligns a window's origin so cell centres land on exact multiples of `h` (for instance, exact
/// 10 um coordinates when `h == 10_000`, matching 0.40 mm and 0.50 mm pad centres).
fn aligned_window(raw: BBox, h: Nm) -> BBox {
    let half = h / 2;
    let snap_down = |v: Nm| v.div_euclid(h) * h - half;
    let snap_up = |v: Nm| (v + h - 1).div_euclid(h) * h + half;
    BBox { min: Point::new(snap_down(raw.min.x), snap_down(raw.min.y)), max: Point::new(snap_up(raw.max.x), snap_up(raw.max.y)) }
}

/// Runs a short trial route on a copy of `board` to discover which nets end up trapped without
/// pre-fanout, for `EscapeOrder::TrappedFirst`.
fn probe_trapped_nets(board: &Board, options: &PrefanoutOptions) -> HashSet<u32> {
    let mut copy = board.clone();
    let mut solver = CpuSolver;
    route_negotiated(
        &mut copy,
        &mut solver,
        &NegotiationOptions {
            cell: options.global_cell,
            max_rounds: 6,
            patience: 2,
            threads: 4,
            max_seconds: Some(4.0),
            pours_block: options.pours_block,
            cancel: options.cancel.clone(),
            ..Default::default()
        },
        &mut |_| {},
        &mut |_, _| {},
    );
    route_open(
        &mut copy,
        &mut solver,
        &Options {
            cell: options.global_cell,
            max_seconds: Some(2.0),
            pours_block: options.pours_block,
            cancel: options.cancel.clone(),
            ..Default::default()
        },
        &mut |_| {},
    );
    board_connectivity(&copy, options.pours_block).airlines.into_iter().map(|a| a.net).collect()
}

/// Lays pre-fanout escapes for `options.parts` on `board`, leaving every laid piece legal under
/// the board's rules and reachable on the global grid.
pub fn prefanout(board: &mut Board, options: &PrefanoutOptions, watch: &mut dyn FnMut(&Board)) -> PrefanoutStats {
    let mut stats = PrefanoutStats::default();
    let cancelled = || options.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));
    if options.parts.is_empty() || cancelled() {
        return stats;
    }

    let trapped_nets = if options.order == EscapeOrder::TrappedFirst {
        probe_trapped_nets(board, options)
    } else {
        HashSet::new()
    };

    // Order parts from most pads to fewest so the densest package (such as U1's QFN-60) claims
    // shared channels between neighbouring parts first.
    let mut parts = options.parts.clone();
    parts.sort_by_key(|&fp| {
        let count = board
            .items()
            .filter(|(_, it)| matches!(it.kind, Kind::Pad { footprint: f, .. } if f == fp) && it.is_copper() && it.hole.is_none() && it.net != 0)
            .count();
        (std::cmp::Reverse(count), fp)
    });
    parts.dedup();

    let mut global_maps: HashMap<String, ObstacleMap> = HashMap::new();
    let mut escaped_handoffs: Vec<EscapedHandoff> = Vec::new();

    for fp in parts {
        if cancelled() {
            break;
        }
        let copper_pads: Vec<ItemId> = board
            .items()
            .filter(|(_, it)| matches!(it.kind, Kind::Pad { footprint: f, .. } if f == fp) && it.is_copper())
            .map(|(id, _)| id)
            .collect();
        if copper_pads.len() < 4 {
            continue;
        }
        stats.parts += 1;

        let mut smd_pads = Vec::new();
        for &id in &copper_pads {
            let Some(it) = board.item(id) else { continue };
            if it.hole.is_some() {
                if it.net == 0 {
                    stats.skipped_no_net += 1;
                } else {
                    stats.skipped_pth += 1;
                }
                continue;
            }
            smd_pads.push(id);
        }
        let Some(frame) = Frame::of(board, fp, &smd_pads) else { continue };

        let spans: Vec<(ItemId, f64, f64, f64, f64)> = smd_pads
            .iter()
            .filter_map(|&id| {
                let it = board.item(id)?;
                let (cu, cv, du, dv) = frame.pad_spans(&it.shape);
                Some((id, cu, cv, du.max(dv), du.min(dv)))
            })
            .collect();
        let med_max = median(spans.iter().map(|s| s.3).collect());
        let med_min = median(spans.iter().map(|s| s.4).collect());

        let mut exposed_pads = Vec::new();
        let mut raw_pads = Vec::new();
        for &(id, cu, cv, _smax, smin) in &spans {
            let Some(it) = board.item(id) else { continue };
            let (out, along, gap, off_mid) = frame.side_info(cu, cv);
            let is_exposed = smd_pads.len() > 4 && gap > med_max * 0.9 && smin > med_min * 1.8;
            if it.net == 0 {
                stats.skipped_no_net += 1;
                continue;
            }
            if is_exposed {
                stats.skipped_exposed += 1;
                exposed_pads.push(id);
                continue;
            }
            let Some(layer) = (0..board.layers.len()).find(|l| it.on(*l)) else { continue };
            let bb = it.shape.bbox();
            let centre = anchor(it, Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2));
            let profile = Profile::for_net(board, it.net);
            raw_pads.push(PadInfo {
                id,
                net: it.net,
                layer,
                centre,
                angle: cv.atan2(cu),
                edge_gap: gap,
                off_mid,
                out,
                along,
                crowding: 0,
                mouth_room: 25,
                seq_index: 0,
                profile_reach: profile.half_width + profile.clearance,
            });
        }

        // Measure local crowding along the pad row and assign perimeter sequence numbers.
        raw_pads.sort_by(|a, b| a.angle.total_cmp(&b.angle));
        let all_smd_centres: Vec<(ItemId, Point, u32)> = smd_pads
            .iter()
            .filter_map(|&id| {
                let it = board.item(id)?;
                if exposed_pads.contains(&id) {
                    return None;
                }
                let bb = it.shape.bbox();
                Some((id, Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2), it.net))
            })
            .collect();
        for (idx, p) in raw_pads.iter_mut().enumerate() {
            p.seq_index = idx;
            p.crowding = all_smd_centres
                .iter()
                .filter(|(oid, c, net)| *oid != p.id && *net != 0 && *net != p.net && c.distance(p.centre) <= 650_000.0)
                .count();
        }

        tell(&options.log, "prefanout", || format!("{}: preparing escapes", board.footprints[fp].reference), Some(Focus::Part(fp)));

        // Step 1: local inward bridges to kept pours, exposed pads, and nearby sibling pads.
        if options.inward && !raw_pads.is_empty() {
            let bridged = bridge_inward(board, fp, &frame, &raw_pads, &exposed_pads, options, &mut global_maps, &mut stats);
            if bridged > 0 {
                watch(board);
            }
        }

        // Step 2: recompute connectivity and group pads by cluster, skipping clusters already
        // escaped out of the package (for instance via a kept pour finger or thermal vias).
        let conn = board_connectivity(board, options.pours_block);
        let open_nets: HashSet<u32> = conn.airlines.iter().map(|a| a.net).collect();
        let mut members_by_cluster: HashMap<usize, Vec<ItemId>> = HashMap::new();
        for (&item_id, &cid) in &conn.cluster {
            members_by_cluster.entry(cid).or_default().push(item_id);
        }
        let escape_horizon = frame.world_bbox.grow((options.ring / 2).max(300_000));
        let cluster_escaped = |cid: usize| -> bool {
            let Some(members) = members_by_cluster.get(&cid) else { return false };
            members.iter().any(|&m| {
                let Some(it) = board.item(m) else { return false };
                match &it.kind {
                    Kind::Via => true,
                    Kind::Pad { footprint: ofp, .. } => *ofp != fp || it.hole.is_some(),
                    Kind::Track | Kind::Pour { kept: true, .. } => {
                        let b = board.bbox(m).unwrap_or_else(|| it.shape.bbox());
                        b.min.x < escape_horizon.min.x
                            || b.max.x > escape_horizon.max.x
                            || b.min.y < escape_horizon.min.y
                            || b.max.y > escape_horizon.max.y
                    }
                    _ => false,
                }
            })
        };

        let win_bounds = aligned_window(frame.world_bbox.grow(options.ring + 650_000), options.local_cell);
        let mut local_maps: HashMap<(String, Nm), ObstacleMap> = HashMap::new();

        let escaped_kept_pours: Vec<(ItemId, u32, BBox)> = board
            .items()
            .filter_map(|(id, it)| {
                if !matches!(it.kind, Kind::Pour { kept: true, .. }) || it.net == 0 {
                    return None;
                }
                let cid = *conn.cluster.get(&id)?;
                cluster_escaped(cid).then(|| (id, it.net, board.bbox(id).unwrap_or_else(|| it.shape.bbox())))
            })
            .collect();

        let mut by_cluster: BTreeMap<usize, Vec<PadInfo>> = BTreeMap::new();
        for mut p in raw_pads {
            if !open_nets.contains(&p.net) {
                stats.skipped_escaped += 1;
                continue;
            }
            if options.skip_nets.contains(&p.net) {
                stats.skipped_nets += 1;
                continue;
            }
            let Some(&cid) = conn.cluster.get(&p.id) else { continue };
            if cluster_escaped(cid) {
                stats.skipped_escaped += 1;
                continue;
            }
            let mut local_profile = Profile::for_net(board, p.net);
            local_profile.half_width = local_profile.neck_into(board.item(p.id));
            let key = (local_profile.class.clone(), local_profile.half_width);
            let lmap = local_maps
                .entry(key)
                .or_insert_with(|| ObstacleMap::window(board, local_profile, options.local_cell, win_bounds, options.pours_block));
            p.mouth_room = measure_mouth_room(&p, lmap);
            by_cluster.entry(cid).or_default().push(p);
        }
        if by_cluster.is_empty() {
            continue;
        }

        // Within a multi-pad cluster inside the package (such as VDD_CORE pads bridged through an
        // inner ring pour), try the least-crowded pad first; once one escapes, the rest are done.
        let mut groups: Vec<Vec<PadInfo>> = by_cluster
            .into_values()
            .map(|mut g| {
                g.sort_by(|a, b| {
                    b.mouth_room
                        .cmp(&a.mouth_room)
                        .then_with(|| a.crowding.cmp(&b.crowding))
                        .then_with(|| b.off_mid.total_cmp(&a.off_mid))
                });
                g
            })
            .collect();

        order_groups(&mut groups, options.order, &trapped_nets);
        stats.candidates += groups.len();
        let mut opp_via_guards: HashMap<usize, Vec<bool>> = HashMap::new();

        for g_idx in 0..groups.len() {
            if cancelled() {
                break;
            }
            let siblings: Vec<PadInfo> = groups[g_idx + 1..].iter().map(|g| g[0].clone()).collect();
            let extra_in_group = groups[g_idx].len().saturating_sub(1);
            let mut group_ok = false;
            for pad in groups[g_idx].clone() {
                let profile = Profile::for_net(board, pad.net);
                global_maps
                    .entry(profile.class.clone())
                    .or_insert_with(|| ObstacleMap::new(board, profile.clone(), options.global_cell, options.pours_block));
                let opp_via_guard = opp_via_guards
                    .entry(pad.layer)
                    .or_insert_with(|| compute_opposite_via_guard(board, pad.layer, win_bounds, options.local_cell));
                let pad_escaped_pours: Vec<ItemId> = escaped_kept_pours
                    .iter()
                    .filter(|(_, net, bb)| *net == pad.net && bb.overlaps(win_bounds))
                    .map(|(id, _, _)| *id)
                    .collect();

                if let Some((items, handoff_cells)) = lay_pad_escape(
                    board,
                    &frame,
                    &pad,
                    &profile,
                    profile.neck_into(board.item(pad.id)),
                    win_bounds,
                    options,
                    &siblings,
                    opp_via_guard,
                    &pad_escaped_pours,
                    &mut local_maps,
                    &global_maps,
                    &escaped_handoffs,
                ) {
                    let vias = items.iter().filter(|id| board.item(**id).is_some_and(|it| it.kind == Kind::Via)).count();
                    stats.escaped += 1;
                    stats.vias += vias;
                    stats.skipped_escaped += extra_in_group;
                    for map in local_maps.values_mut() {
                        for &id in &items {
                            map.stamp(board, id);
                        }
                    }
                    for map in global_maps.values_mut() {
                        for &id in &items {
                            map.stamp(board, id);
                        }
                    }
                    for h in &mut escaped_handoffs {
                        if let Some(gmap) = global_maps.get(&h.class) {
                            h.cells.retain(|&idx| !gmap.blocked_at(h.net, idx));
                        }
                    }
                    escaped_handoffs.push(EscapedHandoff { net: pad.net, class: profile.class, cells: handoff_cells });
                    stats.laid.extend(items);
                    tell(
                        &options.log,
                        "prefanout",
                        || format!("{}: escaped {}", board.net_name(pad.net), item_name(board, pad.id)),
                        Some(Focus::Connection { net: pad.net, from: pad.centre, to: pad.centre }),
                    );
                    watch(board);
                    group_ok = true;
                    break;
                }
            }
            if !group_ok {
                stats.failed += 1;
            }
        }
    }

    stats
}

fn measure_mouth_room(pad: &PadInfo, lmap: &ObstacleMap) -> usize {
    let cells = lmap.nx * lmap.ny;
    let mut free = 0;
    for fwd in [200_000.0, 350_000.0, 500_000.0, 650_000.0, 800_000.0] {
        for lat in [-280_000.0, -140_000.0, 0.0, 140_000.0, 280_000.0] {
            let px = pad.centre.x + (pad.out.0 * fwd + pad.along.0 * lat).round() as Nm;
            let py = pad.centre.y + (pad.out.1 * fwd + pad.along.1 * lat).round() as Nm;
            let i = (px - lmap.x0).div_euclid(lmap.h);
            let j = (py - lmap.y0).div_euclid(lmap.h);
            if i >= 0 && j >= 0 && (i as usize) < lmap.nx && (j as usize) < lmap.ny {
                let idx = pad.layer * cells + (j as usize) * lmap.nx + (i as usize);
                if !lmap.blocked_at(pad.net, idx) {
                    free += 1;
                }
            }
        }
    }
    free
}

fn order_groups(groups: &mut [Vec<PadInfo>], order: EscapeOrder, trapped_nets: &HashSet<u32>) {
    let inner_key = |p: &PadInfo| {
        let row_band = (p.edge_gap / 50_000.0).round() as i64;
        // Pads pinched in front by a foreign pour or neighbour part (< 12 of 25 mouth samples
        // free) claim their narrow chute first; normal interior and corner pads share band 3.
        let mouth_band = p.mouth_room.min(12) / 4;
        (
            std::cmp::Reverse(row_band),
            mouth_band,
            std::cmp::Reverse(p.profile_reach),
            std::cmp::Reverse(p.crowding),
            (p.off_mid as i64),
            p.seq_index,
        )
    };
    match order {
        EscapeOrder::Round => groups.sort_by(|a, b| a[0].angle.total_cmp(&b[0].angle)),
        EscapeOrder::InnerFirst => groups.sort_by(|a, b| inner_key(&a[0]).cmp(&inner_key(&b[0]))),
        EscapeOrder::TrappedFirst => groups.sort_by(|a, b| {
            let ta = trapped_nets.contains(&a[0].net);
            let tb = trapped_nets.contains(&b[0].net);
            tb.cmp(&ta).then_with(|| inner_key(&a[0]).cmp(&inner_key(&b[0])))
        }),
    }
}

/// Bridges pads locally inside the package's immediate footprint to same-net kept pours, exposed
/// pads, or nearby sibling pads so power and paired pins do not consume outward lanes, without
/// routing across neighbouring pads' outward mouths.
#[allow(clippy::too_many_arguments)]
fn bridge_inward(
    board: &mut Board,
    fp: usize,
    frame: &Frame,
    pads: &[PadInfo],
    exposed_pads: &[ItemId],
    options: &PrefanoutOptions,
    global_maps: &mut HashMap<String, ObstacleMap>,
    stats: &mut PrefanoutStats,
) -> usize {
    let bounds = aligned_window(frame.world_bbox.grow(350_000), options.local_cell);
    let max_steps = (1_800_000 / options.local_cell.max(1)) as usize;
    let mut maps: HashMap<(String, Nm), ObstacleMap> = HashMap::new();
    let mut bridged = 0;

    for pad in pads {
        let conn = board_connectivity(board, options.pours_block);
        let Some(&pad_cid) = conn.cluster.get(&pad.id) else { continue };
        let profile = Profile::for_net(board, pad.net);

        let mut target_ids: Vec<ItemId> = board
            .near(pad.layer, bounds)
            .into_iter()
            .filter(|&id| {
                if id == pad.id || conn.cluster.get(&id) == Some(&pad_cid) {
                    return false;
                }
                let Some(it) = board.item(id) else { return false };
                if it.net != pad.net || !it.on(pad.layer) {
                    return false;
                }
                match &it.kind {
                    Kind::Pour { kept: true, .. } => true,
                    Kind::Pad { footprint: ofp, .. } if *ofp == fp => {
                        exposed_pads.contains(&id) || it.shape.bbox().grow(1_600_000).overlaps(BBox::of([pad.centre]))
                    }
                    _ => false,
                }
            })
            .collect();
        target_ids.sort_unstable();
        target_ids.dedup();
        if target_ids.is_empty() {
            continue;
        }

        let mut widths = vec![profile.half_width];
        let neck = profile.neck_into(board.item(pad.id));
        if neck < profile.half_width {
            widths.push(neck);
        }
        for hw in widths {
            let mut p = profile.clone();
            p.half_width = hw;
            let key = (profile.class.clone(), hw);
            let map = maps
                .entry(key)
                .or_insert_with(|| ObstacleMap::window(board, p, options.local_cell, bounds, options.pours_block));
            let grid = single_layer_view(map, pad.net, pad.layer);
            let sources = item_terminals(board, &grid, pad.id, hw, 6, 16);
            if sources.is_empty() {
                continue;
            }
            let mut targets: Vec<Terminal> = Vec::new();
            for &tid in &target_ids {
                targets.extend(item_terminals(board, &grid, tid, hw, 6, 16));
            }
            if targets.is_empty() {
                continue;
            }
            let src_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
            let tgt_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
            let Some(path) = route_astar(&grid, &src_cells, &tgt_cells, 1_000) else { continue };
            if path.len() > max_steps {
                continue;
            }
            let Some(items) = lay(board, &grid, &path, &sources, &targets) else { continue };
            for m in maps.values_mut() {
                for &id in &items {
                    m.stamp(board, id);
                }
            }
            for m in global_maps.values_mut() {
                for &id in &items {
                    m.stamp(board, id);
                }
            }
            stats.bridged_inward += 1;
            stats.laid.extend(items);
            bridged += 1;
            break;
        }
    }
    bridged
}

fn single_layer_view(map: &ObstacleMap, net: u32, board_layer: usize) -> RouteGrid {
    let full = map.view(net);
    let cells = full.nx * full.ny;
    let start = board_layer * cells;
    RouteGrid {
        net,
        x0: full.x0,
        y0: full.y0,
        h: full.h,
        nx: full.nx,
        ny: full.ny,
        layers: vec![board_layer],
        half_width: full.half_width,
        via: None,
        blocked: full.blocked[start..start + cells].to_vec(),
        via_blocked: vec![true; cells],
        penalty: Vec::new(),
        via_penalty: Vec::new(),
        region_cost: Vec::new(),
        pours_block: full.pours_block,
    }
}

/// Global-grid cells lying inside `items` that are unblocked on `net`'s global routing grid.
fn handoff_cells_of(board: &Board, items: &[ItemId], net: u32, gmap: &ObstacleMap) -> Vec<usize> {
    let cells = gmap.nx * gmap.ny;
    let mut out = BTreeSet::new();
    for &id in items {
        let Some(item) = board.item(id) else { continue };
        let bb = item.shape.bbox();
        let i0 = ((bb.min.x - gmap.x0).div_euclid(gmap.h)).max(0) as usize;
        let i1 = (((bb.max.x - gmap.x0).div_euclid(gmap.h)).max(0) as usize).min(gmap.nx.saturating_sub(1));
        let j0 = ((bb.min.y - gmap.y0).div_euclid(gmap.h)).max(0) as usize;
        let j1 = (((bb.max.y - gmap.y0).div_euclid(gmap.h)).max(0) as usize).min(gmap.ny.saturating_sub(1));
        for l in (0..board.layers.len()).filter(|l| item.on(*l)) {
            for j in j0..=j1 {
                for i in i0..=i1 {
                    let centre = Point::new(gmap.x0 + (2 * i as Nm + 1) * gmap.h / 2, gmap.y0 + (2 * j as Nm + 1) * gmap.h / 2);
                    let idx = l * cells + j * gmap.nx + i;
                    if !gmap.blocked_at(net, idx) && item.shape.contains(centre) {
                        out.insert(idx);
                    }
                }
            }
        }
    }
    out.into_iter().collect()
}

/// Checks that stamping `items` of `net` leaves every previously escaped pad with at least one
/// unblocked cell on its global routing grid.
fn preserves_handoffs(
    board: &Board,
    items: &[ItemId],
    net: u32,
    global_maps: &HashMap<String, ObstacleMap>,
    handoffs: &[EscapedHandoff],
) -> bool {
    let hole_clr = nm(board.rules.min_hole_clearance);
    for h in handoffs {
        if h.net == net || h.cells.is_empty() {
            continue;
        }
        let Some(gmap) = global_maps.get(&h.class) else { continue };
        let gcells = gmap.nx * gmap.ny;
        let clr = gmap.clearance_to(board, net);
        let trace_limit = (gmap.profile.half_width + clr + gmap.reach()) as f64;
        let survives = h.cells.iter().any(|&idx| {
            let layer = idx / gcells;
            let rem = idx % gcells;
            let (i, j) = (rem % gmap.nx, rem / gmap.nx);
            let gc = Point::new(gmap.x0 + (2 * i as Nm + 1) * gmap.h / 2, gmap.y0 + (2 * j as Nm + 1) * gmap.h / 2);
            let dot = Shape::Capsule { a: gc, b: gc, r: 0 };
            !items.iter().any(|&id| {
                let Some(it) = board.item(id) else { return false };
                if it.on(layer) && it.shape.gap_within(&dot, trace_limit) < trace_limit {
                    return true;
                }
                if let Some((hc, hr)) = it.hole {
                    let hlimit = (hr + gmap.profile.half_width + hole_clr + gmap.reach()) as f64;
                    if gc.distance(hc) < hlimit {
                        return true;
                    }
                }
                false
            })
        });
        if !survives {
            return false;
        }
    }
    true
}

/// Marks the forbidden halo around each earlier escape's outermost global handoff cell (plus all
/// surviving cells when down to two or fewer) so A* steers around them rather than choking off an
/// earlier pad's exit.
fn protect_vital_handoffs(
    board: &Board,
    grid: &mut RouteGrid,
    net: u32,
    track_r: Nm,
    global_maps: &HashMap<String, ObstacleMap>,
    handoffs: &[EscapedHandoff],
) {
    let hole_clr = nm(board.rules.min_hole_clearance);
    let via_r = grid.via.as_ref().map_or(0, |v| v.radius);
    let drill_r = grid.via.as_ref().map_or(0, |v| v.drill_radius);
    for h in handoffs {
        if h.net == net || h.cells.is_empty() {
            continue;
        }
        let Some(gmap) = global_maps.get(&h.class) else { continue };
        let gcells = gmap.nx * gmap.ny;
        let clr = gmap.clearance_to(board, net);
        let trace_keep = (track_r + gmap.profile.half_width + clr + gmap.reach()) as f64;
        let via_keep = (via_r + gmap.profile.half_width + clr + gmap.reach())
            .max(drill_r + gmap.profile.half_width + hole_clr + gmap.reach()) as f64;
        let guarded: &[usize] = if h.cells.len() <= 2 { &h.cells } else { &h.cells[h.cells.len() - 1..] };
        for &idx in guarded {
            let board_layer = idx / gcells;
            let rem = idx % gcells;
            let (gi, gj) = (rem % gmap.nx, rem / gmap.nx);
            let gc = Point::new(gmap.x0 + (2 * gi as Nm + 1) * gmap.h / 2, gmap.y0 + (2 * gj as Nm + 1) * gmap.h / 2);
            let span = ((via_keep.max(trace_keep) / grid.h as f64).ceil() as usize) + 1;
            let Some((ci, cj)) = grid.cell_at(gc) else { continue };
            for j in cj.saturating_sub(span)..=(cj + span).min(grid.ny - 1) {
                for i in ci.saturating_sub(span)..=(ci + span).min(grid.nx - 1) {
                    let d = grid.centre(i, j).distance(gc);
                    if d < via_keep {
                        grid.via_blocked[j * grid.nx + i] = true;
                    }
                    if d < trace_keep {
                        if let Some(rl) = grid.layers.iter().position(|&l| l == board_layer) {
                            let bidx = grid.index(rl, i, j);
                            grid.blocked[bidx] = true;
                        }
                    }
                }
            }
        }
    }
}

/// Guards the straight exit lane of nearby unescaped sibling pads on the same package side all the
/// way out to the target ring, never encroaching on `pad`'s own centre line. Returns `true` when
/// at least one cell or via site was guarded so the caller knows an unguarded fallback differs.
#[allow(clippy::too_many_arguments)]
fn guard_sibling_lanes(
    board: &Board,
    grid: &mut RouteGrid,
    pad: &PadInfo,
    track_r: Nm,
    narrow_hw: Nm,
    ring: Nm,
    lmap: &ObstacleMap,
    siblings: &[PadInfo],
) -> bool {
    let hole_clr = nm(board.rules.min_hole_clearance);
    let via_r = grid.via.as_ref().map_or(0, |v| v.radius);
    let drill_r = grid.via.as_ref().map_or(0, |v| v.drill_radius);
    let reach = lmap.reach();
    let rl_pad = grid.layers.iter().position(|&l| l == pad.layer);
    let mut changed = false;

    for sib in siblings {
        if sib.layer != pad.layer || pad.out.0 * sib.out.0 + pad.out.1 * sib.out.1 <= 0.9 {
            continue;
        }
        let sib_lat = ((sib.centre.x - pad.centre.x) as f64 * pad.along.0 + (sib.centre.y - pad.centre.y) as f64 * pad.along.1).abs();
        if sib_lat <= (grid.h * 2) as f64 || sib_lat > 900_000.0 {
            continue;
        }
        let max_guard = (sib_lat - (grid.h as f64) * 1.5).max(0.0);
        let clr = lmap.clearance_to(board, sib.net);
        let trace_keep = ((track_r + narrow_hw + clr + reach) as f64).min(max_guard);
        let via_keep = ((via_r + narrow_hw + clr + reach)
            .max(drill_r + narrow_hw + hole_clr + reach) as f64)
            .min(max_guard);
        let radius = via_keep.max(trace_keep);
        if radius <= 0.0 {
            continue;
        }
        let fwd_max = sib.edge_gap + ring as f64 + 250_000.0;
        let tip = Point::new(
            sib.centre.x + (sib.out.0 * fwd_max).round() as Nm,
            sib.centre.y + (sib.out.1 * fwd_max).round() as Nm,
        );
        let box_bb = BBox::of([sib.centre, tip]).grow(radius.ceil() as Nm + grid.h);
        let i0 = ((box_bb.min.x - grid.x0).div_euclid(grid.h)).max(0) as usize;
        let i1 = (((box_bb.max.x - grid.x0).div_euclid(grid.h)).max(0) as usize).min(grid.nx.saturating_sub(1));
        let j0 = ((box_bb.min.y - grid.y0).div_euclid(grid.h)).max(0) as usize;
        let j1 = (((box_bb.max.y - grid.y0).div_euclid(grid.h)).max(0) as usize).min(grid.ny.saturating_sub(1));
        for j in j0..=j1 {
            for i in i0..=i1 {
                let p = grid.centre(i, j);
                let dx = (p.x - sib.centre.x) as f64;
                let dy = (p.y - sib.centre.y) as f64;
                let fwd = dx * sib.out.0 + dy * sib.out.1;
                if !(60_000.0..=fwd_max).contains(&fwd) {
                    continue;
                }
                let lat = (dx * sib.along.0 + dy * sib.along.1).abs();
                if lat < via_keep {
                    let vidx = j * grid.nx + i;
                    if !grid.via_blocked[vidx] {
                        grid.via_blocked[vidx] = true;
                        changed = true;
                    }
                }
                if lat < trace_keep {
                    if let Some(rl) = rl_pad {
                        let bidx = grid.index(rl, i, j);
                        if !grid.blocked[bidx] {
                            grid.blocked[bidx] = true;
                            changed = true;
                        }
                    }
                }
            }
        }
    }
    changed
}

/// Connects a power pad on its own surface layer to a nearby same-net kept pour that already
/// escapes the package, avoiding a redundant dogbone via next to neighbouring signal lanes.
#[allow(clippy::too_many_arguments)]
fn lay_pour_escape(
    board: &mut Board,
    pad: &PadInfo,
    profile: &Profile,
    narrow_hw: Nm,
    win_bounds: BBox,
    options: &PrefanoutOptions,
    siblings: &[PadInfo],
    escaped_pours: &[ItemId],
    local_maps: &mut HashMap<(String, Nm), ObstacleMap>,
    global_maps: &HashMap<String, ObstacleMap>,
    handoffs: &[EscapedHandoff],
) -> Option<(Vec<ItemId>, Vec<usize>)> {
    let max_steps = ((options.ring + 1_400_000) / options.local_cell.max(1)) as usize;
    let mut widths = vec![narrow_hw.min(profile.half_width)];
    if !widths.contains(&profile.half_width) {
        widths.push(profile.half_width);
    }
    for hw in widths {
        let mut local_profile = profile.clone();
        local_profile.half_width = hw;
        let key = (profile.class.clone(), hw);
        let lmap = local_maps
            .entry(key)
            .or_insert_with(|| ObstacleMap::window(board, local_profile, options.local_cell, win_bounds, options.pours_block));
        let mut base_grid = single_layer_view(lmap, pad.net, pad.layer);
        protect_vital_handoffs(board, &mut base_grid, pad.net, hw, global_maps, handoffs);
        base_grid.penalty = vec![0; base_grid.blocked.len()];
        for j in 0..base_grid.ny {
            for i in 0..base_grid.nx {
                let p = base_grid.centre(i, j);
                let dx = (p.x - pad.centre.x) as f64;
                let dy = (p.y - pad.centre.y) as f64;
                let lat = (dx * pad.along.0 + dy * pad.along.1).abs();
                if lat > 15_000.0 {
                    base_grid.penalty[j * base_grid.nx + i] = 3 + ((lat / 40_000.0) as u32) * 3;
                }
            }
        }
        let sources = item_terminals(board, &base_grid, pad.id, hw, 8, 24);
        if sources.is_empty() {
            continue;
        }
        let mut pour_targets: Vec<Terminal> = Vec::new();
        for &pid in escaped_pours {
            pour_targets.extend(item_terminals(board, &base_grid, pid, hw, 6, 24));
        }
        if pour_targets.is_empty() {
            continue;
        }
        let src_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();
        let mut sib_grid = base_grid.clone();
        let mut pour_tiers: Vec<RouteGrid> = Vec::with_capacity(2);
        if guard_sibling_lanes(board, &mut sib_grid, pad, hw, narrow_hw, options.ring, lmap, siblings) {
            pour_tiers.push(sib_grid);
        }
        pour_tiers.push(base_grid);

        for grid in pour_tiers {
            let targets: Vec<Terminal> = pour_targets.iter().copied().filter(|t| !grid.blocked[t.0]).collect();
            if targets.is_empty() {
                continue;
            }
            let tgt_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
            let Some(path) = route_astar(&grid, &src_cells, &tgt_cells, 1_000) else { continue };
            if path.len() > max_steps {
                continue;
            }
            if let Some(ok) = commit_grid_path(board, &grid, &path, &sources, &targets, &profile.class, true, global_maps, handoffs) {
                return Some(ok);
            }
        }
    }
    None
}

/// Marks local-grid cells where a through-via punched from `mount_layer` would sit within one
/// global routing channel of an SMD pad on another layer, sealing off that bystander pad's exit.
fn compute_opposite_via_guard(board: &Board, mount_layer: usize, bounds: BBox, h: Nm) -> Vec<bool> {
    let (x0, y0) = (bounds.min.x, bounds.min.y);
    let nx = ((bounds.max.x - x0) / h).max(1) as usize;
    let ny = ((bounds.max.y - y0) / h).max(1) as usize;
    let mut guard = vec![false; nx * ny];
    if board.layers.len() < 2 {
        return guard;
    }
    let base_prof = Profile::for_net(board, 0);
    let mut seen = HashSet::new();
    for layer in 0..board.layers.len() {
        if layer == mount_layer {
            continue;
        }
        for id in board.near(layer, bounds) {
            if !seen.insert(id) {
                continue;
            }
            let Some(it) = board.item(id) else { continue };
            if !matches!(it.kind, Kind::Pad { .. }) || it.hole.is_some() || it.on(mount_layer) || it.net == 0 {
                continue;
            }
            let b_prof = Profile::for_net(board, it.net);
            let keep = (base_prof.via_radius + 2 * b_prof.half_width + 2 * b_prof.clearance + 80_000) as f64;
            let grow = keep.ceil() as Nm + h;
            let bb = it.shape.bbox().grow(grow);
            let i0 = ((bb.min.x - x0).div_euclid(h)).max(0) as usize;
            let i1 = (((bb.max.x - x0).div_euclid(h)).max(0) as usize).min(nx.saturating_sub(1));
            let j0 = ((bb.min.y - y0).div_euclid(h)).max(0) as usize;
            let j1 = (((bb.max.y - y0).div_euclid(h)).max(0) as usize).min(ny.saturating_sub(1));
            for j in j0..=j1 {
                for i in i0..=i1 {
                    if guard[j * nx + i] {
                        continue;
                    }
                    let c = Point::new(x0 + (2 * i as Nm + 1) * h / 2, y0 + (2 * j as Nm + 1) * h / 2);
                    let dot = Shape::Capsule { a: c, b: c, r: 0 };
                    if it.shape.gap_within(&dot, keep) < keep {
                        guard[j * nx + i] = true;
                    }
                }
            }
        }
    }
    guard
}

#[allow(clippy::too_many_arguments)]
fn lay_pad_escape(
    board: &mut Board,
    frame: &Frame,
    pad: &PadInfo,
    profile: &Profile,
    narrow_hw: Nm,
    win_bounds: BBox,
    options: &PrefanoutOptions,
    siblings: &[PadInfo],
    opp_via_guard: &[bool],
    escaped_pours: &[ItemId],
    local_maps: &mut HashMap<(String, Nm), ObstacleMap>,
    global_maps: &HashMap<String, ObstacleMap>,
    handoffs: &[EscapedHandoff],
) -> Option<(Vec<ItemId>, Vec<usize>)> {
    if options.inward && !escaped_pours.is_empty() {
        if let Some(ok) = lay_pour_escape(
            board,
            pad,
            profile,
            narrow_hw,
            win_bounds,
            options,
            siblings,
            escaped_pours,
            local_maps,
            global_maps,
            handoffs,
        ) {
            return Some(ok);
        }
    }
    match options.mode {
        EscapeMode::Stub => lay_stub(board, pad, profile, narrow_hw, options, global_maps, handoffs),
        EscapeMode::Dogbone => lay_dogbone(board, pad, profile, narrow_hw, options, global_maps, handoffs),
        EscapeMode::GridRing => lay_grid_escape(
            board,
            frame,
            pad,
            profile,
            narrow_hw,
            win_bounds,
            options,
            siblings,
            opp_via_guard,
            escaped_pours,
            local_maps,
            global_maps,
            handoffs,
            false,
        ),
        EscapeMode::GridInner => lay_grid_escape(
            board,
            frame,
            pad,
            profile,
            narrow_hw,
            win_bounds,
            options,
            siblings,
            opp_via_guard,
            escaped_pours,
            local_maps,
            global_maps,
            handoffs,
            true,
        ),
    }
}

fn try_commit_items(
    board: &mut Board,
    pieces: Vec<Item>,
    net: u32,
    class: &str,
    pours_block: bool,
    global_maps: &HashMap<String, ObstacleMap>,
    handoffs: &[EscapedHandoff],
) -> Option<(Vec<ItemId>, Vec<usize>)> {
    let mut ids = Vec::with_capacity(pieces.len());
    for piece in pieces {
        let id = board.insert(piece);
        ids.push(id);
        let bad = if pours_block {
            !board::check_item_with_pours(board, id).is_empty()
        } else {
            !board::check_item(board, id).is_empty()
        };
        if bad {
            for id in ids.into_iter().rev() {
                board.discard(id);
            }
            return None;
        }
    }
    let gmap = &global_maps[class];
    let cells = handoff_cells_of(board, &ids, net, gmap);
    if cells.is_empty() || !preserves_handoffs(board, &ids, net, global_maps, handoffs) {
        for id in ids.into_iter().rev() {
            board.discard(id);
        }
        return None;
    }
    Some((ids, cells))
}

fn lay_stub(
    board: &mut Board,
    pad: &PadInfo,
    profile: &Profile,
    narrow_hw: Nm,
    options: &PrefanoutOptions,
    global_maps: &HashMap<String, ObstacleMap>,
    handoffs: &[EscapedHandoff],
) -> Option<(Vec<ItemId>, Vec<usize>)> {
    let mut widths = vec![profile.half_width];
    if narrow_hw < profile.half_width {
        widths.push(narrow_hw);
    }
    let base = pad.edge_gap + options.ring as f64;
    for hw in widths {
        for scale in [1.0, 0.85, 1.15, 0.70, 1.30] {
            let d = base * scale;
            let tip = Point::new(
                pad.centre.x + (pad.out.0 * d).round() as Nm,
                pad.centre.y + (pad.out.1 * d).round() as Nm,
            );
            let piece = track(pad.net, pad.layer, pad.centre, tip, hw);
            if let Some(ok) = try_commit_items(board, vec![piece], pad.net, &profile.class, options.pours_block, global_maps, handoffs) {
                return Some(ok);
            }
        }
    }
    None
}

fn lay_dogbone(
    board: &mut Board,
    pad: &PadInfo,
    profile: &Profile,
    narrow_hw: Nm,
    options: &PrefanoutOptions,
    global_maps: &HashMap<String, ObstacleMap>,
    handoffs: &[EscapedHandoff],
) -> Option<(Vec<ItemId>, Vec<usize>)> {
    let all_layers = (1u64 << board.layers.len()) - 1;
    let mut widths = Vec::new();
    if pad.crowding == 0 {
        widths.push(profile.half_width);
    }
    if narrow_hw < profile.half_width || widths.is_empty() {
        widths.push(narrow_hw.min(profile.half_width));
    }
    if !widths.contains(&profile.half_width) {
        widths.push(profile.half_width);
    }
    // Stagger depths by sequence index modulo 3 and try all radial depths along the pad's own
    // lane (`lat == 0`) before shifting laterally toward neighbouring lanes.
    let ring = options.ring as f64;
    let depths_base = [0.50 * ring, 0.85 * ring, 1.20 * ring, 0.68 * ring, 1.02 * ring, 1.38 * ring];
    let rot = pad.seq_index % 3;
    let mut depths = vec![depths_base[rot], depths_base[(rot + 1) % 3], depths_base[(rot + 2) % 3]];
    depths.extend_from_slice(&depths_base[3..]);
    let offsets = [0.0, 100_000.0, -100_000.0, 200_000.0, -200_000.0];

    // Sites clear of the board's via-avoid areas first; one in them only where none else fits.
    for (avoid, hw) in [true, false].into_iter().flat_map(|avoid| widths.iter().map(move |&hw| (avoid, hw))) {
        if !avoid && board.via_avoid.is_empty() {
            continue;
        }
        for &lat in &offsets {
            for &depth in &depths {
                let d = pad.edge_gap + depth.max(420_000.0);
                let via_pt = Point::new(
                    pad.centre.x + (pad.out.0 * d + pad.along.0 * lat).round() as Nm,
                    pad.centre.y + (pad.out.1 * d + pad.along.1 * lat).round() as Nm,
                );
                if avoid && board.in_via_avoid(via_pt, profile.via_radius) {
                    continue;
                }
                let stub = track(pad.net, pad.layer, pad.centre, via_pt, hw);
                let via = Item {
                    kind: Kind::Via,
                    net: pad.net,
                    layers: all_layers,
                    shape: Shape::Capsule { a: via_pt, b: via_pt, r: profile.via_radius },
                    hole: Some((via_pt, profile.drill_radius)),
                    locked: false,
                    span: None,
                };
                if let Some(ok) =
                    try_commit_items(board, vec![stub, via], pad.net, &profile.class, options.pours_block, global_maps, handoffs)
                {
                    // Ensure the dogbone via actually offers a free global cell off the pad's surface layer.
                    let gcells = global_maps[&profile.class].nx * global_maps[&profile.class].ny;
                    if board.layers.len() < 2 || ok.1.iter().any(|&idx| idx / gcells != pad.layer) {
                        return Some(ok);
                    }
                    for id in ok.0 {
                        board.remove(id);
                    }
                }
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn lay_grid_escape(
    board: &mut Board,
    frame: &Frame,
    pad: &PadInfo,
    profile: &Profile,
    narrow_hw: Nm,
    win_bounds: BBox,
    options: &PrefanoutOptions,
    siblings: &[PadInfo],
    opp_via_guard: &[bool],
    escaped_pours: &[ItemId],
    local_maps: &mut HashMap<(String, Nm), ObstacleMap>,
    global_maps: &HashMap<String, ObstacleMap>,
    handoffs: &[EscapedHandoff],
    multi_layer: bool,
) -> Option<(Vec<ItemId>, Vec<usize>)> {
    // On tight rows, neck the escape trace inside the fanout zone so adjacent 0.4 mm lanes still
    // have room for 0.4 mm vias; isolated wide pads try their full class width first.
    let tight = pad.crowding > 0 || profile.via_radius + profile.half_width + profile.clearance > 395_000;
    let mut widths = Vec::new();
    if tight && narrow_hw < profile.half_width {
        widths.push(narrow_hw);
        widths.push(profile.half_width);
    } else {
        widths.push(profile.half_width);
        if narrow_hw < profile.half_width {
            widths.push(narrow_hw);
        }
    }

    let gmap = &global_maps[&profile.class];
    let gcells = gmap.nx * gmap.ny;
    // The layers an escape heads for: the inner signal layers, planes left out; with none, as on
    // a two-layer board, the other signal layer, so each escape drops to it by a via.
    let signal = |l: &usize| board.planes & (1 << l) == 0;
    let mut inner_layers: Vec<usize> = (1..board.layers.len().saturating_sub(1)).filter(signal).collect();
    if inner_layers.is_empty() {
        inner_layers = (0..board.layers.len()).filter(signal).filter(|&l| l != pad.layer).collect();
    }
    if inner_layers.is_empty() {
        inner_layers = vec![pad.layer];
    }
    let preferred_inner = inner_layers[pad.seq_index % inner_layers.len()];

    for hw in widths {
        let mut local_profile = profile.clone();
        local_profile.half_width = hw;
        let key = (profile.class.clone(), hw);
        let lmap = local_maps
            .entry(key)
            .or_insert_with(|| ObstacleMap::window(board, local_profile, options.local_cell, win_bounds, options.pours_block));

        let mut base_grid = if multi_layer { lmap.view(pad.net) } else { single_layer_view(lmap, pad.net, pad.layer) };
        protect_vital_handoffs(board, &mut base_grid, pad.net, hw, global_maps, handoffs);

        let sources = item_terminals(board, &base_grid, pad.id, hw, 8, 24);
        if sources.is_empty() {
            continue;
        }
        let src_cells: Vec<usize> = sources.iter().map(|t| t.0).collect();

        let mut pour_targets: Vec<Terminal> = Vec::new();
        for &pid in escaped_pours {
            pour_targets.extend(item_terminals(board, &base_grid, pid, hw, 6, 24));
        }
        let pour_target_cells: BTreeSet<usize> = pour_targets.iter().map(|t| t.0).collect();

        // Penalize lateral drift from the pad's own outward ray so adjacent 0.4 mm pads stagger
        // their vias radially along their own lanes rather than drifting in front of a neighbour.
        let cells = base_grid.cell_count();
        base_grid.penalty = vec![0; base_grid.blocked.len()];
        let rl_pad = base_grid.layers.iter().position(|&l| l == pad.layer);
        for j in 0..base_grid.ny {
            for i in 0..base_grid.nx {
                let p = base_grid.centre(i, j);
                let dx = (p.x - pad.centre.x) as f64;
                let dy = (p.y - pad.centre.y) as f64;
                let lat = (dx * pad.along.0 + dy * pad.along.1).abs();
                let lat_outer = if lat <= 15_000.0 { 0 } else { 3 + ((lat / 40_000.0) as u32) * 3 };
                let lat_inner = (lat / 80_000.0) as u32;
                if let Some(rl) = rl_pad {
                    let mut pen = lat_outer;
                    if multi_layer && base_grid.layers.len() > 1 {
                        let depth = frame.ring_depth(p);
                        if depth > 500_000.0 {
                            pen += 3;
                        }
                        if depth > 900_000.0 {
                            pen += 5;
                        }
                    }
                    base_grid.penalty[rl * cells + j * base_grid.nx + i] += pen;
                }
                if multi_layer && base_grid.layers.len() > 1 {
                    for (rl, &bl) in base_grid.layers.iter().enumerate() {
                        if bl == pad.layer {
                            continue;
                        }
                        let mut pen = lat_inner;
                        if inner_layers.contains(&bl) {
                            if bl != preferred_inner {
                                pen += 3;
                            }
                        } else {
                            pen += 8;
                        }
                        base_grid.penalty[rl * cells + j * base_grid.nx + i] += pen;
                    }
                }
            }
        }

        let collect_targets = |grid: &RouteGrid, layers: &[usize], r_lo: f64, r_hi: f64, with_pours: bool| -> Vec<Terminal> {
            let mut targets = Vec::new();
            if with_pours {
                for &pt in &pour_targets {
                    if !grid.blocked[pt.0] {
                        targets.push(pt);
                    }
                }
            }
            for &bl in layers {
                let Some(rl) = grid.layers.iter().position(|&l| l == bl) else { continue };
                for j in 0..grid.ny {
                    for i in 0..grid.nx {
                        let idx = grid.index(rl, i, j);
                        if grid.blocked[idx] {
                            continue;
                        }
                        let p = grid.centre(i, j);
                        let d = frame.ring_depth(p);
                        if d < r_lo || d > r_hi {
                            continue;
                        }
                        let gi = (p.x - gmap.x0).div_euclid(gmap.h);
                        let gj = (p.y - gmap.y0).div_euclid(gmap.h);
                        if gi < 0 || gj < 0 || (gi as usize) >= gmap.nx || (gj as usize) >= gmap.ny {
                            continue;
                        }
                        let gidx = bl * gcells + (gj as usize) * gmap.nx + (gi as usize);
                        if !gmap.blocked_at(pad.net, gidx) {
                            targets.push((idx, None));
                        }
                    }
                }
            }
            targets
        };

        let target_sets: Vec<Vec<usize>> = if multi_layer && inner_layers != vec![pad.layer] {
            let mut other_outer: Vec<usize> = (0..board.layers.len()).filter(|l| !inner_layers.contains(l)).collect();
            other_outer.sort_by_key(|&l| l == pad.layer);
            vec![inner_layers.clone(), other_outer]
        } else {
            vec![vec![pad.layer]]
        };

        let ring_bands = [
            (options.ring as f64, (options.ring + 5 * options.global_cell) as f64),
            ((options.ring as f64 * 0.65).max(380_000.0), options.ring as f64 * 1.45),
        ];

        let mut sib_grid = base_grid.clone();
        let sib_changed = guard_sibling_lanes(board, &mut sib_grid, pad, hw, narrow_hw, options.ring, lmap, siblings);
        let mut both_grid = sib_grid.clone();
        let mut opp_changed = false;
        if multi_layer && both_grid.via.is_some() && opp_via_guard.len() == both_grid.via_blocked.len() {
            for (vb, &g) in both_grid.via_blocked.iter_mut().zip(opp_via_guard) {
                if g && !*vb {
                    *vb = true;
                    opp_changed = true;
                }
            }
        }
        let mut tiers: Vec<RouteGrid> = Vec::with_capacity(3);
        if opp_changed {
            tiers.push(both_grid);
        }
        if sib_changed {
            tiers.push(sib_grid);
        }
        tiers.push(base_grid);

        for mut grid in tiers {
            for (b_idx, &(r_lo, r_hi)) in ring_bands.iter().enumerate() {
                for (s_idx, layers) in target_sets.iter().enumerate() {
                    let with_pours = b_idx == 0 && s_idx == 0 && !pour_targets.is_empty();
                    for _attempt in 0..4 {
                        let targets = collect_targets(&grid, layers, r_lo, r_hi, with_pours);
                        if targets.is_empty() {
                            break;
                        }
                        let tgt_cells: Vec<usize> = targets.iter().map(|t| t.0).collect();
                        let Some(path) = route_astar(&grid, &src_cells, &tgt_cells, 40) else { break };
                        let landed_on_pour = path
                            .last()
                            .is_some_and(|&(rl, ti, tj)| pour_target_cells.contains(&grid.index(rl, ti, tj)));
                        if let Some(ok) = commit_grid_path(
                            board,
                            &grid,
                            &path,
                            &sources,
                            &targets,
                            &profile.class,
                            landed_on_pour,
                            global_maps,
                            handoffs,
                        ) {
                            return Some(ok);
                        }
                        if let Some(&(rl, ti, tj)) = path.last() {
                            let bidx = grid.index(rl, ti, tj);
                            grid.blocked[bidx] = true;
                        }
                    }
                }
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn commit_grid_path(
    board: &mut Board,
    grid: &RouteGrid,
    path: &GridPath,
    sources: &[Terminal],
    targets: &[Terminal],
    class: &str,
    landed_on_pour: bool,
    global_maps: &HashMap<String, ObstacleMap>,
    handoffs: &[EscapedHandoff],
) -> Option<(Vec<ItemId>, Vec<usize>)> {
    let mut ids = lay(board, grid, path, sources, targets)?;
    if !grid.pours_block {
        let saved: Vec<Item> = ids.iter().filter_map(|&id| board.item(id).cloned()).collect();
        pull_tight(board, &mut ids);
        let cells = handoff_cells_of(board, &ids, grid.net, &global_maps[class]);
        if (!cells.is_empty() || landed_on_pour) && preserves_handoffs(board, &ids, grid.net, global_maps, handoffs) {
            return Some((ids, cells));
        }
        for id in ids {
            board.remove(id);
        }
        ids = saved.into_iter().map(|it| board.insert(it)).collect();
    }
    let cells = handoff_cells_of(board, &ids, grid.net, &global_maps[class]);
    if (cells.is_empty() && !landed_on_pour) || !preserves_handoffs(board, &ids, grid.net, global_maps, handoffs) {
        for id in ids {
            board.remove(id);
        }
        return None;
    }
    Some((ids, cells))
}

/// Prunes dangling escape pieces belonging to nets that global routing has already closed, freeing
/// unused inner-layer straws and orphan vias before rip-up runs while leaving still-open nets'
/// escapes intact.
pub fn prune_closed_escapes(board: &mut Board, escapes: &mut Vec<ItemId>, pours_block: bool) -> (usize, Vec<Span>) {
    let open_nets: HashSet<u32> = board_connectivity(board, pours_block).airlines.into_iter().map(|a| a.net).collect();
    let mut closed = Vec::new();
    let mut open = Vec::new();
    for &id in escapes.iter() {
        match board.item(id) {
            Some(it) if open_nets.contains(&it.net) => open.push(id),
            Some(_) => closed.push(id),
            None => {}
        }
    }
    let (pruned, spans) = prune_dangling(board, &mut closed);
    open.extend(closed);
    *escapes = open;
    (pruned, spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn stripped_earring() -> Board {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pcb");
        let mut board = Board::load(&path).unwrap().0;
        let loose: Vec<ItemId> = board
            .items()
            .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via))
            .map(|(id, _)| id)
            .collect();
        for id in loose {
            board.remove(id);
        }
        board
    }

    #[test]
    fn oriented_frame_recovers_rotated_footprint_axes() {
        let board = stripped_earring();
        let u6 = board.footprints.iter().position(|f| f.reference == "U6").expect("U6 on earring");
        assert!(board.footprints[u6].rotation_deg.abs() > 10.0, "U6 is rotated");
        let smd: Vec<ItemId> = board
            .items()
            .filter(|(_, it)| matches!(it.kind, Kind::Pad { footprint: f, .. } if f == u6) && it.is_copper() && it.hole.is_none())
            .map(|(id, _)| id)
            .collect();
        let frame = Frame::of(&board, u6, &smd).unwrap();
        let width = frame.u_max - frame.u_min;
        let height = frame.v_max - frame.v_min;
        assert!(width > 2_000_000.0 && width < 4_000_000.0, "width {width}");
        assert!(height > 2_000_000.0 && height < 4_000_000.0, "height {height}");
        for &id in &smd {
            let it = board.item(id).unwrap();
            let (cu, cv, du, dv) = frame.pad_spans(&it.shape);
            let (_, _, gap, _) = frame.side_info(cu, cv);
            assert!(gap <= du.max(dv), "every LGA perimeter pad lies near a side of the oriented box: gap {gap}");
        }
    }

    #[test]
    fn prefanout_skips_exposed_and_unconnected_pads_and_lays_legal_escapes() {
        let mut board = stripped_earring();
        let before: BTreeSet<(String, ItemId, Option<ItemId>)> =
            board::check(&board).into_iter().filter(|v| !board.item(v.a).is_some_and(Item::refilled) && !v.b.is_some_and(|b| board.item(b).is_some_and(Item::refilled))).map(|v| (format!("{:?}", v.rule), v.a, v.b)).collect();
        let u1 = board.footprints.iter().position(|f| f.reference == "U1").expect("U1 on earring");
        let stats = prefanout(
            &mut board,
            &PrefanoutOptions { parts: vec![u1], mode: EscapeMode::GridInner, order: EscapeOrder::InnerFirst, ..Default::default() },
            &mut |_| {},
        );
        assert_eq!(stats.parts, 1);
        assert!(stats.skipped_no_net >= 10, "U1 has unconnected pads: {stats:?}");
        assert_eq!(stats.skipped_exposed, 1, "U1's central thermal pad is skipped: {stats:?}");
        assert!(stats.bridged_inward >= 3, "U1's GND and adjacent same-net pads bridge locally: {stats:?}");
        assert!(stats.escaped >= 35, "U1's outward signal pads get escapes: {stats:?}");
        assert!(stats.failed <= 2, "nearly all U1 clusters escape cleanly: {stats:?}");
        let after: BTreeSet<(String, ItemId, Option<ItemId>)> =
            board::check(&board).into_iter().filter(|v| !board.item(v.a).is_some_and(Item::refilled) && !v.b.is_some_and(|b| board.item(b).is_some_and(Item::refilled))).map(|v| (format!("{:?}", v.rule), v.a, v.b)).collect();
        let added: Vec<_> = after.difference(&before).collect();
        assert!(added.is_empty(), "prefanout introduced DRC violations: {added:?}");
    }

    #[test]
    fn inward_bridging_joins_pads_to_kept_block_pours() {
        let job_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../jobs/earring-kicad/input/earring.kicad_pcb");
        assert!(job_path.exists(), "missing {}", job_path.display());
        let mut board = Board::load(&job_path).unwrap().0;
        let loose: Vec<ItemId> = board
            .items()
            .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via))
            .map(|(id, _)| id)
            .collect();
        for id in loose {
            board.remove(id);
        }
        let before: BTreeSet<(String, ItemId, Option<ItemId>)> =
            board::check(&board).into_iter().filter(|v| !board.item(v.a).is_some_and(Item::refilled) && !v.b.is_some_and(|b| board.item(b).is_some_and(Item::refilled))).map(|v| (format!("{:?}", v.rule), v.a, v.b)).collect();
        let u1 = board.footprints.iter().position(|f| f.reference == "U1").expect("U1 on job board");
        let stats = prefanout(
            &mut board,
            &PrefanoutOptions { parts: vec![u1], mode: EscapeMode::GridInner, order: EscapeOrder::InnerFirst, ..Default::default() },
            &mut |_| {},
        );
        assert!(stats.bridged_inward >= 10, "3V3 and VDD_CORE pads bridge inward to kept pours: {stats:?}");
        assert!(stats.skipped_escaped >= 10, "pads joined to escaped kept pours skip outward fanout: {stats:?}");
        assert!(stats.escaped >= 32, "outward signal clusters escape cleanly: {stats:?}");
        let after: BTreeSet<(String, ItemId, Option<ItemId>)> =
            board::check(&board).into_iter().filter(|v| !board.item(v.a).is_some_and(Item::refilled) && !v.b.is_some_and(|b| board.item(b).is_some_and(Item::refilled))).map(|v| (format!("{:?}", v.rule), v.a, v.b)).collect();
        let added: Vec<_> = after.difference(&before).collect();
        assert!(added.is_empty(), "prefanout broke kept-pour or clearance rules: {added:?}");
    }

    #[test]
    fn every_escape_mode_leaves_only_legal_copper_and_prunes_cleanly() {
        for mode in [EscapeMode::Stub, EscapeMode::Dogbone, EscapeMode::GridRing, EscapeMode::GridInner] {
            let mut board = stripped_earring();
            let u2 = board.footprints.iter().position(|f| f.reference == "U2").expect("U2 on earring");
            let mut stats = prefanout(
                &mut board,
                &PrefanoutOptions { parts: vec![u2], mode, order: EscapeOrder::Round, ..Default::default() },
                &mut |_| {},
            );
            assert!(stats.escaped > 0, "{mode:?} laid no escapes on U2: {stats:?}");
            for &id in &stats.laid {
                let v: Vec<_> = board::check_item(&board, id);
                assert!(v.is_empty(), "{mode:?} item {id} broke rules: {v:?}");
            }
            let (pruned, _) = prune_dangling(&mut board, &mut stats.laid);
            assert!(pruned > 0, "{mode:?} unrouted escapes prune cleanly when unused");
        }
    }

    #[test]
    fn opposite_layer_bystander_smd_pads_keep_a_routing_channel_past_through_vias() {
        let mut board = stripped_earring();
        let u2 = board.footprints.iter().position(|f| f.reference == "U2").expect("U2 on B.Cu");
        let u7 = board.footprints.iter().position(|f| f.reference == "U7").expect("U7 on B.Cu");
        let stats = prefanout(
            &mut board,
            &PrefanoutOptions { parts: vec![u2, u7], mode: EscapeMode::GridInner, order: EscapeOrder::InnerFirst, ..Default::default() },
            &mut |_| {},
        );
        assert_eq!(stats.failed, 0, "U2 and U7 escape cleanly: {stats:?}");
        let led42 = board.footprints.iter().position(|f| f.reference == "LED42").expect("LED42");
        let led43 = board.footprints.iter().position(|f| f.reference == "LED43").expect("LED43");
        let pad_a = board
            .items()
            .find(|(_, it)| matches!(&it.kind, Kind::Pad { footprint, number, .. } if *footprint == led42 && number == "2"))
            .map(|(id, _)| id)
            .unwrap();
        let pad_b = board
            .items()
            .find(|(_, it)| matches!(&it.kind, Kind::Pad { footprint, number, .. } if *footprint == led43 && number == "4"))
            .map(|(id, _)| id)
            .unwrap();
        let net = board.item(pad_a).unwrap().net;
        let gmap = ObstacleMap::new(&board, Profile::for_net(&board, net), 50_000, false);
        let grid = gmap.view(net);
        let src: Vec<usize> = crate::route::terminals(&mut board, &grid, pad_a).into_iter().map(|t| t.0).collect();
        let tgt: Vec<usize> = crate::route::terminals(&mut board, &grid, pad_b).into_iter().map(|t| t.0).collect();
        assert!(route_astar(&grid, &src, &tgt, 60).is_some(), "LEDC_D5 between LED42-2 and LED43-4 stays routable after U2/U7 fanout");
    }
}
