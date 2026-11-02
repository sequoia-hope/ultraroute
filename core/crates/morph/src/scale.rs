//! Uniform scaling of a board about its outline's centre: footprints translate to their scaled
//! positions while keeping their size and rotation, the board outline and board-wide zones scale
//! about the same centre, local component zones move with the warp field so regulator rings under
//! a package stay aligned with its pads, and tracks and vias flow with the harmonic field.

use std::collections::{BTreeSet, HashMap, HashSet};

use board::{arc_chords, nm, point_segment, BBox, Block, Board, Item, Kind, Nm, Point, Shape};
use kicad_pcb::{
    apply, mm, parse_board, scale_edits, Copper, Edit, NewTrack, NewVia, Pcb, Rules, Span, Zone,
    ZoneFill,
};

use crate::flow::{cut, layer_of};
use crate::warp::WarpField;

/// Warp grid cell and outer margin in nanometres, and maximum piece length for flowing tracks.
pub(crate) const CELL: f64 = 150_000.0;
pub(crate) const MARGIN: f64 = 3_000_000.0;
const PIECE: f64 = 500_000.0;

/// Raster step for carving block pours, and extra margin past cell centres so every corner of an
/// emitted 25 um cell (`12.5 * sqrt(2) = 17.68 um` from centre) clears obstacles cleanly.
const POUR_STEP: Nm = 25_000;
const POUR_CORNER_PAD: f64 = 19_000.0;
const POUR_SIDE_MAX: i64 = 16_000;

#[derive(Debug, Clone, Default)]
pub struct ScaleOptions {
    pub factor: f64,
    pub lock: HashSet<String>,
    pub uuid_seed: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ScaleReport {
    pub text: String,
    pub before_mm: [f64; 2],
    pub after_mm: [f64; 2],
    pub moved_footprints: usize,
    pub tracks: usize,
    pub vias: usize,
}

/// Scales `text` by `opts.factor` about the board outline's centre.
pub fn scale(text: &str, rules: Rules, opts: &ScaleOptions) -> Result<ScaleReport, String> {
    if !opts.factor.is_finite() || opts.factor <= 0.0 {
        return Err(format!("--factor {}: must be a positive number", opts.factor));
    }
    let (orig_pcb, orig_copper) = parse_board(text).map_err(|e| e.to_string())?;
    let orig_board = Board::new(&orig_pcb, &orig_copper, rules.clone());
    let orig_bb = board_bounds(&orig_board);
    let before_mm = bbox_size_mm(orig_bb);
    let centre = bbox_centre(orig_bb);

    let fp_shifts: Vec<(usize, Point)> = (0..orig_board.footprints.len())
        .map(|f| {
            if opts.lock.contains(&orig_board.footprints[f].reference) {
                return (f, Point::new(0, 0));
            }
            let orig_at = Point::from_m(orig_pcb.footprints[f].at);
            let dx = ((opts.factor - 1.0) * (orig_at.x as f64 - centre.0)).round() as Nm;
            let dy = ((opts.factor - 1.0) * (orig_at.y as f64 - centre.1)).round() as Nm;
            (f, Point::new(dx, dy))
        })
        .collect();
    let shift_map: HashMap<usize, Point> = fp_shifts.iter().copied().collect();
    let moved_footprints = fp_shifts.iter().filter(|(_, d)| *d != Point::new(0, 0)).count();

    let field = build_scale_field(&orig_board, centre, opts.factor, &fp_shifts);

    let (tracks, vias, obstacle_items) =
        carry_copper(&orig_board, &orig_copper, &shift_map, &field);

    let bare_text = strip_routed_copper(text, &orig_copper);
    let (scaled_board, _, scaled_bare_text) = build_scaled_bare_board(
        &bare_text,
        &rules,
        &orig_pcb,
        &orig_copper,
        orig_bb,
        centre,
        opts.factor,
        &fp_shifts,
        &field,
        &obstacle_items,
    )?;
    let after_mm = bbox_size_mm(board_bounds(&scaled_board));

    let out_text = apply(
        &scaled_bare_text,
        &Edit {
            tracks: tracks.clone(),
            vias: vias.clone(),
            uuid_seed: opts.uuid_seed,
            ..Edit::default()
        },
    );

    Ok(ScaleReport {
        text: out_text,
        before_mm,
        after_mm,
        moved_footprints,
        tracks: tracks.len(),
        vias: vias.len(),
    })
}

/// Bounding box of the board outline, falling back to outline edges or copper items when a board
/// has no closed Edge.Cuts loop.
pub(crate) fn board_bounds(board: &Board) -> BBox {
    if let Some(outer) = board.outline.first().filter(|p| !p.is_empty()) {
        return BBox::of(outer.iter().copied());
    }
    if !board.outline_edges.is_empty() {
        return board
            .outline_edges
            .iter()
            .fold(board.outline_edges[0].2, |acc, e| acc.union(e.2));
    }
    let mut it = board.items().map(|(_, item)| item.shape.bbox());
    let Some(first) = it.next() else {
        return BBox { min: Point::new(0, 0), max: Point::new(0, 0) };
    };
    it.fold(first, BBox::union)
}

pub(crate) fn bbox_centre(b: BBox) -> (f64, f64) {
    ((b.min.x as f64 + b.max.x as f64) * 0.5, (b.min.y as f64 + b.max.y as f64) * 0.5)
}

pub(crate) fn bbox_size_mm(b: BBox) -> [f64; 2] {
    [(b.max.x - b.min.x) as f64 * 1e-6, (b.max.y - b.min.y) as f64 * 1e-6]
}

/// Harmonic displacement field whose border scales by `factor` about `centre` and whose footprint
/// inclusions translate rigidly by `fp_shifts`.
pub(crate) fn build_scale_field(
    board: &Board,
    centre: (f64, f64),
    factor: f64,
    fp_shifts: &[(usize, Point)],
) -> WarpField {
    let base = board_bounds(board);
    let scale_pt = |p: Point| {
        Point::new(
            (centre.0 + factor * (p.x as f64 - centre.0)).round() as Nm,
            (centre.1 + factor * (p.y as f64 - centre.1)).round() as Nm,
        )
    };
    let scaled_bb = BBox::of([scale_pt(base.min), scale_pt(base.max)]);
    let domain = base.union(scaled_bb);
    let mut field = WarpField::new(domain, CELL, MARGIN);
    field.set_border(&|(x, y)| ((factor - 1.0) * (x - centre.0), (factor - 1.0) * (y - centre.1)));
    let mut pads_by_fp: Vec<Vec<Shape>> = vec![Vec::new(); board.footprints.len()];
    for (_, it) in board.items() {
        if let Kind::Pad { footprint, .. } = it.kind {
            if let Some(slot) = pads_by_fp.get_mut(footprint) {
                slot.push(it.shape.clone());
            }
        }
    }
    for &(f, shift) in fp_shifts {
        if let Some(shapes) = pads_by_fp.get(f).filter(|s| !s.is_empty()) {
            field.add_inclusion(shapes, (shift.x as f64, shift.y as f64), true);
        }
    }
    field.solve();
    field
}

/// Removes all top-level tracks, arcs and vias from `text` so scaled static geometry can be
/// rebuilt cleanly at any step factor.
pub(crate) fn strip_routed_copper(text: &str, copper: &Copper) -> String {
    let mut remove: Vec<Span> = Vec::with_capacity(
        copper.tracks.len() + copper.arcs.len() + copper.vias.len(),
    );
    remove.extend(copper.tracks.iter().map(|t| t.span));
    remove.extend(copper.arcs.iter().map(|a| a.span));
    remove.extend(copper.vias.iter().map(|v| v.span));
    if remove.is_empty() {
        return text.to_string();
    }
    apply(text, &Edit { remove, ..Edit::default() })
}

/// True when `zone` spans a substantial fraction of the board or sits right against its border, so
/// its polygon should scale uniformly with Edge.Cuts rather than deform around nearby footprints.
pub(crate) fn is_board_plane_zone(board_bb: BBox, zone: &Zone) -> bool {
    if zone.outline.len() < 3 {
        return true;
    }
    let zbb = BBox::of(zone.outline.iter().copied().map(Point::from_m));
    let bw = (board_bb.max.x - board_bb.min.x).max(1) as f64;
    let bh = (board_bb.max.y - board_bb.min.y).max(1) as f64;
    let zw = (zbb.max.x - zbb.min.x).max(0) as f64;
    let zh = (zbb.max.y - zbb.min.y).max(0) as f64;
    if zw * zh >= 0.20 * bw * bh {
        return true;
    }
    let rim = 1_000_000;
    zbb.min.x <= board_bb.min.x + rim
        || zbb.max.x >= board_bb.max.x - rim
        || zbb.min.y <= board_bb.min.y + rim
        || zbb.max.y >= board_bb.max.y - rim
}

/// Builds the scaled static board at `factor` with footprints shifted by `fp_shifts` from
/// `orig_pcb`, local zones carried by `field`, and any block (`kept`) zones carved around static
/// pads, holes, edges and `extra_obstacles`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_scaled_bare_board(
    bare_text: &str,
    rules: &Rules,
    orig_pcb: &Pcb,
    orig_copper: &Copper,
    orig_board_bb: BBox,
    centre: (f64, f64),
    factor: f64,
    fp_shifts: &[(usize, Point)],
    field: &WarpField,
    extra_obstacles: &[Item],
) -> Result<(Board, Copper, String), String> {
    let centre_m = [centre.0 * 1e-9, centre.1 * 1e-9];
    let fp_edits: Vec<(usize, [f64; 2], [f64; 2])> = fp_shifts
        .iter()
        .map(|&(f, shift)| {
            let orig = orig_pcb.footprints[f].at;
            let orig_nm = Point::from_m(orig);
            let to_m = [(orig_nm.x + shift.x) as f64 * 1e-9, (orig_nm.y + shift.y) as f64 * 1e-9];
            (f, orig, to_m)
        })
        .collect();
    let plane_flags: Vec<bool> =
        orig_copper.zones.iter().map(|z| is_board_plane_zone(orig_board_bb, z)).collect();
    let zone_map = |z: usize, p: [f64; 2]| -> [f64; 2] {
        if plane_flags.get(z).copied().unwrap_or(true) {
            [centre_m[0] + factor * (p[0] - centre_m[0]), centre_m[1] + factor * (p[1] - centre_m[1])]
        } else {
            let pt = Point::from_m(p);
            let (dx, dy) = field.displacement(pt);
            [(pt.x as f64 + dx).round() * 1e-9, (pt.y as f64 + dy).round() * 1e-9]
        }
    };

    let (rem, rep) =
        scale_edits(bare_text, centre_m, factor, &fp_edits, Some(&zone_map), &[])
            .map_err(|e| e.to_string())?;
    let unfilled_text = apply(bare_text, &Edit { remove: rem, replace: rep, ..Edit::default() });
    let (pcb, copper) = parse_board(&unfilled_text).map_err(|e| e.to_string())?;
    let blocks = Block::of(&pcb, &copper);
    let has_kept_zones = blocks.iter().any(|b| !b.zones.is_empty());
    if !has_kept_zones {
        let board = Board::new(&pcb, &copper, rules.clone());
        return Ok((board, copper, unfilled_text));
    }

    let mut tmp_board = Board::new(&pcb, &copper, rules.clone());
    let all_unfilled = tmp_board.unfilled.clone();
    for obs in extra_obstacles {
        tmp_board.insert(obs.clone());
    }
    let fills = carve_kept_zones(&tmp_board, &copper, &blocks);
    if fills.is_empty() {
        let board = Board::new(&pcb, &copper, rules.clone());
        return Ok((board, copper, unfilled_text));
    }

    let (rem2, rep2) =
        scale_edits(bare_text, centre_m, factor, &fp_edits, Some(&zone_map), &fills)
            .map_err(|e| e.to_string())?;
    let filled_text = apply(bare_text, &Edit { remove: rem2, replace: rep2, ..Edit::default() });
    let (pcb2, copper2) = parse_board(&filled_text).map_err(|e| e.to_string())?;
    let mut board2 = Board::new(&pcb2, &copper2, rules.clone());
    board2.unfilled = all_unfilled;
    Ok((board2, copper2, filled_text))
}

/// Removes block (`kept`) zone pour polygons from `board` before spring relaxation or local polish
/// so unlocked routed tracks inside a block zone do not bounce off stale pre-step pour rectangles.
pub(crate) fn strip_kept_zone_pours(board: &mut Board) {
    let kept_zones: HashSet<usize> =
        board.blocks.iter().flat_map(|b| b.zones.iter().copied()).collect();
    if kept_zones.is_empty() {
        return;
    }
    let ids: Vec<board::ItemId> = board
        .items()
        .filter_map(|(id, it)| match it.kind {
            Kind::Pour { zone, kept: true } if kept_zones.contains(&zone) => Some(id),
            _ => None,
        })
        .collect();
    for id in ids {
        board.remove(id);
    }
}

/// Re-carves block (`kept`) zone pour polygons around all current pads, holes and dynamic tracks/vias
/// in `board`, mirroring how KiCad's zone filler pours block zones around routed copper.
pub(crate) fn recarve_kept_zones_in_board(board: &mut Board, copper: &Copper) {
    strip_kept_zone_pours(board);
    let blocks = board.blocks.clone();
    if !blocks.iter().any(|b| !b.zones.is_empty()) {
        return;
    }
    let fills = carve_kept_zones(board, copper, &blocks);
    for zf in fills {
        let Some(zone) = copper.zones.get(zf.zone) else { continue };
        let Some(layer_idx) = board.layers.iter().position(|l| l == &zf.layer) else { continue };
        let layer_mask = 1u64 << layer_idx;
        for [x0, y0, x1, y1] in zf.rects {
            let p0 = Point::from_m([x0, y0]);
            let p1 = Point::from_m([x1, y1]);
            if p1.x <= p0.x || p1.y <= p0.y {
                continue;
            }
            board.insert(Item {
                kind: Kind::Pour { zone: zf.zone, kept: true },
                net: zone.net,
                layers: layer_mask,
                shape: Shape::Polygon(vec![
                    Point::new(p0.x, p0.y),
                    Point::new(p1.x, p0.y),
                    Point::new(p1.x, p1.y),
                    Point::new(p0.x, p1.y),
                ]),
                hole: None,
                locked: true,
                span: None,
            });
        }
    }
}

/// Carves conservative inner-box rectangles for block (`kept`) zones so `Board::new` sees valid
/// non-shorting block planes before KiCad refills all zones at the end.
fn carve_kept_zones(board: &Board, copper: &Copper, blocks: &[Block]) -> Vec<ZoneFill> {
    let kept_set: HashSet<usize> = blocks.iter().flat_map(|b| b.zones.iter().copied()).collect();
    if kept_set.is_empty() {
        return Vec::new();
    }
    let mut kept_bb: Option<BBox> = None;
    for &z in &kept_set {
        if let Some(zone) = copper.zones.get(z).filter(|z| z.outline.len() >= 3) {
            let zb = BBox::of(zone.outline.iter().copied().map(Point::from_m));
            kept_bb = Some(kept_bb.map_or(zb, |acc| acc.union(zb)));
        }
    }
    let Some(target_bb) = kept_bb else { return Vec::new() };
    let min_kept_prio = kept_set
        .iter()
        .filter_map(|&z| copper.zones.get(z).map(|zn| zn.priority))
        .min()
        .unwrap_or(0);

    let mut order: Vec<usize> = (0..copper.zones.len())
        .filter(|&z| {
            let zn = &copper.zones[z];
            if zn.keepout.is_some() || zn.outline.len() < 3 {
                return false;
            }
            if kept_set.contains(&z) {
                return true;
            }
            zn.priority > min_kept_prio
                && BBox::of(zn.outline.iter().copied().map(Point::from_m)).overlaps(target_bb)
        })
        .collect();
    order.sort_by_key(|&z| std::cmp::Reverse(copper.zones[z].priority));

    let mut poured: Vec<(u32, usize, PourRaster)> = Vec::new();
    let mut out = Vec::new();
    for z in order {
        let zone = &copper.zones[z];
        let layers: Vec<usize> = copper
            .expand(&zone.layers)
            .iter()
            .filter_map(|n| board.layers.iter().position(|l| l == n))
            .collect();
        for layer in layers {
            if let Some(r) = carve_one_zone(board, zone, layer, &poured) {
                if kept_set.contains(&z) {
                    let rects = r
                        .rects()
                        .into_iter()
                        .map(|[x0, y0, x1, y1]| {
                            [x0 as f64 * 1e-9, y0 as f64 * 1e-9, x1 as f64 * 1e-9, y1 as f64 * 1e-9]
                        })
                        .collect();
                    out.push(ZoneFill { zone: z, layer: board.layers[layer].clone(), rects });
                }
                poured.push((zone.net, layer, r));
            }
        }
    }
    out
}

#[derive(Clone)]
struct PourRaster {
    i0: i64,
    j0: i64,
    w: usize,
    h: usize,
    cells: Vec<bool>,
}

impl PourRaster {
    fn over(b: BBox) -> Option<PourRaster> {
        let (i0, j0) = (b.min.x.div_euclid(POUR_STEP), b.min.y.div_euclid(POUR_STEP));
        let (i1, j1) = (b.max.x.div_euclid(POUR_STEP) + 1, b.max.y.div_euclid(POUR_STEP) + 1);
        if i1 - i0 > POUR_SIDE_MAX || j1 - j0 > POUR_SIDE_MAX || i1 <= i0 || j1 <= j0 {
            return None;
        }
        let (w, h) = ((i1 - i0) as usize, (j1 - j0) as usize);
        Some(PourRaster { i0, j0, w, h, cells: vec![false; w * h] })
    }

    fn center(&self, i: usize, j: usize) -> Point {
        Point::new(
            (self.i0 + i as i64) * POUR_STEP + POUR_STEP / 2,
            (self.j0 + j as i64) * POUR_STEP + POUR_STEP / 2,
        )
    }

    fn get(&self, i: i64, j: i64) -> bool {
        i >= 0
            && j >= 0
            && (i as usize) < self.w
            && (j as usize) < self.h
            && self.cells[j as usize * self.w + i as usize]
    }

    fn span(&self, b: BBox) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
        let clamp = |v: i64, n: usize| v.clamp(0, n as i64) as usize;
        let cols = clamp(b.min.x.div_euclid(POUR_STEP) - self.i0, self.w)
            ..clamp(b.max.x.div_euclid(POUR_STEP) + 1 - self.i0, self.w);
        let rows = clamp(b.min.y.div_euclid(POUR_STEP) - self.j0, self.h)
            ..clamp(b.max.y.div_euclid(POUR_STEP) + 1 - self.j0, self.h);
        (cols, rows)
    }

    fn scan(&mut self, polys: &[&[Point]], value: bool) {
        let mut xs = Vec::new();
        for j in 0..self.h {
            let y = self.center(0, j).y;
            xs.clear();
            for poly in polys {
                for k in 0..poly.len() {
                    let (a, b) = (poly[k], poly[(k + 1) % poly.len()]);
                    if (a.y > y) != (b.y > y) {
                        xs.push(
                            a.x as f64
                                + (y - a.y) as f64 * (b.x - a.x) as f64 / (b.y - a.y) as f64,
                        );
                    }
                }
            }
            xs.sort_by(f64::total_cmp);
            for pair in xs.chunks_exact(2) {
                let from =
                    ((pair[0] - POUR_STEP as f64 / 2.0) / POUR_STEP as f64).ceil() as i64 - self.i0;
                let to =
                    ((pair[1] - POUR_STEP as f64 / 2.0) / POUR_STEP as f64).ceil() as i64 - self.i0;
                for i in from.max(0)..to.min(self.w as i64) {
                    self.cells[j * self.w + i as usize] = value;
                }
            }
        }
    }

    fn clear(&mut self, b: BBox, near: impl Fn(Point) -> bool) {
        let (cols, rows) = self.span(b);
        for j in rows {
            for i in cols.clone() {
                let k = j * self.w + i;
                if self.cells[k] && near(self.center(i, j)) {
                    self.cells[k] = false;
                }
            }
        }
    }

    fn edge(&self, i: usize, j: usize) -> bool {
        let v = self.cells[j * self.w + i];
        let (i, j) = (i as i64, j as i64);
        [(i - 1, j), (i + 1, j), (i, j - 1), (i, j + 1)].iter().any(|&(x, y)| self.get(x, y) != v)
    }

    fn open(&mut self, r: Nm) {
        let reach = r / POUR_STEP;
        if reach == 0 {
            return;
        }
        let disc: Vec<(i64, i64)> = (-reach..=reach)
            .flat_map(|di| (-reach..=reach).map(move |dj| (di, dj)))
            .filter(|(di, dj)| (di * di + dj * dj) * POUR_STEP * POUR_STEP <= r * r)
            .collect();
        let before = self.cells.clone();
        let mut eroded = self.cells.clone();
        for j in 0..self.h {
            for i in 0..self.w {
                let on_rim = i == 0 || j == 0 || i + 1 == self.w || j + 1 == self.h;
                if (!before[j * self.w + i] && self.edge(i, j)) || (before[j * self.w + i] && on_rim)
                {
                    for &(di, dj) in &disc {
                        let (x, y) = (i as i64 + di, j as i64 + dj);
                        if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
                            eroded[y as usize * self.w + x as usize] = false;
                        }
                    }
                }
            }
        }
        self.cells = eroded;
        let eroded_snap = self.clone();
        for j in 0..self.h {
            for i in 0..self.w {
                if eroded_snap.cells[j * self.w + i] && eroded_snap.edge(i, j) {
                    for &(di, dj) in &disc {
                        let (x, y) = (i as i64 + di, j as i64 + dj);
                        if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
                            let k = y as usize * self.w + x as usize;
                            self.cells[k] |= before[k];
                        }
                    }
                }
            }
        }
    }

    fn keep_seeded(&mut self, seeds: &[usize]) {
        let mut kept = vec![false; self.cells.len()];
        let mut stack: Vec<usize> = seeds.iter().copied().filter(|&k| self.cells[k]).collect();
        while let Some(k) = stack.pop() {
            if kept[k] || !self.cells[k] {
                continue;
            }
            kept[k] = true;
            let (i, j) = (k % self.w, k / self.w);
            if i > 0 {
                stack.push(k - 1);
            }
            if i + 1 < self.w {
                stack.push(k + 1);
            }
            if j > 0 {
                stack.push(k - self.w);
            }
            if j + 1 < self.h {
                stack.push(k + self.w);
            }
        }
        self.cells = kept;
    }

    fn rects(&self) -> Vec<[Nm; 4]> {
        let mut done = Vec::new();
        let mut open: Vec<(usize, usize, usize)> = Vec::new();
        for j in 0..=self.h {
            let mut runs = Vec::new();
            if j < self.h {
                let mut i = 0;
                while i < self.w {
                    if self.cells[j * self.w + i] {
                        let from = i;
                        while i < self.w && self.cells[j * self.w + i] {
                            i += 1;
                        }
                        runs.push((from, i));
                    }
                    i += 1;
                }
            }
            let mut still = Vec::with_capacity(runs.len());
            for (from, to, top) in open.drain(..) {
                if let Some(pos) = runs.iter().position(|&r| r == (from, to)) {
                    runs.swap_remove(pos);
                    still.push((from, to, top));
                } else {
                    let x = |i: usize| (self.i0 + i as i64) * POUR_STEP;
                    let y = |j: usize| (self.j0 + j as i64) * POUR_STEP;
                    done.push([x(from), y(top), x(to), y(j)]);
                }
            }
            still.extend(runs.into_iter().map(|(from, to)| (from, to, j)));
            open = still;
        }
        done
    }
}

fn shape_point_distance(shape: &Shape, p: Point) -> f64 {
    match shape {
        Shape::Capsule { a, b, r } => (point_segment(p, *a, *b) - *r as f64).max(0.0),
        Shape::Polygon(q) if board::inside(p, q) => 0.0,
        Shape::Polygon(q) => (0..q.len())
            .map(|k| point_segment(p, q[k], q[(k + 1) % q.len()]))
            .fold(f64::INFINITY, f64::min),
    }
}

fn carve_one_zone(
    board: &Board,
    zone: &Zone,
    layer: usize,
    poured: &[(u32, usize, PourRaster)],
) -> Option<PourRaster> {
    let outline: Vec<Point> = zone.outline.iter().copied().map(Point::from_m).collect();
    if outline.len() < 3 {
        return None;
    }
    let mut b = BBox::of(outline.iter().copied());
    if !board.outline.is_empty() {
        let edge = BBox::of(board.outline.iter().flatten().copied());
        b = BBox {
            min: Point::new(b.min.x.max(edge.min.x), b.min.y.max(edge.min.y)),
            max: Point::new(b.max.x.min(edge.max.x), b.max.y.min(edge.max.y)),
        };
    }
    let mut r = PourRaster::over(b)?;
    r.scan(&[&outline], true);
    for cutout in &zone.cutouts {
        if cutout.len() >= 3 {
            let cp: Vec<Point> = cutout.iter().copied().map(Point::from_m).collect();
            r.scan(&[&cp], false);
        }
    }
    for k in 0..outline.len() {
        let (a, e) = (outline[k], outline[(k + 1) % outline.len()]);
        let eb = BBox::of([a, e]).grow(POUR_STEP);
        r.clear(eb, |p| point_segment(p, a, e) < POUR_CORNER_PAD);
    }
    if !board.outline.is_empty() {
        let mut board_area = r.clone();
        board_area.cells.fill(false);
        board_area.scan(&board.outline.iter().map(Vec::as_slice).collect::<Vec<_>>(), true);
        for (c, inside) in r.cells.iter_mut().zip(&board_area.cells) {
            *c &= inside;
        }
        for &(a, e, eb, edge) in &board.outline_edges {
            let reach = edge + POUR_CORNER_PAD as Nm;
            if eb.grow(reach).overlaps(b) {
                r.clear(eb.grow(reach), |p| point_segment(p, a, e) < edge as f64 + POUR_CORNER_PAD);
            }
        }
    }
    let own = nm(zone.clearance);
    let net = zone.net;
    let dummy_pour = Item {
        kind: Kind::Pour { zone: 0, kept: true },
        net,
        layers: 1u64 << layer,
        shape: Shape::Capsule { a: b.min, b: b.min, r: 0 },
        hole: None,
        locked: true,
        span: None,
    };
    let mut seeds = Vec::new();
    for id in board.near(layer, b.grow(board.max_rule_reach())) {
        let Some(item) = board.item(id) else { continue };
        if !item.on(layer) {
            continue;
        }
        match &item.kind {
            Kind::Keepout { forbids, .. } => {
                if let (true, Shape::Polygon(p)) = (forbids.copperpour, &item.shape) {
                    r.scan(&[p], false);
                    for k in 0..p.len() {
                        let (a, e) = (p[k], p[(k + 1) % p.len()]);
                        r.clear(BBox::of([a, e]).grow(POUR_STEP), |q| {
                            point_segment(q, a, e) < POUR_CORNER_PAD
                        });
                    }
                }
            }
            Kind::Pour { .. } | Kind::Spoke { .. } => {}
            _ if net != 0 && item.net == net => {
                let (cols, rows) = r.span(item.shape.bbox().grow(POUR_STEP));
                for j in rows {
                    for i in cols.clone() {
                        if shape_point_distance(&item.shape, r.center(i, j)) <= POUR_STEP as f64 {
                            seeds.push(j * r.w + i);
                        }
                    }
                }
            }
            _ => {
                let (ta, tb) =
                    (board::Thing::Item(board, usize::MAX, &dummy_pour), board::Thing::Item(board, id, item));
                let req = own.max(board.clearance_kept(&ta, &tb, layer)) as f64 + POUR_CORNER_PAD;
                r.clear(item.shape.bbox().grow(req as Nm + POUR_STEP), |p| {
                    shape_point_distance(&item.shape, p) < req
                });
            }
        }
    }
    let holes = board.layers.len();
    for id in board.near(holes, b.grow(board.max_rule_reach())) {
        let Some(item) = board.item(id) else { continue };
        let Some(Shape::Capsule { a, b: end, r: radius }) = board.hole_shape(id, item) else {
            continue;
        };
        let req = board.hole_clearance(id, item, usize::MAX, &dummy_pour);
        if req <= 0 && item.plated_hole() && net != 0 && item.net == net {
            continue;
        }
        let reach = radius as f64 + req.max(own) as f64 + POUR_CORNER_PAD;
        r.clear(BBox::of([a, end]).grow(reach as Nm + POUR_STEP), |p| {
            point_segment(p, a, end) < reach
        });
    }
    for (other, l, done) in poured {
        if *l != layer || *other == net {
            continue;
        }
        let c = own.max(board.clearance(net, *other)) as f64 + POUR_CORNER_PAD;
        for j in 0..done.h {
            for i in 0..done.w {
                if !done.cells[j * done.w + i] {
                    continue;
                }
                let (x, y) = (done.i0 + i as i64 - r.i0, done.j0 + j as i64 - r.j0);
                if x >= 0 && y >= 0 && (x as usize) < r.w && (y as usize) < r.h {
                    r.cells[y as usize * r.w + x as usize] = false;
                }
                if done.edge(i, j) {
                    let p = done.center(i, j);
                    r.clear(BBox::of([p]).grow(c as Nm + POUR_STEP), |q| {
                        q.distance(p) < c + POUR_STEP as f64 * 0.5
                    });
                }
            }
        }
    }
    r.open(nm(zone.min_thickness) / 2);
    if net != 0 {
        r.keep_seeded(&seeds);
    }
    r.cells.iter().any(|&c| c).then_some(r)
}

/// Carries all tracks (including chorded arcs and locked stubs) and vias by `field`, keeping
/// pad-fastened points and via-in-pad holes rigid with their footprint.
fn carry_copper(
    board: &Board,
    copper: &Copper,
    shifts: &HashMap<usize, Point>,
    field: &WarpField,
) -> (Vec<NewTrack>, Vec<NewVia>, Vec<Item>) {
    let layer_idx = |name: &str| board.layers.iter().position(|l| l == name);
    let pad_shift = |p: Point, net: u32, layers: u64| -> Option<Point> {
        for l in (0..board.layers.len()).filter(|&l| layers & (1u64 << l) != 0) {
            for id in board.near(l, BBox::of([p])) {
                let Some(it) = board.item(id) else { continue };
                if it.net == net && it.on(l) && it.shape.contains(p) {
                    if let Kind::Pad { footprint, .. } = it.kind {
                        return Some(shifts.get(&footprint).copied().unwrap_or(Point::new(0, 0)));
                    }
                }
            }
        }
        None
    };

    let mut raw_tracks: Vec<(Point, Point, Nm, usize, u32, bool)> = Vec::new();
    for t in &copper.tracks {
        if let Some(l) = layer_idx(&t.layer) {
            let (a, b) = (Point::from_m(t.start), Point::from_m(t.end));
            if a != b {
                raw_tracks.push((a, b, nm(t.width / 2.0), l, t.net, t.locked));
            }
        }
    }
    for arc in &copper.arcs {
        if let Some(l) = layer_idx(&arc.layer) {
            let (pts, _) = arc_chords(arc.start, arc.mid, arc.end, 0.0);
            let r = nm(arc.width / 2.0);
            for w in pts.windows(2) {
                if w[0] != w[1] {
                    raw_tracks.push((w[0], w[1], r, l, arc.net, arc.locked));
                }
            }
        }
    }

    let mut junction_sets: HashMap<usize, BTreeSet<Point>> = HashMap::new();
    for &(a, b, _, l, _, _) in &raw_tracks {
        junction_sets.entry(l).or_default().extend([a, b]);
    }
    let mut via_pins: HashMap<(u32, Point), Point> = HashMap::new();
    let mut new_vias = Vec::with_capacity(copper.vias.len());
    let mut obstacle_items = Vec::new();

    for v in &copper.vias {
        let at = Point::from_m(v.at);
        let (Some(a), Some(b)) = (layer_idx(&v.layers[0]), layer_idx(&v.layers[1])) else {
            continue;
        };
        let mask = (a.min(b)..=a.max(b)).fold(0u64, |m, l| m | (1u64 << l));
        for l in a.min(b)..=a.max(b) {
            junction_sets.entry(l).or_default().insert(at);
        }
        let by = pad_shift(at, v.net, mask).unwrap_or_else(|| {
            let (dx, dy) = field.displacement(at);
            Point::new(dx.round() as Nm, dy.round() as Nm)
        });
        via_pins.insert((v.net, at), by);
        let to = Point::new(at.x + by.x, at.y + by.y);
        new_vias.push(NewVia {
            at: [to.x as f64 * 1e-9, to.y as f64 * 1e-9],
            size: v.size,
            drill: v.drill,
            layers: v.layers.clone(),
            net: v.net,
            locked: v.locked,
        });
        obstacle_items.push(Item {
            kind: Kind::Via,
            net: v.net,
            layers: mask,
            shape: Shape::Capsule { a: to, b: to, r: nm(v.size / 2.0) },
            hole: Some((to, nm(v.drill / 2.0))),
            locked: v.locked,
            span: None,
        });
    }

    let junctions: HashMap<usize, Vec<Point>> =
        junction_sets.into_iter().map(|(l, s)| (l, s.into_iter().collect())).collect();
    let _ = layer_of;

    let map_point = |p: Point, net: u32, layer: usize| -> Point {
        let by = pad_shift(p, net, 1u64 << layer)
            .or_else(|| via_pins.get(&(net, p)).copied())
            .unwrap_or_else(|| {
                let (dx, dy) = field.displacement(p);
                Point::new(dx.round() as Nm, dy.round() as Nm)
            });
        Point::new(p.x + by.x, p.y + by.y)
    };

    let mut new_tracks = Vec::new();
    for (a, b, r, l, net, locked) in raw_tracks {
        let pts = cut(a, b, junctions.get(&l).map_or(&[][..], Vec::as_slice), PIECE);
        let mut moved: Vec<Point> = pts.into_iter().map(|p| map_point(p, net, l)).collect();
        moved.dedup();
        if moved.len() < 2 {
            continue;
        }
        // Collapse strictly collinear runs of one track so straight segments stay whole.
        let mut simplified = vec![moved[0]];
        for k in 1..moved.len() - 1 {
            let prev = *simplified.last().unwrap();
            let cur = moved[k];
            let next = moved[k + 1];
            if point_segment(cur, prev, next) > 200.0 {
                simplified.push(cur);
            }
        }
        simplified.push(*moved.last().unwrap());

        let width = (r * 2) as f64 * 1e-9;
        let layer_name = board.layers[l].clone();
        for w in simplified.windows(2) {
            if w[0] == w[1] {
                continue;
            }
            new_tracks.push(NewTrack {
                start: [w[0].x as f64 * 1e-9, w[0].y as f64 * 1e-9],
                end: [w[1].x as f64 * 1e-9, w[1].y as f64 * 1e-9],
                width,
                layer: layer_name.clone(),
                net,
                locked,
            });
            obstacle_items.push(Item {
                kind: Kind::Track,
                net,
                layers: 1u64 << l,
                shape: Shape::Capsule { a: w[0], b: w[1], r },
                hole: None,
                locked,
                span: None,
            });
        }
    }
    let _ = mm;

    (new_tracks, new_vias, obstacle_items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use board::{check, connectivity_without_pours};

    const SAMPLE: &str = "(kicad_pcb
\t(version 20241229)
\t(general (thickness 1.6))
\t(layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))
\t(net 0 \"\")
\t(net 1 \"SIG\")
\t(net 2 \"GND\")
\t(gr_line (start 0 0) (end 20 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e1\"))
\t(gr_line (start 20 0) (end 20 10) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e2\"))
\t(gr_line (start 20 10) (end 0 10) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e3\"))
\t(gr_line (start 0 10) (end 0 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e4\"))
\t(footprint \"R\" (locked yes) (layer \"F.Cu\") (uuid \"f1\") (at 4 5)
\t\t(pad \"1\" smd rect (at -1 0) (size 1 1) (layers \"F.Cu\") (net 2 \"GND\") (uuid \"p1\"))
\t\t(pad \"2\" smd rect (at 1 0) (size 1 1) (layers \"F.Cu\") (net 1 \"SIG\") (uuid \"p2\"))
\t)
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"f2\") (at 16 5)
\t\t(pad \"1\" smd rect (at -1 0) (size 1 1) (layers \"F.Cu\") (net 1 \"SIG\") (uuid \"p3\"))
\t\t(pad \"2\" smd rect (at 1 0) (size 1 1) (layers \"F.Cu\") (net 2 \"GND\") (uuid \"p4\"))
\t)
\t(segment (start 5 5) (end 15 5) (width 0.2) (layer \"F.Cu\") (net 1) (uuid \"s1\"))
\t(via (at 3 5) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net 2) (uuid \"v1\"))
\t(via (at 17 5) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net 2) (uuid \"v2\"))
\t(segment (start 3 5) (end 17 5) (width 0.25) (layer \"B.Cu\") (net 2) (uuid \"s2\"))
\t(zone (net 2) (net_name \"GND\") (layer \"B.Cu\") (uuid \"z1\")
\t\t(polygon (pts (xy 0.5 0.5) (xy 19.5 0.5) (xy 19.5 9.5) (xy 0.5 9.5)))
\t)
)\n";

    #[test]
    fn scale_expands_outline_parts_and_copper_while_keeping_part_sizes_and_joints() {
        let rules = Rules::parse("{}").unwrap();
        let rep = scale(
            SAMPLE,
            rules.clone(),
            &ScaleOptions { factor: 1.1, lock: HashSet::new(), uuid_seed: Some(7) },
        )
        .unwrap();
        assert!((rep.before_mm[0] - 20.0).abs() < 1e-6 && (rep.before_mm[1] - 10.0).abs() < 1e-6);
        assert!((rep.after_mm[0] - 22.0).abs() < 1e-6 && (rep.after_mm[1] - 11.0).abs() < 1e-6);
        assert_eq!(rep.moved_footprints, 2);

        let (pcb, copper) = parse_board(&rep.text).unwrap();
        // Centre is (10, 5): footprint at (4, 5) moves to 10 + 1.1 * (-6) = 3.4, pad gap stays 2 mm.
        assert!((pcb.footprints[0].at[0] - 3.4e-3).abs() < 1e-9);
        assert!((pcb.footprints[1].at[0] - 16.6e-3).abs() < 1e-9);
        let pad_dx = pcb.footprints[0].pads[1].position[0] - pcb.footprints[0].pads[0].position[0];
        assert!((pad_dx - 2.0e-3).abs() < 1e-9, "footprint size stayed rigid");

        let board = Board::new(&pcb, &copper, rules);
        assert!(check(&board).is_empty(), "zero DRC violations after 1.1x scale");
        assert!(connectivity_without_pours(&board).airlines.is_empty(), "SIG stayed connected");
    }
}
