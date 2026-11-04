//! Uniform scaling of a board about its outline's centre: footprints translate to their scaled
//! positions while keeping their size and rotation, the board outline and board-wide zones scale
//! about the same centre, local component zones move with the warp field so regulator rings under
//! a package stay aligned with its pads, and tracks and vias flow with the harmonic field.

use std::collections::{BTreeSet, HashMap, HashSet};

use board::{arc_chords, nm, point_segment, BBox, Block, Board, Item, Kind, Nm, Point, Shape};
use kicad_pcb::sexpr::Node;
use kicad_pcb::{
    apply, mm, parse_board, scale_edits, Copper, Edit, Footprint, NewTrack, NewVia, Pcb, Rules,
    Span, Zone, ZoneFill,
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

/// Per-side courtyard polygons of one footprint at its original board position (`factor = 1.0`).
/// Disjoint courtyard islands (such as the two solder tabs of a coin-cell holder) and concave
/// closed courtyard loops stay separate instead of collapsing into one board-spanning convex hull.
#[derive(Debug, Clone, Default)]
pub(crate) struct FootprintYards {
    pub front: Vec<Vec<Point>>,
    pub back: Vec<Vec<Point>>,
}

/// Physical placement envelope of one footprint at `factor = 1.0`.
#[derive(Debug, Clone)]
pub(crate) struct FootprintEnvelope {
    /// Physical anchor `(x_nm, y_nm)` about which uniform scaling translates the footprint.
    pub anchor: (f64, f64),
    /// Optional parent footprint `e` when this small part sits right next to `e`'s rigid pins/holes
    /// inside `e`'s pad/hole bounding box (for example passives nestled between a connector's bosses).
    pub parent: Option<usize>,
    /// True when the footprint draws `Edge.Cuts` primitives directly.
    pub owns_outline: bool,
    /// True when the footprint contains copper graphics (such as a net-tie bridge) that only move
    /// when text is rewritten.
    pub has_copper_graphic: bool,
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

    let orig_fp_at: Vec<Point> =
        orig_pcb.footprints.iter().map(|f| Point::from_m(f.at)).collect();
    let fp_locked: Vec<bool> = orig_board
        .footprints
        .iter()
        .map(|f| opts.lock.contains(&f.reference))
        .collect();
    let orig_yards = extract_footprint_yards(text, &orig_pcb, &orig_copper);
    let fp_envs = compute_footprint_envelopes(&orig_pcb, &orig_copper, &orig_board, &orig_yards);

    let mut cand_pos =
        nominal_scaled_positions(&orig_fp_at, &fp_envs, &fp_locked, centre, opts.factor);
    if (opts.factor - 1.0).abs() > 1e-12 {
        crate::compact::project_scaled_footprints(
            &orig_board,
            &orig_yards,
            &fp_envs,
            &fp_locked,
            centre,
            opts.factor,
            &orig_fp_at,
            &mut cand_pos,
        );
    }

    let fp_shifts: Vec<(usize, Point)> = (0..orig_fp_at.len())
        .map(|f| (f, Point::new(cand_pos[f].x - orig_fp_at[f].x, cand_pos[f].y - orig_fp_at[f].y)))
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

pub(crate) fn nominal_scaled_positions(
    orig_fp_at: &[Point],
    envs: &[FootprintEnvelope],
    fp_locked: &[bool],
    centre: (f64, f64),
    factor: f64,
) -> Vec<Point> {
    (0..orig_fp_at.len())
        .map(|f| {
            if fp_locked[f] {
                return orig_fp_at[f];
            }
            let eff = envs[f].parent.unwrap_or(f);
            if fp_locked[eff] {
                return orig_fp_at[f];
            }
            let anch = envs[eff].anchor;
            let dx = ((factor - 1.0) * (anch.0 - centre.0)).round() as Nm;
            let dy = ((factor - 1.0) * (anch.1 - centre.1)).round() as Nm;
            Point::new(orig_fp_at[f].x + dx, orig_fp_at[f].y + dy)
        })
        .collect()
}

pub(crate) fn compute_footprint_envelopes(
    pcb: &Pcb,
    copper: &Copper,
    board: &Board,
    yards: &[FootprintYards],
) -> Vec<FootprintEnvelope> {
    let n = board.footprints.len();
    let mut shapes_by_fp: Vec<Vec<(Shape, u64, bool)>> = vec![Vec::new(); n];
    let mut fp_layers = vec![0u64; n];
    let mut pad_hole_bb: Vec<Option<BBox>> = vec![None; n];
    let union_opt = |acc: Option<BBox>, b: BBox| Some(acc.map_or(b, |x| x.union(b)));

    for (id, it) in board.items() {
        if let Kind::Pad { footprint: f, .. } = it.kind {
            if f < n {
                fp_layers[f] |= it.layers;
                pad_hole_bb[f] = union_opt(pad_hole_bb[f], it.shape.bbox());
                if it.layers != 0 {
                    shapes_by_fp[f].push((it.shape.clone(), it.layers, false));
                }
                if let Some(hs) = board.hole_shape(id, it) {
                    pad_hole_bb[f] = union_opt(pad_hole_bb[f], hs.bbox());
                    shapes_by_fp[f].push((hs, u64::MAX, true));
                }
            }
        }
    }

    let mut has_copper_graphic = vec![false; n];
    let mut graphic_bb: Vec<Option<BBox>> = vec![None; n];
    for g in &copper.graphics {
        if let Some(f) = g.footprint.filter(|&f| f < n) {
            has_copper_graphic[f] = true;
            for poly in &g.polygons {
                if !poly.is_empty() {
                    let bb = BBox::of(poly.iter().copied().map(Point::from_m));
                    graphic_bb[f] = union_opt(graphic_bb[f], bb);
                }
            }
        }
    }

    let outline_uuids: HashSet<&str> = pcb
        .outline
        .iter()
        .filter_map(|p| p.footprint.as_deref())
        .collect();

    let mut env_bb: Vec<Option<BBox>> = vec![None; n];
    let mut anchors: Vec<(f64, f64)> = Vec::with_capacity(n);
    let mut owns_outline = vec![false; n];

    for f in 0..n {
        owns_outline[f] = pcb
            .footprints
            .get(f)
            .is_some_and(|fp| outline_uuids.contains(fp.uuid.as_str()));
        let mut ebb = pad_hole_bb[f];
        if let Some(y) = yards.get(f) {
            for poly in y.front.iter().chain(&y.back) {
                if !poly.is_empty() {
                    ebb = union_opt(ebb, BBox::of(poly.iter().copied()));
                }
            }
        }
        if ebb.is_none() {
            ebb = graphic_bb[f];
        }
        env_bb[f] = ebb;
        let anchor = match ebb {
            Some(b) => bbox_centre(b),
            None => {
                let p = board.footprints[f].at;
                (p.x as f64, p.y as f64)
            }
        };
        anchors.push(anchor);
    }

    let bbox_area = |b: Option<BBox>| -> f64 {
        b.map_or(0.0, |x| {
            (x.max.x - x.min.x).max(1) as f64 * (x.max.y - x.min.y).max(1) as f64
        })
    };

    let mut raw_parent: Vec<Option<usize>> = vec![None; n];
    for s in 0..n {
        if owns_outline[s] || shapes_by_fp[s].is_empty() {
            continue;
        }
        let area_s = bbox_area(env_bb[s]).max(1.0);
        let layers_s = fp_layers[s];
        let s_has_hole = shapes_by_fp[s].iter().any(|x| x.2);
        let anch_pt = Point::new(anchors[s].0.round() as Nm, anchors[s].1.round() as Nm);
        let mut best: Option<(f64, usize)> = None;
        for e in 0..n {
            if e == s || bbox_area(env_bb[e]) <= 4.0 * area_s {
                continue;
            }
            let mut inter_bb: Option<BBox> = None;
            let mut inter_cnt = 0usize;
            for (sh_e, l_e, hole_e) in &shapes_by_fp[e] {
                if *hole_e || s_has_hole || (*l_e & layers_s) != 0 {
                    inter_bb = union_opt(inter_bb, sh_e.bbox());
                    inter_cnt += 1;
                }
            }
            let Some(ibb) = inter_bb else { continue };
            if inter_cnt < 3 || !ibb.grow(600_000).contains(anch_pt) {
                continue;
            }
            let mut min_gap = f64::INFINITY;
            for (sh_s, l_s, hole_s) in &shapes_by_fp[s] {
                let s_reach = sh_s.bbox().grow(1_400_000);
                for (sh_e, l_e, hole_e) in &shapes_by_fp[e] {
                    if !(*hole_e || *hole_s || (*l_e & *l_s) != 0) || !s_reach.overlaps(sh_e.bbox())
                    {
                        continue;
                    }
                    let g = sh_s.gap_within(sh_e, 1_400_000.0);
                    if g < min_gap {
                        min_gap = g;
                    }
                }
            }
            if min_gap <= 1_400_000.0 && best.is_none_or(|(bg, _)| min_gap < bg) {
                best = Some((min_gap, e));
            }
        }
        raw_parent[s] = best.map(|(_, e)| e);
    }

    (0..n)
        .map(|f| {
            let mut p = raw_parent[f];
            let mut depth = 0;
            while let Some(cur) = p {
                if depth >= 4 || raw_parent[cur].is_none() || raw_parent[cur] == Some(f) {
                    break;
                }
                p = raw_parent[cur];
                depth += 1;
            }
            FootprintEnvelope {
                anchor: anchors[f],
                parent: p,
                owns_outline: owns_outline[f],
                has_copper_graphic: has_copper_graphic[f],
            }
        })
        .collect()
}

pub(crate) fn footprint_file_ordinals(copper: &Copper) -> (Vec<usize>, Vec<usize>) {
    let n = copper.footprint_at.len();
    let mut file_to_canon: Vec<usize> = (0..n).collect();
    file_to_canon.sort_by_key(|&f| (copper.footprint_at[f].map_or(usize::MAX, |s| s.start), f));
    let mut canon_to_file = vec![0usize; n];
    for (ord, &f) in file_to_canon.iter().enumerate() {
        canon_to_file[f] = ord;
    }
    (file_to_canon, canon_to_file)
}

pub(crate) fn zone_file_ordinals(copper: &Copper) -> (Vec<usize>, Vec<usize>) {
    let n = copper.zones.len();
    let mut file_to_canon: Vec<usize> = (0..n).collect();
    file_to_canon.sort_by_key(|&z| (copper.zones[z].span.start, z));
    let mut canon_to_file = vec![0usize; n];
    for (ord, &z) in file_to_canon.iter().enumerate() {
        canon_to_file[z] = ord;
    }
    (file_to_canon, canon_to_file)
}

fn align_parsed_to_orig(orig_copper: &Copper, pcb: &mut Pcb, copper: &mut Copper) {
    let (_, orig_fp_ord) = footprint_file_ordinals(orig_copper);
    let (new_file_to_fp, _) = footprint_file_ordinals(copper);
    if orig_fp_ord.len() == new_file_to_fp.len() && pcb.footprints.len() == orig_fp_ord.len() {
        let n = orig_fp_ord.len();
        let new_of_orig: Vec<usize> = (0..n).map(|f| new_file_to_fp[orig_fp_ord[f]]).collect();
        if new_of_orig.iter().enumerate().any(|(i, &j)| i != j) {
            let mut orig_of_new = vec![0usize; n];
            for (orig_f, &new_f) in new_of_orig.iter().enumerate() {
                orig_of_new[new_f] = orig_f;
            }
            let mut old_fps: Vec<Option<Footprint>> =
                std::mem::take(&mut pcb.footprints).into_iter().map(Some).collect();
            pcb.footprints = new_of_orig.iter().filter_map(|&f| old_fps[f].take()).collect();
            copper.locked_footprints =
                new_of_orig.iter().map(|&f| copper.locked_footprints[f]).collect();
            copper.footprint_at = new_of_orig.iter().map(|&f| copper.footprint_at[f]).collect();
            let mut old_yards = std::mem::take(&mut copper.courtyards);
            copper.courtyards = new_of_orig.iter().map(|&f| old_yards[f].take()).collect();
            for pad in &mut copper.pads {
                pad.footprint = orig_of_new[pad.footprint];
            }
            copper.pads.sort_by_key(|p| (p.footprint, p.pad));
            for z in &mut copper.zones {
                z.footprint = z.footprint.map(|f| orig_of_new[f]);
            }
            for g in copper.graphics.iter_mut().chain(&mut copper.mask_graphics) {
                g.footprint = g.footprint.map(|f| orig_of_new[f]);
            }
        }
    }

    let (_, orig_z_ord) = zone_file_ordinals(orig_copper);
    let (new_file_to_z, _) = zone_file_ordinals(copper);
    if orig_z_ord.len() == new_file_to_z.len() && copper.zones.len() == orig_z_ord.len() {
        let nz = orig_z_ord.len();
        let new_of_orig_z: Vec<usize> = (0..nz).map(|z| new_file_to_z[orig_z_ord[z]]).collect();
        if new_of_orig_z.iter().enumerate().any(|(i, &j)| i != j) {
            let mut old_zones: Vec<Option<Zone>> =
                std::mem::take(&mut copper.zones).into_iter().map(Some).collect();
            copper.zones = new_of_orig_z.iter().filter_map(|&z| old_zones[z].take()).collect();
        }
    }
}

pub(crate) fn extract_footprint_yards(
    text: &str,
    pcb: &Pcb,
    copper: &Copper,
) -> Vec<FootprintYards> {
    let mut out = vec![FootprintYards::default(); pcb.footprints.len()];
    let Ok(root) = kicad_pcb::sexpr::parse(text) else {
        return out;
    };
    let (file_to_canon, _) = footprint_file_ordinals(copper);
    let mut fp_file_ord = 0usize;
    for child in root.args() {
        if child.head() != Some("footprint") {
            continue;
        }
        if let Some(&f) = file_to_canon.get(fp_file_ord) {
            if let Some(fp) = pcb.footprints.get(f) {
                out[f] = parse_one_footprint_yards(child, fp);
            }
        }
        fp_file_ord += 1;
    }
    out
}

fn parse_one_footprint_yards(fp_node: &Node, fp: &Footprint) -> FootprintYards {
    const MM: f64 = 1e-3;
    let pt_of = |n: &Node| -> Option<[f64; 2]> {
        Some([n.number_arg(0, "pt").ok()? * MM, n.number_arg(1, "pt").ok()? * MM])
    };
    let named_pt = |g: &Node, tag: &str| -> Option<[f64; 2]> { g.find(tag).and_then(pt_of) };

    let mut closed_front: Vec<Vec<Point>> = Vec::new();
    let mut closed_back: Vec<Vec<Point>> = Vec::new();
    let mut chains_front: Vec<Vec<Point>> = Vec::new();
    let mut chains_back: Vec<Vec<Point>> = Vec::new();

    for g in fp_node.args() {
        let Some(layer) = g.find("layer").and_then(|l| l.args().first()).and_then(Node::text)
        else {
            continue;
        };
        let (on_f, on_b) = match layer {
            "F.CrtYd" => (true, false),
            "B.CrtYd" => (false, true),
            "*.CrtYd" | "F&B.CrtYd" => (true, true),
            _ => continue,
        };
        match g.head() {
            Some("fp_rect") => {
                let (Some(a), Some(b)) = (named_pt(g, "start"), named_pt(g, "end")) else {
                    continue;
                };
                let corners = [a, [b[0], a[1]], b, [a[0], b[1]]];
                let poly = clean_yard_poly(
                    &corners.into_iter().map(|p| Point::from_m(fp.place(p))).collect::<Vec<_>>(),
                );
                if poly.len() >= 3 {
                    if on_f {
                        closed_front.push(poly.clone());
                    }
                    if on_b {
                        closed_back.push(poly);
                    }
                }
            }
            Some("fp_poly") => {
                let Some(pts_node) = g.find("pts") else { continue };
                let mut raw: Vec<Point> = Vec::new();
                for item in pts_node.args() {
                    match item.head() {
                        Some("xy") => {
                            if let Some(p) = pt_of(item) {
                                raw.push(Point::from_m(fp.place(p)));
                            }
                        }
                        Some("arc") => {
                            if let (Some(s), Some(m), Some(e)) = (
                                named_pt(item, "start"),
                                named_pt(item, "mid"),
                                named_pt(item, "end"),
                            ) {
                                let (ch, _) =
                                    arc_chords(fp.place(s), fp.place(m), fp.place(e), 0.0);
                                raw.extend(ch);
                            }
                        }
                        _ => {}
                    }
                }
                let cleaned = clean_yard_poly(&raw);
                let poly = if cleaned.len() >= 3 && !poly_self_intersects(&cleaned) {
                    cleaned
                } else {
                    convex_hull_points(raw)
                };
                if poly.len() >= 3 && polygon_abs_area2(&poly) >= 2_000_000.0 {
                    if on_f {
                        closed_front.push(poly.clone());
                    }
                    if on_b {
                        closed_back.push(poly);
                    }
                }
            }
            Some("fp_circle") => {
                let (Some(c), Some(e)) = (named_pt(g, "center"), named_pt(g, "end")) else {
                    continue;
                };
                let r = (e[0] - c[0]).hypot(e[1] - c[1]);
                if r <= 0.0 {
                    continue;
                }
                let n = 24usize;
                let ro = r / (std::f64::consts::PI / n as f64).cos();
                let poly: Vec<Point> = (0..n)
                    .map(|k| {
                        let ang = std::f64::consts::TAU * k as f64 / n as f64;
                        Point::from_m(fp.place([c[0] + ro * ang.cos(), c[1] + ro * ang.sin()]))
                    })
                    .collect();
                if on_f {
                    closed_front.push(poly.clone());
                }
                if on_b {
                    closed_back.push(poly);
                }
            }
            Some("fp_line") => {
                let (Some(a), Some(b)) = (named_pt(g, "start"), named_pt(g, "end")) else {
                    continue;
                };
                let (pa, pb) = (Point::from_m(fp.place(a)), Point::from_m(fp.place(b)));
                if pa != pb {
                    let ch = vec![pa, pb];
                    if on_f {
                        chains_front.push(ch.clone());
                    }
                    if on_b {
                        chains_back.push(ch);
                    }
                }
            }
            Some("fp_arc") => {
                let (Some(s), Some(m), Some(e)) =
                    (named_pt(g, "start"), named_pt(g, "mid"), named_pt(g, "end"))
                else {
                    continue;
                };
                let (mut ch, _) = arc_chords(fp.place(s), fp.place(m), fp.place(e), 0.0);
                ch.dedup();
                if ch.len() >= 2 {
                    if on_f {
                        chains_front.push(ch.clone());
                    }
                    if on_b {
                        chains_back.push(ch);
                    }
                }
            }
            _ => {}
        }
    }

    closed_front.extend(assemble_yard_chains(chains_front));
    closed_back.extend(assemble_yard_chains(chains_back));
    FootprintYards { front: closed_front, back: closed_back }
}

fn assemble_yard_chains(mut chains: Vec<Vec<Point>>) -> Vec<Vec<Point>> {
    const WELD_NM: f64 = 15_000.0;
    let nc = chains.len();
    if nc == 0 {
        return Vec::new();
    }

    let mut ep_pts: Vec<Point> = Vec::with_capacity(2 * nc);
    for c in &chains {
        ep_pts.push(c[0]);
        ep_pts.push(*c.last().unwrap());
    }
    let mut uf: Vec<usize> = (0..2 * nc).collect();
    fn find(uf: &mut [usize], mut x: usize) -> usize {
        while uf[x] != x {
            uf[x] = uf[uf[x]];
            x = uf[x];
        }
        x
    }
    for i in 0..2 * nc {
        for j in (i + 1)..2 * nc {
            if ep_pts[i].distance(ep_pts[j]) <= WELD_NM {
                let (ri, rj) = (find(&mut uf, i), find(&mut uf, j));
                if ri != rj {
                    uf[rj] = ri;
                }
            }
        }
    }

    let mut root_to_vid: HashMap<usize, usize> = HashMap::new();
    let mut v_sum: Vec<(i128, i128, usize)> = Vec::new();
    let mut ep_vid = vec![0usize; 2 * nc];
    for i in 0..2 * nc {
        let r = find(&mut uf, i);
        let vid = *root_to_vid.entry(r).or_insert_with(|| {
            let id = v_sum.len();
            v_sum.push((0, 0, 0));
            id
        });
        ep_vid[i] = vid;
        v_sum[vid].0 += ep_pts[i].x as i128;
        v_sum[vid].1 += ep_pts[i].y as i128;
        v_sum[vid].2 += 1;
    }
    let v_pos: Vec<Point> = v_sum
        .into_iter()
        .map(|(sx, sy, cnt)| Point::new((sx / cnt as i128) as Nm, (sy / cnt as i128) as Nm))
        .collect();

    let mut ch_ends: Vec<(usize, usize)> = Vec::with_capacity(nc);
    for (ci, c) in chains.iter_mut().enumerate() {
        let u = ep_vid[2 * ci];
        let v = ep_vid[2 * ci + 1];
        c[0] = v_pos[u];
        *c.last_mut().unwrap() = v_pos[v];
        ch_ends.push((u, v));
    }

    let mut c_uf: Vec<usize> = (0..nc).collect();
    let mut first_chain_at_v: Vec<Option<usize>> = vec![None; v_pos.len()];
    for (ci, &(u, v)) in ch_ends.iter().enumerate() {
        for vid in [u, v] {
            if let Some(prev) = first_chain_at_v[vid] {
                let (ra, rb) = (find(&mut c_uf, prev), find(&mut c_uf, ci));
                if ra != rb {
                    c_uf[rb] = ra;
                }
            } else {
                first_chain_at_v[vid] = Some(ci);
            }
        }
    }

    let mut comps: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut comp_order: Vec<usize> = Vec::new();
    for ci in 0..nc {
        let r = find(&mut c_uf, ci);
        let entry = comps.entry(r).or_insert_with(|| {
            comp_order.push(r);
            Vec::new()
        });
        entry.push(ci);
    }

    let mut out = Vec::new();
    for r in comp_order {
        let comp = &comps[&r];
        if comp.len() == 1 {
            let c0 = comp[0];
            if ch_ends[c0].0 == ch_ends[c0].1 && chains[c0].len() >= 4 {
                let poly = clean_yard_poly(&chains[c0][..chains[c0].len() - 1]);
                let fin = if poly.len() >= 3 && !poly_self_intersects(&poly) {
                    poly
                } else {
                    convex_hull_points(chains[c0].clone())
                };
                if fin.len() >= 3 && polygon_abs_area2(&fin) >= 2_000_000.0 {
                    out.push(fin);
                }
                continue;
            }
        }

        let mut deg: HashMap<usize, usize> = HashMap::new();
        let mut all_open = true;
        for &ci in comp {
            let (u, v) = ch_ends[ci];
            if u == v {
                all_open = false;
                break;
            }
            *deg.entry(u).or_insert(0) += 1;
            *deg.entry(v).or_insert(0) += 1;
        }

        if all_open && deg.values().all(|&d| d == 2) {
            let mut visited = vec![false; comp.len()];
            let mut cycles = Vec::new();
            let mut ok = true;
            for start_idx in 0..comp.len() {
                if visited[start_idx] {
                    continue;
                }
                visited[start_idx] = true;
                let c0 = comp[start_idx];
                let start_v = ch_ends[c0].0;
                let mut cur_v = ch_ends[c0].1;
                let mut pts: Vec<Point> = chains[c0][..chains[c0].len() - 1].to_vec();
                for _ in 0..=comp.len() {
                    if cur_v == start_v {
                        break;
                    }
                    let Some(next_idx) = (0..comp.len()).find(|&k| {
                        !visited[k]
                            && (ch_ends[comp[k]].0 == cur_v || ch_ends[comp[k]].1 == cur_v)
                    }) else {
                        ok = false;
                        break;
                    };
                    visited[next_idx] = true;
                    let ck = comp[next_idx];
                    if ch_ends[ck].0 == cur_v {
                        pts.extend_from_slice(&chains[ck][..chains[ck].len() - 1]);
                        cur_v = ch_ends[ck].1;
                    } else {
                        pts.extend(chains[ck][1..].iter().rev().copied());
                        cur_v = ch_ends[ck].0;
                    }
                }
                if !ok || cur_v != start_v {
                    ok = false;
                    break;
                }
                let poly = clean_yard_poly(&pts);
                if poly.len() >= 3
                    && polygon_abs_area2(&poly) >= 2_000_000.0
                    && !poly_self_intersects(&poly)
                {
                    cycles.push(poly);
                } else {
                    ok = false;
                    break;
                }
            }
            if ok && !cycles.is_empty() {
                out.extend(cycles);
                continue;
            }
        }

        let all_pts: Vec<Point> = comp.iter().flat_map(|&ci| chains[ci].iter().copied()).collect();
        let hull = convex_hull_points(all_pts);
        if hull.len() >= 3 && polygon_abs_area2(&hull) >= 2_000_000.0 {
            out.push(hull);
        }
    }
    out
}

fn clean_yard_poly(pts: &[Point]) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(pts.len());
    for &p in pts {
        if out.last().is_none_or(|&q| q.distance(p) > 500.0) {
            out.push(p);
        }
    }
    while out.len() >= 2 && out.first().unwrap().distance(*out.last().unwrap()) <= 500.0 {
        out.pop();
    }
    out
}

fn polygon_abs_area2(poly: &[Point]) -> f64 {
    if poly.len() < 3 {
        return 0.0;
    }
    let mut s = 0.0f64;
    for i in 0..poly.len() {
        let a = poly[i];
        let b = poly[(i + 1) % poly.len()];
        s += a.x as f64 * b.y as f64 - b.x as f64 * a.y as f64;
    }
    s.abs()
}

fn poly_self_intersects(poly: &[Point]) -> bool {
    let n = poly.len();
    if n < 3 {
        return true;
    }
    if n == 3 {
        return polygon_abs_area2(poly) < 2_000_000.0;
    }
    let cross = |o: Point, a: Point, b: Point| -> f64 {
        (a.x - o.x) as f64 * (b.y - o.y) as f64 - (a.y - o.y) as f64 * (b.x - o.x) as f64
    };
    let on_seg = |p: Point, a: Point, b: Point| -> bool {
        point_segment(p, a, b) <= 250.0
    };
    for i in 0..n {
        let (a0, a1) = (poly[i], poly[(i + 1) % n]);
        let bba = BBox::of([a0, a1]);
        for j in (i + 2)..n {
            if i == 0 && j + 1 == n {
                continue;
            }
            let (b0, b1) = (poly[j], poly[(j + 1) % n]);
            if !bba.overlaps(BBox::of([b0, b1])) {
                continue;
            }
            let d1 = cross(a0, a1, b0);
            let d2 = cross(a0, a1, b1);
            let d3 = cross(b0, b1, a0);
            let d4 = cross(b0, b1, a1);
            if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
                && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
            {
                return true;
            }
            if on_seg(b0, a0, a1) || on_seg(b1, a0, a1) || on_seg(a0, b0, b1) || on_seg(a1, b0, b1)
            {
                return true;
            }
        }
    }
    false
}

fn convex_hull_points(mut pts: Vec<Point>) -> Vec<Point> {
    pts.sort_by_key(|p| (p.x, p.y));
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let cross = |o: Point, a: Point, b: Point| -> i128 {
        (a.x - o.x) as i128 * (b.y - o.y) as i128 - (a.y - o.y) as i128 * (b.x - o.x) as i128
    };
    let mut lower: Vec<Point> = Vec::new();
    for &p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<Point> = Vec::new();
    for &p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
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
    let (_, fp_canon_to_file) = footprint_file_ordinals(orig_copper);
    let (z_file_to_canon, z_canon_to_file) = zone_file_ordinals(orig_copper);
    let fp_edits: Vec<(usize, [f64; 2], [f64; 2])> = fp_shifts
        .iter()
        .map(|&(f, shift)| {
            let orig = orig_pcb.footprints[f].at;
            let orig_nm = Point::from_m(orig);
            let to_m = [(orig_nm.x + shift.x) as f64 * 1e-9, (orig_nm.y + shift.y) as f64 * 1e-9];
            (fp_canon_to_file[f], orig, to_m)
        })
        .collect();
    let plane_flags: Vec<bool> =
        orig_copper.zones.iter().map(|z| is_board_plane_zone(orig_board_bb, z)).collect();
    let zone_map = |z_file: usize, p: [f64; 2]| -> [f64; 2] {
        let is_plane = z_file_to_canon
            .get(z_file)
            .and_then(|&z| plane_flags.get(z))
            .copied()
            .unwrap_or(true);
        if is_plane {
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
    let (mut pcb, mut copper) = parse_board(&unfilled_text).map_err(|e| e.to_string())?;
    align_parsed_to_orig(orig_copper, &mut pcb, &mut copper);
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

    let file_fills: Vec<ZoneFill> = fills
        .into_iter()
        .map(|zf| ZoneFill { zone: z_canon_to_file[zf.zone], ..zf })
        .collect();
    let (rem2, rep2) =
        scale_edits(bare_text, centre_m, factor, &fp_edits, Some(&zone_map), &file_fills)
            .map_err(|e| e.to_string())?;
    let filled_text = apply(bare_text, &Edit { remove: rem2, replace: rep2, ..Edit::default() });
    let (mut pcb2, mut copper2) = parse_board(&filled_text).map_err(|e| e.to_string())?;
    align_parsed_to_orig(orig_copper, &mut pcb2, &mut copper2);
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

    let mut poured: Vec<(u32, Nm, usize, PourRaster)> = Vec::new();
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
                poured.push((zone.net, nm(zone.clearance), layer, r));
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
    poured: &[(u32, Nm, usize, PourRaster)],
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
    let inside_or_on_outline = |c: Point| -> bool {
        board::inside(c, &outline)
            || (0..outline.len())
                .any(|m| point_segment(c, outline[m], outline[(m + 1) % outline.len()]) <= 500.0)
    };
    for k in 0..outline.len() {
        let (a, e) = (outline[k], outline[(k + 1) % outline.len()]);
        let (cols, rows) = r.span(BBox::of([a, e]).grow(POUR_STEP));
        for j in rows {
            for i in cols.clone() {
                let idx = j * r.w + i;
                if !r.cells[idx] || point_segment(r.center(i, j), a, e) >= POUR_CORNER_PAD {
                    continue;
                }
                let x0 = (r.i0 + i as i64) * POUR_STEP;
                let y0 = (r.j0 + j as i64) * POUR_STEP;
                let vert_inside = [a, e].iter().any(|v| {
                    v.x > x0 + 500
                        && v.x < x0 + POUR_STEP - 500
                        && v.y > y0 + 500
                        && v.y < y0 + POUR_STEP - 500
                });
                let corners = [
                    Point::new(x0, y0),
                    Point::new(x0 + POUR_STEP, y0),
                    Point::new(x0, y0 + POUR_STEP),
                    Point::new(x0 + POUR_STEP, y0 + POUR_STEP),
                ];
                if vert_inside || !corners.iter().all(|&c| inside_or_on_outline(c)) {
                    r.cells[idx] = false;
                }
            }
        }
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
    for (other, other_own, l, done) in poured {
        if *l != layer || *other == net {
            continue;
        }
        let c = own.max(*other_own).max(board.clearance(net, *other)) as f64 + POUR_CORNER_PAD;
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
                        q.distance(p) < c + POUR_CORNER_PAD
                    });
                }
            }
        }
    }
    r.open((nm(zone.min_thickness) - POUR_STEP).max(0) / 2);
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
            net: board.file_net(v.net),
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
                net: board.file_net(net),
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

    #[test]
    fn scale_anchors_offset_origin_parts_and_carries_passives_inside_connector_bosses() {
        // Mirrors earring's BT1 (origin at board centre (100, 100), two disjoint B.Cu tabs and
        // B.CrtYd islands near y = 83.65) and J1 (connector at (100, 83.41) with four thru_hole
        // shield tabs and two unnamed "" np_thru_hole bosses framing an F.Cu passive R5 at
        // (102, 85.4)). Under 1.1x expansion about centre (100, 100), BT1 must move north (-y,
        // away from 100) rather than south, and R5 must ride rigidly with J1 without hitting J1's
        // unnamed NPTH boss.
        let text = "(kicad_pcb
\t(version 20241229)
\t(general (thickness 1.6))
\t(layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal) (44 \"Edge.Cuts\" user) (46 \"B.CrtYd\" user) (47 \"F.CrtYd\" user))
\t(net 0 \"\")
\t(net 1 \"VBAT\")
\t(net 2 \"GND\")
\t(net 3 \"CC1\")
\t(gr_line (start 80 75) (end 120 75) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e1\"))
\t(gr_line (start 120 75) (end 120 125) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e2\"))
\t(gr_line (start 120 125) (end 80 125) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e3\"))
\t(gr_line (start 80 125) (end 80 75) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e4\"))
\t(footprint \"BT\" (layer \"B.Cu\") (uuid \"fbt\") (at 100 100 180)
\t\t(property \"Reference\" \"BT1\" (at 0 0 0) (layer \"B.SilkS\") (uuid \"rbt\"))
\t\t(fp_rect (start -9.5 14.5) (end -6.5 17.5) (stroke (width 0.05)) (layer \"B.CrtYd\") (uuid \"cbt1\"))
\t\t(fp_rect (start 6.5 15.0) (end 9.5 18.0) (stroke (width 0.05)) (layer \"B.CrtYd\") (uuid \"cbt2\"))
\t\t(pad \"1\" smd rect (at -8.0 16.0 180) (size 2.0 2.0) (layers \"B.Cu\") (net 1 \"VBAT\") (uuid \"pbt1\"))
\t\t(pad \"2\" smd rect (at 8.0 16.7 180) (size 2.0 2.0) (layers \"B.Cu\") (net 2 \"GND\") (uuid \"pbt2\"))
\t)
\t(footprint \"USB\" (layer \"B.Cu\") (uuid \"fj1\") (at 100 83.41)
\t\t(property \"Reference\" \"J1\" (at 0 0 0) (layer \"B.SilkS\") (uuid \"rj1\"))
\t\t(fp_rect (start -4.8 -3.2) (end 4.8 2.8) (stroke (width 0.05)) (layer \"B.CrtYd\") (uuid \"cj1\"))
\t\t(pad \"\" np_thru_hole circle (at -2.9 1.21) (size 1.2 1.2) (drill 1.2) (layers \"*.Cu\" \"*.Mask\") (uuid \"pj_np1\"))
\t\t(pad \"\" np_thru_hole circle (at 2.9 1.21) (size 1.2 1.2) (drill 1.2) (layers \"*.Cu\" \"*.Mask\") (uuid \"pj_np2\"))
\t\t(pad \"S1\" thru_hole oval (at -4.33 -2.47) (size 1.0 1.8) (drill oval 0.6 1.4) (layers \"*.Cu\" \"*.Mask\") (net 2 \"GND\") (uuid \"pjs1\"))
\t\t(pad \"S2\" thru_hole oval (at 4.33 -2.47) (size 1.0 1.8) (drill oval 0.6 1.4) (layers \"*.Cu\" \"*.Mask\") (net 2 \"GND\") (uuid \"pjs2\"))
\t\t(pad \"S3\" thru_hole oval (at -4.33 2.00) (size 1.0 1.8) (drill oval 0.6 1.4) (layers \"*.Cu\" \"*.Mask\") (net 2 \"GND\") (uuid \"pjs3\"))
\t\t(pad \"S4\" thru_hole oval (at 4.33 2.00) (size 1.0 1.8) (drill oval 0.6 1.4) (layers \"*.Cu\" \"*.Mask\") (net 2 \"GND\") (uuid \"pjs4\"))
\t\t(pad \"A5\" smd rect (at 0.5 2.4) (size 0.3 0.9) (layers \"B.Cu\" \"B.Mask\") (net 3 \"CC1\") (uuid \"pja5\"))
\t)
\t(footprint \"R0402\" (layer \"F.Cu\") (uuid \"fr5\") (at 101.65 85.10 90)
\t\t(property \"Reference\" \"R5\" (at 0 0 90) (layer \"F.SilkS\") (uuid \"rr5\"))
\t\t(fp_rect (start -0.75 -0.45) (end 0.75 0.45) (stroke (width 0.05)) (layer \"F.CrtYd\") (uuid \"cr5\"))
\t\t(pad \"1\" smd rect (at -0.48 0 90) (size 0.5 0.6) (layers \"F.Cu\" \"F.Mask\") (net 3 \"CC1\") (uuid \"pr51\"))
\t\t(pad \"2\" smd rect (at 0.48 0 90) (size 0.5 0.6) (layers \"F.Cu\" \"F.Mask\") (net 2 \"GND\") (uuid \"pr52\"))
\t)
)\n";
        let rules = Rules::parse("{}").unwrap();
        let rep = scale(
            text,
            rules.clone(),
            &ScaleOptions { factor: 1.1, lock: HashSet::new(), uuid_seed: Some(21) },
        )
        .unwrap();
        let (pcb, copper) = parse_board(&rep.text).unwrap();
        // BT1's physical pads sit near y = 83.65 mm (< centre y = 100 mm), so 1.1x expansion must
        // shift BT1 north (at.y < 100 mm), whereas origin-based scaling left it at y = 100 mm.
        assert!(
            pcb.footprints[0].at[1] < 98.6e-3,
            "BT1 shifted north with its physical pads: y={}",
            pcb.footprints[0].at[1]
        );
        // R5 sits inside J1's through-hole/NPTH pocket, so R5 translates by the exact same delta
        // as J1 instead of colliding with J1's +2.9 mm NPTH boss.
        let dj_y = pcb.footprints[1].at[1] - 83.41e-3;
        let dr_y = pcb.footprints[2].at[1] - 85.10e-3;
        assert!(
            (dj_y - dr_y).abs() < 2e-6,
            "R5 rode rigidly with enclosing J1: dj_y={dj_y}, dr_y={dr_y}"
        );
        let board = Board::new(&pcb, &copper, rules);
        assert!(check(&board).is_empty(), "zero DRC violations after 1.1x scale: {:?}", check(&board));
    }
}
