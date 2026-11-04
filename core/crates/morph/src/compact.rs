//! Iterative board compaction: parts step toward scaled positions about their physical anchors,
//! copper flows with each step's harmonic warp field, pinched through-vias can slide along their
//! own net's tracks out of narrow pad throats, and a joint spring-and-clearance solver relaxes
//! free track vertices, vias and unlocked rigid footprints together so one pinched part stops in
//! place while the rest of the board continues shrinking. No track or via is ever ripped or
//! rerouted; if a step cannot satisfy every rule without losing connectivity, compaction reverts
//! cleanly to the last passing step.

use std::collections::{HashMap, HashSet};

use board::{
    arc_chords, check_item, connectivity_without_pours, inside, mask_pair_gap, nm, point_segment,
    BBox, Board, Item, ItemId, Kind, Nm, Point, Rule, Shape, Thing, Violation, ANY_ONE_NET,
};
use kicad_pcb::{apply, parse_board, Copper, Edit, NewTrack, NewVia, Pcb, Rules};

use crate::scale::{
    bbox_centre, bbox_size_mm, board_bounds, build_scale_field, build_scaled_bare_board,
    compute_footprint_envelopes, extract_footprint_yards, recarve_kept_zones_in_board,
    strip_kept_zone_pours, strip_routed_copper, FootprintEnvelope, FootprintYards,
};
use crate::warp::WarpField;

/// Maximum segment length before each warp step so long stretches follow curved warp fields while
/// shorter straight runs stay whole unless split on demand by an obstacle (`MIN_SPLIT_LEN`).
const SUBDIVIDE_LEN: f64 = 1_200_000.0;
/// Shortest edge that dynamic obstacle-bending will split in two.
const MIN_SPLIT_LEN: f64 = 90_000.0;
/// Extra cushion in nanometres above `check_item` clearances during continuous relaxation so
/// integer rounding still leaves every gap strictly legal.
const CLEARANCE_BUFFER: f64 = 2_500.0;
/// Spatial hash cell for dynamic track/via proximity queries during relaxation.
const DYN_CELL: i64 = 500_000;
/// Per-iteration clearance displacement cap in nanometres (35 um) so no track or via can jump
/// across a pad's centreline in a single step.
const MAX_STEP_NM: f64 = 35_000.0;
/// Per-iteration constant-tension pull in nanometres (3.5 um) during the spring phase of each pass.
const TENSION_STEP_NM: f64 = 3_500.0;

#[derive(Debug, Clone)]
pub struct CompactOptions {
    pub factor: f64,
    pub steps: Option<usize>,
    pub lock: HashSet<String>,
    pub uuid_seed: Option<u64>,
    pub verbose: bool,
}

impl Default for CompactOptions {
    fn default() -> Self {
        CompactOptions {
            factor: 1.0,
            steps: None,
            lock: HashSet::new(),
            uuid_seed: None,
            verbose: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AcidTraps {
    pub acute: usize,
    pub slots: usize,
}

#[derive(Debug, Clone)]
pub struct StepReport {
    pub step: usize,
    pub factor: f64,
    pub accepted: bool,
    pub size_mm: [f64; 2],
    pub violations: usize,
    pub track_length_mm: f64,
    pub segments: usize,
    pub vias: usize,
    pub acid_traps: AcidTraps,
    pub courtyard_stops: usize,
    pub part_stops: usize,
    pub via_slides: usize,
    pub jam: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CompactReport {
    pub text: String,
    pub target_factor: f64,
    pub achieved_factor: f64,
    pub steps_requested: usize,
    pub steps_accepted: usize,
    pub steps: Vec<StepReport>,
    pub jam: Option<String>,
}

#[derive(Debug, Clone)]
enum Anchor {
    Pad {
        footprint: usize,
        offset: (f64, f64),
    },
    PadVia {
        footprint: usize,
        fp_ref: Point,
        orig_pad: Shape,
        cur_pad: Shape,
        init_margin: f64,
        r: Nm,
        drill_r: Nm,
        layers: u64,
        net: u32,
        locked: bool,
    },
    Via {
        r: Nm,
        drill_r: Nm,
        layers: u64,
        net: u32,
        locked: bool,
    },
    Free,
}

#[derive(Debug, Clone)]
struct Particle {
    pos: (f64, f64),
    anchor: Anchor,
    locked: bool,
    via_tether: Option<(usize, f64)>,
    plane_zones: Vec<(usize, f64)>,
}

impl Particle {
    fn mobility(&self) -> f64 {
        if self.locked {
            return 0.0;
        }
        match self.anchor {
            Anchor::Pad { .. } => 0.0,
            Anchor::PadVia { locked: true, .. } | Anchor::Via { locked: true, .. } => 0.0,
            Anchor::PadVia { locked: false, .. } => 0.50,
            Anchor::Via { .. } => 0.60,
            Anchor::Free => 1.0,
        }
    }

    fn is_via(&self) -> Option<(Nm, Nm, u64, u32, bool)> {
        match self.anchor {
            Anchor::PadVia { r, drill_r, layers, net, locked, .. }
            | Anchor::Via { r, drill_r, layers, net, locked } => {
                Some((r, drill_r, layers, net, locked))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Edge {
    u: usize,
    v: usize,
    layer: usize,
    net: u32,
    r: Nm,
    locked: bool,
    active: bool,
}

#[derive(Debug, Clone)]
struct CopperGraph {
    particles: Vec<Particle>,
    edges: Vec<Edge>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum StaticCheckKind {
    BoardRule(Rule),
    PadMaskBridge,
    HoleEdge,
}

/// Static violation signature `(fp_a, pad_ord_a, fp_b_opt, pad_ord_b_opt, kind)` already present
/// on the input board at `factor = 1.0`, so pre-existing footprint geometry does not falsely fail
/// step 1. Pad ordinals within each footprint stay invariant across bare-board rebuilds and
/// distinguish unnamed (`""`) NPTH mounting pads on the same footprint.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StaticViolKey {
    kind: StaticCheckKind,
    a_fp: usize,
    a_pad_ord: usize,
    b_fp: Option<usize>,
    b_pad_ord: Option<usize>,
}

/// Compacts (or expands) a routed `.kicad_pcb` toward `opts.factor` in small steps without
/// ripping or rerouting any copper.
pub fn compact(text: &str, rules: Rules, opts: &CompactOptions) -> Result<CompactReport, String> {
    if !opts.factor.is_finite() || opts.factor <= 0.0 {
        return Err(format!("--factor {}: must be a positive number", opts.factor));
    }
    let steps_req = opts
        .steps
        .unwrap_or_else(|| ((1.0 - opts.factor).abs() * 100.0).ceil().max(1.0) as usize)
        .max(1);

    let (orig_pcb, orig_copper) = parse_board(text).map_err(|e| e.to_string())?;
    let orig_board = Board::new(&orig_pcb, &orig_copper, rules.clone());
    let orig_bb = board_bounds(&orig_board);
    let centre = bbox_centre(orig_bb);
    let orig_had_outline = !orig_board.outline.is_empty();

    let orig_fp_at: Vec<Point> =
        orig_pcb.footprints.iter().map(|f| Point::from_m(f.at)).collect();
    let fp_locked: Vec<bool> = orig_board
        .footprints
        .iter()
        .map(|f| opts.lock.contains(&f.reference))
        .collect();
    let orig_yards = extract_footprint_yards(text, &orig_pcb, &orig_copper);
    let fp_envs = compute_footprint_envelopes(&orig_pcb, &orig_copper, &orig_board, &orig_yards);
    let initial_yard_pen = courtyard_penetrations(&orig_yards, &orig_fp_at, &orig_fp_at);

    let bare_text = strip_routed_copper(text, &orig_copper);
    let mut graph = CopperGraph::from_board(&orig_board, &orig_copper, &orig_fp_at);
    let orig_locked_items = graph.locked_items();

    // Build step 0 in the same representation as all subsequent steps for exact baseline metrics.
    let zero_shifts: Vec<(usize, Point)> =
        (0..orig_fp_at.len()).map(|f| (f, Point::new(0, 0))).collect();
    let zero_field = build_scale_field(&orig_board, centre, 1.0, &zero_shifts);
    let (mut cur_board, zero_copper, mut cur_bare_text) = build_scaled_bare_board(
        &bare_text,
        &rules,
        &orig_pcb,
        &orig_copper,
        orig_bb,
        centre,
        1.0,
        &zero_shifts,
        &zero_field,
        &orig_locked_items,
    )?;
    let baseline_static_viols = collect_static_violations(&cur_board, &HashSet::new()).0;
    let (_, _) = graph.sync_to_board(&mut cur_board);
    recarve_kept_zones_in_board(&mut cur_board, &zero_copper);
    let baseline_airlines = baseline_connectivity(&cur_board);
    let baseline_dyn_viols = count_dynamic_violations(&cur_board).0;

    // Record which unfilled zone outlines each via initially sat inside so plane stitching vias do
    // not drift outside their plane polygon.
    record_via_zones(&cur_board, &mut graph);

    let step0 = StepReport {
        step: 0,
        factor: 1.0,
        accepted: true,
        size_mm: bbox_size_mm(board_bounds(&cur_board)),
        violations: baseline_dyn_viols,
        track_length_mm: total_track_length_mm(&cur_board),
        segments: count_tracks(&cur_board),
        vias: count_vias(&cur_board),
        acid_traps: dfm_counts(&cur_board),
        courtyard_stops: 0,
        part_stops: 0,
        via_slides: 0,
        jam: None,
    };
    if opts.verbose {
        eprintln!(
            "compact step 0/{steps_req} (factor 1.0000): {:.3} x {:.3} mm, {} violations, {:.2} mm ({} segs), {} vias, acid traps {} acute / {} slots",
            step0.size_mm[0],
            step0.size_mm[1],
            step0.violations,
            step0.track_length_mm,
            step0.segments,
            step0.vias,
            step0.acid_traps.acute,
            step0.acid_traps.slots
        );
    }

    let mut step_reports = vec![step0];
    let mut cur_fp_pos = orig_fp_at.clone();
    let mut cur_factor = 1.0;
    let mut accepted_steps = 0usize;
    let mut final_jam: Option<String> = None;

    if (opts.factor - 1.0).abs() <= 1e-12 {
        let out_text = graph.write_onto(&cur_bare_text, &cur_board, opts.uuid_seed);
        return Ok(CompactReport {
            text: out_text,
            target_factor: opts.factor,
            achieved_factor: 1.0,
            steps_requested: steps_req,
            steps_accepted: steps_req,
            steps: step_reports,
            jam: None,
        });
    }

    for step in 1..=steps_req {
        let f_s = if step == steps_req {
            opts.factor
        } else {
            1.0 + (opts.factor - 1.0) * (step as f64 / steps_req as f64)
        };
        let step_df = f_s - cur_factor;

        let mut nominal_step_pos = cur_fp_pos.clone();
        for f in 0..orig_fp_at.len() {
            if fp_locked[f] || fp_envs[f].parent.is_some() {
                continue;
            }
            let anch = fp_envs[f].anchor;
            let ideal = (
                orig_fp_at[f].x as f64 + (f_s - 1.0) * (anch.0 - centre.0),
                orig_fp_at[f].y as f64 + (f_s - 1.0) * (anch.1 - centre.1),
            );
            let inc = (
                cur_fp_pos[f].x as f64 + step_df * (anch.0 - centre.0),
                cur_fp_pos[f].y as f64 + step_df * (anch.1 - centre.1),
            );
            let catchup = clamp_vec((ideal.0 - inc.0, ideal.1 - inc.1), 35_000.0);
            nominal_step_pos[f] = Point::new(
                (inc.0 + catchup.0).round() as Nm,
                (inc.1 + catchup.1).round() as Nm,
            );
        }
        for f in 0..orig_fp_at.len() {
            if fp_locked[f] {
                continue;
            }
            if let Some(p) = fp_envs[f].parent {
                nominal_step_pos[f] = Point::new(
                    cur_fp_pos[f].x + (nominal_step_pos[p].x - cur_fp_pos[p].x),
                    cur_fp_pos[f].y + (nominal_step_pos[p].y - cur_fp_pos[p].y),
                );
            }
        }
        let mut cand_pos = nominal_step_pos.clone();

        let yard_stops = resolve_courtyard_stops(
            &orig_yards,
            &fp_envs,
            &fp_locked,
            &orig_fp_at,
            &cur_fp_pos,
            &mut cand_pos,
            &initial_yard_pen,
        );

        let Some((step_bare, mut step_copper, mut step_bare_text, resolved_pos)) =
            resolve_static_step_board(
                &bare_text,
                &rules,
                &orig_pcb,
                &orig_copper,
                &orig_board,
                orig_bb,
                centre,
                f_s,
                &orig_fp_at,
                &cur_fp_pos,
                cand_pos,
                &orig_yards,
                &fp_envs,
                &fp_locked,
                &initial_yard_pen,
                &baseline_static_viols,
                &orig_locked_items,
            )?
        else {
            // Even moving footprints inward could not fit static pads/holes inside Edge.Cuts.
            let raw_shifts: Vec<(usize, Point)> = (0..orig_fp_at.len())
                .map(|f| {
                    (
                        f,
                        Point::new(
                            cur_fp_pos[f].x - orig_fp_at[f].x,
                            cur_fp_pos[f].y - orig_fp_at[f].y,
                        ),
                    )
                })
                .collect();
            let raw_field = build_scale_field(&orig_board, centre, f_s, &raw_shifts);
            let (fail_board, _, _) = build_scaled_bare_board(
                &bare_text,
                &rules,
                &orig_pcb,
                &orig_copper,
                orig_bb,
                centre,
                f_s,
                &raw_shifts,
                &raw_field,
                &[],
            )?;
            let jam_msg = if orig_had_outline && fail_board.outline.is_empty() {
                format!("board outline loop broke at factor {f_s:.4}")
            } else {
                collect_static_violations(&fail_board, &baseline_static_viols).1.unwrap_or_else(
                    || format!("static footprint or board-edge violation at factor {f_s:.4}"),
                )
            };
            let sz = bbox_size_mm(board_bounds(&fail_board));
            let rep = StepReport {
                step,
                factor: f_s,
                accepted: false,
                size_mm: sz,
                violations: 1,
                track_length_mm: total_track_length_mm(&cur_board),
                segments: count_tracks(&cur_board),
                vias: count_vias(&cur_board),
                acid_traps: dfm_counts(&cur_board),
                courtyard_stops: yard_stops,
                part_stops: 0,
                via_slides: 0,
                jam: Some(jam_msg.clone()),
            };
            if opts.verbose {
                eprintln!(
                    "compact step {step}/{steps_req} (factor {f_s:.4}): JAMMED on static check ({jam_msg})"
                );
            }
            step_reports.push(rep);
            final_jam = Some(jam_msg);
            break;
        };

        let saved_graph = graph.clone();
        graph.subdivide_long_edges(SUBDIVIDE_LEN);

        // Incremental harmonic warp from step `s - 1` frame (`cur_board`, `cur_factor`) to `f_s`.
        let inc_shifts: Vec<(usize, Point)> = (0..orig_fp_at.len())
            .map(|f| {
                (
                    f,
                    Point::new(
                        resolved_pos[f].x - cur_fp_pos[f].x,
                        resolved_pos[f].y - cur_fp_pos[f].y,
                    ),
                )
            })
            .collect();
        let rel_factor = f_s / cur_factor;
        let inc_field = build_incremental_field(&cur_board, centre, rel_factor, &inc_shifts);
        graph.advect(&cur_fp_pos, &resolved_pos, &inc_field);

        let mut trial_board = step_bare;
        strip_kept_zone_pours(&mut trial_board);
        let (final_fp_pos, mut via_slides) = relax_step(
            &mut graph,
            &mut trial_board,
            &orig_fp_at,
            &resolved_pos,
            &fp_locked,
            &fp_envs,
            &orig_yards,
            &initial_yard_pen,
            &baseline_static_viols,
        );

        // When joint relaxation shifted any unlocked footprint away from `resolved_pos`, rebuild
        // the bare board and text at `final_fp_pos` so footprint zones, mask openings and output
        // text match `final_fp_pos` byte for byte, then run one copper-only settle pass.
        if final_fp_pos != resolved_pos {
            let final_shifts: Vec<(usize, Point)> = (0..orig_fp_at.len())
                .map(|f| {
                    (
                        f,
                        Point::new(
                            final_fp_pos[f].x - orig_fp_at[f].x,
                            final_fp_pos[f].y - orig_fp_at[f].y,
                        ),
                    )
                })
                .collect();
            let final_field = build_scale_field(&orig_board, centre, f_s, &final_shifts);
            let warped_locked = warp_items(&orig_locked_items, &final_field);
            let (rebuilt_board, rebuilt_copper, rebuilt_text) = build_scaled_bare_board(
                &bare_text,
                &rules,
                &orig_pcb,
                &orig_copper,
                orig_bb,
                centre,
                f_s,
                &final_shifts,
                &final_field,
                &warped_locked,
            )?;
            trial_board = rebuilt_board;
            step_copper = rebuilt_copper;
            step_bare_text = rebuilt_text;
            strip_kept_zone_pours(&mut trial_board);
            via_slides += settle_copper_only(&mut graph, &mut trial_board);
        }
        graph.compact_storage();

        let _ = graph.sync_to_board(&mut trial_board);
        recarve_kept_zones_in_board(&mut trial_board, &step_copper);

        let (mut viol_count, mut worst_desc) = count_dynamic_violations(&trial_board);
        if viol_count > baseline_dyn_viols && viol_count <= baseline_dyn_viols + 160 {
            strip_kept_zone_pours(&mut trial_board);
            via_slides += slide_pinched_vias(&mut graph, &mut trial_board);
            polish_on_board(&mut graph, &mut trial_board);
            let _ = graph.sync_to_board(&mut trial_board);
            recarve_kept_zones_in_board(&mut trial_board, &step_copper);
            let (c2, d2) = count_dynamic_violations(&trial_board);
            viol_count = c2;
            worst_desc = d2;
        }

        if viol_count <= baseline_dyn_viols {
            strip_kept_zone_pours(&mut trial_board);
            shorten_taut_chains(&mut graph, &mut trial_board);
            graph.compact_storage();
            let _ = graph.sync_to_board(&mut trial_board);
            recarve_kept_zones_in_board(&mut trial_board, &step_copper);
            let (c3, d3) = count_dynamic_violations(&trial_board);
            viol_count = c3;
            worst_desc = d3;
        }

        let outline_broke = orig_had_outline && trial_board.outline.is_empty();
        let yard_err = courtyard_violation_desc(
            &orig_board,
            &orig_yards,
            &orig_fp_at,
            &final_fp_pos,
            &initial_yard_pen,
        );
        let (static_set, static_desc) =
            collect_static_violations(&trial_board, &baseline_static_viols);
        let static_cnt = static_set.len();
        let (extra_open, open_desc) = added_airlines(&trial_board, &baseline_airlines);
        let drifted_via = check_via_zones(&trial_board, &graph);

        let total_viols = viol_count
            + static_cnt
            + extra_open
            + usize::from(drifted_via.is_some())
            + usize::from(outline_broke)
            + usize::from(yard_err.is_some());
        let size_mm = bbox_size_mm(board_bounds(&trial_board));
        let track_len = total_track_length_mm(&trial_board);
        let segs_n = count_tracks(&trial_board);
        let vias_n = count_vias(&trial_board);
        let traps = dfm_counts(&trial_board);
        let part_stops = (0..orig_fp_at.len())
            .filter(|&f| !fp_locked[f] && final_fp_pos[f].distance(nominal_step_pos[f]) > 5_000.0)
            .count();

        if total_viols <= baseline_dyn_viols
            && extra_open == 0
            && static_cnt == 0
            && drifted_via.is_none()
            && !outline_broke
            && yard_err.is_none()
        {
            cur_board = trial_board;
            cur_bare_text = step_bare_text;
            cur_fp_pos = final_fp_pos;
            cur_factor = f_s;
            accepted_steps = step;
            let rep = StepReport {
                step,
                factor: f_s,
                accepted: true,
                size_mm,
                violations: viol_count,
                track_length_mm: track_len,
                segments: segs_n,
                vias: vias_n,
                acid_traps: traps,
                courtyard_stops: yard_stops,
                part_stops,
                via_slides,
                jam: None,
            };
            if opts.verbose {
                eprintln!(
                    "compact step {step}/{steps_req} (factor {f_s:.4}): {:.3} x {:.3} mm, {} violations, {:.2} mm ({} segs), {} vias, {} part stops, {} via slides, acid traps {} acute / {} slots",
                    rep.size_mm[0],
                    rep.size_mm[1],
                    rep.violations,
                    rep.track_length_mm,
                    rep.segments,
                    rep.vias,
                    rep.part_stops,
                    rep.via_slides,
                    rep.acid_traps.acute,
                    rep.acid_traps.slots
                );
            }
            step_reports.push(rep);
        } else {
            let reason = if outline_broke {
                format!("board outline loop broke at factor {f_s:.4}")
            } else if let Some(yd) = yard_err {
                yd
            } else if static_cnt > 0 {
                static_desc.unwrap_or_else(|| format!("{static_cnt} static violation(s)"))
            } else if let Some(vd) = drifted_via {
                vd
            } else if extra_open > 0 {
                open_desc.unwrap_or_else(|| {
                    format!("{extra_open} net connection(s) pulled apart at factor {f_s:.4}")
                })
            } else if let Some(d) = worst_desc {
                d
            } else {
                format!("step failed rule check at factor {f_s:.4}")
            };
            graph = saved_graph;
            let rep = StepReport {
                step,
                factor: f_s,
                accepted: false,
                size_mm,
                violations: total_viols,
                track_length_mm: track_len,
                segments: segs_n,
                vias: vias_n,
                acid_traps: traps,
                courtyard_stops: yard_stops,
                part_stops,
                via_slides,
                jam: Some(reason.clone()),
            };
            if opts.verbose {
                eprintln!(
                    "compact step {step}/{steps_req} (factor {f_s:.4}): JAMMED ({total_viols} violations: {reason})"
                );
            }
            step_reports.push(rep);
            final_jam = Some(reason);
            break;
        }
    }

    let out_text = graph.write_onto(&cur_bare_text, &cur_board, opts.uuid_seed);
    Ok(CompactReport {
        text: out_text,
        target_factor: opts.factor,
        achieved_factor: cur_factor,
        steps_requested: steps_req,
        steps_accepted: accepted_steps,
        steps: step_reports,
        jam: final_jam,
    })
}

fn build_incremental_field(
    cur_board: &Board,
    centre: (f64, f64),
    rel_factor: f64,
    inc_shifts: &[(usize, Point)],
) -> WarpField {
    build_scale_field(cur_board, centre, rel_factor, inc_shifts)
}

impl CopperGraph {
    fn from_board(board: &Board, copper: &Copper, fp_at: &[Point]) -> CopperGraph {
        let layer_idx = |name: &str| board.layers.iter().position(|l| l == name);
        let find_pad = |p: Point, net: u32, layers: u64| -> Option<(usize, Shape)> {
            for l in (0..board.layers.len()).filter(|&l| layers & (1u64 << l) != 0) {
                for id in board.near(l, BBox::of([p])) {
                    let Some(it) = board.item(id) else { continue };
                    if it.net == net && it.on(l) && it.shape.contains(p) {
                        if let Kind::Pad { footprint, .. } = it.kind {
                            return Some((footprint, it.shape.clone()));
                        }
                    }
                }
            }
            None
        };
        let touch_pad = |p: Point, r: Nm, net: u32, layer: usize| -> Option<usize> {
            let reach = (r - 500).max(0) as f64;
            let dot = Shape::Capsule { a: p, b: p, r: 0 };
            let qbb = BBox::of([p]).grow(r);
            for id in board.near(layer, qbb) {
                let Some(it) = board.item(id) else { continue };
                if it.net == net
                    && it.on(layer)
                    && (it.shape.contains(p) || it.shape.gap_within(&dot, reach) < reach)
                {
                    if let Kind::Pad { footprint, .. } = it.kind {
                        return Some(footprint);
                    }
                }
            }
            None
        };

        let mut particles: Vec<Particle> = Vec::new();
        let mut edges: Vec<Edge> = Vec::new();
        let mut via_map: HashMap<(u32, Point), usize> = HashMap::new();

        for v in &copper.vias {
            let (Some(a), Some(b)) = (layer_idx(&v.layers[0]), layer_idx(&v.layers[1])) else {
                continue;
            };
            let mask = (a.min(b)..=a.max(b)).fold(0u64, |m, l| m | (1u64 << l));
            let at = Point::from_m(v.at);
            let r = nm(v.size / 2.0);
            let drill_r = nm(v.drill / 2.0);
            if let Some(&existing) = via_map.get(&(v.net, at)) {
                match &mut particles[existing].anchor {
                    Anchor::Via { layers, r: vr, drill_r: dr, locked, .. }
                    | Anchor::PadVia { layers, r: vr, drill_r: dr, locked, .. } => {
                        *layers |= mask;
                        *vr = (*vr).max(r);
                        *dr = (*dr).max(drill_r);
                        *locked |= v.locked;
                    }
                    _ => {}
                }
                particles[existing].locked |= v.locked;
                continue;
            }
            let anchor = match find_pad(at, v.net, mask) {
                Some((fp, pad_shape)) => {
                    let (_, _, _, gap, _, _) =
                        closest_seg_shape((at.x as f64, at.y as f64), (at.x as f64, at.y as f64), r, &pad_shape);
                    let init_margin = (-gap - r as f64).clamp(1_000.0, 15_000.0);
                    Anchor::PadVia {
                        footprint: fp,
                        fp_ref: fp_at[fp],
                        orig_pad: pad_shape.clone(),
                        cur_pad: pad_shape,
                        init_margin,
                        r,
                        drill_r,
                        layers: mask,
                        net: v.net,
                        locked: v.locked,
                    }
                }
                None => Anchor::Via { r, drill_r, layers: mask, net: v.net, locked: v.locked },
            };
            let pid = particles.len();
            particles.push(Particle {
                pos: (at.x as f64, at.y as f64),
                anchor,
                locked: v.locked,
                via_tether: None,
                plane_zones: Vec::new(),
            });
            via_map.insert((v.net, at), pid);
        }

        let mut raw: Vec<(Point, Point, Nm, usize, u32, bool)> = Vec::new();
        for t in &copper.tracks {
            if let Some(l) = layer_idx(&t.layer) {
                let (a, b) = (Point::from_m(t.start), Point::from_m(t.end));
                if a != b {
                    raw.push((a, b, nm(t.width / 2.0), l, t.net, t.locked));
                }
            }
        }
        for arc in &copper.arcs {
            if let Some(l) = layer_idx(&arc.layer) {
                let (pts, _) = arc_chords(arc.start, arc.mid, arc.end, 0.0);
                let r = nm(arc.width / 2.0);
                for w in pts.windows(2) {
                    if w[0] != w[1] {
                        raw.push((w[0], w[1], r, l, arc.net, arc.locked));
                    }
                }
            }
        }

        // Candidate split points per (net, layer): all track endpoints (with max half-width) and via centres.
        let mut end_r: HashMap<(u32, usize, Point), Nm> = HashMap::new();
        let mut split_pts: HashMap<(u32, usize), Vec<Point>> = HashMap::new();
        for &(a, b, r, l, net, _) in &raw {
            split_pts.entry((net, l)).or_default().extend([a, b]);
            end_r.entry((net, l, a)).and_modify(|er| *er = (*er).max(r)).or_insert(r);
            end_r.entry((net, l, b)).and_modify(|er| *er = (*er).max(r)).or_insert(r);
        }
        for (&(net, at), &pid) in &via_map {
            if let Some((vr, _, mask, _, _)) = particles[pid].is_via() {
                for l in (0..board.layers.len()).filter(|&l| mask & (1u64 << l) != 0) {
                    split_pts.entry((net, l)).or_default().push(at);
                    end_r.entry((net, l, at)).and_modify(|er| *er = (*er).max(vr)).or_insert(vr);
                }
            }
        }
        for pts in split_pts.values_mut() {
            pts.sort_by_key(|p| (p.x, p.y));
            pts.dedup();
        }

        let mut node_map: HashMap<(u32, usize, Point), usize> = HashMap::new();
        let mut get_node =
            |particles: &mut Vec<Particle>, net: u32, layer: usize, p: Point| -> usize {
                if let Some(&vpid) = via_map.get(&(net, p)) {
                    if let Some((_, _, mask, _, _)) = particles[vpid].is_via() {
                        if mask & (1u64 << layer) != 0 {
                            return vpid;
                        }
                    }
                }
                if let Some(&id) = node_map.get(&(net, layer, p)) {
                    return id;
                }
                let anchor = match find_pad(p, net, 1u64 << layer) {
                    Some((fp, _)) => Anchor::Pad {
                        footprint: fp,
                        offset: ((p.x - fp_at[fp].x) as f64, (p.y - fp_at[fp].y) as f64),
                    },
                    None => Anchor::Free,
                };
                let id = particles.len();
                particles.push(Particle {
                    pos: (p.x as f64, p.y as f64),
                    anchor,
                    locked: false,
                    via_tether: None,
                    plane_zones: Vec::new(),
                });
                node_map.insert((net, layer, p), id);
                id
            };

        // Record off-centre T-junction pairs `(endpoint_pt, split_q_on_seg, max_dist, layer, net)`
        // so they stay tethered without emitting fake hook segments that would form acute angles.
        let mut t_tethers: Vec<(Point, Point, f64, usize, u32)> = Vec::new();

        for &(a, b, r, l, net, locked) in &raw {
            let (dx, dy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
            let len2 = dx * dx + dy * dy;
            let mut stops: Vec<(f64, Point)> = vec![(0.0, a), (1.0, b)];

            if let Some(cands) = split_pts.get(&(net, l)) {
                let reach = r + 350_000;
                let bb = BBox::of([a, b]).grow(reach);
                let i0 = cands.partition_point(|p| p.x < bb.min.x);
                let i1 = cands.partition_point(|p| p.x <= bb.max.x);
                for &p in &cands[i0..i1] {
                    if p == a || p == b || p.y < bb.min.y || p.y > bb.max.y {
                        continue;
                    }
                    let t = ((p.x - a.x) as f64 * dx + (p.y - a.y) as f64 * dy) / len2;
                    if t <= 1e-6 || t >= 1.0 - 1e-6 {
                        continue;
                    }
                    let q = Point::new(
                        (a.x as f64 + dx * t).round() as Nm,
                        (a.y as f64 + dy * t).round() as Nm,
                    );
                    let d = q.distance(p);
                    if d <= 2_000.0 {
                        stops.push((t, p));
                    } else {
                        let extra_r = end_r.get(&(net, l, p)).copied().unwrap_or(0);
                        let touch_lim = (r + extra_r) as f64;
                        if d + 500.0 < touch_lim {
                            stops.push((t, q));
                            let is_via_cand = via_map
                                .get(&(net, p))
                                .and_then(|&vid| particles[vid].is_via())
                                .is_some_and(|(_, _, mask, _, _)| mask & (1u64 << l) != 0);
                            if !is_via_cand {
                                let max_d = (d + 3_000.0).min((touch_lim - 1_500.0).max(d));
                                t_tethers.push((p, q, max_d, l, net));
                            }
                        }
                    }
                }
            }

            stops.sort_by(|x, y| x.0.total_cmp(&y.0));
            let mut pts: Vec<Point> = Vec::with_capacity(stops.len());
            for &(_, pt) in &stops {
                if pts.last().copied() != Some(pt) {
                    pts.push(pt);
                }
            }
            for w in pts.windows(2) {
                let u = get_node(&mut particles, net, l, w[0]);
                let v = get_node(&mut particles, net, l, w[1]);
                if u != v {
                    if locked {
                        particles[u].locked = true;
                        particles[v].locked = true;
                    }
                    edges.push(Edge { u, v, layer: l, net, r, locked, active: true });
                }
            }
        }

        // Tether off-centre particles inside same-net vias, apply T-junction tethers, and pin
        // grazing pad contacts without adding any artificial segments.
        let mut deg = vec![0usize; particles.len()];
        let mut max_r = vec![0i64; particles.len()];
        let mut sample_layer = vec![0usize; particles.len()];
        let mut sample_net = vec![0u32; particles.len()];
        for e in &edges {
            for &p in &[e.u, e.v] {
                deg[p] += 1;
                max_r[p] = max_r[p].max(e.r);
                sample_layer[p] = e.layer;
                sample_net[p] = e.net;
            }
        }

        for pid in 0..particles.len() {
            if !matches!(particles[pid].anchor, Anchor::Free) || deg[pid] == 0 {
                continue;
            }
            let pt = Point::new(
                particles[pid].pos.0.round() as Nm,
                particles[pid].pos.1.round() as Nm,
            );
            let net = sample_net[pid];
            let l = sample_layer[pid];
            let r = max_r[pid];
            // Check if this particle sits inside a same-net via's copper disc off-centre.
            let mut best_via: Option<(usize, f64, Nm)> = None;
            for (&(vnet, vat), &vpid) in &via_map {
                if vnet != net || vpid == pid {
                    continue;
                }
                if let Some((vr, _, mask, _, _)) = particles[vpid].is_via() {
                    let d = pt.distance(vat);
                    if mask & (1u64 << l) != 0
                        && d + 500.0 < (vr + r) as f64
                        && best_via.is_none_or(|(_, bd, _)| d < bd)
                    {
                        best_via = Some((vpid, d, vr));
                    }
                }
            }
            if let Some((vpid, d, vr)) = best_via {
                let lim = (vr + r) as f64;
                let max_d = (vr as f64 - 5_000.0).max(d + 5_000.0).min((lim - 5_000.0).max(d));
                particles[pid].via_tether = Some((vpid, max_d));
                continue;
            }
            // If a degree-1 endpoint overlaps a same-net pad without its centre inside, pin it.
            if deg[pid] == 1 {
                if let Some(fp) = touch_pad(pt, r, net, l) {
                    particles[pid].anchor = Anchor::Pad {
                        footprint: fp,
                        offset: ((pt.x - fp_at[fp].x) as f64, (pt.y - fp_at[fp].y) as f64),
                    };
                }
            }
        }

        for (ep, q, max_d, l, net) in t_tethers {
            if let (Some(&epid), Some(&qpid)) =
                (node_map.get(&(net, l, ep)), node_map.get(&(net, l, q)))
            {
                if epid != qpid
                    && matches!(particles[epid].anchor, Anchor::Free)
                    && particles[epid].via_tether.is_none()
                {
                    particles[epid].via_tether = Some((qpid, max_d));
                }
            }
        }

        // Also tether round-cap-to-round-cap endpoint joints (where an endpoint `p` overlaps
        // another same-net, same-layer vertex `q` within `r_p + r_q - 500 nm` outside `0 < t < 1`).
        let direct_edges: HashSet<(usize, usize)> =
            edges.iter().map(|e| (e.u.min(e.v), e.u.max(e.v))).collect();
        let mut by_nl: HashMap<(u32, usize), Vec<usize>> = HashMap::new();
        for pid in 0..particles.len() {
            if deg[pid] > 0 && particles[pid].is_via().is_none() {
                by_nl.entry((sample_net[pid], sample_layer[pid])).or_default().push(pid);
            }
        }
        for ((net, _), list) in by_nl {
            if net == 0 || list.len() < 2 {
                continue;
            }
            for &pid in &list {
                if !matches!(particles[pid].anchor, Anchor::Free)
                    || particles[pid].via_tether.is_some()
                {
                    continue;
                }
                let p_pt = Point::new(
                    particles[pid].pos.0.round() as Nm,
                    particles[pid].pos.1.round() as Nm,
                );
                let rp = max_r[pid];
                let mut best_q: Option<(usize, f64, Nm)> = None;
                for &qpid in &list {
                    if qpid == pid || direct_edges.contains(&(pid.min(qpid), pid.max(qpid))) {
                        continue;
                    }
                    // Strict priority order `(is_pad, deg, rev_id)` guarantees an acyclic tether DAG.
                    let q_is_pad = matches!(particles[qpid].anchor, Anchor::Pad { .. });
                    let q_higher = q_is_pad
                        || deg[qpid] > deg[pid]
                        || (deg[qpid] == deg[pid] && qpid < pid);
                    if !q_higher {
                        continue;
                    }
                    let q_pt = Point::new(
                        particles[qpid].pos.0.round() as Nm,
                        particles[qpid].pos.1.round() as Nm,
                    );
                    let rq = max_r[qpid];
                    let d = p_pt.distance(q_pt);
                    if d + 500.0 < (rp + rq) as f64 && best_q.is_none_or(|(_, bd, _)| d < bd) {
                        best_q = Some((qpid, d, rq));
                    }
                }
                if let Some((qpid, d, rq)) = best_q {
                    let lim = (rp + rq) as f64;
                    let max_d = (d + 3_000.0).min((lim - 1_500.0).max(d));
                    particles[pid].via_tether = Some((qpid, max_d));
                }
            }
        }

        CopperGraph { particles, edges }
    }

    fn locked_items(&self) -> Vec<Item> {
        let mut out = Vec::new();
        for p in &self.particles {
            if let Some((r, dr, layers, net, true)) = p.is_via() {
                let at = Point::new(p.pos.0.round() as Nm, p.pos.1.round() as Nm);
                out.push(Item {
                    kind: Kind::Via,
                    net,
                    layers,
                    shape: Shape::Capsule { a: at, b: at, r },
                    hole: Some((at, dr)),
                    locked: true,
                    span: None,
                });
            }
        }
        for e in &self.edges {
            if e.active && e.locked {
                let a = Point::new(
                    self.particles[e.u].pos.0.round() as Nm,
                    self.particles[e.u].pos.1.round() as Nm,
                );
                let b = Point::new(
                    self.particles[e.v].pos.0.round() as Nm,
                    self.particles[e.v].pos.1.round() as Nm,
                );
                if a != b {
                    out.push(Item {
                        kind: Kind::Track,
                        net: e.net,
                        layers: 1u64 << e.layer,
                        shape: Shape::Capsule { a, b, r: e.r },
                        hole: None,
                        locked: true,
                        span: None,
                    });
                }
            }
        }
        out
    }

    fn subdivide_long_edges(&mut self, max_len: f64) {
        let n_orig = self.edges.len();
        for idx in 0..n_orig {
            if !self.edges[idx].active || self.edges[idx].locked {
                continue;
            }
            let (u, v, layer, net, r, locked) = {
                let e = &self.edges[idx];
                (e.u, e.v, e.layer, e.net, e.r, e.locked)
            };
            let p0 = self.particles[u].pos;
            let p1 = self.particles[v].pos;
            let len = (p1.0 - p0.0).hypot(p1.1 - p0.1);
            let pieces = (len / max_len).ceil() as usize;
            if pieces <= 1 {
                continue;
            }
            self.edges[idx].active = false;
            let mut prev = u;
            for k in 1..pieces {
                let t = k as f64 / pieces as f64;
                let mid = (p0.0 + (p1.0 - p0.0) * t, p0.1 + (p1.1 - p0.1) * t);
                let w = self.particles.len();
                self.particles.push(Particle {
                    pos: mid,
                    anchor: Anchor::Free,
                    locked: false,
                    via_tether: None,
                    plane_zones: Vec::new(),
                });
                self.edges.push(Edge { u: prev, v: w, layer, net, r, locked, active: true });
                prev = w;
            }
            self.edges.push(Edge { u: prev, v, layer, net, r, locked, active: true });
        }
    }

    fn advect(&mut self, prev_fp_pos: &[Point], fp_pos: &[Point], field: &WarpField) {
        let old_pos: Vec<(f64, f64)> = self.particles.iter().map(|p| p.pos).collect();
        for p in &mut self.particles {
            if p.via_tether.is_some() && matches!(p.anchor, Anchor::Free) {
                continue;
            }
            match &mut p.anchor {
                Anchor::Pad { footprint, offset } => {
                    p.pos = (fp_pos[*footprint].x as f64 + offset.0, fp_pos[*footprint].y as f64 + offset.1);
                }
                Anchor::PadVia { footprint, fp_ref, orig_pad, cur_pad, .. } => {
                    let dfp = (
                        fp_pos[*footprint].x - prev_fp_pos[*footprint].x,
                        fp_pos[*footprint].y - prev_fp_pos[*footprint].y,
                    );
                    p.pos.0 += dfp.0 as f64;
                    p.pos.1 += dfp.1 as f64;
                    let tot = Point::new(
                        fp_pos[*footprint].x - fp_ref.x,
                        fp_pos[*footprint].y - fp_ref.y,
                    );
                    *cur_pad = shift_shape(orig_pad, tot);
                }
                Anchor::Via { .. } | Anchor::Free => {
                    let pt = Point::new(p.pos.0.round() as Nm, p.pos.1.round() as Nm);
                    let (dx, dy) = field.displacement(pt);
                    p.pos.0 += dx;
                    p.pos.1 += dy;
                }
            }
        }
        // Two passes propagate displacement cleanly even if a T-junction tethers onto a via-tethered split.
        for _ in 0..2 {
            for i in 0..self.particles.len() {
                if !matches!(self.particles[i].anchor, Anchor::Free) {
                    continue;
                }
                if let Some((tpid, _)) = self.particles[i].via_tether {
                    let dv = (
                        self.particles[tpid].pos.0 - old_pos[tpid].0,
                        self.particles[tpid].pos.1 - old_pos[tpid].1,
                    );
                    self.particles[i].pos = (old_pos[i].0 + dv.0, old_pos[i].1 + dv.1);
                }
            }
        }
    }

    fn shift_footprint_anchors(
        &mut self,
        fp: usize,
        new_fp_pos: Point,
        delta: Point,
        tethered_by: &[Vec<(usize, f64)>],
    ) {
        if delta == Point::new(0, 0) {
            return;
        }
        let mut shifted: Vec<usize> = Vec::new();
        for (i, p) in self.particles.iter_mut().enumerate() {
            match &mut p.anchor {
                Anchor::Pad { footprint, offset } if *footprint == fp => {
                    p.pos = (
                        new_fp_pos.x as f64 + offset.0,
                        new_fp_pos.y as f64 + offset.1,
                    );
                    shifted.push(i);
                }
                Anchor::PadVia { footprint, fp_ref, orig_pad, cur_pad, .. }
                    if *footprint == fp =>
                {
                    p.pos.0 += delta.x as f64;
                    p.pos.1 += delta.y as f64;
                    let tot = Point::new(new_fp_pos.x - fp_ref.x, new_fp_pos.y - fp_ref.y);
                    *cur_pad = shift_shape(orig_pad, tot);
                    shifted.push(i);
                }
                _ => {}
            }
        }
        let mut cursor = 0usize;
        while cursor < shifted.len() {
            let cur = shifted[cursor];
            cursor += 1;
            if let Some(children) = tethered_by.get(cur) {
                for &(ch, _) in children {
                    if !self.particles[ch].locked
                        && matches!(self.particles[ch].anchor, Anchor::Free)
                        && !shifted.contains(&ch)
                    {
                        self.particles[ch].pos.0 += delta.x as f64;
                        self.particles[ch].pos.1 += delta.y as f64;
                        shifted.push(ch);
                    }
                }
            }
        }
    }

    fn compact_storage(&mut self) {
        self.edges.retain(|e| e.active && e.u != e.v);
        let mut used = vec![false; self.particles.len()];
        for (i, p) in self.particles.iter().enumerate() {
            if p.is_via().is_some() {
                used[i] = true;
            }
        }
        for e in &self.edges {
            used[e.u] = true;
            used[e.v] = true;
        }
        for i in 0..self.particles.len() {
            if used[i] {
                if let Some((tpid, _)) = self.particles[i].via_tether {
                    used[tpid] = true;
                }
            }
        }
        let mut remap = vec![usize::MAX; self.particles.len()];
        let mut new_particles = Vec::with_capacity(self.particles.len());
        for (i, p) in self.particles.drain(..).enumerate() {
            if used[i] {
                remap[i] = new_particles.len();
                new_particles.push(p);
            }
        }
        self.particles = new_particles;
        for p in &mut self.particles {
            if let Some((tpid, _)) = &mut p.via_tether {
                *tpid = remap[*tpid];
            }
        }
        for e in &mut self.edges {
            e.u = remap[e.u];
            e.v = remap[e.v];
        }
        // Deduplicate any parallel edges on the same `(u, v, layer, net)` keeping the widest `r`.
        let mut best_edge: HashMap<(usize, usize, usize, u32), usize> = HashMap::new();
        for i in 0..self.edges.len() {
            let e = self.edges[i].clone();
            let key = (e.u.min(e.v), e.u.max(e.v), e.layer, e.net);
            if let Some(&prev) = best_edge.get(&key) {
                if e.r > self.edges[prev].r {
                    self.edges[prev].r = e.r;
                }
                if e.locked {
                    self.edges[prev].locked = true;
                }
                self.edges[i].active = false;
            } else {
                best_edge.insert(key, i);
            }
        }
        self.edges.retain(|e| e.active);
    }

    /// Removes any previously inserted dynamic tracks/vias and inserts the current graph state into
    /// `board`, returning `(edge_item_ids, via_item_ids)`.
    fn sync_to_board(&self, board: &mut Board) -> (Vec<Option<ItemId>>, Vec<Option<ItemId>>) {
        let stale: Vec<ItemId> = board
            .items()
            .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via))
            .map(|(id, _)| id)
            .collect();
        for id in stale {
            board.remove(id);
        }
        let mut via_ids = vec![None; self.particles.len()];
        for (i, p) in self.particles.iter().enumerate() {
            if let Some((r, dr, layers, net, locked)) = p.is_via() {
                let at = Point::new(p.pos.0.round() as Nm, p.pos.1.round() as Nm);
                let id = board.insert(Item {
                    kind: Kind::Via,
                    net,
                    layers,
                    shape: Shape::Capsule { a: at, b: at, r },
                    hole: Some((at, dr)),
                    locked,
                    span: None,
                });
                via_ids[i] = Some(id);
            }
        }
        let mut edge_ids = vec![None; self.edges.len()];
        for (i, e) in self.edges.iter().enumerate() {
            if !e.active {
                continue;
            }
            let a = Point::new(
                self.particles[e.u].pos.0.round() as Nm,
                self.particles[e.u].pos.1.round() as Nm,
            );
            let b = Point::new(
                self.particles[e.v].pos.0.round() as Nm,
                self.particles[e.v].pos.1.round() as Nm,
            );
            if a == b {
                continue;
            }
            let id = board.insert(Item {
                kind: Kind::Track,
                net: e.net,
                layers: 1u64 << e.layer,
                shape: Shape::Capsule { a, b, r: e.r },
                hole: None,
                locked: e.locked,
                span: None,
            });
            edge_ids[i] = Some(id);
        }
        (edge_ids, via_ids)
    }

    fn write_onto(&self, bare_text: &str, board: &Board, uuid_seed: Option<u64>) -> String {
        let mut tracks = Vec::new();
        for e in &self.edges {
            if !e.active {
                continue;
            }
            let a = Point::new(
                self.particles[e.u].pos.0.round() as Nm,
                self.particles[e.u].pos.1.round() as Nm,
            );
            let b = Point::new(
                self.particles[e.v].pos.0.round() as Nm,
                self.particles[e.v].pos.1.round() as Nm,
            );
            if a == b {
                continue;
            }
            tracks.push(NewTrack {
                start: [a.x as f64 * 1e-9, a.y as f64 * 1e-9],
                end: [b.x as f64 * 1e-9, b.y as f64 * 1e-9],
                width: (e.r * 2) as f64 * 1e-9,
                layer: board.layers[e.layer].clone(),
                net: board.file_net(e.net),
                locked: e.locked,
            });
        }
        let mut vias = Vec::new();
        for p in &self.particles {
            if let Some((r, dr, layers, net, locked)) = p.is_via() {
                if layers == 0 {
                    continue;
                }
                let first = layers.trailing_zeros() as usize;
                let last = 63 - layers.leading_zeros() as usize;
                let at = Point::new(p.pos.0.round() as Nm, p.pos.1.round() as Nm);
                vias.push(NewVia {
                    at: [at.x as f64 * 1e-9, at.y as f64 * 1e-9],
                    size: (r * 2) as f64 * 1e-9,
                    drill: (dr * 2) as f64 * 1e-9,
                    layers: [board.layers[first].clone(), board.layers[last].clone()],
                    net: board.file_net(net),
                    locked,
                });
            }
        }
        apply(bare_text, &Edit { tracks, vias, uuid_seed, ..Edit::default() })
    }
}

fn shift_shape(shape: &Shape, d: Point) -> Shape {
    match shape {
        Shape::Capsule { a, b, r } => Shape::Capsule {
            a: Point::new(a.x + d.x, a.y + d.y),
            b: Point::new(b.x + d.x, b.y + d.y),
            r: *r,
        },
        Shape::Polygon(poly) => {
            Shape::Polygon(poly.iter().map(|p| Point::new(p.x + d.x, p.y + d.y)).collect())
        }
    }
}

/// Maximum interior penetration depth (in `nm`) between two courtyard polygons, returning `0.0`
/// for disjoint or strictly abutting courtyards (`depth == 0`) so exact edge contact is permitted
/// while any new interior penetration (or any deepening of a pre-existing grazingly overlapping
/// pair) is detected symmetrically.
fn courtyard_penetration_nm(a: &Shape, b: &Shape) -> f64 {
    if a.gap_within(b, 1.0) > 0.0 {
        return 0.0;
    }
    let (Shape::Polygon(pa), Shape::Polygon(pb)) = (a, b) else {
        return 1_000_000.0;
    };
    let mut max_d = 0.0f64;
    for &p in pa {
        if b.contains(p) {
            max_d = max_d.max(b.depth(p));
        }
    }
    for &q in pb {
        if a.contains(q) {
            max_d = max_d.max(a.depth(q));
        }
    }
    // Check edge subsegments partitioned by crossings in both directions so corner pokes without
    // interior vertices are caught symmetrically.
    let cross = |u: (f64, f64), v: (f64, f64)| u.0 * v.1 - u.1 * v.0;
    let scan_edges = |src: &[Point], dst_pts: &[Point], dst_shape: &Shape| -> f64 {
        let mut best = 0.0f64;
        for i in 0..src.len() {
            let a0 = src[i];
            let a1 = src[(i + 1) % src.len()];
            let ua = ((a1.x - a0.x) as f64, (a1.y - a0.y) as f64);
            let mut ts = vec![0.0f64, 1.0f64];
            for j in 0..dst_pts.len() {
                let b0 = dst_pts[j];
                let b1 = dst_pts[(j + 1) % dst_pts.len()];
                let ub = ((b1.x - b0.x) as f64, (b1.y - b0.y) as f64);
                let den = cross(ua, ub);
                if den.abs() > 1e-9 {
                    let w = ((b0.x - a0.x) as f64, (b0.y - a0.y) as f64);
                    let s = cross(w, ub) / den;
                    let t = cross(w, ua) / den;
                    if (0.0..=1.0).contains(&s) && (0.0..=1.0).contains(&t) {
                        ts.push(s);
                    }
                }
            }
            ts.sort_by(f64::total_cmp);
            for w in ts.windows(2) {
                if w[1] - w[0] > 1e-6 {
                    let m = (w[0] + w[1]) * 0.5;
                    let mp = Point::new(
                        (a0.x as f64 + ua.0 * m).round() as Nm,
                        (a0.y as f64 + ua.1 * m).round() as Nm,
                    );
                    if dst_shape.contains(mp) {
                        best = best.max(dst_shape.depth(mp));
                    }
                }
            }
        }
        best
    };
    max_d = max_d.max(scan_edges(pa, pb, b));
    max_d.max(scan_edges(pb, pa, a))
}

/// Outward contact normal pointing from courtyard `pb` toward courtyard `pa`.
fn courtyard_contact_normal(pa: &[Point], pb: &[Point]) -> (f64, f64) {
    let mut best_d = f64::INFINITY;
    let mut best_pair = ((0.0, 0.0), (0.0, 0.0));
    for i in 0..pa.len() {
        let a0 = (pa[i].x as f64, pa[i].y as f64);
        let a1 = (pa[(i + 1) % pa.len()].x as f64, pa[(i + 1) % pa.len()].y as f64);
        for j in 0..pb.len() {
            let b0 = (pb[j].x as f64, pb[j].y as f64);
            let b1 = (pb[(j + 1) % pb.len()].x as f64, pb[(j + 1) % pb.len()].y as f64);
            let (_, _, qa, qb, d) = closest_seg_seg(a0, a1, b0, b1);
            if d < best_d {
                best_d = d;
                best_pair = (qa, qb);
            }
        }
    }
    if best_d > 10.0 {
        let (qa, qb) = best_pair;
        return ((qa.0 - qb.0) / best_d, (qa.1 - qb.1) / best_d);
    }
    // Abutting or slightly touching edges: look for opposing edge normals or fall back to centroids.
    let ca = (
        pa.iter().map(|p| p.x as f64).sum::<f64>() / pa.len().max(1) as f64,
        pa.iter().map(|p| p.y as f64).sum::<f64>() / pa.len().max(1) as f64,
    );
    let cb = (
        pb.iter().map(|p| p.x as f64).sum::<f64>() / pb.len().max(1) as f64,
        pb.iter().map(|p| p.y as f64).sum::<f64>() / pb.len().max(1) as f64,
    );
    // Check if an edge of `pa` lies on `best_pair.0` and use its outward normal toward `ca - cb`.
    let (dcx, dcy) = (ca.0 - cb.0, ca.1 - cb.1);
    for i in 0..pa.len() {
        let a0 = pa[i];
        let a1 = pa[(i + 1) % pa.len()];
        let q = Point::new(best_pair.0.0.round() as Nm, best_pair.0.1.round() as Nm);
        if point_segment(q, a0, a1) <= 1_000.0 && a0.distance(a1) > 1_000.0 {
            let ( mut nx, mut ny) = seg_normal((a0.x as f64, a0.y as f64), (a1.x as f64, a1.y as f64));
            if nx * dcx + ny * dcy < 0.0 {
                nx = -nx;
                ny = -ny;
            }
            if (nx * dcx + ny * dcy).abs() > 1.0 {
                return (nx, ny);
            }
        }
    }
    let dl = dcx.hypot(dcy);
    if dl > 1.0 { (dcx / dl, dcy / dl) } else { (1.0, 0.0) }
}

fn yard_side_polys(yards: &FootprintYards, side: u8) -> &[Vec<Point>] {
    if side == 0 {
        &yards.front
    } else {
        &yards.back
    }
}

fn fp_family_groups(
    n: usize,
    envs: &[FootprintEnvelope],
    fp_locked: &[bool],
) -> (Vec<usize>, Vec<Vec<usize>>) {
    let mut leader: Vec<usize> = (0..n).collect();
    for f in 0..n {
        if !fp_locked[f] {
            if let Some(p) = envs[f].parent {
                if p < n && !fp_locked[p] {
                    leader[f] = p;
                }
            }
        }
    }
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (f, &ld) in leader.iter().enumerate() {
        members[ld].push(f);
    }
    (leader, members)
}

/// Courtyard pairs `(f, g, side)` (`f < g`, `side` = 0 front / 1 back) mapped to their maximum
/// interior penetration depth in `nm` whenever that depth is positive at `fp_pos`.
fn courtyard_penetrations(
    orig_yards: &[FootprintYards],
    orig_at: &[Point],
    fp_pos: &[Point],
) -> HashMap<(usize, usize, u8), f64> {
    let n = orig_yards.len();
    let build_side = |side: u8| -> Vec<Vec<(Shape, BBox)>> {
        (0..n)
            .map(|f| {
                let d = Point::new(fp_pos[f].x - orig_at[f].x, fp_pos[f].y - orig_at[f].y);
                yard_side_polys(&orig_yards[f], side)
                    .iter()
                    .filter(|p| p.len() >= 3)
                    .map(|p| {
                        let s = Shape::Polygon(
                            p.iter().map(|pt| Point::new(pt.x + d.x, pt.y + d.y)).collect(),
                        );
                        let bb = s.bbox();
                        (s, bb)
                    })
                    .collect()
            })
            .collect()
    };
    let mut out = HashMap::new();
    for side in [0u8, 1u8] {
        let polys = build_side(side);
        for f in 0..n {
            if polys[f].is_empty() {
                continue;
            }
            for g in (f + 1)..n {
                if polys[g].is_empty() {
                    continue;
                }
                let mut max_pen = 0.0f64;
                for (sf, bbf) in &polys[f] {
                    for (sg, bbg) in &polys[g] {
                        if bbf.overlaps(*bbg) {
                            max_pen = max_pen.max(courtyard_penetration_nm(sf, sg));
                        }
                    }
                }
                if max_pen > 0.0 {
                    out.insert((f, g, side), max_pen);
                }
            }
        }
    }
    out
}

/// KiCad's `GetCourtyard` deflates each footprint's courtyard inward by `5_000 nm`, so a stroke
/// centreline overlap under `10_000 nm` is still non-overlapping in KiCad DRC and must not deepen,
/// whereas pairs already overlapping by `>= 10_000 nm` on the input board are exempt.
const KICAD_COURTYARD_TOL_NM: f64 = 10_000.0;

fn pair_side_penetration(
    orig_yards: &[FootprintYards],
    orig_at: &[Point],
    f: usize,
    pos_f: Point,
    g: usize,
    pos_g: Point,
    side: u8,
) -> (f64, usize, usize) {
    let df = Point::new(pos_f.x - orig_at[f].x, pos_f.y - orig_at[f].y);
    let dg = Point::new(pos_g.x - orig_at[g].x, pos_g.y - orig_at[g].y);
    let mut best = (0.0f64, 0usize, 0usize);
    for (ia, pa) in yard_side_polys(&orig_yards[f], side).iter().enumerate() {
        if pa.len() < 3 {
            continue;
        }
        let sa = Shape::Polygon(pa.iter().map(|p| Point::new(p.x + df.x, p.y + df.y)).collect());
        let bba = sa.bbox();
        for (ib, pb) in yard_side_polys(&orig_yards[g], side).iter().enumerate() {
            if pb.len() < 3 {
                continue;
            }
            let sb =
                Shape::Polygon(pb.iter().map(|p| Point::new(p.x + dg.x, p.y + dg.y)).collect());
            if bba.overlaps(sb.bbox()) {
                let pen = courtyard_penetration_nm(&sa, &sb);
                if pen > best.0 {
                    best = (pen, ia, ib);
                }
            }
        }
    }
    best
}

/// Stops moving footprints short of creating any new courtyard overlap (or deepening any initial
/// sub-tolerance courtyard graze) on the same side while preserving common-mode translation toward
/// the board centre, tangential sliding, `--lock` invariance, and parent-child rigid pockets.
fn resolve_courtyard_stops(
    orig_yards: &[FootprintYards],
    envs: &[FootprintEnvelope],
    fp_locked: &[bool],
    orig_at: &[Point],
    prev_pos: &[Point],
    cand_pos: &mut [Point],
    allowed: &HashMap<(usize, usize, u8), f64>,
) -> usize {
    let (leader, members) = fp_family_groups(orig_at.len(), envs, fp_locked);
    let mut stopped: HashSet<usize> = HashSet::new();
    for _pass in 0..16 {
        let pens = courtyard_penetrations(orig_yards, orig_at, cand_pos);
        let mut pairs: Vec<(usize, usize, u8)> = pens
            .into_iter()
            .filter_map(|(key, pen)| {
                if leader[key.0] == leader[key.1] {
                    return None;
                }
                let d0 = allowed.get(&key).copied().unwrap_or(0.0);
                if d0 >= KICAD_COURTYARD_TOL_NM {
                    return None;
                }
                (pen > d0 + 1e-6).then_some(key)
            })
            .collect();
        if pairs.is_empty() {
            break;
        }
        pairs.sort_unstable();
        for (f, g, side) in pairs {
            let (lf, lg) = (leader[f], leader[g]);
            if fp_locked[lf] && fp_locked[lg] {
                continue;
            }
            let cap = allowed.get(&(f, g, side)).copied().unwrap_or(0.0);
            let (cur_pen, ia, ib) = pair_side_penetration(
                orig_yards,
                orig_at,
                f,
                cand_pos[f],
                g,
                cand_pos[g],
                side,
            );
            if cur_pen <= cap + 1e-6 {
                continue;
            }

            let (plf, plg) = (prev_pos[lf], prev_pos[lg]);
            let (clf, clg) = (cand_pos[lf], cand_pos[lg]);
            let dlf = ((clf.x - plf.x) as f64, (clf.y - plf.y) as f64);
            let dlg = ((clg.x - plg.x) as f64, (clg.y - plg.y) as f64);

            // Locked footprints never move by even 1 nm: common-mode translation is zero whenever
            // either side is locked.
            let cm = if fp_locked[lf] || fp_locked[lg] {
                Point::new(0, 0)
            } else {
                Point::new(
                    ((dlf.0 + dlg.0) * 0.5).round() as Nm,
                    ((dlf.1 + dlg.1) * 0.5).round() as Nm,
                )
            };
            let rel_f = if fp_locked[lf] {
                (0.0, 0.0)
            } else {
                (dlf.0 - cm.x as f64, dlf.1 - cm.y as f64)
            };
            let rel_g = if fp_locked[lg] {
                (0.0, 0.0)
            } else {
                (dlg.0 - cm.x as f64, dlg.1 - cm.y as f64)
            };

            let yf = &yard_side_polys(&orig_yards[f], side)[ia];
            let yg = &yard_side_polys(&orig_yards[g], side)[ib];
            let (pf, pg) = (prev_pos[f], prev_pos[g]);
            let prev_yf: Vec<Point> = yf
                .iter()
                .map(|p| Point::new(p.x + pf.x - orig_at[f].x, p.y + pf.y - orig_at[f].y))
                .collect();
            let prev_yg: Vec<Point> = yg
                .iter()
                .map(|p| Point::new(p.x + pg.x - orig_at[g].x, p.y + pg.y - orig_at[g].y))
                .collect();
            let (nx, ny) = courtyard_contact_normal(&prev_yf, &prev_yg);

            let proj_f = rel_f.0 * nx + rel_f.1 * ny;
            let proj_g = rel_g.0 * nx + rel_g.1 * ny;
            let rn_f = (proj_f * nx, proj_f * ny);
            let rn_g = (proj_g * nx, proj_g * ny);
            let rt_f = (rel_f.0 - rn_f.0, rel_f.1 - rn_f.1);
            let rt_g = (rel_g.0 - rn_g.0, rel_g.1 - rn_g.1);

            let eval_deltas = |alpha_n: f64, beta_t: f64| -> (Point, Point) {
                let d_lf = if fp_locked[lf] {
                    Point::new(0, 0)
                } else {
                    Point::new(
                        cm.x + (rt_f.0 * beta_t + rn_f.0 * alpha_n).round() as Nm,
                        cm.y + (rt_f.1 * beta_t + rn_f.1 * alpha_n).round() as Nm,
                    )
                };
                let d_lg = if fp_locked[lg] {
                    Point::new(0, 0)
                } else {
                    Point::new(
                        cm.x + (rt_g.0 * beta_t + rn_g.0 * alpha_n).round() as Nm,
                        cm.y + (rt_g.1 * beta_t + rn_g.1 * alpha_n).round() as Nm,
                    )
                };
                (d_lf, d_lg)
            };

            let ok_deltas = |d_lf: Point, d_lg: Point| -> bool {
                let qf = Point::new(pf.x + d_lf.x, pf.y + d_lf.y);
                let qg = Point::new(pg.x + d_lg.x, pg.y + d_lg.y);
                pair_side_penetration(orig_yards, orig_at, f, qf, g, qg, side).0 <= cap + 1e-6
            };

            let (d0_f, d0_g) = eval_deltas(0.0, 1.0);
            let (fin_df, fin_dg) = if ok_deltas(d0_f, d0_g) {
                let (mut lo, mut hi) = (0.0f64, 1.0f64);
                for _ in 0..10 {
                    let mid = (lo + hi) * 0.5;
                    let (dm_f, dm_g) = eval_deltas(mid, 1.0);
                    if ok_deltas(dm_f, dm_g) {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                eval_deltas(lo, 1.0)
            } else {
                let (mut lo, mut hi) = (0.0f64, 1.0f64);
                for _ in 0..10 {
                    let mid = (lo + hi) * 0.5;
                    let (dm_f, dm_g) = eval_deltas(0.0, mid);
                    if ok_deltas(dm_f, dm_g) {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                eval_deltas(0.0, lo)
            };

            if !fp_locked[lf] {
                for &m in &members[lf] {
                    cand_pos[m] = Point::new(prev_pos[m].x + fin_df.x, prev_pos[m].y + fin_df.y);
                }
                stopped.insert(lf);
            }
            if !fp_locked[lg] {
                for &m in &members[lg] {
                    cand_pos[m] = Point::new(prev_pos[m].x + fin_dg.x, prev_pos[m].y + fin_dg.y);
                }
                stopped.insert(lg);
            }
        }
    }
    stopped.len()
}

fn courtyard_violation_desc(
    board: &Board,
    orig_yards: &[FootprintYards],
    orig_at: &[Point],
    fp_pos: &[Point],
    allowed: &HashMap<(usize, usize, u8), f64>,
) -> Option<String> {
    let pens = courtyard_penetrations(orig_yards, orig_at, fp_pos);
    let mut keys: Vec<(usize, usize, u8)> = pens.keys().copied().collect();
    keys.sort_unstable();
    for key in keys {
        let d0 = allowed.get(&key).copied().unwrap_or(0.0);
        if d0 >= KICAD_COURTYARD_TOL_NM {
            continue;
        }
        let pen = pens[&key];
        if pen > d0 + 10.0 {
            let side_name = if key.2 == 0 { "F.CrtYd" } else { "B.CrtYd" };
            return Some(format!(
                "CourtyardOverlap on {side_name}: {} vs {} (overlap {:.3} mm)",
                board.footprints[key.0].reference,
                board.footprints[key.1].reference,
                pen * 1e-6
            ));
        }
    }
    None
}

type StaticStepBoard = (Board, Copper, String, Vec<Point>);

fn cross_fp_pad_mask_gap(
    board: &Board,
    id_a: ItemId,
    it_a: &Item,
    id_b: ItemId,
    it_b: &Item,
    layer: usize,
) -> Nm {
    let lbit = 1u64 << layer;
    if !it_a.on(layer) || !it_b.on(layer) {
        return 0;
    }
    if (it_a.net == it_b.net && it_a.net != 0) || board.is_tied(id_a, it_a, id_b, it_b) {
        return 0;
    }
    let oa = board
        .mask_openings
        .get(&id_a)
        .filter(|(m, _)| m & lbit != 0)
        .map(|(_, d)| *d);
    let ob = board
        .mask_openings
        .get(&id_b)
        .filter(|(m, _)| m & lbit != 0)
        .map(|(_, d)| *d);
    let mut req = 0;
    if let Some(da) = oa {
        req = req.max(da + board.mask_margin());
    }
    if let Some(db) = ob {
        req = req.max(db + board.mask_margin());
    }
    if let (Some(da), Some(db)) = (oa, ob) {
        if board.mask_web > 0 {
            req = req.max(da + db + board.mask_web);
        }
    }
    req
}

fn pad_ordinals_on_board(board: &Board) -> HashMap<ItemId, (usize, usize)> {
    let mut counts = vec![0usize; board.footprints.len()];
    let mut map = HashMap::new();
    for (id, it) in board.items() {
        if let Kind::Pad { footprint: f, .. } = it.kind {
            if f < counts.len() {
                let ord = counts[f];
                counts[f] += 1;
                map.insert(id, (f, ord));
            }
        }
    }
    map
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn project_scaled_footprints(
    orig_board: &Board,
    orig_yards: &[FootprintYards],
    envs: &[FootprintEnvelope],
    fp_locked: &[bool],
    centre: (f64, f64),
    factor: f64,
    orig_at: &[Point],
    cand_pos: &mut [Point],
) {
    let allowed_yards = courtyard_penetrations(orig_yards, orig_at, orig_at);
    resolve_courtyard_stops(
        orig_yards,
        envs,
        fp_locked,
        orig_at,
        orig_at,
        cand_pos,
        &allowed_yards,
    );

    let mut bare = orig_board.clone();
    let drop_ids: Vec<ItemId> = bare
        .items()
        .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) || it.refilled())
        .map(|(id, _)| id)
        .collect();
    for id in drop_ids {
        bare.remove(id);
    }
    let baseline_static = collect_static_violations(&bare, &HashSet::new()).0;

    let scale_pt = |p: Point| -> Point {
        Point::new(
            (centre.0 + factor * (p.x as f64 - centre.0)).round() as Nm,
            (centre.1 + factor * (p.y as f64 - centre.1)).round() as Nm,
        )
    };
    if !envs.iter().any(|e| e.owns_outline) {
        for poly in &mut bare.outline {
            for p in poly.iter_mut() {
                *p = scale_pt(*p);
            }
        }
        for e in &mut bare.outline_edges {
            e.0 = scale_pt(e.0);
            e.1 = scale_pt(e.1);
            e.2 = BBox::of([e.0, e.1]);
        }
    }
    let board_keepouts: Vec<ItemId> = bare
        .items()
        .filter(|(_, it)| matches!(it.kind, Kind::Keepout { footprint: None, .. }))
        .map(|(id, _)| id)
        .collect();
    for kid in board_keepouts {
        if let Some(mut it) = bare.remove(kid) {
            it.shape = match it.shape {
                Shape::Capsule { a, b, r } => Shape::Capsule { a: scale_pt(a), b: scale_pt(b), r },
                Shape::Polygon(poly) => Shape::Polygon(poly.into_iter().map(scale_pt).collect()),
            };
            bare.restore(kid, it);
        }
    }

    for f in 0..orig_at.len() {
        let d = Point::new(cand_pos[f].x - orig_at[f].x, cand_pos[f].y - orig_at[f].y);
        if d != Point::new(0, 0) {
            bare.shift_footprint(f, d);
        }
    }
    project_footprints_on_board(
        &mut bare,
        orig_yards,
        envs,
        fp_locked,
        orig_at,
        orig_at,
        cand_pos,
        &allowed_yards,
        &baseline_static,
    );
}

#[allow(clippy::too_many_arguments)]
fn resolve_static_step_board(
    bare_text: &str,
    rules: &Rules,
    orig_pcb: &Pcb,
    orig_copper: &Copper,
    orig_board: &Board,
    orig_bb: BBox,
    centre: (f64, f64),
    factor: f64,
    orig_at: &[Point],
    prev_pos: &[Point],
    mut cand_pos: Vec<Point>,
    orig_yards: &[FootprintYards],
    envs: &[FootprintEnvelope],
    fp_locked: &[bool],
    allowed_yards: &HashMap<(usize, usize, u8), f64>,
    baseline_static: &HashSet<StaticViolKey>,
    orig_locked_items: &[Item],
) -> Result<Option<StaticStepBoard>, String> {
    let orig_had_outline = !orig_board.outline.is_empty();
    for attempt in 0..4 {
        let shifts: Vec<(usize, Point)> = (0..orig_at.len())
            .map(|f| (f, Point::new(cand_pos[f].x - orig_at[f].x, cand_pos[f].y - orig_at[f].y)))
            .collect();
        let field = build_scale_field(orig_board, centre, factor, &shifts);
        let warped_locked = warp_items(orig_locked_items, &field);
        let (mut board, copper, text) = build_scaled_bare_board(
            bare_text,
            rules,
            orig_pcb,
            orig_copper,
            orig_bb,
            centre,
            factor,
            &shifts,
            &field,
            &warped_locked,
        )?;
        if orig_had_outline && board.outline.is_empty() {
            return Ok(None);
        }
        let (viols, _) = collect_static_violations(&board, baseline_static);
        let yard_err =
            courtyard_violation_desc(&board, orig_yards, orig_at, &cand_pos, allowed_yards);
        if viols.is_empty() && yard_err.is_none() {
            return Ok(Some((board, copper, text, cand_pos)));
        }
        if attempt == 3 {
            break;
        }
        let before = cand_pos.clone();
        project_footprints_on_board(
            &mut board,
            orig_yards,
            envs,
            fp_locked,
            orig_at,
            prev_pos,
            &mut cand_pos,
            allowed_yards,
            baseline_static,
        );
        if cand_pos == before {
            break;
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn project_footprints_on_board(
    board: &mut Board,
    orig_yards: &[FootprintYards],
    envs: &[FootprintEnvelope],
    fp_locked: &[bool],
    orig_at: &[Point],
    prev_pos: &[Point],
    cand_pos: &mut [Point],
    allowed_yards: &HashMap<(usize, usize, u8), f64>,
    baseline_static: &HashSet<StaticViolKey>,
) {
    let n = orig_at.len();
    if n == 0 {
        return;
    }
    let (leader, members) = fp_family_groups(n, envs, fp_locked);
    let group_immob: Vec<bool> = (0..n)
        .map(|g| members[g].iter().any(|&m| fp_locked[m] || envs[m].owns_outline))
        .collect();
    let pad_ords = pad_ordinals_on_board(board);
    let mut fp_pads: Vec<Vec<ItemId>> = vec![Vec::new(); n];
    for (&id, &(f, _)) in &pad_ords {
        fp_pads[f].push(id);
    }
    for ps in &mut fp_pads {
        ps.sort_unstable();
    }

    for _iter in 0..28 {
        let mut hs_group: Vec<Vec<HalfPlane>> = vec![Vec::new(); n];
        append_static_fp_guard_constraints(
            board,
            &fp_pads,
            &pad_ords,
            &leader,
            &group_immob,
            orig_yards,
            orig_at,
            cand_pos,
            allowed_yards,
            baseline_static,
            &mut hs_group,
        );
        let mut moved_any = false;
        for g in 0..n {
            if members[g].is_empty() || group_immob[g] || hs_group[g].is_empty() {
                continue;
            }
            let raw = solve_half_planes_2d((0.0, 0.0), &hs_group[g]);
            let step = clamp_vec(raw, 400_000.0);
            let delta = Point::new(step.0.round() as Nm, step.1.round() as Nm);
            if delta != Point::new(0, 0) {
                for &m in &members[g] {
                    board.shift_footprint(m, delta);
                    cand_pos[m] = Point::new(cand_pos[m].x + delta.x, cand_pos[m].y + delta.y);
                }
                moved_any = true;
            }
        }
        if !moved_any {
            break;
        }
    }

    let before_yard = cand_pos.to_vec();
    resolve_courtyard_stops(
        orig_yards,
        envs,
        fp_locked,
        orig_at,
        prev_pos,
        cand_pos,
        allowed_yards,
    );
    for f in 0..n {
        let d = Point::new(cand_pos[f].x - before_yard[f].x, cand_pos[f].y - before_yard[f].y);
        if d != Point::new(0, 0) {
            board.shift_footprint(f, d);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn append_static_fp_guard_constraints(
    board: &Board,
    fp_pads: &[Vec<ItemId>],
    pad_ords: &HashMap<ItemId, (usize, usize)>,
    leader: &[usize],
    group_immob: &[bool],
    orig_yards: &[FootprintYards],
    orig_at: &[Point],
    cur_fp_pos: &[Point],
    allowed_yards: &HashMap<(usize, usize, u8), f64>,
    baseline_static: &HashSet<StaticViolKey>,
    hs_group: &mut [Vec<HalfPlane>],
) {
    let n = orig_at.len();
    let num_layers = board.layers.len();
    let holes_slot = num_layers;
    let rule_reach = board.max_rule_reach();
    let hole_edge_req = nm(board.rules.min_hole_clearance);
    const BUF: f64 = 2_000.0;

    let push_pair = |ga: usize,
                     gb: usize,
                     nx: f64,
                     ny: f64,
                     need: f64,
                     hs: &mut [Vec<HalfPlane>]| {
        if ga == gb || need <= 0.0 {
            return;
        }
        match (!group_immob[ga], !group_immob[gb]) {
            (true, true) => {
                hs[ga].push(HalfPlane { nx, ny, need: need * 0.55 });
                hs[gb].push(HalfPlane { nx: -nx, ny: -ny, need: need * 0.55 });
            }
            (true, false) => hs[ga].push(HalfPlane { nx, ny, need }),
            (false, true) => hs[gb].push(HalfPlane { nx: -nx, ny: -ny, need }),
            (false, false) => {}
        }
    };

    // 1. Courtyard overlaps between distinct family leaders.
    let pens = courtyard_penetrations(orig_yards, orig_at, cur_fp_pos);
    let mut ykeys: Vec<(usize, usize, u8)> = pens.keys().copied().collect();
    ykeys.sort_unstable();
    for key in ykeys {
        let (f, g, side) = key;
        let (lf, lg) = (leader[f], leader[g]);
        if lf == lg || (group_immob[lf] && group_immob[lg]) {
            continue;
        }
        let d0 = allowed_yards.get(&key).copied().unwrap_or(0.0);
        if d0 >= KICAD_COURTYARD_TOL_NM {
            continue;
        }
        let (pen, ia, ib) =
            pair_side_penetration(orig_yards, orig_at, f, cur_fp_pos[f], g, cur_fp_pos[g], side);
        if pen > d0 {
            let df = Point::new(cur_fp_pos[f].x - orig_at[f].x, cur_fp_pos[f].y - orig_at[f].y);
            let dg = Point::new(cur_fp_pos[g].x - orig_at[g].x, cur_fp_pos[g].y - orig_at[g].y);
            let yf: Vec<Point> = yard_side_polys(&orig_yards[f], side)[ia]
                .iter()
                .map(|p| Point::new(p.x + df.x, p.y + df.y))
                .collect();
            let yg: Vec<Point> = yard_side_polys(&orig_yards[g], side)[ib]
                .iter()
                .map(|p| Point::new(p.x + dg.x, p.y + dg.y))
                .collect();
            let (nx, ny) = courtyard_contact_normal(&yf, &yg);
            push_pair(lf, lg, nx, ny, (pen - d0) + 1_500.0, hs_group);
        }
    }

    // 2. Board outline edges, board keepouts, and cross-footprint pad/hole/mask clearances.
    for f in 0..n {
        let lf = leader[f];
        for &id_a in &fp_pads[f] {
            let Some(it_a) = board.item(id_a) else { continue };
            let ord_a = pad_ords[&id_a].1;
            let bb_a = board.bbox(id_a).unwrap_or_else(|| it_a.shape.bbox());

            if !group_immob[lf] {
                if it_a.is_copper()
                    && !baseline_static.contains(&StaticViolKey {
                        kind: StaticCheckKind::BoardRule(Rule::EdgeClearance),
                        a_fp: f,
                        a_pad_ord: ord_a,
                        b_fp: None,
                        b_pad_ord: None,
                    })
                {
                    let qedge = it_a.shape.bbox().grow(rule_reach);
                    for &(ea, eb, ebb, eclr) in &board.outline_edges {
                        if !ebb.overlaps(qedge) {
                            continue;
                        }
                        let eshape = Shape::Capsule { a: ea, b: eb, r: 0 };
                        let (gap, nx, ny) = closest_shape_shape(&it_a.shape, &eshape);
                        let req = eclr as f64 + BUF;
                        if gap < req {
                            hs_group[lf].push(HalfPlane { nx, ny, need: req - gap });
                        }
                    }
                }
                if hole_edge_req > 0 {
                    if let Some(hs_a) = board.hole_shape(id_a, it_a) {
                        if !baseline_static.contains(&StaticViolKey {
                            kind: StaticCheckKind::HoleEdge,
                            a_fp: f,
                            a_pad_ord: ord_a,
                            b_fp: None,
                            b_pad_ord: None,
                        }) {
                            let qedge = hs_a.bbox().grow(hole_edge_req + BUF as Nm);
                            for &(ea, eb, ebb, _) in &board.outline_edges {
                                if !ebb.overlaps(qedge) {
                                    continue;
                                }
                                let eshape = Shape::Capsule { a: ea, b: eb, r: 0 };
                                let (gap, nx, ny) = closest_shape_shape(&hs_a, &eshape);
                                let req = hole_edge_req as f64 + BUF;
                                if gap < req {
                                    hs_group[lf].push(HalfPlane { nx, ny, need: req - gap });
                                }
                            }
                        }
                    }
                }
            }

            // Shared-layer copper pads and keepouts.
            if it_a.is_copper() {
                let qbb = bb_a.grow(rule_reach);
                for l in (0..num_layers).filter(|&l| it_a.on(l)) {
                    for id_b in board.near(l, qbb) {
                        if id_b == id_a {
                            continue;
                        }
                        let Some(it_b) = board.item(id_b) else { continue };
                        if !it_b.on(l) || it_b.refilled() {
                            continue;
                        }
                        match &it_b.kind {
                            Kind::Keepout { forbids, footprint: kfp }
                                if forbids.pads && *kfp != Some(f) =>
                            {
                                let lg_opt = kfp.filter(|&k| k < n).map(|k| leader[k]);
                                if lg_opt == Some(lf) {
                                    continue;
                                }
                                let (b_fp, b_ord) = pad_ords
                                    .get(&id_b)
                                    .map_or((None, None), |&(bf, bo)| (Some(bf), Some(bo)));
                                if baseline_static.contains(&StaticViolKey {
                                    kind: StaticCheckKind::BoardRule(Rule::Keepout),
                                    a_fp: f,
                                    a_pad_ord: ord_a,
                                    b_fp,
                                    b_pad_ord: b_ord,
                                }) {
                                    continue;
                                }
                                let in_hole = board.keepout_holes.get(&id_b).is_some_and(|hs| {
                                    hs.iter().any(|h| board::within(h, &it_a.shape, 0))
                                });
                                if !in_hole {
                                    let (gap, nx, ny) = closest_shape_shape(&it_a.shape, &it_b.shape);
                                    if gap < BUF {
                                        if let Some(lg) = lg_opt {
                                            push_pair(lf, lg, nx, ny, BUF - gap, hs_group);
                                        } else if !group_immob[lf] {
                                            hs_group[lf].push(HalfPlane { nx, ny, need: BUF - gap });
                                        }
                                    }
                                }
                            }
                            Kind::Pad { footprint: g, .. } if id_b > id_a && *g != f => {
                                let lg = leader[*g];
                                if lg == lf || (group_immob[lf] && group_immob[lg]) {
                                    continue;
                                }
                                let ord_b = pad_ords[&id_b].1;
                                if (it_a.net != it_b.net || it_a.net == 0)
                                    && !board.is_tied(id_a, it_a, id_b, it_b)
                                {
                                    let mut req = 0i64;
                                    if !baseline_static.contains(&StaticViolKey {
                                        kind: StaticCheckKind::BoardRule(Rule::Clearance),
                                        a_fp: f,
                                        a_pad_ord: ord_a,
                                        b_fp: Some(*g),
                                        b_pad_ord: Some(ord_b),
                                    }) {
                                        req = req.max(board.clearance_kept(
                                            &Thing::Item(board, id_a, it_a),
                                            &Thing::Item(board, id_b, it_b),
                                            l,
                                        ));
                                    }
                                    if !baseline_static.contains(&StaticViolKey {
                                        kind: StaticCheckKind::PadMaskBridge,
                                        a_fp: f,
                                        a_pad_ord: ord_a,
                                        b_fp: Some(*g),
                                        b_pad_ord: Some(ord_b),
                                    }) {
                                        req = req.max(cross_fp_pad_mask_gap(
                                            board, id_a, it_a, id_b, it_b, l,
                                        ));
                                    }
                                    if req > 0 {
                                        let req_f = req as f64 + BUF;
                                        if it_a.shape.bbox().grow(req + BUF as Nm).overlaps(it_b.shape.bbox()) {
                                            let (gap, nx, ny) =
                                                closest_shape_shape(&it_a.shape, &it_b.shape);
                                            if gap < req_f {
                                                push_pair(lf, lg, nx, ny, req_f - gap, hs_group);
                                            }
                                        }
                                    }
                                }
                            }
                            Kind::Pour { .. }
                                if !group_immob[lf]
                                    && (it_a.net != it_b.net || it_a.net == 0)
                                    && !board.is_tied(id_a, it_a, id_b, it_b)
                                    && !baseline_static.contains(&StaticViolKey {
                                        kind: StaticCheckKind::BoardRule(Rule::Clearance),
                                        a_fp: f,
                                        a_pad_ord: ord_a,
                                        b_fp: None,
                                        b_pad_ord: None,
                                    }) =>
                            {
                                let req = board.clearance_kept(
                                    &Thing::Item(board, id_a, it_a),
                                    &Thing::Item(board, id_b, it_b),
                                    l,
                                ) as f64
                                    + BUF;
                                if it_a.shape.bbox().grow(req as Nm).overlaps(it_b.shape.bbox()) {
                                    let (gap, nx, ny) =
                                        closest_shape_shape(&it_a.shape, &it_b.shape);
                                    if gap < req {
                                        hs_group[lf].push(HalfPlane {
                                            nx,
                                            ny,
                                            need: req - gap,
                                        });
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }

            // Drilled holes of `id_a` vs other footprints' holes and copper pads.
            if let Some(hs_a) = board.hole_shape(id_a, it_a) {
                let qh = hs_a.bbox().grow(rule_reach);
                for id_b in board.near(holes_slot, qh) {
                    if id_b <= id_a {
                        continue;
                    }
                    let Some(it_b) = board.item(id_b) else { continue };
                    let Kind::Pad { footprint: g, .. } = it_b.kind else { continue };
                    if g == f {
                        continue;
                    }
                    let lg = leader[g];
                    if lg == lf || (group_immob[lf] && group_immob[lg]) {
                        continue;
                    }
                    let ord_b = pad_ords[&id_b].1;
                    if baseline_static.contains(&StaticViolKey {
                        kind: StaticCheckKind::BoardRule(Rule::HoleToHole),
                        a_fp: f,
                        a_pad_ord: ord_a,
                        b_fp: Some(g),
                        b_pad_ord: Some(ord_b),
                    }) {
                        continue;
                    }
                    if let Some(hs_b) = board.hole_shape(id_b, it_b) {
                        let req = board.hole_to_hole_clearance(id_a, it_a, id_b, it_b) as f64 + BUF;
                        let (gap, nx, ny) = closest_shape_shape(&hs_a, &hs_b);
                        if gap < req {
                            push_pair(lf, lg, nx, ny, req - gap, hs_group);
                        }
                    }
                }
                for l in 0..num_layers {
                    for id_b in board.near(l, qh) {
                        if id_b == id_a {
                            continue;
                        }
                        let Some(it_b) = board.item(id_b) else { continue };
                        let Kind::Pad { footprint: g, .. } = it_b.kind else { continue };
                        if g == f || !it_b.is_copper() || !it_b.on(l) {
                            continue;
                        }
                        let lg = leader[g];
                        if lg == lf || (group_immob[lf] && group_immob[lg]) {
                            continue;
                        }
                        let ord_b = pad_ords[&id_b].1;
                        if baseline_static.contains(&StaticViolKey {
                            kind: StaticCheckKind::BoardRule(Rule::HoleClearance),
                            a_fp: f,
                            a_pad_ord: ord_a,
                            b_fp: Some(g),
                            b_pad_ord: Some(ord_b),
                        }) {
                            continue;
                        }
                        let hc = board.hole_clearance(id_a, it_a, id_b, it_b);
                        if hc > 0 {
                            let req = hc as f64 + BUF;
                            let (gap, nx, ny) = closest_shape_shape(&hs_a, &it_b.shape);
                            if gap < req {
                                push_pair(lf, lg, nx, ny, req - gap, hs_group);
                            }
                        }
                    }
                }
            }
        }
    }
}

fn warp_items(items: &[Item], field: &WarpField) -> Vec<Item> {
    let shift_pt = |p: Point| -> Point {
        let (dx, dy) = field.displacement(p);
        Point::new((p.x as f64 + dx).round() as Nm, (p.y as f64 + dy).round() as Nm)
    };
    items
        .iter()
        .map(|it| {
            let shape = match &it.shape {
                Shape::Capsule { a, b, r } => Shape::Capsule { a: shift_pt(*a), b: shift_pt(*b), r: *r },
                Shape::Polygon(poly) => Shape::Polygon(poly.iter().copied().map(shift_pt).collect()),
            };
            let hole = it.hole.map(|(p, r)| (shift_pt(p), r));
            Item { shape, hole, ..it.clone() }
        })
        .collect()
}

struct BaselineConn {
    per_net: HashMap<u32, usize>,
    pad_cluster: HashMap<ItemId, usize>,
    /// Pairs of pads `(id_a, id_b)` on different footprints that directly overlapped at step 0 and
    /// both sit inside a shared same-net refilled zone `z`. If scaling footprints back toward 100%
    /// separates those two pads while both remain inside zone `z`, the zone pour still bridges them.
    zone_bridged_overlaps: Vec<(ItemId, ItemId, usize)>,
}

fn zone_covers_pad(board: &Board, z: usize, zlayers: u64, zshape: &Shape, id: ItemId, it: &Item) -> bool {
    if it.layers & zlayers == 0 {
        return false;
    }
    let connects = match board.pad_zone_connect.get(&id) {
        Some(&c) => c != 0,
        None => !board.zones_without_pads.contains(&z),
    };
    connects && zshape.gap_within(&it.shape, 0.0) == 0.0
}

fn baseline_connectivity(board: &Board) -> BaselineConn {
    let conn = connectivity_without_pours(board);
    let mut per_net = HashMap::new();
    for a in &conn.airlines {
        *per_net.entry(a.net).or_insert(0) += 1;
    }
    let mut pad_cluster = HashMap::new();
    let mut pads_by_net: HashMap<u32, Vec<(ItemId, usize)>> = HashMap::new();
    for (id, it) in board.items() {
        if let Kind::Pad { footprint, .. } = it.kind {
            if let Some(&c) = conn.cluster.get(&id) {
                pad_cluster.insert(id, c);
            }
            if it.net != 0 {
                pads_by_net.entry(it.net).or_default().push((id, footprint));
            }
        }
    }
    let mut zone_bridged_overlaps = Vec::new();
    for (&net, list) in &pads_by_net {
        for i in 0..list.len() {
            for j in (i + 1)..list.len() {
                let (id1, fp1) = list[i];
                let (id2, fp2) = list[j];
                if fp1 == fp2 || !board::joined(board, id1, id2) {
                    continue;
                }
                let (Some(it1), Some(it2)) = (board.item(id1), board.item(id2)) else { continue };
                for &(z, znet, zlayers, ref zshape) in &board.unfilled {
                    if znet == net
                        && (it1.layers & it2.layers & zlayers) != 0
                        && zone_covers_pad(board, z, zlayers, zshape, id1, it1)
                        && zone_covers_pad(board, z, zlayers, zshape, id2, it2)
                    {
                        zone_bridged_overlaps.push((id1, id2, z));
                        break;
                    }
                }
            }
        }
    }
    BaselineConn { per_net, pad_cluster, zone_bridged_overlaps }
}

fn added_airlines(board: &Board, baseline: &BaselineConn) -> (usize, Option<String>) {
    let conn = connectivity_without_pours(board);
    // Union clusters on a net when their separation is solely two cross-footprint pads that
    // un-overlapped inside a shared same-net pour zone.
    let mut uf: HashMap<usize, usize> = conn.cluster.values().copied().map(|c| (c, c)).collect();
    fn uf_find(uf: &mut HashMap<usize, usize>, mut x: usize) -> usize {
        while let Some(&p) = uf.get(&x) {
            if p == x {
                break;
            }
            x = p;
        }
        x
    }
    for &(id1, id2, z) in &baseline.zone_bridged_overlaps {
        let (Some(&c1), Some(&c2)) = (conn.cluster.get(&id1), conn.cluster.get(&id2)) else {
            continue;
        };
        let (r1, r2) = (uf_find(&mut uf, c1), uf_find(&mut uf, c2));
        if r1 == r2 {
            continue;
        }
        let (Some(it1), Some(it2)) = (board.item(id1), board.item(id2)) else { continue };
        let still_covered = board.unfilled.iter().any(|&(uz, znet, zlayers, ref zshape)| {
            uz == z
                && znet == it1.net
                && zone_covers_pad(board, uz, zlayers, zshape, id1, it1)
                && zone_covers_pad(board, uz, zlayers, zshape, id2, it2)
        });
        if still_covered {
            uf.insert(r1, r2);
        }
    }

    let mut bridged_by_net: HashMap<u32, usize> = HashMap::new();
    let mut seen_merge: HashSet<(u32, usize)> = HashSet::new();
    for (&id, &c) in &conn.cluster {
        let r = uf_find(&mut uf, c);
        if r != c {
            if let Some(it) = board.item(id) {
                if seen_merge.insert((it.net, c)) {
                    *bridged_by_net.entry(it.net).or_insert(0) += 1;
                }
            }
        }
    }

    let mut now: HashMap<u32, usize> = HashMap::new();
    for a in &conn.airlines {
        *now.entry(a.net).or_insert(0) += 1;
    }
    for (net, dec) in bridged_by_net {
        if let Some(cnt) = now.get_mut(&net) {
            *cnt = cnt.saturating_sub(dec);
        }
    }
    let mut pads_in_cluster: HashMap<usize, Vec<ItemId>> = HashMap::new();
    for (id, it) in board.items() {
        if matches!(it.kind, Kind::Pad { .. }) {
            if let Some(&c) = conn.cluster.get(&id) {
                pads_in_cluster.entry(c).or_default().push(id);
            }
        }
    }

    let mut extra = 0usize;
    let mut desc = None;
    for (&net, &cnt) in &now {
        let base = baseline.per_net.get(&net).copied().unwrap_or(0);
        if cnt > base {
            extra += cnt - base;
            if desc.is_none() {
                // Prefer an airline whose two clusters were united at step 0 (and not zone-bridged),
                // or a loose padless track/via fragment that detached from its cluster.
                let mut best: Option<(u8, f64, Point, Point)> = None;
                for a in conn.airlines.iter().filter(|a| a.net == net) {
                    let c1 = conn.cluster.get(&a.from_item).copied();
                    let c2 = conn.cluster.get(&a.to_item).copied();
                    if let (Some(x), Some(y)) = (c1, c2) {
                        if uf_find(&mut uf, x) == uf_find(&mut uf, y) {
                            continue;
                        }
                    }
                    let p1 = c1.and_then(|c| pads_in_cluster.get(&c));
                    let p2 = c2.and_then(|c| pads_in_cluster.get(&c));
                    let prio = if p1.is_none_or(Vec::is_empty) || p2.is_none_or(Vec::is_empty) {
                        0u8
                    } else if p1.unwrap().iter().any(|id1| {
                        let bc1 = baseline.pad_cluster.get(id1);
                        bc1.is_some()
                            && p2.unwrap().iter().any(|id2| baseline.pad_cluster.get(id2) == bc1)
                    }) {
                        1u8
                    } else {
                        2u8
                    };
                    let len = a.from.distance(a.to);
                    if best.is_none_or(|(bp, bl, _, _)| (prio, len) < (bp, bl)) {
                        best = Some((prio, len, a.from, a.to));
                    }
                }
                if let Some((_, _, p, q)) = best {
                    desc = Some(format!(
                        "net {} ({}) pulled apart between ({:.3}, {:.3}) mm and ({:.3}, {:.3}) mm",
                        board.net_name(net),
                        cnt - base,
                        p.x as f64 * 1e-6,
                        p.y as f64 * 1e-6,
                        q.x as f64 * 1e-6,
                        q.y as f64 * 1e-6
                    ));
                }
            }
        }
    }
    (extra, desc)
}

fn collect_static_violations(
    board: &Board,
    baseline: &HashSet<StaticViolKey>,
) -> (HashSet<StaticViolKey>, Option<String>) {
    let pad_ords = pad_ordinals_on_board(board);
    let num_layers = board.layers.len();
    let rule_reach = board.max_rule_reach();
    let hole_edge_req = nm(board.rules.min_hole_clearance);
    let mut out = HashSet::new();
    let mut first_desc = None;

    for (id, it) in board.items() {
        let Kind::Pad { footprint: fp_a, number: ref pad_a, .. } = it.kind else { continue };
        let Some(&(_, ord_a)) = pad_ords.get(&id) else { continue };

        for v in check_item(board, id) {
            let (b_fp, b_pad_ord) = v
                .b
                .and_then(|bid| pad_ords.get(&bid).copied())
                .map_or((None, None), |(bf, bo)| (Some(bf), Some(bo)));
            let key = StaticViolKey {
                kind: StaticCheckKind::BoardRule(v.rule),
                a_fp: fp_a,
                a_pad_ord: ord_a,
                b_fp,
                b_pad_ord,
            };
            if baseline.contains(&key) {
                continue;
            }
            if first_desc.is_none() {
                first_desc = Some(describe_violation(board, &v));
            }
            out.insert(key);
        }

        // Cross-footprint pad-to-pad solder-mask bridge check.
        if it.is_copper() {
            let qbb = it.shape.bbox().grow(rule_reach);
            for l in (0..num_layers).filter(|&l| it.on(l)) {
                for id_b in board.near(l, qbb) {
                    if id_b <= id {
                        continue;
                    }
                    let Some(it_b) = board.item(id_b) else { continue };
                    let Kind::Pad { footprint: fp_b, number: ref pad_b, .. } = it_b.kind else {
                        continue;
                    };
                    if fp_b == fp_a {
                        continue;
                    }
                    let req = cross_fp_pad_mask_gap(board, id, it, id_b, it_b, l);
                    if req <= 0 || !it.shape.bbox().grow(req).overlaps(it_b.shape.bbox()) {
                        continue;
                    }
                    let gap = it.shape.gap_within(&it_b.shape, req as f64);
                    if gap + 500.0 < req as f64 {
                        let ord_b = pad_ords[&id_b].1;
                        let key = StaticViolKey {
                            kind: StaticCheckKind::PadMaskBridge,
                            a_fp: fp_a,
                            a_pad_ord: ord_a,
                            b_fp: Some(fp_b),
                            b_pad_ord: Some(ord_b),
                        };
                        if !baseline.contains(&key) {
                            if first_desc.is_none() {
                                first_desc = Some(format!(
                                    "SolderMaskBridge on {}: {}-{} vs {}-{} (gap {:.3} mm < required {:.3} mm)",
                                    board.layers[l],
                                    board.footprints[fp_a].reference,
                                    pad_a,
                                    board.footprints[fp_b].reference,
                                    pad_b,
                                    gap * 1e-6,
                                    req as f64 * 1e-6
                                ));
                            }
                            out.insert(key);
                        }
                    }
                }
            }
        }

        // Footprint drilled hole (PTH or NPTH) vs board outline edges.
        if hole_edge_req > 0 && !board.outline_edges.is_empty() {
            if let Some(hs) = board.hole_shape(id, it) {
                let qedge = hs.bbox().grow(hole_edge_req);
                let mut worst_gap = hole_edge_req as f64;
                for &(ea, eb, ebb, _) in &board.outline_edges {
                    if !ebb.overlaps(qedge) {
                        continue;
                    }
                    let eshape = Shape::Capsule { a: ea, b: eb, r: 0 };
                    worst_gap = worst_gap.min(hs.gap_within(&eshape, hole_edge_req as f64));
                }
                if worst_gap + 500.0 < hole_edge_req as f64 {
                    let key = StaticViolKey {
                        kind: StaticCheckKind::HoleEdge,
                        a_fp: fp_a,
                        a_pad_ord: ord_a,
                        b_fp: None,
                        b_pad_ord: None,
                    };
                    if !baseline.contains(&key) {
                        if first_desc.is_none() {
                            first_desc = Some(format!(
                                "HoleEdgeClearance: {}-{} hole vs board edge (gap {:.3} mm < required {:.3} mm)",
                                board.footprints[fp_a].reference,
                                pad_a,
                                worst_gap * 1e-6,
                                hole_edge_req as f64 * 1e-6
                            ));
                        }
                        out.insert(key);
                    }
                }
            }
        }
    }
    (out, first_desc)
}

fn count_dynamic_violations(board: &Board) -> (usize, Option<String>) {
    let mut count = 0usize;
    let mut worst: Option<(f64, Violation)> = None;
    let mut seen_pairs: HashSet<(ItemId, ItemId, Rule)> = HashSet::new();
    for (id, it) in board.items() {
        if !matches!(it.kind, Kind::Track | Kind::Via) {
            continue;
        }
        for v in check_item(board, id) {
            if let Some(other) = v.b {
                if let Some(oit) = board.item(other) {
                    if matches!(oit.kind, Kind::Track | Kind::Via) {
                        let key = (id.min(other), id.max(other), v.rule);
                        if !seen_pairs.insert(key) {
                            continue;
                        }
                    }
                }
            }
            count += 1;
            let shortfall = v.required as f64 - v.gap;
            if worst.as_ref().is_none_or(|(w, _)| shortfall > *w) {
                worst = Some((shortfall, v));
            }
        }
    }
    (count, worst.map(|(_, v)| describe_violation(board, &v)))
}

fn describe_violation(board: &Board, v: &Violation) -> String {
    let item_label = |id: ItemId| -> String {
        let Some(it) = board.item(id) else { return format!("item#{id}") };
        let net = board.net_name(it.net);
        let bb = it.shape.bbox();
        let (mx, my) = (
            (bb.min.x + bb.max.x) as f64 * 0.5e-6,
            (bb.min.y + bb.max.y) as f64 * 0.5e-6,
        );
        match &it.kind {
            Kind::Pad { footprint, number, .. } => {
                format!("{}-{} ({net}) at ({mx:.2}, {my:.2}) mm", board.footprints[*footprint].reference, number)
            }
            Kind::Via => format!("via ({net}) at ({mx:.2}, {my:.2}) mm"),
            Kind::Track => format!("track ({net}) near ({mx:.2}, {my:.2}) mm"),
            Kind::Pour { .. } => format!("pour ({net}) near ({mx:.2}, {my:.2}) mm"),
            Kind::Keepout { .. } => format!("keepout near ({mx:.2}, {my:.2}) mm"),
            Kind::Spoke { footprint, .. } => {
                format!("thermal spoke of {} ({net}) at ({mx:.2}, {my:.2}) mm", board.footprints[*footprint].reference)
            }
        }
    };
    let layer_str = v
        .layer
        .and_then(|l| board.layers.get(l))
        .map_or_else(String::new, |n| format!(" on {n}"));
    let b_str = v
        .b
        .map_or_else(|| "board edge / rule area".to_string(), &item_label);
    format!(
        "{:?}{layer_str}: {} vs {} (gap {:.3} mm < required {:.3} mm)",
        v.rule,
        item_label(v.a),
        b_str,
        v.gap * 1e-6,
        v.required as f64 * 1e-6
    )
}

fn record_via_zones(board: &Board, graph: &mut CopperGraph) {
    for p in &mut graph.particles {
        let Some((vr, _, layers, net, _)) = p.is_via() else { continue };
        if net == 0 {
            continue;
        }
        let pt = Point::new(p.pos.0.round() as Nm, p.pos.1.round() as Nm);
        for &(z, znet, zmask, ref shape) in &board.unfilled {
            if znet == net && (zmask & layers) != 0 && shape.contains(pt) {
                let (_, _, _, gap, _, _) = closest_seg_shape(p.pos, p.pos, vr, shape);
                let dist_in = (-gap - vr as f64).max(0.0);
                let init_margin = dist_in.clamp(500.0, 20_000.0);
                p.plane_zones.push((z, init_margin));
            }
        }
    }
}

fn check_via_zones(board: &Board, graph: &CopperGraph) -> Option<String> {
    for p in &graph.particles {
        if p.plane_zones.is_empty() {
            continue;
        }
        let Some((_, _, _, net, _)) = p.is_via() else { continue };
        let pt = Point::new(p.pos.0.round() as Nm, p.pos.1.round() as Nm);
        for &(z, _) in &p.plane_zones {
            if let Some((_, _, _, shape)) = board.unfilled.iter().find(|u| u.0 == z) {
                if !shape.contains(pt) {
                    return Some(format!(
                        "via of {} at ({:.2}, {:.2}) mm drifted outside its plane zone",
                        board.net_name(net),
                        pt.x as f64 * 1e-6,
                        pt.y as f64 * 1e-6
                    ));
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Projected Spring + Clearance + Bend-Insertion Solver
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct HalfPlane {
    nx: f64,
    ny: f64,
    need: f64,
}

type SplitCand = (f64, f64, f64, f64);

fn cos_at_vertex(vertex: Point, a: Point, b: Point) -> f64 {
    let (ux, uy) = ((a.x - vertex.x) as f64, (a.y - vertex.y) as f64);
    let (vx, vy) = ((b.x - vertex.x) as f64, (b.y - vertex.y) as f64);
    let (lu, lv) = (ux.hypot(uy), vx.hypot(vy));
    if lu < 1.0 || lv < 1.0 {
        -1.0
    } else {
        (ux * vx + uy * vy) / (lu * lv)
    }
}

/// Legal topological via escape: when an unlocked standalone via (or one of its incident segments)
/// is pinched in a narrow channel (most commonly between a front SMD pad and an opposite-side back
/// SMD pad), tries sliding the via along its own net's track chain into nearby open space while
/// collapsing the traversed chain prefix (Mode A), leaving a single-layer bend behind on the
/// opposite layer when transitioning layers (Mode B), or stepping radially within local open space
/// (Mode C). Every accepted move preserves exact net topology, keeps plane-stitching vias inside
/// their plane outlines, introduces no acute angle, and passes `check_item` on the via and every
/// modified or added segment.
fn slide_pinched_vias(graph: &mut CopperGraph, board: &mut Board) -> usize {
    const ARC_STEPS_NM: [f64; 8] = [
        100_000.0,
        220_000.0,
        360_000.0,
        520_000.0,
        720_000.0,
        960_000.0,
        1_250_000.0,
        1_600_000.0,
    ];
    const LAT_A_NM: [f64; 7] = [
        0.0,
        90_000.0,
        -90_000.0,
        220_000.0,
        -220_000.0,
        380_000.0,
        -380_000.0,
    ];
    const LAT_B_NM: [f64; 5] = [0.0, 100_000.0, -100_000.0, 240_000.0, -240_000.0];
    const W_OFFS: [(Nm, Nm); 5] = [(0, 0), (0, 90_000), (0, -90_000), (90_000, 0), (-90_000, 0)];
    const RADIAL_NM: [f64; 5] = [240_000.0, 400_000.0, 600_000.0, 850_000.0, 1_100_000.0];

    let mut moved_count = 0usize;
    let init_np = graph.particles.len();

    for pv in 0..init_np {
        let Anchor::Via { r: vr, drill_r: vdr, layers: vlayers, net: vnet, locked: false } =
            graph.particles[pv].anchor
        else {
            continue;
        };
        if graph.particles[pv].locked || graph.particles[pv].via_tether.is_some() {
            continue;
        }
        if graph
            .particles
            .iter()
            .any(|p| p.via_tether.is_some_and(|(tpid, _)| tpid == pv))
        {
            continue;
        }

        let (edge_ids, via_ids) = graph.sync_to_board(board);
        let Some(vid) = via_ids[pv] else { continue };

        let mut inc: Vec<Vec<usize>> = vec![Vec::new(); graph.particles.len()];
        for (ei, e) in graph.edges.iter().enumerate() {
            if e.active {
                inc[e.u].push(ei);
                inc[e.v].push(ei);
            }
        }
        if inc[pv].iter().any(|&ei| graph.edges[ei].locked) {
            continue;
        }

        let via_bad = !check_item(board, vid).is_empty();
        let edge_bad = inc[pv]
            .iter()
            .any(|&ei| edge_ids[ei].is_some_and(|eid| !check_item(board, eid).is_empty()));
        if !via_bad && !edge_bad {
            continue;
        }

        let old_pt = Point::new(
            graph.particles[pv].pos.0.round() as Nm,
            graph.particles[pv].pos.1.round() as Nm,
        );
        let vit_shape = Shape::Capsule { a: old_pt, b: old_pt, r: vr };

        // Do not slide a via that directly overlaps static same-net copper without an explicit
        // incident segment into that shape, lest sliding break an implicit overlap connection.
        let grazes_untracked_same_net = vnet != 0
            && (0..board.layers.len())
                .filter(|&l| vlayers & (1u64 << l) != 0)
                .any(|l| {
                    board.near(l, vit_shape.bbox()).into_iter().any(|oid| {
                        board.item(oid).is_some_and(|oit| {
                            !matches!(oit.kind, Kind::Track | Kind::Via)
                                && oit.is_copper()
                                && oit.net == vnet
                                && !oit.refilled()
                                && vit_shape.gap_within(&oit.shape, 0.0) == 0.0
                                && !inc[pv].iter().any(|&ei| {
                                    let nb = if graph.edges[ei].u == pv {
                                        graph.edges[ei].v
                                    } else {
                                        graph.edges[ei].u
                                    };
                                    let pnb = Point::new(
                                        graph.particles[nb].pos.0.round() as Nm,
                                        graph.particles[nb].pos.1.round() as Nm,
                                    );
                                    oit.shape.contains(pnb)
                                })
                        })
                    })
                });
        if grazes_untracked_same_net {
            continue;
        }

        let mut tethered_targets = vec![false; graph.particles.len()];
        for p in &graph.particles {
            if let Some((tpid, _)) = p.via_tether {
                tethered_targets[tpid] = true;
            }
        }

        let in_plane_zones = |cand: Point, b: &Board, g: &CopperGraph| -> bool {
            g.particles[pv].plane_zones.iter().all(|&(z, _)| {
                b.unfilled
                    .iter()
                    .find(|u| u.0 == z)
                    .is_none_or(|(_, _, _, zsh)| zsh.contains(cand))
            })
        };

        let pv_edges = inc[pv].clone();
        let mut slid = false;

        // 1. Try sliding along each incident track chain `e0` (Mode A direct, then Mode B dogleg).
        'chain_loop: for &e0 in &pv_edges {
            let l_fwd = graph.edges[e0].layer;
            let r_fwd = graph.edges[e0].r;
            let next0 = if graph.edges[e0].u == pv {
                graph.edges[e0].v
            } else {
                graph.edges[e0].u
            };
            let mut chain_edges = vec![e0];
            let mut chain_nodes = vec![pv, next0];
            let mut cum_len = vec![0.0f64];
            let seg0_len = (graph.particles[next0].pos.0 - graph.particles[pv].pos.0)
                .hypot(graph.particles[next0].pos.1 - graph.particles[pv].pos.1);
            cum_len.push(seg0_len);

            while chain_edges.len() < 8 && *cum_len.last().unwrap() < 1_850_000.0 {
                let cur = *chain_nodes.last().unwrap();
                let part = &graph.particles[cur];
                if !matches!(part.anchor, Anchor::Free)
                    || part.locked
                    || part.via_tether.is_some()
                    || tethered_targets[cur]
                    || inc[cur].len() != 2
                {
                    break;
                }
                let prev_e = *chain_edges.last().unwrap();
                let next_e = if inc[cur][0] == prev_e { inc[cur][1] } else { inc[cur][0] };
                let ne = &graph.edges[next_e];
                if !ne.active
                    || ne.locked
                    || ne.layer != l_fwd
                    || ne.net != vnet
                    || ne.r != r_fwd
                {
                    break;
                }
                let nxt = if ne.u == cur { ne.v } else { ne.u };
                if chain_nodes.contains(&nxt) {
                    break;
                }
                let d = (graph.particles[nxt].pos.0 - part.pos.0)
                    .hypot(graph.particles[nxt].pos.1 - part.pos.1);
                if d < 1_000.0 {
                    break;
                }
                chain_edges.push(next_e);
                chain_nodes.push(nxt);
                cum_len.push(*cum_len.last().unwrap() + d);
            }

            let total_chain = *cum_len.last().unwrap();
            if total_chain < 60_000.0 {
                continue;
            }
            let other_eis: Vec<usize> =
                pv_edges.iter().copied().filter(|&ei| ei != e0).collect();

            // Mode A: direct slide along `chain_edges` while stretching `other_eis` to `cand`.
            for &s_arc in &ARC_STEPS_NM {
                if s_arc >= total_chain * 0.94 {
                    break;
                }
                let mut m = 0usize;
                while m + 1 < chain_edges.len() && cum_len[m + 1] <= s_arc {
                    m += 1;
                }
                let seg_d = (cum_len[m + 1] - cum_len[m]).max(1.0);
                let t_seg = ((s_arc - cum_len[m]) / seg_d).clamp(0.05, 0.92);
                let pa = graph.particles[chain_nodes[m]].pos;
                let pb = graph.particles[chain_nodes[m + 1]].pos;
                let base = (pa.0 + (pb.0 - pa.0) * t_seg, pa.1 + (pb.1 - pa.1) * t_seg);
                let (nx, ny) = seg_normal(pa, pb);
                let end_node = chain_nodes[m + 1];
                let end_pt = Point::new(pb.0.round() as Nm, pb.1.round() as Nm);

                for &lat in &LAT_A_NM {
                    let cand = Point::new(
                        (base.0 + nx * lat).round() as Nm,
                        (base.1 + ny * lat).round() as Nm,
                    );
                    if cand == old_pt || cand == end_pt || !in_plane_zones(cand, board, graph) {
                        continue;
                    }
                    if creates_acute_at(
                        graph,
                        &inc[end_node],
                        chain_edges[m],
                        end_node,
                        end_pt,
                        cand,
                        l_fwd,
                    ) {
                        continue;
                    }

                    let mut arms_at_cand: Vec<(Point, usize)> = vec![(end_pt, l_fwd)];
                    let mut geom_ok = true;
                    for &oei in &other_eis {
                        let oe = &graph.edges[oei];
                        let onb = if oe.u == pv { oe.v } else { oe.u };
                        if onb == end_node && oe.layer == l_fwd {
                            geom_ok = false;
                            break;
                        }
                        let opt = Point::new(
                            graph.particles[onb].pos.0.round() as Nm,
                            graph.particles[onb].pos.1.round() as Nm,
                        );
                        if opt == cand
                            || creates_acute_at(graph, &inc[onb], oei, onb, opt, cand, oe.layer)
                        {
                            geom_ok = false;
                            break;
                        }
                        arms_at_cand.push((opt, oe.layer));
                    }
                    if !geom_ok {
                        continue;
                    }
                    for i in 0..arms_at_cand.len() {
                        for j in (i + 1)..arms_at_cand.len() {
                            if arms_at_cand[i].1 == arms_at_cand[j].1
                                && cos_at_vertex(cand, arms_at_cand[i].0, arms_at_cand[j].0) > 0.008
                            {
                                geom_ok = false;
                                break;
                            }
                        }
                    }
                    if !geom_ok {
                        continue;
                    }

                    let mut removed: Vec<(ItemId, Item)> = Vec::new();
                    if let Some(it) = board.remove(vid) {
                        removed.push((vid, it));
                    }
                    for &cei in &chain_edges[..=m] {
                        if let Some(eid) = edge_ids[cei] {
                            if let Some(it) = board.remove(eid) {
                                removed.push((eid, it));
                            }
                        }
                    }
                    for &oei in &other_eis {
                        if let Some(eid) = edge_ids[oei] {
                            if let Some(it) = board.remove(eid) {
                                removed.push((eid, it));
                            }
                        }
                    }

                    let mut inserted: Vec<ItemId> = Vec::new();
                    inserted.push(board.insert(Item {
                        kind: Kind::Via,
                        net: vnet,
                        layers: vlayers,
                        shape: Shape::Capsule { a: cand, b: cand, r: vr },
                        hole: Some((cand, vdr)),
                        locked: false,
                        span: None,
                    }));
                    inserted.push(board.insert(Item {
                        kind: Kind::Track,
                        net: vnet,
                        layers: 1u64 << l_fwd,
                        shape: Shape::Capsule { a: cand, b: end_pt, r: r_fwd },
                        hole: None,
                        locked: false,
                        span: None,
                    }));
                    for &oei in &other_eis {
                        let oe = &graph.edges[oei];
                        let onb = if oe.u == pv { oe.v } else { oe.u };
                        let opt = Point::new(
                            graph.particles[onb].pos.0.round() as Nm,
                            graph.particles[onb].pos.1.round() as Nm,
                        );
                        inserted.push(board.insert(Item {
                            kind: Kind::Track,
                            net: vnet,
                            layers: 1u64 << oe.layer,
                            shape: Shape::Capsule { a: opt, b: cand, r: oe.r },
                            hole: None,
                            locked: false,
                            span: None,
                        }));
                    }

                    let all_clean = inserted.iter().all(|&tid| check_item(board, tid).is_empty());
                    for tid in inserted {
                        board.discard(tid);
                    }
                    for (rid, rit) in removed {
                        board.restore(rid, rit);
                    }

                    if all_clean {
                        graph.particles[pv].pos = (cand.x as f64, cand.y as f64);
                        graph.edges[chain_edges[0]].u = pv;
                        graph.edges[chain_edges[0]].v = end_node;
                        for &cei in &chain_edges[1..=m] {
                            graph.edges[cei].active = false;
                        }
                        moved_count += 1;
                        slid = true;
                        break 'chain_loop;
                    }
                }
            }

            // Mode B: dogleg layer-transition slide when `other_eis` lie on one layer `l_other != l_fwd`.
            if !other_eis.is_empty() && other_eis.iter().all(|&oei| graph.edges[oei].layer != l_fwd)
            {
                let l_other = graph.edges[other_eis[0]].layer;
                if other_eis.iter().all(|&oei| graph.edges[oei].layer == l_other) {
                    let r_bridge = other_eis.iter().map(|&oei| graph.edges[oei].r).max().unwrap();
                    for &s_arc in &ARC_STEPS_NM {
                        if s_arc >= total_chain * 0.94 {
                            break;
                        }
                        let mut m = 0usize;
                        while m + 1 < chain_edges.len() && cum_len[m + 1] <= s_arc {
                            m += 1;
                        }
                        let seg_d = (cum_len[m + 1] - cum_len[m]).max(1.0);
                        let t_seg = ((s_arc - cum_len[m]) / seg_d).clamp(0.08, 0.92);
                        let pa = graph.particles[chain_nodes[m]].pos;
                        let pb = graph.particles[chain_nodes[m + 1]].pos;
                        let base = (pa.0 + (pb.0 - pa.0) * t_seg, pa.1 + (pb.1 - pa.1) * t_seg);
                        let (nx, ny) = seg_normal(pa, pb);
                        let end_node = chain_nodes[m + 1];
                        let end_pt = Point::new(pb.0.round() as Nm, pb.1.round() as Nm);

                        for &lat in &LAT_B_NM {
                            let cand = Point::new(
                                (base.0 + nx * lat).round() as Nm,
                                (base.1 + ny * lat).round() as Nm,
                            );
                            if cand == old_pt
                                || cand == end_pt
                                || !in_plane_zones(cand, board, graph)
                                || creates_acute_at(
                                    graph,
                                    &inc[end_node],
                                    chain_edges[m],
                                    end_node,
                                    end_pt,
                                    cand,
                                    l_fwd,
                                )
                            {
                                continue;
                            }

                            for &(wx, wy) in &W_OFFS {
                                let w_pt = Point::new(old_pt.x + wx, old_pt.y + wy);
                                if w_pt == cand || w_pt.distance(cand) < 80_000.0 {
                                    continue;
                                }
                                let mut w_arms: Vec<Point> = vec![cand];
                                let mut geom_ok = true;
                                for &oei in &other_eis {
                                    let oe = &graph.edges[oei];
                                    let onb = if oe.u == pv { oe.v } else { oe.u };
                                    let opt = Point::new(
                                        graph.particles[onb].pos.0.round() as Nm,
                                        graph.particles[onb].pos.1.round() as Nm,
                                    );
                                    if opt == w_pt
                                        || creates_acute_at(
                                            graph, &inc[onb], oei, onb, opt, w_pt, l_other,
                                        )
                                    {
                                        geom_ok = false;
                                        break;
                                    }
                                    w_arms.push(opt);
                                }
                                if !geom_ok {
                                    continue;
                                }
                                for i in 0..w_arms.len() {
                                    for j in (i + 1)..w_arms.len() {
                                        if cos_at_vertex(w_pt, w_arms[i], w_arms[j]) > 0.008 {
                                            geom_ok = false;
                                            break;
                                        }
                                    }
                                }
                                if !geom_ok {
                                    continue;
                                }

                                let mut removed: Vec<(ItemId, Item)> = Vec::new();
                                if let Some(it) = board.remove(vid) {
                                    removed.push((vid, it));
                                }
                                for &cei in &chain_edges[..=m] {
                                    if let Some(eid) = edge_ids[cei] {
                                        if let Some(it) = board.remove(eid) {
                                            removed.push((eid, it));
                                        }
                                    }
                                }
                                for &oei in &other_eis {
                                    if let Some(eid) = edge_ids[oei] {
                                        if let Some(it) = board.remove(eid) {
                                            removed.push((eid, it));
                                        }
                                    }
                                }

                                let mut inserted: Vec<ItemId> = Vec::new();
                                inserted.push(board.insert(Item {
                                    kind: Kind::Via,
                                    net: vnet,
                                    layers: vlayers,
                                    shape: Shape::Capsule { a: cand, b: cand, r: vr },
                                    hole: Some((cand, vdr)),
                                    locked: false,
                                    span: None,
                                }));
                                inserted.push(board.insert(Item {
                                    kind: Kind::Track,
                                    net: vnet,
                                    layers: 1u64 << l_fwd,
                                    shape: Shape::Capsule { a: cand, b: end_pt, r: r_fwd },
                                    hole: None,
                                    locked: false,
                                    span: None,
                                }));
                                inserted.push(board.insert(Item {
                                    kind: Kind::Track,
                                    net: vnet,
                                    layers: 1u64 << l_other,
                                    shape: Shape::Capsule { a: w_pt, b: cand, r: r_bridge },
                                    hole: None,
                                    locked: false,
                                    span: None,
                                }));
                                for &oei in &other_eis {
                                    let oe = &graph.edges[oei];
                                    let onb = if oe.u == pv { oe.v } else { oe.u };
                                    let opt = Point::new(
                                        graph.particles[onb].pos.0.round() as Nm,
                                        graph.particles[onb].pos.1.round() as Nm,
                                    );
                                    inserted.push(board.insert(Item {
                                        kind: Kind::Track,
                                        net: vnet,
                                        layers: 1u64 << l_other,
                                        shape: Shape::Capsule { a: opt, b: w_pt, r: oe.r },
                                        hole: None,
                                        locked: false,
                                        span: None,
                                    }));
                                }

                                let all_clean =
                                    inserted.iter().all(|&tid| check_item(board, tid).is_empty());
                                for tid in inserted {
                                    board.discard(tid);
                                }
                                for (rid, rit) in removed {
                                    board.restore(rid, rit);
                                }

                                if all_clean {
                                    let w_id = graph.particles.len();
                                    graph.particles.push(Particle {
                                        pos: (w_pt.x as f64, w_pt.y as f64),
                                        anchor: Anchor::Free,
                                        locked: false,
                                        via_tether: None,
                                        plane_zones: Vec::new(),
                                    });
                                    graph.particles[pv].pos = (cand.x as f64, cand.y as f64);
                                    graph.edges[chain_edges[0]].u = pv;
                                    graph.edges[chain_edges[0]].v = end_node;
                                    for &cei in &chain_edges[1..=m] {
                                        graph.edges[cei].active = false;
                                    }
                                    for &oei in &other_eis {
                                        if graph.edges[oei].u == pv {
                                            graph.edges[oei].u = w_id;
                                        } else {
                                            graph.edges[oei].v = w_id;
                                        }
                                    }
                                    graph.edges.push(Edge {
                                        u: w_id,
                                        v: pv,
                                        layer: l_other,
                                        net: vnet,
                                        r: r_bridge,
                                        locked: false,
                                        active: true,
                                    });
                                    moved_count += 1;
                                    slid = true;
                                    break 'chain_loop;
                                }
                            }
                        }
                    }
                }
            }
        }
        if slid {
            continue;
        }

        // 2. Mode C: radial escape when chain slides did not apply or were blocked.
        'radial_loop: for &rad in &RADIAL_NM {
            for k in 0..12 {
                let ang = (k as f64) * (std::f64::consts::TAU / 12.0);
                let cand = Point::new(
                    (old_pt.x as f64 + rad * ang.cos()).round() as Nm,
                    (old_pt.y as f64 + rad * ang.sin()).round() as Nm,
                );
                if cand == old_pt || !in_plane_zones(cand, board, graph) {
                    continue;
                }
                let mut arms_at_cand: Vec<(Point, usize)> = Vec::new();
                let mut geom_ok = true;
                for &oei in &pv_edges {
                    let oe = &graph.edges[oei];
                    let onb = if oe.u == pv { oe.v } else { oe.u };
                    let opt = Point::new(
                        graph.particles[onb].pos.0.round() as Nm,
                        graph.particles[onb].pos.1.round() as Nm,
                    );
                    if opt == cand
                        || creates_acute_at(graph, &inc[onb], oei, onb, opt, cand, oe.layer)
                    {
                        geom_ok = false;
                        break;
                    }
                    arms_at_cand.push((opt, oe.layer));
                }
                if !geom_ok {
                    continue;
                }
                for i in 0..arms_at_cand.len() {
                    for j in (i + 1)..arms_at_cand.len() {
                        if arms_at_cand[i].1 == arms_at_cand[j].1
                            && cos_at_vertex(cand, arms_at_cand[i].0, arms_at_cand[j].0) > 0.008
                        {
                            geom_ok = false;
                            break;
                        }
                    }
                }
                if !geom_ok {
                    continue;
                }

                let mut removed: Vec<(ItemId, Item)> = Vec::new();
                if let Some(it) = board.remove(vid) {
                    removed.push((vid, it));
                }
                for &oei in &pv_edges {
                    if let Some(eid) = edge_ids[oei] {
                        if let Some(it) = board.remove(eid) {
                            removed.push((eid, it));
                        }
                    }
                }
                let mut inserted: Vec<ItemId> = Vec::new();
                inserted.push(board.insert(Item {
                    kind: Kind::Via,
                    net: vnet,
                    layers: vlayers,
                    shape: Shape::Capsule { a: cand, b: cand, r: vr },
                    hole: Some((cand, vdr)),
                    locked: false,
                    span: None,
                }));
                for &oei in &pv_edges {
                    let oe = &graph.edges[oei];
                    let onb = if oe.u == pv { oe.v } else { oe.u };
                    let opt = Point::new(
                        graph.particles[onb].pos.0.round() as Nm,
                        graph.particles[onb].pos.1.round() as Nm,
                    );
                    inserted.push(board.insert(Item {
                        kind: Kind::Track,
                        net: vnet,
                        layers: 1u64 << oe.layer,
                        shape: Shape::Capsule { a: opt, b: cand, r: oe.r },
                        hole: None,
                        locked: false,
                        span: None,
                    }));
                }

                let all_clean = inserted.iter().all(|&tid| check_item(board, tid).is_empty());
                for tid in inserted {
                    board.discard(tid);
                }
                for (rid, rit) in removed {
                    board.restore(rid, rit);
                }
                if all_clean {
                    graph.particles[pv].pos = (cand.x as f64, cand.y as f64);
                    moved_count += 1;
                    break 'radial_loop;
                }
            }
        }
    }
    moved_count
}

fn settle_copper_only(graph: &mut CopperGraph, board: &mut Board) -> usize {
    let empty_at: [Point; 0] = [];
    let empty_locked: [bool; 0] = [];
    let empty_envs: [FootprintEnvelope; 0] = [];
    let empty_yards: [FootprintYards; 0] = [];
    let empty_allowed = HashMap::new();
    let empty_base = HashSet::new();
    relax_core(
        graph,
        board,
        &empty_at,
        &empty_at,
        &empty_locked,
        &empty_envs,
        &empty_yards,
        &empty_allowed,
        &empty_base,
        false,
        3,
    )
    .1
}

#[allow(clippy::too_many_arguments)]
fn relax_step(
    graph: &mut CopperGraph,
    board: &mut Board,
    orig_fp_at: &[Point],
    init_fp_pos: &[Point],
    fp_locked: &[bool],
    fp_envs: &[FootprintEnvelope],
    orig_yards: &[FootprintYards],
    allowed_yards: &HashMap<(usize, usize, u8), f64>,
    baseline_static: &HashSet<StaticViolKey>,
) -> (Vec<Point>, usize) {
    relax_core(
        graph,
        board,
        orig_fp_at,
        init_fp_pos,
        fp_locked,
        fp_envs,
        orig_yards,
        allowed_yards,
        baseline_static,
        true,
        6,
    )
}

#[allow(clippy::too_many_arguments)]
fn relax_core(
    graph: &mut CopperGraph,
    board: &mut Board,
    orig_fp_at: &[Point],
    init_fp_pos: &[Point],
    fp_locked: &[bool],
    fp_envs: &[FootprintEnvelope],
    orig_yards: &[FootprintYards],
    allowed_yards: &HashMap<(usize, usize, u8), f64>,
    baseline_static: &HashSet<StaticViolKey>,
    enable_fp_coupling: bool,
    num_passes: usize,
) -> (Vec<Point>, usize) {
    const FP_STEP_CAP_NM: f64 = 22_000.0;

    let num_layers = board.layers.len();
    let rule_reach = board.max_rule_reach();
    let min_clr = nm(board.rules.min_clearance).max(100_000) as f64;
    let outer_mask = if num_layers >= 2 {
        1u64 | (1u64 << (num_layers - 1))
    } else {
        1u64
    };

    let n_fp = orig_fp_at.len();
    let mut cur_fp_pos = init_fp_pos.to_vec();
    let (leader, members) = if enable_fp_coupling && n_fp > 0 {
        fp_family_groups(n_fp, fp_envs, fp_locked)
    } else {
        (Vec::new(), Vec::new())
    };

    let mut fp_has_locked_copper = vec![false; n_fp];
    if enable_fp_coupling && n_fp > 0 {
        let mut p_locked_edge = vec![false; graph.particles.len()];
        for e in &graph.edges {
            if e.active && e.locked {
                p_locked_edge[e.u] = true;
                p_locked_edge[e.v] = true;
            }
        }
        for (pi, p) in graph.particles.iter().enumerate() {
            let fp_opt = match p.anchor {
                Anchor::Pad { footprint, .. } | Anchor::PadVia { footprint, .. } => Some(footprint),
                _ => None,
            };
            if let Some(f) = fp_opt {
                if f < n_fp
                    && (p.locked
                        || p_locked_edge[pi]
                        || matches!(p.anchor, Anchor::PadVia { locked: true, .. }))
                {
                    fp_has_locked_copper[f] = true;
                }
            }
        }
    }

    let group_immob: Vec<bool> = if enable_fp_coupling && n_fp > 0 {
        (0..n_fp)
            .map(|g| {
                members[g].is_empty()
                    || members[g].iter().any(|&m| {
                        fp_locked[m]
                            || fp_envs[m].owns_outline
                            || fp_envs[m].has_copper_graphic
                            || fp_has_locked_copper[m]
                    })
            })
            .collect()
    } else {
        Vec::new()
    };

    let pad_ords = if enable_fp_coupling && n_fp > 0 {
        pad_ordinals_on_board(board)
    } else {
        HashMap::new()
    };
    let mut fp_pads: Vec<Vec<ItemId>> = vec![Vec::new(); n_fp];
    for (&id, &(f, _)) in &pad_ords {
        fp_pads[f].push(id);
    }
    for ps in &mut fp_pads {
        ps.sort_unstable();
    }

    let mut via_slides = 0usize;

    for outer_pass in 0..num_passes {
        let (edge_item_ids, via_item_ids) = graph.sync_to_board(board);
        if outer_pass >= 1 && count_dynamic_violations(board).0 == 0 {
            break;
        }
        let allow_fp_move = enable_fp_coupling && n_fp > 0 && outer_pass >= 1;
        let inner_iters = if outer_pass == 0 {
            32
        } else if outer_pass >= 3 {
            38
        } else {
            28
        };
        let spring_iters = if outer_pass == 0 { 16 } else { 13 };

        for iter in 0..inner_iters {
            let np = graph.particles.len();
            let spring_phase = if iter < spring_iters {
                1.0 - (iter as f64 / spring_iters as f64)
            } else {
                0.0
            };

            // 1. Constant-tension spring pull + tangential spacing + acute-angle opening during
            //    the first `spring_iters` iterations; 0.0 during the pure-clearance tail phase.
            if spring_phase > 0.0 {
                let mut adj: Vec<Vec<(usize, usize)>> = vec![Vec::new(); np];
                for (ei, e) in graph.edges.iter().enumerate() {
                    if e.active {
                        adj[e.u].push((e.v, ei));
                        adj[e.v].push((e.u, ei));
                    }
                }
                let mut spring_dx = vec![(0.0f64, 0.0f64); np];

                for i in 0..np {
                    let mob = graph.particles[i].mobility();
                    // Vias ride the smooth harmonic warp field and clearance repulsion rather than
                    // spring tension, so dogbone fanout vias are not pulled between fine-pitch pads.
                    if mob <= 0.0 || graph.particles[i].is_via().is_some() || adj[i].len() < 2 {
                        continue;
                    }
                    let pi = graph.particles[i].pos;
                    let (mut unit_x, mut unit_y) = (0.0, 0.0);
                    for &(j, _) in &adj[i] {
                        let pj = graph.particles[j].pos;
                        let (dx, dy) = (pj.0 - pi.0, pj.1 - pi.1);
                        let d = dx.hypot(dy).max(10_000.0);
                        unit_x += dx / d;
                        unit_y += dy / d;
                    }
                    let pull = TENSION_STEP_NM * spring_phase;
                    spring_dx[i].0 += unit_x * pull;
                    spring_dx[i].1 += unit_y * pull;

                    if adj[i].len() == 2 && graph.particles[i].is_via().is_none() {
                        let (j1, e1) = adj[i][0];
                        let (j2, e2) = adj[i][1];
                        if graph.edges[e1].layer == graph.edges[e2].layer {
                            let (p1, p2) = (graph.particles[j1].pos, graph.particles[j2].pos);
                            let (cx, cy) = (p2.0 - p1.0, p2.1 - p1.1);
                            let clen = cx.hypot(cy);
                            if clen > 10_000.0 {
                                let (tx, ty) = (cx / clen, cy / clen);
                                let mid_err_x = (p1.0 + p2.0) * 0.5 - pi.0;
                                let mid_err_y = (p1.1 + p2.1) * 0.5 - pi.1;
                                let along = (mid_err_x * tx + mid_err_y * ty) * 0.15 * spring_phase;
                                spring_dx[i].0 += tx * along;
                                spring_dx[i].1 += ty * along;
                            }
                        }
                    }

                    if adj[i].len() <= 4 {
                        for a1 in 0..adj[i].len() {
                            for a2 in (a1 + 1)..adj[i].len() {
                                let (j, e1) = adj[i][a1];
                                let (k, e2) = adj[i][a2];
                                if graph.edges[e1].layer != graph.edges[e2].layer {
                                    continue;
                                }
                                let vj = (
                                    graph.particles[j].pos.0 - pi.0,
                                    graph.particles[j].pos.1 - pi.1,
                                );
                                let vk = (
                                    graph.particles[k].pos.0 - pi.0,
                                    graph.particles[k].pos.1 - pi.1,
                                );
                                let (lj, lk) = (vj.0.hypot(vj.1), vk.0.hypot(vk.1));
                                if lj < 10_000.0 || lk < 10_000.0 {
                                    continue;
                                }
                                let cos_ang = (vj.0 * vk.0 + vj.1 * vk.1) / (lj * lk);
                                if cos_ang > -0.05 {
                                    let bx = (vj.0 / lj + vk.0 / lk) * 0.5;
                                    let by = (vj.1 / lj + vk.1 / lk) * 0.5;
                                    let push =
                                        (0.18 * lj.min(lk).min(35_000.0) * (cos_ang + 0.05))
                                            .min(6_000.0)
                                            * spring_phase;
                                    spring_dx[i].0 += bx * push * mob;
                                    spring_dx[i].1 += by * push * mob;
                                }
                            }
                        }
                    }
                }

                for (i, &sdx) in spring_dx.iter().enumerate() {
                    let (dx, dy) = clamp_vec(sdx, TENSION_STEP_NM * 2.2);
                    graph.particles[i].pos.0 += dx;
                    graph.particles[i].pos.1 += dy;
                }
            }

            // 2. Build dynamic spatial grid for edges & vias.
            let mut dyn_edge_grid: HashMap<(usize, i64, i64), Vec<usize>> = HashMap::new();
            for (ei, e) in graph.edges.iter().enumerate() {
                if !e.active {
                    continue;
                }
                let bb = seg_bbox(graph.particles[e.u].pos, graph.particles[e.v].pos, e.r);
                for (cx, cy) in grid_cells(bb) {
                    dyn_edge_grid.entry((e.layer, cx, cy)).or_default().push(ei);
                }
            }
            let mut dyn_via_grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
            for (pi, p) in graph.particles.iter().enumerate() {
                if let Some((r, dr, _, _, _)) = p.is_via() {
                    let rad = r.max(dr);
                    let bb = seg_bbox(p.pos, p.pos, rad);
                    for (cx, cy) in grid_cells(bb) {
                        dyn_via_grid.entry((cx, cy)).or_default().push(pi);
                    }
                }
            }

            // 3. Collect violated clearance half-planes per particle and per mobile footprint family.
            let mut constraints: Vec<Vec<HalfPlane>> = vec![Vec::new(); np];
            let mut hs_fp_group: Vec<Vec<HalfPlane>> = vec![Vec::new(); n_fp];
            let can_split = outer_pass < 3 && iter == spring_iters;
            let mut edge_splits: HashMap<usize, SplitCand> = HashMap::new();

            let item_fp_leader = |oit: &Item| -> Option<usize> {
                if !allow_fp_move {
                    return None;
                }
                let f = match oit.kind {
                    Kind::Pad { footprint, .. } | Kind::Spoke { footprint, .. } => footprint,
                    Kind::Keepout { footprint: Some(footprint), .. } => footprint,
                    _ => return None,
                };
                (f < n_fp).then(|| leader[f]).filter(|&lg| !group_immob[lg])
            };

            let particle_fp_leader = |pid: usize, g: &CopperGraph| -> Option<usize> {
                if !allow_fp_move {
                    return None;
                }
                let f = match g.particles[pid].anchor {
                    Anchor::Pad { footprint, .. } | Anchor::PadVia { footprint, .. } => footprint,
                    _ => return None,
                };
                (f < n_fp).then(|| leader[f]).filter(|&lg| !group_immob[lg])
            };

            let record_edge_push = |ei: usize,
                                    u: usize,
                                    v: usize,
                                    t: f64,
                                    nx: f64,
                                    ny: f64,
                                    deficit: f64,
                                    obs_lg: Option<usize>,
                                    constraints: &mut [Vec<HalfPlane>],
                                    hs_fp: &mut [Vec<HalfPlane>],
                                    splits: &mut HashMap<usize, SplitCand>| {
                if deficit <= 0.0 {
                    return;
                }
                let (mu, mv) = (graph.particles[u].mobility(), graph.particles[v].mobility());
                let p0 = graph.particles[u].pos;
                let p1 = graph.particles[v].pos;
                let seg_len = (p1.0 - p0.0).hypot(p1.1 - p0.1);

                if can_split
                    && !graph.edges[ei].locked
                    && seg_len >= MIN_SPLIT_LEN
                    && (0.16..=0.84).contains(&t)
                    && deficit > 1_500.0
                {
                    let prev = splits.get(&ei).map_or(0.0, |s| s.0);
                    if deficit > prev {
                        splits.insert(ei, (deficit, t.clamp(0.25, 0.75), nx, ny));
                    }
                }

                let denom = (1.0 - t) * (1.0 - t) * mu + t * t * mv;
                if denom > 1e-6 {
                    if mu > 0.0 {
                        let scale = (((1.0 - t) * mu) / denom).min(2.4);
                        constraints[u].push(HalfPlane { nx, ny, need: deficit * scale });
                    }
                    if mv > 0.0 {
                        let scale = ((t * mv) / denom).min(2.4);
                        constraints[v].push(HalfPlane { nx, ny, need: deficit * scale });
                    }
                } else if mu > 0.0 {
                    constraints[u].push(HalfPlane { nx, ny, need: deficit * 1.5 });
                } else if mv > 0.0 {
                    constraints[v].push(HalfPlane { nx, ny, need: deficit * 1.5 });
                }

                if allow_fp_move {
                    let lu = particle_fp_leader(u, graph);
                    let lv = particle_fp_leader(v, graph);
                    if let Some(lg) = obs_lg {
                        if Some(lg) != lu && Some(lg) != lv {
                            let share = if mu <= 0.0 && mv <= 0.0 { 0.80 } else { 0.50 };
                            hs_fp[lg].push(HalfPlane {
                                nx: -nx,
                                ny: -ny,
                                need: deficit * share,
                            });
                        }
                    }
                    if let Some(gu) = lu {
                        if Some(gu) != obs_lg && (mv <= 0.0 || t < 0.35) {
                            hs_fp[gu].push(HalfPlane {
                                nx,
                                ny,
                                need: deficit * (1.0 - t).max(0.35) * 0.55,
                            });
                        }
                    }
                    if let Some(gv) = lv {
                        if Some(gv) != obs_lg && (mu <= 0.0 || t > 0.65) {
                            hs_fp[gv].push(HalfPlane {
                                nx,
                                ny,
                                need: deficit * t.max(0.35) * 0.55,
                            });
                        }
                    }
                }
            };

            let is_foreign_mask = |mnet: u32, net: u32, self_id: Option<ItemId>| -> bool {
                if mnet < ANY_ONE_NET {
                    mnet == 0 || mnet != net
                } else {
                    match board.mask_holder(mnet, true, &|other| Some(other) == self_id) {
                        Some(holder) => holder == 0 || holder != net,
                        None => false,
                    }
                }
            };

            let holes_slot = num_layers;

            // 3a. Via constraints against static board + other dynamic vias.
            for (pi, p) in graph.particles.iter().enumerate() {
                let Some((vr, vdr, vlayers, vnet, _)) = p.is_via() else { continue };
                let mob_i = p.mobility();
                let pos_i = p.pos;
                let v_id = via_item_ids.get(pi).and_then(|x| *x);
                let v_item = v_id.and_then(|id| board.item(id));
                let host_lg = particle_fp_leader(pi, graph);

                // Keep unlocked via-in-pad inside its host pad's copper polygon.
                if let Anchor::PadVia { ref cur_pad, init_margin, locked: false, .. } = p.anchor {
                    let (_, _, _, gap, nx, ny) = closest_seg_shape(pos_i, pos_i, vr, cur_pad);
                    let dist_in = -gap - vr as f64;
                    if dist_in < init_margin {
                        constraints[pi].push(HalfPlane {
                            nx: -nx,
                            ny: -ny,
                            need: init_margin - dist_in,
                        });
                    }
                }

                // Keep plane-stitching vias inside their initial plane polygon.
                if mob_i > 0.0 && !p.plane_zones.is_empty() {
                    for &(z, init_margin) in &p.plane_zones {
                        if let Some((_, _, _, zshape)) = board.unfilled.iter().find(|u| u.0 == z) {
                            let (_, _, _, gap, nx, ny) =
                                closest_seg_shape(pos_i, pos_i, vr, zshape);
                            let dist_in = -gap - vr as f64;
                            if dist_in < init_margin {
                                constraints[pi].push(HalfPlane {
                                    nx: -nx,
                                    ny: -ny,
                                    need: init_margin - dist_in,
                                });
                            }
                        }
                    }
                }

                if mob_i > 0.0 || allow_fp_move {
                    // Board edges.
                    if mob_i > 0.0 || host_lg.is_some() {
                        let query_edge = seg_bbox(pos_i, pos_i, vr + rule_reach);
                        for &(ea, eb, ebb, eclr) in &board.outline_edges {
                            if !ebb.overlaps(query_edge) {
                                continue;
                            }
                            let eshape = Shape::Capsule { a: ea, b: eb, r: 0 };
                            let (_, _, _, gap, nx, ny) =
                                closest_seg_shape(pos_i, pos_i, vr, &eshape);
                            let req = eclr as f64 + CLEARANCE_BUFFER;
                            if gap < req {
                                let def = req - gap;
                                if mob_i > 0.0 {
                                    constraints[pi].push(HalfPlane { nx, ny, need: def });
                                }
                                if let Some(hlg) = host_lg {
                                    hs_fp_group[hlg].push(HalfPlane { nx, ny, need: def * 0.65 });
                                }
                            }
                        }
                    }

                    // Mask areas.
                    if mob_i > 0.0 {
                        if let Some(vit) = v_item {
                            for (mshape, mlayers, mnet) in &board.mask_areas {
                                if mlayers & vlayers != 0 && is_foreign_mask(*mnet, vnet, v_id) {
                                    let req =
                                        (board.mask_margin() + board.via_opening_on(vit, *mlayers))
                                            as f64
                                            + CLEARANCE_BUFFER;
                                    if mshape
                                        .bbox()
                                        .grow(req as Nm + vr)
                                        .overlaps(seg_bbox(pos_i, pos_i, 0))
                                    {
                                        let (_, _, _, gap, nx, ny) =
                                            closest_seg_shape(pos_i, pos_i, vr, mshape);
                                        if gap < req {
                                            constraints[pi].push(HalfPlane {
                                                nx,
                                                ny,
                                                need: req - gap,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Static copper, keepouts, spokes on shared layers.
                    let qbb = seg_bbox(pos_i, pos_i, vr.max(vdr) + rule_reach);
                    for l in (0..num_layers).filter(|&l| vlayers & (1u64 << l) != 0) {
                        for oid in board.near(l, qbb) {
                            let Some(oit) = board.item(oid) else { continue };
                            if matches!(oit.kind, Kind::Track | Kind::Via)
                                || !oit.on(l)
                                || oit.refilled()
                            {
                                continue;
                            }
                            let obs_lg = item_fp_leader(oit);
                            if mob_i <= 0.0 && obs_lg.is_none() && host_lg.is_none() {
                                continue;
                            }
                            let push_via_and_fp =
                                |nx: f64,
                                 ny: f64,
                                 def: f64,
                                 cs: &mut [Vec<HalfPlane>],
                                 hs_fp: &mut [Vec<HalfPlane>]| {
                                    if def <= 0.0 {
                                        return;
                                    }
                                    if mob_i > 0.0 {
                                        let v_scale = if obs_lg.is_some() { 0.80 } else { 1.0 };
                                        cs[pi].push(HalfPlane { nx, ny, need: def * v_scale });
                                    }
                                    if let Some(lg) = obs_lg {
                                        if Some(lg) != host_lg {
                                            let f_scale = if mob_i > 0.0 { 0.60 } else { 1.0 };
                                            hs_fp[lg].push(HalfPlane {
                                                nx: -nx,
                                                ny: -ny,
                                                need: def * f_scale,
                                            });
                                        }
                                    }
                                    if let Some(hlg) = host_lg {
                                        if Some(hlg) != obs_lg {
                                            hs_fp[hlg].push(HalfPlane { nx, ny, need: def * 0.50 });
                                        }
                                    }
                                };

                            match &oit.kind {
                                Kind::Keepout { forbids, .. } if forbids.vias => {
                                    let in_hole = board.keepout_holes.get(&oid).is_some_and(|hs| {
                                        hs.iter().any(|h| {
                                            h.contains(Point::new(
                                                pos_i.0.round() as Nm,
                                                pos_i.1.round() as Nm,
                                            ))
                                        })
                                    });
                                    if !in_hole {
                                        let (_, _, _, gap, nx, ny) =
                                            closest_seg_shape(pos_i, pos_i, vr, &oit.shape);
                                        if gap < CLEARANCE_BUFFER {
                                            push_via_and_fp(
                                                nx,
                                                ny,
                                                CLEARANCE_BUFFER - gap,
                                                &mut constraints,
                                                &mut hs_fp_group,
                                            );
                                        }
                                    }
                                }
                                Kind::Spoke { clearance, spare, .. } if oit.net != vnet => {
                                    let base_req =
                                        (*clearance).max(board.clearance(vnet, oit.net)) as f64
                                            + CLEARANCE_BUFFER;
                                    let (_, _, _, gap, nx, ny) =
                                        closest_seg_shape(pos_i, pos_i, vr, &oit.shape);
                                    if gap < base_req {
                                        let weight = if *spare == 0 { 1.0 } else { 0.35 };
                                        push_via_and_fp(
                                            nx,
                                            ny,
                                            (base_req - gap) * weight,
                                            &mut constraints,
                                            &mut hs_fp_group,
                                        );
                                    }
                                }
                                _ if oit.is_copper() => {
                                    if let (Some(vid), Some(vit)) = (v_id, v_item) {
                                        if (oit.net != vnet || vnet == 0)
                                            && !board.is_tied(vid, vit, oid, oit)
                                        {
                                            let req = board
                                                .clearance_kept(
                                                    &Thing::Item(board, vid, vit),
                                                    &Thing::Item(board, oid, oit),
                                                    l,
                                                )
                                                .max(mask_pair_gap(board, vid, vit, oid, oit, l))
                                                as f64
                                                + CLEARANCE_BUFFER;
                                            let (_, _, _, gap, nx, ny) =
                                                closest_seg_shape(pos_i, pos_i, vr, &oit.shape);
                                            if gap < req {
                                                push_via_and_fp(
                                                    nx,
                                                    ny,
                                                    req - gap,
                                                    &mut constraints,
                                                    &mut hs_fp_group,
                                                );
                                            }
                                        }
                                        // Check the via's drill hole against static copper `oit`
                                        // (including same-net pads under `physical_hole_clearance`,
                                        // except a `PadVia`'s own host pad).
                                        let own_host_pad = matches!(
                                            (&p.anchor, &oit.kind),
                                            (
                                                Anchor::PadVia { footprint: fh, cur_pad, .. },
                                                Kind::Pad { footprint: fo, .. }
                                            ) if fh == fo && cur_pad == &oit.shape
                                        );
                                        if !own_host_pad {
                                            let hc = board.hole_clearance(vid, vit, oid, oit);
                                            if hc > 0 {
                                                let hreq = hc as f64 + CLEARANCE_BUFFER;
                                                let (_, _, _, hgap, hnx, hny) = closest_seg_shape(
                                                    pos_i, pos_i, vdr, &oit.shape,
                                                );
                                                if (oit.net != vnet || vnet == 0 || hgap > 0.0)
                                                    && hgap < hreq
                                                {
                                                    push_via_and_fp(
                                                        hnx,
                                                        hny,
                                                        hreq - hgap,
                                                        &mut constraints,
                                                        &mut hs_fp_group,
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }

                    // Static holes vs via copper and via hole.
                    for oid in board.near(holes_slot, qbb) {
                        let Some(oit) = board.item(oid) else { continue };
                        if matches!(oit.kind, Kind::Via) {
                            continue;
                        }
                        let obs_lg = item_fp_leader(oit);
                        if mob_i <= 0.0 && obs_lg.is_none() && host_lg.is_none() {
                            continue;
                        }
                        let Some(hshape) = board.hole_shape(oid, oit) else { continue };
                        if let (Some(vid), Some(vit)) = (v_id, v_item) {
                            let h2h = board.hole_to_hole_clearance(vid, vit, oid, oit) as f64
                                + CLEARANCE_BUFFER;
                            let (_, _, _, gap_hh, nx_hh, ny_hh) =
                                closest_seg_shape(pos_i, pos_i, vdr, &hshape);
                            if gap_hh < h2h && gap_hh > -(vdr as f64) {
                                let def = h2h - gap_hh;
                                if mob_i > 0.0 {
                                    constraints[pi].push(HalfPlane {
                                        nx: nx_hh,
                                        ny: ny_hh,
                                        need: def * if obs_lg.is_some() { 0.80 } else { 1.0 },
                                    });
                                }
                                if let Some(lg) = obs_lg {
                                    if Some(lg) != host_lg {
                                        hs_fp_group[lg].push(HalfPlane {
                                            nx: -nx_hh,
                                            ny: -ny_hh,
                                            need: def * if mob_i > 0.0 { 0.60 } else { 1.0 },
                                        });
                                    }
                                }
                            }
                            let h_cu = board.hole_clearance(oid, oit, vid, vit);
                            if h_cu > 0 {
                                let req = h_cu as f64 + CLEARANCE_BUFFER;
                                let (_, _, _, gap_hc, nx_hc, ny_hc) =
                                    closest_seg_shape(pos_i, pos_i, vr, &hshape);
                                if gap_hc < req {
                                    let def = req - gap_hc;
                                    if mob_i > 0.0 {
                                        constraints[pi].push(HalfPlane {
                                            nx: nx_hc,
                                            ny: ny_hc,
                                            need: def * if obs_lg.is_some() { 0.80 } else { 1.0 },
                                        });
                                    }
                                    if let Some(lg) = obs_lg {
                                        if Some(lg) != host_lg {
                                            hs_fp_group[lg].push(HalfPlane {
                                                nx: -nx_hc,
                                                ny: -ny_hc,
                                                need: def * if mob_i > 0.0 { 0.60 } else { 1.0 },
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Via vs other dynamic vias (`pj > pi`).
                let qv = seg_bbox(pos_i, pos_i, vr.max(vdr) + rule_reach);
                let mut seen_v: Vec<usize> = Vec::new();
                for (cx, cy) in grid_cells(qv) {
                    if let Some(list) = dyn_via_grid.get(&(cx, cy)) {
                        for &pj in list {
                            if pj > pi {
                                seen_v.push(pj);
                            }
                        }
                    }
                }
                seen_v.sort_unstable();
                seen_v.dedup();
                for pj in seen_v {
                    let Some((vr2, vdr2, vlayers2, vnet2, _)) = graph.particles[pj].is_via() else {
                        continue;
                    };
                    let mob_j = graph.particles[pj].mobility();
                    let host_lg_j = particle_fp_leader(pj, graph);
                    if mob_i <= 0.0 && mob_j <= 0.0 && host_lg.is_none() && host_lg_j.is_none() {
                        continue;
                    }
                    let pos_j = graph.particles[pj].pos;
                    let (dx, dy) = (pos_i.0 - pos_j.0, pos_i.1 - pos_j.1);
                    let dist = dx.hypot(dy);
                    let (nx, ny) = if dist > 1.0 { (dx / dist, dy / dist) } else { (1.0, 0.0) };

                    let mut needed_centre_dist = 0.0f64;
                    if let (Some(id1), Some(it1), Some(id2), Some(it2)) = (
                        v_id,
                        v_item,
                        via_item_ids.get(pj).and_then(|x| *x),
                        via_item_ids.get(pj).and_then(|x| *x).and_then(|id| board.item(id)),
                    ) {
                        if dist > 1_000.0 {
                            let h2h = board.hole_to_hole_clearance(id1, it1, id2, it2) as f64
                                + CLEARANCE_BUFFER;
                            needed_centre_dist =
                                needed_centre_dist.max((vdr + vdr2) as f64 + h2h);
                        }
                        if vnet != vnet2 || vnet == 0 {
                            let shared = vlayers & vlayers2;
                            for l in (0..num_layers).filter(|&l| shared & (1u64 << l) != 0) {
                                let c = board
                                    .clearance_kept(
                                        &Thing::Item(board, id1, it1),
                                        &Thing::Item(board, id2, it2),
                                        l,
                                    )
                                    .max(mask_pair_gap(board, id1, it1, id2, it2, l))
                                    as f64
                                    + CLEARANCE_BUFFER;
                                needed_centre_dist =
                                    needed_centre_dist.max((vr + vr2) as f64 + c);
                            }
                        }
                        let hc1 = board.hole_clearance(id1, it1, id2, it2);
                        if hc1 > 0 {
                            needed_centre_dist = needed_centre_dist
                                .max((vdr + vr2) as f64 + hc1 as f64 + CLEARANCE_BUFFER);
                        }
                        let hc2 = board.hole_clearance(id2, it2, id1, it1);
                        if hc2 > 0 {
                            needed_centre_dist = needed_centre_dist
                                .max((vr + vdr2) as f64 + hc2 as f64 + CLEARANCE_BUFFER);
                        }
                    }
                    if dist < needed_centre_dist {
                        let def = needed_centre_dist - dist;
                        let sum_m = (mob_i + mob_j).max(1e-6);
                        if mob_i > 0.0 {
                            let share = if mob_j > 0.0 { mob_i / sum_m } else { 1.0 };
                            constraints[pi].push(HalfPlane { nx, ny, need: def * share });
                        }
                        if mob_j > 0.0 {
                            let share = if mob_i > 0.0 { mob_j / sum_m } else { 1.0 };
                            constraints[pj].push(HalfPlane { nx: -nx, ny: -ny, need: def * share });
                        }
                        if let Some(gi) = host_lg {
                            if Some(gi) != host_lg_j {
                                hs_fp_group[gi].push(HalfPlane { nx, ny, need: def * 0.55 });
                            }
                        }
                        if let Some(gj) = host_lg_j {
                            if Some(gj) != host_lg {
                                hs_fp_group[gj].push(HalfPlane {
                                    nx: -nx,
                                    ny: -ny,
                                    need: def * 0.55,
                                });
                            }
                        }
                    }
                }
            }

            // 3b. Edge constraints against static board, dynamic vias, and other dynamic edges.
            let num_edges = graph.edges.len();
            for ei in 0..num_edges {
                let e = &graph.edges[ei];
                if !e.active {
                    continue;
                }
                let (u, v, layer, net, r, e_locked) = (e.u, e.v, e.layer, e.net, e.r, e.locked);
                let (mu, mv) = (graph.particles[u].mobility(), graph.particles[v].mobility());
                let (p0, p1) = (graph.particles[u].pos, graph.particles[v].pos);
                let seg_len = (p1.0 - p0.0).hypot(p1.1 - p0.1);
                let splittable = !e_locked && seg_len >= MIN_SPLIT_LEN;
                let e_id = edge_item_ids.get(ei).and_then(|x| *x);
                let e_item = e_id.and_then(|id| board.item(id));
                let qbb = seg_bbox(p0, p1, r + rule_reach);

                if mu > 0.0 || mv > 0.0 || splittable || allow_fp_move {
                    // Board edges.
                    for &(ea, eb, ebb, eclr) in &board.outline_edges {
                        if !ebb.overlaps(qbb) {
                            continue;
                        }
                        let eshape = Shape::Capsule { a: ea, b: eb, r: 0 };
                        let (t, _, _, gap, nx, ny) = closest_seg_shape(p0, p1, r, &eshape);
                        let req = eclr as f64 + CLEARANCE_BUFFER;
                        if gap < req {
                            record_edge_push(
                                ei,
                                u,
                                v,
                                t,
                                nx,
                                ny,
                                req - gap,
                                None,
                                &mut constraints,
                                &mut hs_fp_group,
                                &mut edge_splits,
                            );
                        }
                    }

                    // Mask areas.
                    let lmask = 1u64 << layer;
                    for (mshape, mlayers, mnet) in &board.mask_areas {
                        if mlayers & lmask != 0 && is_foreign_mask(*mnet, net, e_id) {
                            let req = board.mask_margin() as f64 + CLEARANCE_BUFFER;
                            if mshape.bbox().grow(req as Nm + r).overlaps(seg_bbox(p0, p1, 0)) {
                                let (t, _, _, gap, nx, ny) = closest_seg_shape(p0, p1, r, mshape);
                                if gap < req {
                                    record_edge_push(
                                        ei,
                                        u,
                                        v,
                                        t,
                                        nx,
                                        ny,
                                        req - gap,
                                        None,
                                        &mut constraints,
                                        &mut hs_fp_group,
                                        &mut edge_splits,
                                    );
                                }
                            }
                        }
                    }

                    // Static items on `layer`.
                    for oid in board.near(layer, qbb) {
                        let Some(oit) = board.item(oid) else { continue };
                        if matches!(oit.kind, Kind::Track | Kind::Via)
                            || !oit.on(layer)
                            || oit.refilled()
                        {
                            continue;
                        }
                        let obs_lg = item_fp_leader(oit);
                        match &oit.kind {
                            Kind::Keepout { forbids, .. } if forbids.tracks => {
                                let mid_pt = Point::new(
                                    ((p0.0 + p1.0) * 0.5).round() as Nm,
                                    ((p0.1 + p1.1) * 0.5).round() as Nm,
                                );
                                let in_hole = board
                                    .keepout_holes
                                    .get(&oid)
                                    .is_some_and(|hs| hs.iter().any(|h| h.contains(mid_pt)));
                                if !in_hole {
                                    let (t, _, _, gap, nx, ny) =
                                        closest_seg_shape(p0, p1, r, &oit.shape);
                                    if gap < CLEARANCE_BUFFER {
                                        record_edge_push(
                                            ei,
                                            u,
                                            v,
                                            t,
                                            nx,
                                            ny,
                                            CLEARANCE_BUFFER - gap,
                                            obs_lg,
                                            &mut constraints,
                                            &mut hs_fp_group,
                                            &mut edge_splits,
                                        );
                                    }
                                }
                            }
                            Kind::Spoke { clearance, spare, .. } if oit.net != net => {
                                let req = (*clearance).max(board.clearance(net, oit.net)) as f64
                                    + CLEARANCE_BUFFER;
                                let (t, _, _, gap, nx, ny) =
                                    closest_seg_shape(p0, p1, r, &oit.shape);
                                if gap < req {
                                    let w = if *spare == 0 { 1.0 } else { 0.35 };
                                    record_edge_push(
                                        ei,
                                        u,
                                        v,
                                        t,
                                        nx,
                                        ny,
                                        (req - gap) * w,
                                        obs_lg,
                                        &mut constraints,
                                        &mut hs_fp_group,
                                        &mut edge_splits,
                                    );
                                }
                            }
                            _ if oit.is_copper() && (oit.net != net || net == 0) => {
                                let (req, tied_skip) = match (e_id, e_item) {
                                    (Some(eid), Some(eit)) => {
                                        if board.is_tied(eid, eit, oid, oit) {
                                            (0.0, true)
                                        } else {
                                            let c = board
                                                .clearance_kept(
                                                    &Thing::Item(board, eid, eit),
                                                    &Thing::Item(board, oid, oit),
                                                    layer,
                                                )
                                                .max(mask_pair_gap(board, eid, eit, oid, oit, layer));
                                            (c as f64 + CLEARANCE_BUFFER, false)
                                        }
                                    }
                                    _ => (
                                        board.clearance(net, oit.net) as f64 + CLEARANCE_BUFFER,
                                        false,
                                    ),
                                };
                                if tied_skip {
                                    continue;
                                }
                                let (t, _, _, gap, nx, ny) =
                                    closest_seg_shape(p0, p1, r, &oit.shape);
                                if gap < req {
                                    record_edge_push(
                                        ei,
                                        u,
                                        v,
                                        t,
                                        nx,
                                        ny,
                                        req - gap,
                                        obs_lg,
                                        &mut constraints,
                                        &mut hs_fp_group,
                                        &mut edge_splits,
                                    );
                                }
                            }
                            _ => {}
                        }
                    }

                    // Static holes vs track.
                    for oid in board.near(holes_slot, qbb) {
                        let Some(oit) = board.item(oid) else { continue };
                        if matches!(oit.kind, Kind::Via) {
                            continue;
                        }
                        let Some(hshape) = board.hole_shape(oid, oit) else { continue };
                        let req = match (e_id, e_item) {
                            (Some(eid), Some(eit)) => board.hole_clearance(oid, oit, eid, eit),
                            _ => nm(board.rules.min_hole_clearance),
                        };
                        if req <= 0 {
                            continue;
                        }
                        let obs_lg = item_fp_leader(oit);
                        let req_f = req as f64 + CLEARANCE_BUFFER;
                        let (t, _, _, gap, nx, ny) = closest_seg_shape(p0, p1, r, &hshape);
                        if gap < req_f {
                            record_edge_push(
                                ei,
                                u,
                                v,
                                t,
                                nx,
                                ny,
                                req_f - gap,
                                obs_lg,
                                &mut constraints,
                                &mut hs_fp_group,
                                &mut edge_splits,
                            );
                        }
                    }
                }

                // Dynamic vias vs edge `ei`.
                let mut near_vias: Vec<usize> = Vec::new();
                for (cx, cy) in grid_cells(qbb) {
                    if let Some(list) = dyn_via_grid.get(&(cx, cy)) {
                        near_vias.extend(list.iter().copied());
                    }
                }
                near_vias.sort_unstable();
                near_vias.dedup();
                for pv in near_vias {
                    if pv == u || pv == v {
                        continue;
                    }
                    let Some((vr, vdr, vlayers, vnet, _)) = graph.particles[pv].is_via() else {
                        continue;
                    };
                    if net == vnet && net != 0 {
                        continue;
                    }
                    let mv_via = graph.particles[pv].mobility();
                    let via_lg = particle_fp_leader(pv, graph);
                    if mu <= 0.0 && mv <= 0.0 && mv_via <= 0.0 && !splittable && !allow_fp_move {
                        continue;
                    }
                    let vpos = graph.particles[pv].pos;
                    let (t, qa, _, d_centre) = closest_seg_point(p0, p1, vpos);
                    let mut needed_centre = 0.0f64;
                    let vid = via_item_ids.get(pv).and_then(|x| *x);
                    let vit = vid.and_then(|id| board.item(id));

                    if vlayers & (1u64 << layer) != 0 {
                        let cu_req = match (e_id, e_item, vid, vit) {
                            (Some(eid), Some(eit), Some(vid), Some(vit)) => board
                                .clearance_kept(
                                    &Thing::Item(board, eid, eit),
                                    &Thing::Item(board, vid, vit),
                                    layer,
                                )
                                .max(mask_pair_gap(board, eid, eit, vid, vit, layer)),
                            _ => board.clearance(net, vnet),
                        } as f64
                            + CLEARANCE_BUFFER;
                        needed_centre = needed_centre.max((r + vr) as f64 + cu_req);
                    }
                    let h_req = match (e_id, e_item, vid, vit) {
                        (Some(eid), Some(eit), Some(vid), Some(vit)) => {
                            board.hole_clearance(vid, vit, eid, eit)
                        }
                        _ => nm(board.rules.min_hole_clearance),
                    } as f64
                        + CLEARANCE_BUFFER;
                    needed_centre = needed_centre.max((r + vdr) as f64 + h_req);

                    if d_centre < needed_centre {
                        let def = needed_centre - d_centre;
                        let (nx, ny) = if d_centre > 1.0 {
                            ((qa.0 - vpos.0) / d_centre, (qa.1 - vpos.1) / d_centre)
                        } else {
                            seg_normal(p0, p1)
                        };
                        let m_edge = (1.0 - t) * mu + t * mv;
                        let total_m = m_edge + mv_via;
                        if total_m > 1e-6 {
                            let edge_share = if mv_via > 0.0 { m_edge / total_m } else { 1.0 };
                            let via_share = if m_edge > 0.0 { mv_via / total_m } else { 1.0 };
                            if edge_share > 0.0 || splittable || allow_fp_move {
                                record_edge_push(
                                    ei,
                                    u,
                                    v,
                                    t,
                                    nx,
                                    ny,
                                    def * edge_share.max(0.55),
                                    via_lg,
                                    &mut constraints,
                                    &mut hs_fp_group,
                                    &mut edge_splits,
                                );
                            }
                            if mv_via > 0.0 {
                                constraints[pv].push(HalfPlane {
                                    nx: -nx,
                                    ny: -ny,
                                    need: def * via_share,
                                });
                            }
                        } else {
                            record_edge_push(
                                ei,
                                u,
                                v,
                                t,
                                nx,
                                ny,
                                def,
                                via_lg,
                                &mut constraints,
                                &mut hs_fp_group,
                                &mut edge_splits,
                            );
                        }
                    }
                }

                // Dynamic edges (`ej > ei`) on the same layer.
                let mut near_edges: Vec<usize> = Vec::new();
                for (cx, cy) in grid_cells(qbb) {
                    if let Some(list) = dyn_edge_grid.get(&(layer, cx, cy)) {
                        for &ej in list {
                            if ej > ei {
                                near_edges.push(ej);
                            }
                        }
                    }
                }
                near_edges.sort_unstable();
                near_edges.dedup();
                for ej in near_edges {
                    let e2 = &graph.edges[ej];
                    if !e2.active || e2.u == u || e2.u == v || e2.v == u || e2.v == v {
                        continue;
                    }
                    let (u2, v2, net2, r2, e2_locked) = (e2.u, e2.v, e2.net, e2.r, e2.locked);
                    let (mu2, mv2) =
                        (graph.particles[u2].mobility(), graph.particles[v2].mobility());
                    let (q0, q1) = (graph.particles[u2].pos, graph.particles[v2].pos);
                    let splittable2 = !e2_locked && (q1.0 - q0.0).hypot(q1.1 - q0.1) >= MIN_SPLIT_LEN;
                    if mu <= 0.0
                        && mv <= 0.0
                        && mu2 <= 0.0
                        && mv2 <= 0.0
                        && !splittable
                        && !splittable2
                        && !allow_fp_move
                    {
                        continue;
                    }
                    let (ta, tb, qa, qb, d_centre) = closest_seg_seg(p0, p1, q0, q1);
                    let gap = d_centre - (r + r2) as f64;

                    if net == net2 && net != 0 {
                        if spring_phase > 0.0
                            && (outer_mask & (1u64 << layer)) != 0
                            && gap > 20_000.0
                            && gap < min_clr
                        {
                            let soft_def = (min_clr - gap) * 0.14 * spring_phase;
                            if d_centre > 1.0 {
                                let (nx, ny) =
                                    ((qa.0 - qb.0) / d_centre, (qa.1 - qb.1) / d_centre);
                                if mu > 0.0 || mv > 0.0 {
                                    record_edge_push(
                                        ei,
                                        u,
                                        v,
                                        ta,
                                        nx,
                                        ny,
                                        soft_def * 0.5,
                                        None,
                                        &mut constraints,
                                        &mut hs_fp_group,
                                        &mut HashMap::new(),
                                    );
                                }
                                if mu2 > 0.0 || mv2 > 0.0 {
                                    record_edge_push(
                                        ej,
                                        u2,
                                        v2,
                                        tb,
                                        -nx,
                                        -ny,
                                        soft_def * 0.5,
                                        None,
                                        &mut constraints,
                                        &mut hs_fp_group,
                                        &mut HashMap::new(),
                                    );
                                }
                            }
                        }
                        continue;
                    }

                    let e2_id = edge_item_ids.get(ej).and_then(|x| *x);
                    let e2_item = e2_id.and_then(|id| board.item(id));
                    let req = match (e_id, e_item, e2_id, e2_item) {
                        (Some(id1), Some(it1), Some(id2), Some(it2)) => board.clearance_kept(
                            &Thing::Item(board, id1, it1),
                            &Thing::Item(board, id2, it2),
                            layer,
                        ),
                        _ => board.clearance(net, net2),
                    } as f64
                        + CLEARANCE_BUFFER;

                    if gap < req {
                        let def = req - gap;
                        let (nx, ny) = if d_centre > 1.0 {
                            ((qa.0 - qb.0) / d_centre, (qa.1 - qb.1) / d_centre)
                        } else {
                            seg_normal(p0, p1)
                        };
                        let m1 = (1.0 - ta) * mu + ta * mv;
                        let m2 = (1.0 - tb) * mu2 + tb * mv2;
                        let sum = m1 + m2;
                        let (s1, s2) = if sum > 1e-6 {
                            (
                                if m2 > 0.0 { m1 / sum } else { 1.0 },
                                if m1 > 0.0 { m2 / sum } else { 1.0 },
                            )
                        } else {
                            (0.55, 0.55)
                        };
                        if s1 > 0.0 || splittable || allow_fp_move {
                            record_edge_push(
                                ei,
                                u,
                                v,
                                ta,
                                nx,
                                ny,
                                def * s1.max(0.50),
                                None,
                                &mut constraints,
                                &mut hs_fp_group,
                                &mut edge_splits,
                            );
                        }
                        if s2 > 0.0 || splittable2 || allow_fp_move {
                            record_edge_push(
                                ej,
                                u2,
                                v2,
                                tb,
                                -nx,
                                -ny,
                                def * s2.max(0.50),
                                None,
                                &mut constraints,
                                &mut hs_fp_group,
                                &mut edge_splits,
                            );
                        }
                    }
                }
            }

            // 4a. If any mobile footprint group received copper reaction pushes, add static
            //     courtyard/pad/hole/edge guard half-planes and translate those rigid families.
            if allow_fp_move && hs_fp_group.iter().any(|hs| !hs.is_empty()) {
                append_static_fp_guard_constraints(
                    board,
                    &fp_pads,
                    &pad_ords,
                    &leader,
                    &group_immob,
                    orig_yards,
                    orig_fp_at,
                    &cur_fp_pos,
                    allowed_yards,
                    baseline_static,
                    &mut hs_fp_group,
                );
                let mut tethered_by: Vec<Vec<(usize, f64)>> = vec![Vec::new(); np];
                for (i, p) in graph.particles.iter().enumerate() {
                    if let Some((tpid, max_d)) = p.via_tether {
                        tethered_by[tpid].push((i, max_d));
                    }
                }
                for g in 0..n_fp {
                    if group_immob[g] || hs_fp_group[g].is_empty() {
                        continue;
                    }
                    let raw = solve_half_planes_2d((0.0, 0.0), &hs_fp_group[g]);
                    let step = clamp_vec(raw, FP_STEP_CAP_NM);
                    let delta = Point::new(step.0.round() as Nm, step.1.round() as Nm);
                    if delta != Point::new(0, 0) {
                        for &m in &members[g] {
                            cur_fp_pos[m] =
                                Point::new(cur_fp_pos[m].x + delta.x, cur_fp_pos[m].y + delta.y);
                            board.shift_footprint(m, delta);
                            graph.shift_footprint_anchors(m, cur_fp_pos[m], delta, &tethered_by);
                        }
                    }
                }
            }

            // 4b. Alternating projections onto clearance half-planes (`constraints`) and off-centre
            //     joint tether balls (`via_tether`), ending on clearance POCS.
            let pos0: Vec<(f64, f64)> = graph.particles.iter().map(|p| p.pos).collect();
            let project_half_planes = |g: &mut CopperGraph| {
                for (i, hs) in constraints.iter().enumerate() {
                    if hs.is_empty() || g.particles[i].mobility() <= 0.0 {
                        continue;
                    }
                    let dx0 = (
                        g.particles[i].pos.0 - pos0[i].0,
                        g.particles[i].pos.1 - pos0[i].1,
                    );
                    let dx = solve_half_planes_2d(dx0, hs);
                    let clamped = clamp_vec(dx, MAX_STEP_NM);
                    g.particles[i].pos = (pos0[i].0 + clamped.0, pos0[i].1 + clamped.1);
                }
            };

            project_half_planes(graph);
            for _alt in 0..3 {
                let mut moved_tether = false;
                for (i, cs_i) in constraints.iter().enumerate().take(np) {
                    let Some((tpid, max_d)) = graph.particles[i].via_tether else { continue };
                    let pi = graph.particles[i].pos;
                    let pt = graph.particles[tpid].pos;
                    let (dx, dy) = (pi.0 - pt.0, pi.1 - pt.1);
                    let dist = dx.hypot(dy);
                    if dist > max_d && dist > 1.0 {
                        let excess = dist - max_d;
                        let (ux, uy) = (dx / dist, dy / dist);
                        let mi = graph.particles[i].mobility();
                        let mt = graph.particles[tpid].mobility();
                        let (wi, wt) = if mi > 0.0
                            && graph.particles[tpid].is_via().is_some()
                            && cs_i.is_empty()
                        {
                            (1.0, 0.0)
                        } else if mi + mt > 0.0 {
                            (mi / (mi + mt), mt / (mi + mt))
                        } else {
                            (0.0, 0.0)
                        };
                        if wi + wt > 0.0 {
                            graph.particles[i].pos.0 -= ux * excess * wi;
                            graph.particles[i].pos.1 -= uy * excess * wi;
                            graph.particles[tpid].pos.0 += ux * excess * wt;
                            graph.particles[tpid].pos.1 += uy * excess * wt;
                            moved_tether = true;
                        }
                    }
                }
                if !moved_tether {
                    break;
                }
                project_half_planes(graph);
            }

            // 5. Split interior-penetrated edges so straight tracks bend round obstacles.
            if !edge_splits.is_empty() {
                let mut split_list: Vec<(usize, SplitCand)> = edge_splits.into_iter().collect();
                split_list.sort_unstable_by_key(|(ei, _)| *ei);
                for (ei, (deficit, t, nx, ny)) in split_list {
                    if !graph.edges[ei].active || graph.edges[ei].locked {
                        continue;
                    }
                    let Edge { u, v, layer, net, r, locked, .. } = graph.edges[ei];
                    let p0 = graph.particles[u].pos;
                    let p1 = graph.particles[v].pos;
                    let push = deficit.min(MAX_STEP_NM) * 0.85;
                    let mid = (
                        p0.0 + (p1.0 - p0.0) * t + nx * push,
                        p0.1 + (p1.1 - p0.1) * t + ny * push,
                    );
                    let w = graph.particles.len();
                    graph.particles.push(Particle {
                        pos: mid,
                        anchor: Anchor::Free,
                        locked: false,
                        via_tether: None,
                        plane_zones: Vec::new(),
                    });
                    graph.edges[ei].active = false;
                    graph.edges.push(Edge { u, v: w, layer, net, r, locked, active: true });
                    graph.edges.push(Edge { u: w, v, layer, net, r, locked, active: true });
                }
            }
        }

        // After Pass 0 (copper-only), attempt legal topological via sliding before allowing any
        // footprint to be pushed back from its target shrink position.
        if outer_pass == 0 {
            let _ = graph.sync_to_board(board);
            if count_dynamic_violations(board).0 > 0 {
                via_slides += slide_pinched_vias(graph, board);
            }
        }
    }

    if enable_fp_coupling && cur_fp_pos != init_fp_pos {
        let before_yard = cur_fp_pos.clone();
        resolve_courtyard_stops(
            orig_yards,
            fp_envs,
            fp_locked,
            orig_fp_at,
            init_fp_pos,
            &mut cur_fp_pos,
            allowed_yards,
        );
        let mut tethered_by: Vec<Vec<(usize, f64)>> = vec![Vec::new(); graph.particles.len()];
        for (i, p) in graph.particles.iter().enumerate() {
            if let Some((tpid, max_d)) = p.via_tether {
                tethered_by[tpid].push((i, max_d));
            }
        }
        for f in 0..n_fp {
            let d = Point::new(cur_fp_pos[f].x - before_yard[f].x, cur_fp_pos[f].y - before_yard[f].y);
            if d != Point::new(0, 0) {
                board.shift_footprint(f, d);
                graph.shift_footprint_anchors(f, cur_fp_pos[f], d, &tethered_by);
            }
        }
    }

    (cur_fp_pos, via_slides)
}

/// Projects `dx0` onto the intersection of clearance half-planes `hs` (`h.nx * x + h.ny * y >= h.need`).
/// Uses 4 fast sequential POCS passes first, then falls back to exact 2D active-set line/wedge
/// intersections (and cycle-averaged minimax centring when over-constrained) so obtuse wedges such
/// as fine-pitch QFN pad teeth eject vias cleanly in one iteration.
fn solve_half_planes_2d(dx0: (f64, f64), hs: &[HalfPlane]) -> (f64, f64) {
    let max_viol = |p: (f64, f64)| -> f64 {
        hs.iter()
            .map(|h| (h.need - (p.0 * h.nx + p.1 * h.ny)).max(0.0))
            .fold(0.0, f64::max)
    };
    if max_viol(dx0) <= 0.5 {
        return dx0;
    }
    if hs.len() == 1 {
        let h = hs[0];
        let add = (h.need - (dx0.0 * h.nx + dx0.1 * h.ny)).max(0.0);
        return (dx0.0 + add * h.nx, dx0.1 + add * h.ny);
    }

    let mut dx = dx0;
    for _ in 0..4 {
        for h in hs {
            let proj = dx.0 * h.nx + dx.1 * h.ny;
            if proj < h.need {
                let add = h.need - proj;
                dx.0 += add * h.nx;
                dx.1 += add * h.ny;
            }
        }
    }
    if max_viol(dx) <= 0.5 {
        return dx;
    }

    // Average one additional cycle of sequential projections for antiparallel pinches (`det ~ 0`).
    let mut cycle_sum = (0.0, 0.0);
    let mut cur = dx;
    for h in hs {
        let proj = cur.0 * h.nx + cur.1 * h.ny;
        if proj < h.need {
            let add = h.need - proj;
            cur.0 += add * h.nx;
            cur.1 += add * h.ny;
        }
        cycle_sum.0 += cur.0;
        cycle_sum.1 += cur.1;
    }
    let cycle_avg = (cycle_sum.0 / hs.len() as f64, cycle_sum.1 / hs.len() as f64);

    let mut cands: Vec<(f64, f64)> = vec![dx, cur, cycle_avg];
    let subset_buf: Vec<HalfPlane>;
    let active: &[HalfPlane] = if hs.len() <= 16 {
        hs
    } else {
        let mut sorted = hs.to_vec();
        sorted.sort_by(|a, b| b.need.total_cmp(&a.need));
        sorted.truncate(16);
        subset_buf = sorted;
        &subset_buf
    };

    for (i, ha) in active.iter().enumerate() {
        let add = (ha.need - (dx0.0 * ha.nx + dx0.1 * ha.ny)).max(0.0);
        cands.push((dx0.0 + add * ha.nx, dx0.1 + add * ha.ny));
        for hb in &active[(i + 1)..] {
            let det = ha.nx * hb.ny - ha.ny * hb.nx;
            if det.abs() > 1e-4 {
                let ix = (ha.need * hb.ny - hb.need * ha.ny) / det;
                let iy = (ha.nx * hb.need - hb.nx * ha.need) / det;
                if (ix - dx0.0).hypot(iy - dx0.1) <= MAX_STEP_NM * 3.0 {
                    cands.push((ix, iy));
                }
            }
        }
    }

    let dist2 = |p: (f64, f64)| (p.0 - dx0.0).powi(2) + (p.1 - dx0.1).powi(2);
    let mut best_feas: Option<((f64, f64), f64)> = None;
    let mut best_infeas: Option<((f64, f64), f64, f64)> = None;
    for p in cands {
        let v = max_viol(p);
        let d2 = dist2(p);
        if v <= 1.0 {
            if best_feas.is_none_or(|(_, bd2)| d2 < bd2) {
                best_feas = Some((p, d2));
            }
        } else if best_infeas
            .is_none_or(|(_, bv, bd2)| v + 1.0 < bv || ((v - bv).abs() <= 1.0 && d2 < bd2))
        {
            best_infeas = Some((p, v, d2));
        }
    }
    if let Some((p, _)) = best_feas {
        p
    } else if let Some((p, _, _)) = best_infeas {
        p
    } else {
        dx
    }
}

/// Exact `check_item`-guided coordinate descent on rounded nanometre positions to clear any
/// remaining micron-scale violations (mask webs, thermal spokes, custom `.kicad_dru` rules).
fn polish_on_board(graph: &mut CopperGraph, board: &mut Board) {
    const RADII: [Nm; 9] = [
        1_000, 3_000, 8_000, 20_000, 50_000, 120_000, 240_000, 380_000, 550_000,
    ];
    const DIRS: [(i64, i64); 8] = [
        (1, 0),
        (-1, 0),
        (0, 1),
        (0, -1),
        (1, 1),
        (1, -1),
        (-1, 1),
        (-1, -1),
    ];

    let mut tethered_by: Vec<Vec<(usize, f64)>> = vec![Vec::new(); graph.particles.len()];
    for (i, p) in graph.particles.iter().enumerate() {
        if let Some((tpid, max_d)) = p.via_tether {
            tethered_by[tpid].push((i, max_d));
        }
    }

    for _round in 0..20 {
        let (edge_ids, via_ids) = graph.sync_to_board(board);
        let mut item_to_particles: HashMap<ItemId, Vec<usize>> = HashMap::new();
        let mut particle_edges: Vec<Vec<usize>> = vec![Vec::new(); graph.particles.len()];
        for (ei, &oid) in edge_ids.iter().enumerate() {
            if let Some(id) = oid {
                let e = &graph.edges[ei];
                item_to_particles.insert(id, vec![e.u, e.v]);
                particle_edges[e.u].push(ei);
                particle_edges[e.v].push(ei);
            }
        }
        for (pi, &oid) in via_ids.iter().enumerate() {
            if let Some(id) = oid {
                item_to_particles.entry(id).or_default().push(pi);
            }
        }

        let mut suspects: Vec<usize> = Vec::new();
        for (&id, ps) in &item_to_particles {
            if !check_item(board, id).is_empty() {
                suspects.extend(ps.iter().copied());
            }
        }
        suspects.sort_unstable();
        suspects.dedup();
        suspects.retain(|&p| graph.particles[p].mobility() > 0.0);
        if suspects.is_empty() {
            break;
        }

        let mut improved_any = false;
        for pid in suspects {
            // Translate `pid` together with any mobile tethered children (`tethered_by`) so vias
            // with off-centre track ends can shift cleanly during local polish.
            let mut group: Vec<usize> = vec![pid];
            let mut cursor = 0usize;
            while cursor < group.len() {
                let cur = group[cursor];
                cursor += 1;
                for &(ch, _) in &tethered_by[cur] {
                    if graph.particles[ch].mobility() > 0.0 && !group.contains(&ch) {
                        group.push(ch);
                    }
                }
            }

            let mut local_ids: Vec<ItemId> = Vec::new();
            let mut group_edges: Vec<usize> = Vec::new();
            for &gp in &group {
                if let Some(vid) = via_ids[gp] {
                    local_ids.push(vid);
                }
                for &ei in &particle_edges[gp] {
                    if !group_edges.contains(&ei) {
                        group_edges.push(ei);
                        if let Some(eid) = edge_ids[ei] {
                            local_ids.push(eid);
                        }
                    }
                }
            }
            if local_ids.is_empty() {
                continue;
            }
            let score = |b: &Board, ids: &[ItemId]| -> (usize, f64) {
                let (mut cnt, mut pen) = (0usize, 0.0f64);
                for &id in ids {
                    for v in check_item(b, id) {
                        cnt += 1;
                        pen += (v.required as f64 - v.gap).max(1_000.0);
                    }
                }
                (cnt, pen)
            };

            let base_pts: Vec<Point> = group
                .iter()
                .map(|&gp| {
                    Point::new(
                        graph.particles[gp].pos.0.round() as Nm,
                        graph.particles[gp].pos.1.round() as Nm,
                    )
                })
                .collect();
            let mut best_score = score(board, &local_ids);
            if best_score.0 == 0 {
                continue;
            }
            let mut best_delta = Point::new(0, 0);

            let shifted_pt = |p: usize, delta: Point| -> Point {
                if let Some(idx) = group.iter().position(|&gp| gp == p) {
                    Point::new(base_pts[idx].x + delta.x, base_pts[idx].y + delta.y)
                } else {
                    Point::new(
                        graph.particles[p].pos.0.round() as Nm,
                        graph.particles[p].pos.1.round() as Nm,
                    )
                }
            };

            let apply_delta = |b: &mut Board, delta: Point| {
                for (idx, &gp) in group.iter().enumerate() {
                    if let Some(vid) = via_ids[gp] {
                        let pt = Point::new(base_pts[idx].x + delta.x, base_pts[idx].y + delta.y);
                        if let Some(mut it) = b.remove(vid) {
                            if let Shape::Capsule { r, .. } = it.shape {
                                it.shape = Shape::Capsule { a: pt, b: pt, r };
                            }
                            it.hole = it.hole.map(|(_, dr)| (pt, dr));
                            b.restore(vid, it);
                        }
                    }
                }
                for &ei in &group_edges {
                    if let Some(eid) = edge_ids[ei] {
                        let e = &graph.edges[ei];
                        let pa = shifted_pt(e.u, delta);
                        let pb = shifted_pt(e.v, delta);
                        if let Some(mut it) = b.remove(eid) {
                            let r = match it.shape {
                                Shape::Capsule { r, .. } => r,
                                _ => e.r,
                            };
                            it.shape = Shape::Capsule { a: pa, b: pb, r };
                            b.restore(eid, it);
                        }
                    }
                }
            };

            for &rad in &RADII {
                for &(dx, dy) in &DIRS {
                    let delta = Point::new(dx * rad, dy * rad);
                    let mut legal_group = true;
                    for (idx, &gp) in group.iter().enumerate() {
                        let cand = Point::new(base_pts[idx].x + delta.x, base_pts[idx].y + delta.y);
                        if let Anchor::PadVia { ref cur_pad, .. } = graph.particles[gp].anchor {
                            if !cur_pad.contains(cand) {
                                legal_group = false;
                                break;
                            }
                        }
                        if let Some((tpid, max_d)) = graph.particles[gp].via_tether {
                            if cand.distance(shifted_pt(tpid, delta)) > max_d {
                                legal_group = false;
                                break;
                            }
                        }
                        if tethered_by[gp]
                            .iter()
                            .any(|&(ch, max_d)| cand.distance(shifted_pt(ch, delta)) > max_d)
                        {
                            legal_group = false;
                            break;
                        }
                        if graph.particles[gp].plane_zones.iter().any(|&(z, _)| {
                            board
                                .unfilled
                                .iter()
                                .find(|u| u.0 == z)
                                .is_some_and(|(_, _, _, sh)| !sh.contains(cand))
                        }) {
                            legal_group = false;
                            break;
                        }
                    }
                    if !legal_group {
                        continue;
                    }
                    apply_delta(board, delta);
                    let sc = score(board, &local_ids);
                    if sc.0 < best_score.0 || (sc.0 == best_score.0 && sc.1 + 100.0 < best_score.1)
                    {
                        best_score = sc;
                        best_delta = delta;
                    }
                }
                if best_score.0 == 0 {
                    break;
                }
            }

            apply_delta(board, best_delta);
            if best_delta != Point::new(0, 0) {
                for (idx, &gp) in group.iter().enumerate() {
                    graph.particles[gp].pos = (
                        (base_pts[idx].x + best_delta.x) as f64,
                        (base_pts[idx].y + best_delta.y) as f64,
                    );
                }
                improved_any = true;
            }
        }
        if !improved_any {
            break;
        }
    }
}

/// Replaces subdivided or slack degree-2 chains `(u, v) + (v, w)` with `(u, w)` whenever the
/// shortcut stays within the homotopic altitude bound (`dev <= 200 um`, whose triangle inradius
/// `< 100 um` cannot enclose any foreign pad or via), passes `check_item`, and creates no acute
/// angle or new outer-layer narrow slot.
fn shorten_taut_chains(graph: &mut CopperGraph, board: &mut Board) {
    let min_clr = nm(board.rules.min_clearance).max(100_000) as f64;
    let outer_layers: Vec<usize> = if board.layers.len() >= 2 {
        vec![0, board.layers.len() - 1]
    } else {
        vec![0]
    };
    let mut tethered_targets = vec![false; graph.particles.len()];
    for p in &graph.particles {
        if let Some((tpid, _)) = p.via_tether {
            tethered_targets[tpid] = true;
        }
    }

    for _pass in 0..10 {
        let (mut edge_ids, _) = graph.sync_to_board(board);
        let mut inc: Vec<Vec<usize>> = vec![Vec::new(); graph.particles.len()];
        for (ei, e) in graph.edges.iter().enumerate() {
            if e.active {
                inc[e.u].push(ei);
                inc[e.v].push(ei);
            }
        }
        let mut merged_any = false;
        for pid in 0..graph.particles.len() {
            let part = &graph.particles[pid];
            if !matches!(part.anchor, Anchor::Free)
                || part.locked
                || part.via_tether.is_some()
                || tethered_targets[pid]
                || inc[pid].len() != 2
            {
                continue;
            }
            let (e1, e2) = (inc[pid][0], inc[pid][1]);
            if !graph.edges[e1].active || !graph.edges[e2].active {
                continue;
            }
            let (a1, a2) = (&graph.edges[e1], &graph.edges[e2]);
            if a1.layer != a2.layer
                || a1.net != a2.net
                || a1.r != a2.r
                || a1.locked
                || a2.locked
            {
                continue;
            }
            let u = if a1.u == pid { a1.v } else { a1.u };
            let w = if a2.u == pid { a2.v } else { a2.u };
            if u == w {
                continue;
            }
            let (layer, net, r, locked) = (a1.layer, a1.net, a1.r, a1.locked);
            // Do not create a parallel duplicate edge if `(u, w)` is already connected on `layer`.
            if inc[u].iter().any(|&ei| {
                ei != e1
                    && graph.edges[ei].active
                    && graph.edges[ei].layer == layer
                    && (graph.edges[ei].u == w || graph.edges[ei].v == w)
            }) {
                continue;
            }
            let pu = Point::new(
                graph.particles[u].pos.0.round() as Nm,
                graph.particles[u].pos.1.round() as Nm,
            );
            let pv = Point::new(
                graph.particles[pid].pos.0.round() as Nm,
                graph.particles[pid].pos.1.round() as Nm,
            );
            let pw = Point::new(
                graph.particles[w].pos.0.round() as Nm,
                graph.particles[w].pos.1.round() as Nm,
            );
            if pu == pw {
                continue;
            }
            let dev = point_segment(pv, pu, pw);
            let old_len = pu.distance(pv) + pv.distance(pw);
            let new_len = pu.distance(pw);
            let nearly_collinear = dev <= 25_000.0;
            if dev > 200_000.0 || (!nearly_collinear && new_len > old_len + 500.0) {
                continue;
            }
            // Do not create an acute angle `< 90 deg` with another edge at `u` or `w`.
            if creates_acute_at(graph, &inc[u], e1, u, pu, pw, layer)
                || creates_acute_at(graph, &inc[w], e2, w, pw, pu, layer)
            {
                continue;
            }

            let old1_shape = Shape::Capsule { a: pu, b: pv, r };
            let old2_shape = Shape::Capsule { a: pv, b: pw, r };
            let old1 = edge_ids[e1].and_then(|id| board.remove(id).map(|it| (id, it)));
            let old2 = edge_ids[e2].and_then(|id| board.remove(id).map(|it| (id, it)));
            let trial_shape = Shape::Capsule { a: pu, b: pw, r };

            // On outer layers, reject non-collinear shortcuts that move closer to non-incident
            // same-net copper than `(pu, pv)` and `(pv, pw)` already were when within `min_clr`.
            let mut makes_slot = false;
            if !nearly_collinear && net != 0 && outer_layers.contains(&layer) {
                let qbb = trial_shape.bbox().grow(min_clr as Nm);
                for oid in board.near(layer, qbb) {
                    let Some(oit) = board.item(oid) else { continue };
                    if oit.net != net || !oit.on(layer) || oit.refilled() {
                        continue;
                    }
                    if oit.shape.contains(pu) || oit.shape.contains(pw) {
                        continue;
                    }
                    let g = trial_shape.gap_within(&oit.shape, min_clr);
                    if g > 10_000.0 && g < min_clr {
                        let g_old = old1_shape
                            .gap_within(&oit.shape, min_clr)
                            .min(old2_shape.gap_within(&oit.shape, min_clr));
                        if g + 3_000.0 < g_old {
                            makes_slot = true;
                            break;
                        }
                    }
                }
            }

            let trial_id = board.insert(Item {
                kind: Kind::Track,
                net,
                layers: 1u64 << layer,
                shape: trial_shape,
                hole: None,
                locked,
                span: None,
            });
            if !makes_slot && check_item(board, trial_id).is_empty() {
                graph.edges[e1].u = u;
                graph.edges[e1].v = w;
                graph.edges[e2].active = false;
                edge_ids[e1] = Some(trial_id);
                edge_ids[e2] = None;
                inc[u].retain(|&x| x != e2);
                inc[w].retain(|&x| x != e2);
                if !inc[w].contains(&e1) {
                    inc[w].push(e1);
                }
                inc[pid].clear();
                merged_any = true;
            } else {
                board.discard(trial_id);
                if let Some((id, it)) = old1 {
                    board.restore(id, it);
                }
                if let Some((id, it)) = old2 {
                    board.restore(id, it);
                }
            }
        }
        if !merged_any {
            break;
        }
    }
}

fn creates_acute_at(
    graph: &CopperGraph,
    incident: &[usize],
    skip_ei: usize,
    at_pid: usize,
    at_pt: Point,
    other_pt: Point,
    layer: usize,
) -> bool {
    let (dx, dy) = ((other_pt.x - at_pt.x) as f64, (other_pt.y - at_pt.y) as f64);
    let len = dx.hypot(dy);
    if len < 1.0 {
        return false;
    }
    for &ei in incident {
        if ei == skip_ei || !graph.edges[ei].active || graph.edges[ei].layer != layer {
            continue;
        }
        let nb = if graph.edges[ei].u == at_pid {
            graph.edges[ei].v
        } else {
            graph.edges[ei].u
        };
        let pn = graph.particles[nb].pos;
        let (ndx, ndy) = (pn.0 - at_pt.x as f64, pn.1 - at_pt.y as f64);
        let nlen = ndx.hypot(ndy);
        if nlen < 1.0 {
            continue;
        }
        if (dx * ndx + dy * ndy) / (len * nlen) > 0.008 {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Fast Continuous Closest-Point Geometry
// ---------------------------------------------------------------------------

fn clamp_vec(v: (f64, f64), max_len: f64) -> (f64, f64) {
    let len = v.0.hypot(v.1);
    if len <= max_len || len == 0.0 {
        v
    } else {
        (v.0 * (max_len / len), v.1 * (max_len / len))
    }
}

fn seg_normal(p0: (f64, f64), p1: (f64, f64)) -> (f64, f64) {
    let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
    let len = dx.hypot(dy);
    if len > 1.0 {
        (-dy / len, dx / len)
    } else {
        (1.0, 0.0)
    }
}

fn seg_bbox(p0: (f64, f64), p1: (f64, f64), r: Nm) -> BBox {
    BBox {
        min: Point::new(p0.0.min(p1.0).floor() as Nm - r, p0.1.min(p1.1).floor() as Nm - r),
        max: Point::new(p0.0.max(p1.0).ceil() as Nm + r, p0.1.max(p1.1).ceil() as Nm + r),
    }
}

fn grid_cells(b: BBox) -> impl Iterator<Item = (i64, i64)> {
    let (x0, x1) = (b.min.x.div_euclid(DYN_CELL), b.max.x.div_euclid(DYN_CELL));
    let (y0, y1) = (b.min.y.div_euclid(DYN_CELL), b.max.y.div_euclid(DYN_CELL));
    (x0..=x1).flat_map(move |x| (y0..=y1).map(move |y| (x, y)))
}

fn closest_seg_point(p0: (f64, f64), p1: (f64, f64), q: (f64, f64)) -> (f64, (f64, f64), (f64, f64), f64) {
    let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 1e-9 {
        (((q.0 - p0.0) * dx + (q.1 - p0.1) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.5
    };
    let qa = (p0.0 + dx * t, p0.1 + dy * t);
    let d = (qa.0 - q.0).hypot(qa.1 - q.1);
    (t, qa, q, d)
}

fn closest_seg_seg(
    a0: (f64, f64),
    a1: (f64, f64),
    b0: (f64, f64),
    b1: (f64, f64),
) -> (f64, f64, (f64, f64), (f64, f64), f64) {
    let u = (a1.0 - a0.0, a1.1 - a0.1);
    let v = (b1.0 - b0.0, b1.1 - b0.1);
    let a = u.0 * u.0 + u.1 * u.1;
    let c = v.0 * v.0 + v.1 * v.1;

    if a <= 1e-9 && c <= 1e-9 {
        return (0.5, 0.5, a0, b0, (a0.0 - b0.0).hypot(a0.1 - b0.1));
    }
    if a <= 1e-9 {
        let (tb, qb, _, d) = closest_seg_point(b0, b1, a0);
        return (0.5, tb, a0, qb, d);
    }
    if c <= 1e-9 {
        let (ta, qa, _, d) = closest_seg_point(a0, a1, b0);
        return (ta, 0.5, qa, b0, d);
    }

    // Check segment intersection first so crossed segments report 0 distance at their crossing.
    let cross = |x: (f64, f64), y: (f64, f64)| x.0 * y.1 - x.1 * y.0;
    let denom = cross(u, v);
    if denom.abs() > 1e-9 {
        let s = cross((b0.0 - a0.0, b0.1 - a0.1), v) / denom;
        let t = cross((b0.0 - a0.0, b0.1 - a0.1), u) / denom;
        if (0.0..=1.0).contains(&s) && (0.0..=1.0).contains(&t) {
            let q = (a0.0 + u.0 * s, a0.1 + u.1 * s);
            return (s, t, q, q, 0.0);
        }
    }

    // Closest pair lies at one of the 4 endpoint-to-segment projections.
    let (_, qb0, _, d_a0) = closest_seg_point(b0, b1, a0);
    let mut best = (0.0, ((qb0.0 - b0.0) * v.0 + (qb0.1 - b0.1) * v.1) / c, a0, qb0, d_a0);

    let (tb1, qb1, _, d_a1) = closest_seg_point(b0, b1, a1);
    if d_a1 < best.4 {
        best = (1.0, tb1, a1, qb1, d_a1);
    }
    let (ta0, qa0, _, d_b0) = closest_seg_point(a0, a1, b0);
    if d_b0 < best.4 {
        best = (ta0, 0.0, qa0, b0, d_b0);
    }
    let (ta1, qa1, _, d_b1) = closest_seg_point(a0, a1, b1);
    if d_b1 < best.4 {
        best = (ta1, 1.0, qa1, b1, d_b1);
    }
    best
}

/// Signed gap and outward push normal `(nx, ny)` for dynamic capsule `(p0, p1, ra)` against `shape`.
fn closest_seg_shape(
    p0: (f64, f64),
    p1: (f64, f64),
    ra: Nm,
    shape: &Shape,
) -> (f64, (f64, f64), (f64, f64), f64, f64, f64) {
    match shape {
        Shape::Capsule { a, b, r: rb } => {
            let q0 = (a.x as f64, a.y as f64);
            let q1 = (b.x as f64, b.y as f64);
            let (ta, _, qa, qb, d) = closest_seg_seg(p0, p1, q0, q1);
            let gap = d - (ra + *rb) as f64;
            let (nx, ny) = if d > 1.0 {
                ((qa.0 - qb.0) / d, (qa.1 - qb.1) / d)
            } else {
                let mid_a = ((p0.0 + p1.0) * 0.5, (p0.1 + p1.1) * 0.5);
                let mid_b = ((q0.0 + q1.0) * 0.5, (q0.1 + q1.1) * 0.5);
                let (mx, my) = (mid_a.0 - mid_b.0, mid_a.1 - mid_b.1);
                let ml = mx.hypot(my);
                if ml > 1.0 {
                    (mx / ml, my / ml)
                } else {
                    seg_normal(p0, p1)
                }
            };
            (ta, qa, qb, gap, nx, ny)
        }
        Shape::Polygon(poly) => {
            if poly.is_empty() {
                return (0.5, p0, p0, f64::INFINITY, 1.0, 0.0);
            }
            let mut best = (0.5, p0, (poly[0].x as f64, poly[0].y as f64), f64::INFINITY);
            for k in 0..poly.len() {
                let e0 = (poly[k].x as f64, poly[k].y as f64);
                let e1 = (poly[(k + 1) % poly.len()].x as f64, poly[(k + 1) % poly.len()].y as f64);
                let (ta, _, qa, qb, d) = closest_seg_seg(p0, p1, e0, e1);
                if d < best.3 {
                    best = (ta, qa, qb, d);
                }
            }
            let (ta, qa, qb, d_edge) = best;
            let qa_pt = Point::new(qa.0.round() as Nm, qa.1.round() as Nm);
            if inside(qa_pt, poly) {
                let (nx, ny) = if d_edge > 1.0 {
                    ((qb.0 - qa.0) / d_edge, (qb.1 - qa.1) / d_edge)
                } else {
                    let cx = poly.iter().map(|p| p.x as f64).sum::<f64>() / poly.len() as f64;
                    let cy = poly.iter().map(|p| p.y as f64).sum::<f64>() / poly.len() as f64;
                    let (dx, dy) = (qa.0 - cx, qa.1 - cy);
                    let dl = dx.hypot(dy);
                    if dl > 1.0 { (dx / dl, dy / dl) } else { (1.0, 0.0) }
                };
                (ta, qa, qb, -(d_edge + ra as f64), nx, ny)
            } else {
                let (nx, ny) = if d_edge > 1.0 {
                    ((qa.0 - qb.0) / d_edge, (qa.1 - qb.1) / d_edge)
                } else {
                    let cx = poly.iter().map(|p| p.x as f64).sum::<f64>() / poly.len() as f64;
                    let cy = poly.iter().map(|p| p.y as f64).sum::<f64>() / poly.len() as f64;
                    let (dx, dy) = (qa.0 - cx, qa.1 - cy);
                    let dl = dx.hypot(dy);
                    if dl > 1.0 { (dx / dl, dy / dl) } else { (1.0, 0.0) }
                };
                (ta, qa, qb, d_edge - ra as f64, nx, ny)
            }
        }
    }
}

/// Signed gap (`< 0` when overlapping) and outward unit normal `(nx, ny)` pointing from `sb`
/// toward `sa` (so translating `sa` by `+(nx, ny)` increases the gap).
fn closest_shape_shape(sa: &Shape, sb: &Shape) -> (f64, f64, f64) {
    match (sa, sb) {
        (Shape::Capsule { a, b, r }, _) => {
            let (_, _, _, gap, nx, ny) =
                closest_seg_shape((a.x as f64, a.y as f64), (b.x as f64, b.y as f64), *r, sb);
            (gap, nx, ny)
        }
        (_, Shape::Capsule { a, b, r }) => {
            let (_, _, _, gap, nx, ny) =
                closest_seg_shape((a.x as f64, a.y as f64), (b.x as f64, b.y as f64), *r, sa);
            (gap, -nx, -ny)
        }
        (Shape::Polygon(pa), Shape::Polygon(pb)) => {
            if pa.is_empty() || pb.is_empty() {
                return (f64::INFINITY, 1.0, 0.0);
            }
            let pen = courtyard_penetration_nm(sa, sb);
            if pen > 0.0 {
                let (nx, ny) = courtyard_contact_normal(pa, pb);
                return (-pen, nx, ny);
            }
            let mut best_d = f64::INFINITY;
            let mut best_pair = ((0.0, 0.0), (0.0, 0.0));
            for i in 0..pa.len() {
                let a0 = (pa[i].x as f64, pa[i].y as f64);
                let a1 = (pa[(i + 1) % pa.len()].x as f64, pa[(i + 1) % pa.len()].y as f64);
                for j in 0..pb.len() {
                    let b0 = (pb[j].x as f64, pb[j].y as f64);
                    let b1 = (pb[(j + 1) % pb.len()].x as f64, pb[(j + 1) % pb.len()].y as f64);
                    let (_, _, qa, qb, d) = closest_seg_seg(a0, a1, b0, b1);
                    if d < best_d {
                        best_d = d;
                        best_pair = (qa, qb);
                    }
                }
            }
            if best_d > 10.0 {
                let (qa, qb) = best_pair;
                (best_d, (qa.0 - qb.0) / best_d, (qa.1 - qb.1) / best_d)
            } else {
                let (nx, ny) = courtyard_contact_normal(pa, pb);
                (best_d, nx, ny)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Step Metrics & Pure-Rust DFM Acid-Trap Measurement
// ---------------------------------------------------------------------------

type TrackSeg = (Point, Point, Nm);

fn total_track_length_mm(board: &Board) -> f64 {
    let mut sum_nm = 0.0;
    for (_, it) in board.items() {
        if it.kind == Kind::Track {
            if let Shape::Capsule { a, b, .. } = it.shape {
                sum_nm += a.distance(b);
            }
        }
    }
    sum_nm * 1e-6
}

fn count_tracks(board: &Board) -> usize {
    board.items().filter(|(_, it)| it.kind == Kind::Track).count()
}

fn count_vias(board: &Board) -> usize {
    board.items().filter(|(_, it)| it.kind == Kind::Via).count()
}

/// Counts acute track-junction angles (< 89.5 deg away from pads where a wedge opens) and narrow
/// same-net slots using exact 2D Euclidean morphological closing, mirroring `tools/dfm.py`.
pub fn dfm_counts(board: &Board) -> AcidTraps {
    let gap = nm(board.rules.min_clearance).max(100_000);
    let mut tracks_by_nl: HashMap<(u32, usize), Vec<TrackSeg>> = HashMap::new();
    let mut vias_by_nl: HashMap<(u32, usize), Vec<(Point, Nm)>> = HashMap::new();
    let mut pads_by_nl: HashMap<(u32, usize), Vec<Shape>> = HashMap::new();

    let outer: Vec<usize> = if board.layers.len() >= 2 {
        vec![0, board.layers.len() - 1]
    } else if !board.layers.is_empty() {
        vec![0]
    } else {
        Vec::new()
    };

    for (_, it) in board.items() {
        match it.kind {
            Kind::Track => {
                let l = it.layers.trailing_zeros() as usize;
                if let Shape::Capsule { a, b, r } = it.shape {
                    if a != b {
                        tracks_by_nl.entry((it.net, l)).or_default().push((a, b, r));
                    }
                }
            }
            Kind::Via => {
                if let Shape::Capsule { a, r, .. } = it.shape {
                    for &l in &outer {
                        if it.on(l) {
                            vias_by_nl.entry((it.net, l)).or_default().push((a, r));
                        }
                    }
                }
            }
            Kind::Pad { .. } => {
                for &l in &outer {
                    if it.on(l) {
                        pads_by_nl.entry((it.net, l)).or_default().push(it.shape.clone());
                    }
                }
            }
            _ => {}
        }
    }

    let mut acute = 0usize;
    let mut slots = 0usize;
    let thresh_rad = 89.5f64.to_radians();

    for (&(net, layer), ts) in &tracks_by_nl {
        let pads = pads_by_nl.get(&(net, layer)).map_or(&[][..], Vec::as_slice);
        let mut ends: HashMap<Point, Vec<(f64, f64, f64)>> = HashMap::new();
        for &(s, e, r) in ts {
            let w = (r * 2) as f64;
            ends.entry(s).or_default().push(((e.x - s.x) as f64, (e.y - s.y) as f64, w));
            ends.entry(e).or_default().push(((s.x - e.x) as f64, (s.y - e.y) as f64, w));
        }
        for (pt, dirs) in ends {
            if dirs.len() < 2 || pads.iter().any(|p| p.contains(pt)) {
                continue;
            }
            let mut arms: Vec<(f64, f64, f64)> = dirs
                .iter()
                .map(|&(dx, dy, w)| (dy.atan2(dx), dx.hypot(dy), w))
                .collect();
            arms.sort_by(|a, b| a.0.total_cmp(&b.0));
            let tau = std::f64::consts::TAU;
            let mut opens_acute = false;
            for i in 0..arms.len() {
                let (a1, l1, w1) = arms[i];
                let (a2, l2, w2) = arms[(i + 1) % arms.len()];
                let angle = (a2 - a1).rem_euclid(tau);
                if angle >= thresh_rad {
                    continue;
                }
                let part = w1.max(w2) / (2.0 * (angle.max(1e-9) * 0.5).sin());
                if l1.min(l2) > part {
                    opens_acute = true;
                    break;
                }
            }
            if opens_acute {
                acute += 1;
            }
        }

        if outer.contains(&layer) {
            let vias = vias_by_nl.get(&(net, layer)).map_or(&[][..], Vec::as_slice);
            slots += count_net_slots(ts, vias, pads, gap);
        }
    }

    AcidTraps { acute, slots }
}

fn count_net_slots(
    tracks: &[(Point, Point, Nm)],
    vias: &[(Point, Nm)],
    pads: &[Shape],
    gap: Nm,
) -> usize {
    const S: Nm = 15_000; // 15 um raster step
    let mut bb = BBox::of([tracks[0].0, tracks[0].1]).grow(tracks[0].2);
    for &(a, b, r) in tracks {
        bb = bb.union(BBox::of([a, b]).grow(r));
    }
    for &(c, r) in vias {
        bb = bb.union(BBox::of([c]).grow(r));
    }
    for p in pads {
        bb = bb.union(p.bbox());
    }
    let pad_margin = gap / 2 + 2 * S;
    bb = bb.grow(pad_margin);
    let w = ((bb.max.x - bb.min.x) / S + 1) as usize;
    let h = ((bb.max.y - bb.min.y) / S + 1) as usize;
    if w < 3 || h < 3 || w * h > 6_000_000 {
        return 0;
    }

    let mut whole = vec![false; w * h];
    let mut track_touch = vec![false; w * h];
    let cell_pt = |i: usize, j: usize| Point::new(bb.min.x + i as Nm * S, bb.min.y + j as Nm * S);
    let span_of = |b: BBox| {
        let i0 = ((b.min.x - bb.min.x).div_euclid(S)).clamp(0, w as i64 - 1) as usize;
        let i1 = ((b.max.x - bb.min.x).div_euclid(S) + 1).clamp(0, w as i64) as usize;
        let j0 = ((b.min.y - bb.min.y).div_euclid(S)).clamp(0, h as i64 - 1) as usize;
        let j1 = ((b.max.y - bb.min.y).div_euclid(S) + 1).clamp(0, h as i64) as usize;
        (i0..i1, j0..j1)
    };

    for &(a, b, r) in tracks {
        let halo = r + S;
        let (cols, rows) = span_of(BBox::of([a, b]).grow(halo));
        for j in rows {
            for i in cols.clone() {
                let d = point_segment(cell_pt(i, j), a, b);
                if d <= r as f64 {
                    whole[j * w + i] = true;
                }
                if d <= r as f64 + S as f64 * 1.1 {
                    track_touch[j * w + i] = true;
                }
            }
        }
    }
    for &(c, r) in vias {
        let (cols, rows) = span_of(BBox::of([c]).grow(r));
        let r2 = (r as f64) * (r as f64);
        for j in rows {
            for i in cols.clone() {
                let p = cell_pt(i, j);
                let dx = (p.x - c.x) as f64;
                let dy = (p.y - c.y) as f64;
                if dx * dx + dy * dy <= r2 {
                    whole[j * w + i] = true;
                }
            }
        }
    }
    for p in pads {
        let (cols, rows) = span_of(p.bbox());
        for j in rows {
            for i in cols.clone() {
                if p.contains(cell_pt(i, j)) {
                    whole[j * w + i] = true;
                }
            }
        }
    }

    let r_half_cells = (gap as f64 * 0.5) / S as f64;
    let r_half_sq = r_half_cells * r_half_cells;
    let d_to_whole = edt_2d(&whole, w, h);
    let inflated: Vec<bool> = d_to_whole.iter().map(|&d2| d2 <= r_half_sq).collect();
    let outside_inf: Vec<bool> = inflated.iter().map(|&v| !v).collect();
    let d_to_out = edt_2d(&outside_inf, w, h);
    let fill_only: Vec<bool> = (0..w * h)
        .map(|k| d_to_out[k] > r_half_sq && !whole[k])
        .collect();

    let outside_fill: Vec<bool> = fill_only.iter().map(|&v| !v).collect();
    let d_depth = edt_2d(&outside_fill, w, h);
    let deep_thresh_sq = ((gap as f64 * 0.125) / S as f64).powi(2);

    let mut seen = vec![false; w * h];
    let mut count = 0usize;
    let mut stack = Vec::new();
    for start in 0..w * h {
        if !fill_only[start] || seen[start] {
            continue;
        }
        seen[start] = true;
        stack.push(start);
        let mut is_deep = false;
        let mut borders_track = false;
        while let Some(k) = stack.pop() {
            if d_depth[k] > deep_thresh_sq {
                is_deep = true;
            }
            if track_touch[k] {
                borders_track = true;
            }
            let (i, j) = (k % w, k / w);
            for (ni, nj) in [
                (i.wrapping_sub(1), j),
                (i + 1, j),
                (i, j.wrapping_sub(1)),
                (i, j + 1),
            ] {
                if ni < w && nj < h {
                    let nk = nj * w + ni;
                    if fill_only[nk] && !seen[nk] {
                        seen[nk] = true;
                        stack.push(nk);
                    }
                }
            }
        }
        if is_deep && borders_track {
            count += 1;
        }
    }
    count
}

/// Exact 2D squared Euclidean distance transform (in grid-cell units) to the `true` set in `mask`.
fn edt_2d(mask: &[bool], w: usize, h: usize) -> Vec<f64> {
    const INF: f64 = 1e18;
    let mut g = vec![INF; w * h];
    for k in 0..w * h {
        if mask[k] {
            g[k] = 0.0;
        }
    }
    let mut f = vec![0.0; w.max(h)];
    let mut d = vec![0.0; w.max(h)];
    let mut v = vec![0usize; w.max(h)];
    let mut z = vec![0.0; w.max(h) + 1];

    // Columns first.
    for i in 0..w {
        for j in 0..h {
            f[j] = g[j * w + i];
        }
        edt_1d(&f[..h], &mut d[..h], &mut v[..h], &mut z[..=h]);
        for j in 0..h {
            g[j * w + i] = d[j];
        }
    }
    // Then rows.
    for j in 0..h {
        let row = j * w;
        f[..w].copy_from_slice(&g[row..row + w]);
        edt_1d(&f[..w], &mut d[..w], &mut v[..w], &mut z[..=w]);
        g[row..row + w].copy_from_slice(&d[..w]);
    }
    g
}

fn edt_1d(f: &[f64], d: &mut [f64], v: &mut [usize], z: &mut [f64]) {
    let n = f.len();
    v[0] = 0;
    z[0] = f64::NEG_INFINITY;
    z[1] = f64::INFINITY;
    let mut k = 0usize;
    for q in 1..n {
        let mut s;
        loop {
            let vk = v[k];
            s = ((f[q] + (q * q) as f64) - (f[vk] + (vk * vk) as f64))
                / (2.0 * (q as f64 - vk as f64));
            if s > z[k] || k == 0 {
                break;
            }
            k -= 1;
        }
        if s <= z[k] && k == 0 {
            v[0] = q;
            z[1] = f64::INFINITY;
        } else {
            k += 1;
            v[k] = q;
            z[k] = s;
            z[k + 1] = f64::INFINITY;
        }
    }
    k = 0;
    for (q, out) in d.iter_mut().enumerate() {
        while z[k + 1] < q as f64 {
            k += 1;
        }
        let dx = q as f64 - v[k] as f64;
        *out = dx * dx + f[v[k]];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use board::check;

    #[test]
    fn compact_bends_a_straight_track_around_an_obstacle_pad_without_ripping() {
        // Track of net 1 runs straight at y = 5.25 mm between f1 (4, 5.25) and f2 (16, 5.25).
        // Obstacle f3 of net 2 sits above centre at (10, 6.20), so compacting toward y_centre = 5.0
        // pulls f3 down across y = 5.25 and forces the net-1 track to bend upward around f3!
        let text = "(kicad_pcb
\t(version 20241229)
\t(general (thickness 1.6))
\t(layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))
\t(net 0 \"\")
\t(net 1 \"SIG\")
\t(net 2 \"OBS\")
\t(gr_line (start 0 0) (end 20 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e1\"))
\t(gr_line (start 20 0) (end 20 10) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e2\"))
\t(gr_line (start 20 10) (end 0 10) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e3\"))
\t(gr_line (start 0 10) (end 0 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e4\"))
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"f1\") (at 4 5.4)
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 1 \"SIG\") (uuid \"p1\"))
\t)
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"f2\") (at 16 5.4)
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 1 \"SIG\") (uuid \"p2\"))
\t)
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"f3\") (at 10 6.35)
\t\t(pad \"1\" smd roundrect (at 0 0) (size 1.0 1.0) (roundrect_rratio 0.25) (layers \"F.Cu\") (net 2 \"OBS\") (uuid \"p3\"))
\t)
\t(segment (start 4 5.4) (end 16 5.4) (width 0.2) (layer \"F.Cu\") (net 1) (uuid \"s1\"))
)\n";
        let rules = Rules::parse("{}").unwrap();
        let rep = compact(
            text,
            rules.clone(),
            &CompactOptions {
                factor: 0.70,
                steps: Some(15),
                lock: HashSet::new(),
                uuid_seed: Some(1),
                verbose: false,
            },
        )
        .unwrap();
        assert_eq!(rep.steps_accepted, 15, "jam: {:?}", rep.jam);
        assert!((rep.achieved_factor - 0.70).abs() < 1e-9);
        let (pcb, copper) = parse_board(&rep.text).unwrap();
        assert!(copper.tracks.len() >= 2, "track bent into multiple segments: {}", copper.tracks.len());
        let b = Board::new(&pcb, &copper, rules);
        assert!(check(&b).is_empty(), "zero violations after bending: {:?}", check(&b));
        assert!(connectivity_without_pours(&b).airlines.is_empty(), "SIG remained connected");
    }

    #[test]
    fn compact_stops_footprints_short_of_courtyard_overlap() {
        let text = "(kicad_pcb
\t(version 20241229)
\t(general (thickness 1.6))
\t(layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))
\t(net 0 \"\")
\t(net 1 \"SIG\")
\t(gr_line (start 0 0) (end 20 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e1\"))
\t(gr_line (start 20 0) (end 20 10) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e2\"))
\t(gr_line (start 20 10) (end 0 10) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e3\"))
\t(gr_line (start 0 10) (end 0 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e4\"))
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"f1\") (at 8.8 5)
\t\t(fp_rect (start -1 -1) (end 1 1) (stroke (width 0.05)) (layer \"F.CrtYd\") (uuid \"c1\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 1 \"SIG\") (uuid \"p1\"))
\t)
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"f2\") (at 11.2 5)
\t\t(fp_rect (start -1 -1) (end 1 1) (stroke (width 0.05)) (layer \"F.CrtYd\") (uuid \"c2\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 1 \"SIG\") (uuid \"p2\"))
\t)
\t(segment (start 8.8 5) (end 11.2 5) (width 0.2) (layer \"F.Cu\") (net 1) (uuid \"s1\"))
)\n";
        let rules = Rules::parse("{}").unwrap();
        let rep = compact(
            text,
            rules.clone(),
            &CompactOptions {
                factor: 0.70,
                steps: Some(10),
                lock: HashSet::new(),
                uuid_seed: Some(2),
                verbose: false,
            },
        )
        .unwrap();
        assert!(rep.steps.iter().any(|s| s.courtyard_stops > 0), "courtyard stop triggered");
        let (pcb, copper) = parse_board(&rep.text).unwrap();
        let gap_centres = pcb.footprints[1].at[0] - pcb.footprints[0].at[0];
        assert!(gap_centres >= 2.0e-3 - 2e-6, "courtyards did not overlap: dx={gap_centres}");
        let b = Board::new(&pcb, &copper, rules);
        assert!(check(&b).is_empty());
        assert!(connectivity_without_pours(&b).airlines.is_empty());
    }

    #[test]
    fn compact_stops_at_the_last_legal_step_when_a_channel_jams() {
        // Two obstacle pads p_top and p_bot pinch a signal track running between them at x = 10:
        // initial vertical gap between obstacles is 1.4 mm (track 0.2 mm + 2 * 0.2 mm clr = 0.6 mm
        // needed), so shrinking toward 0.30 jams partway and reverts to the last passing step.
        let text = "(kicad_pcb
\t(version 20241229)
\t(general (thickness 1.6))
\t(layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))
\t(net 0 \"\")
\t(net 1 \"SIG\")
\t(net 2 \"OBS_A\")
\t(net 3 \"OBS_B\")
\t(gr_line (start 0 0) (end 20 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e1\"))
\t(gr_line (start 20 0) (end 20 10) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e2\"))
\t(gr_line (start 20 10) (end 0 10) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e3\"))
\t(gr_line (start 0 10) (end 0 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e4\"))
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"f1\") (at 4 5)
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 1 \"SIG\") (uuid \"p1\"))
\t)
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"f2\") (at 16 5)
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 1 \"SIG\") (uuid \"p2\"))
\t)
\t(footprint \"W\" (layer \"F.Cu\") (uuid \"f3\") (at 10 3.7)
\t\t(pad \"1\" smd rect (at 0 0) (size 8.0 1.2) (layers \"F.Cu\") (net 2 \"OBS_A\") (uuid \"p3\"))
\t)
\t(footprint \"W\" (layer \"F.Cu\") (uuid \"f4\") (at 10 6.3)
\t\t(pad \"1\" smd rect (at 0 0) (size 8.0 1.2) (layers \"F.Cu\") (net 3 \"OBS_B\") (uuid \"p4\"))
\t)
\t(segment (start 4 5) (end 16 5) (width 0.2) (layer \"F.Cu\") (net 1) (uuid \"s1\"))
)\n";
        let rules = Rules::parse("{}").unwrap();
        let rep = compact(
            text,
            rules.clone(),
            &CompactOptions {
                factor: 0.25,
                steps: Some(15),
                lock: HashSet::new(),
                uuid_seed: Some(3),
                verbose: false,
            },
        )
        .unwrap();
        assert!(rep.steps_accepted > 0 && rep.steps_accepted < 15, "accepted={}", rep.steps_accepted);
        assert!(rep.jam.is_some());
        let (pcb, copper) = parse_board(&rep.text).unwrap();
        let b = Board::new(&pcb, &copper, rules);
        assert!(check(&b).is_empty(), "output at last legal step has 0 violations");
        assert!(connectivity_without_pours(&b).airlines.is_empty(), "SIG never ripped");
    }

    #[test]
    fn compact_keeps_off_centre_via_joints_without_adding_acute_hooks() {
        // An F.Cu track ends 0.10 mm off-centre inside a 0.45 mm via's copper disc, and a B.Cu
        // track runs from the via centre to f2. Compacting by 0.80 must keep the off-centre joint
        // connected without emitting a tiny hook segment that would form an acute angle.
        let text = "(kicad_pcb
\t(version 20241229)
\t(general (thickness 1.6))
\t(layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))
\t(net 0 \"\")
\t(net 1 \"SIG\")
\t(gr_line (start 0 0) (end 20 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e1\"))
\t(gr_line (start 20 0) (end 20 10) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e2\"))
\t(gr_line (start 20 10) (end 0 10) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e3\"))
\t(gr_line (start 0 10) (end 0 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e4\"))
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"f1\") (at 4 5)
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 1 \"SIG\") (uuid \"p1\"))
\t)
\t(footprint \"R\" (layer \"B.Cu\") (uuid \"f2\") (at 16 5)
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"B.Cu\") (net 1 \"SIG\") (uuid \"p2\"))
\t)
\t(via (at 10 5) (size 0.45) (drill 0.2) (layers \"F.Cu\" \"B.Cu\") (net 1) (uuid \"v1\"))
\t(segment (start 4 5) (end 9.93 5.08) (width 0.15) (layer \"F.Cu\") (net 1) (uuid \"s1\"))
\t(segment (start 10 5) (end 16 5) (width 0.15) (layer \"B.Cu\") (net 1) (uuid \"s2\"))
)\n";
        let rules = Rules::parse("{}").unwrap();
        let rep = compact(
            text,
            rules.clone(),
            &CompactOptions {
                factor: 0.80,
                steps: Some(8),
                lock: HashSet::new(),
                uuid_seed: Some(4),
                verbose: false,
            },
        )
        .unwrap();
        assert_eq!(rep.steps_accepted, 8, "jam: {:?}", rep.jam);
        assert_eq!(rep.steps[0].acid_traps.acute, 0);
        assert_eq!(rep.steps.last().unwrap().acid_traps.acute, 0);
        let (pcb, copper) = parse_board(&rep.text).unwrap();
        let b = Board::new(&pcb, &copper, rules);
        assert!(check(&b).is_empty(), "violations: {:?}", check(&b));
        assert!(connectivity_without_pours(&b).airlines.is_empty(), "off-centre via stayed joined");
    }

    #[test]
    fn compact_caps_preexisting_subtolerance_courtyard_graze() {
        // Two footprints at x = 4 and x = 5.997 have 2 mm wide courtyards (half-width 1.0 mm) that
        // already overlap by 3 um on their stroke centrelines (inside KiCad's 5 um per-footprint
        // courtyard tolerance). Compaction toward 0.70 must translate the pair toward the board
        // centre without letting their 3 um centreline penetration deepen.
        let text = "(kicad_pcb
\t(version 20241229)
\t(general (thickness 1.6))
\t(layers (0 \"F.Cu\" signal) (44 \"Edge.Cuts\" user) (47 \"F.CrtYd\" user))
\t(net 0 \"\")
\t(net 1 \"SIG\")
\t(gr_line (start 0 0) (end 20 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e1\"))
\t(gr_line (start 20 0) (end 20 10) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e2\"))
\t(gr_line (start 20 10) (end 0 10) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e3\"))
\t(gr_line (start 0 10) (end 0 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e4\"))
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"f1\") (at 4 5)
\t\t(fp_rect (start -1 -1) (end 1 1) (stroke (width 0.05)) (layer \"F.CrtYd\") (uuid \"c1\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 1 \"SIG\") (uuid \"p1\"))
\t)
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"f2\") (at 5.997 5)
\t\t(fp_rect (start -1 -1) (end 1 1) (stroke (width 0.05)) (layer \"F.CrtYd\") (uuid \"c2\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 1 \"SIG\") (uuid \"p2\"))
\t)
\t(segment (start 4 5) (end 5.997 5) (width 0.15) (layer \"F.Cu\") (net 1) (uuid \"s1\"))
)\n";
        let rules = Rules::parse("{}").unwrap();
        let rep = compact(
            text,
            rules,
            &CompactOptions {
                factor: 0.70,
                steps: Some(6),
                lock: HashSet::new(),
                uuid_seed: Some(5),
                verbose: false,
            },
        )
        .unwrap();
        assert_eq!(rep.steps_accepted, 6, "jam: {:?}", rep.jam);
        let (pcb, _) = parse_board(&rep.text).unwrap();
        let dx = pcb.footprints[1].at[0] - pcb.footprints[0].at[0];
        assert!(dx >= 1.997e-3 - 1e-8, "graze did not deepen: dx={dx}");
        // Pair still moved right toward x_centre = 10 mm together via common-mode translation.
        assert!(pcb.footprints[0].at[0] > 4.5e-3, "pair translated inward: x0={}", pcb.footprints[0].at[0]);
    }

    #[test]
    fn solve_half_planes_2d_resolves_acute_wedges_and_antiparallel_pinches() {
        let len = (0.35f64).hypot(0.94);
        let hs = [
            HalfPlane { nx: -0.35 / len, ny: 0.94 / len, need: 6_000.0 },
            HalfPlane { nx: -0.35 / len, ny: -0.94 / len, need: 6_000.0 },
        ];
        let p = solve_half_planes_2d((0.0, 0.0), &hs);
        assert!(p.0 * hs[0].nx + p.1 * hs[0].ny >= 6_000.0 - 1e-6);
        assert!(p.0 * hs[1].nx + p.1 * hs[1].ny >= 6_000.0 - 1e-6);
        assert!(p.1.abs() < 1e-6, "symmetric wedge apex stays centred: y={}", p.1);

        let pinch = [
            HalfPlane { nx: 1.0, ny: 0.0, need: 10_000.0 },
            HalfPlane { nx: -1.0, ny: 0.0, need: 10_000.0 },
        ];
        let c = solve_half_planes_2d((0.0, 0.0), &pinch);
        assert!(c.0.abs() < 1.0, "antiparallel pinch centres symmetrically: x={}", c.0);
    }

    #[test]
    fn compact_stops_opposite_side_via_pinch_locally_while_board_shrinks_to_target() {
        // Opposite-side pinch: U_FRONT (F.Cu, y = 7.08) and U_BACK (B.Cu, y = 8.92) sit on
        // opposite sides of the board with separate F.CrtYd and B.CrtYd courtyards, flanking a
        // 0.60 mm through-via at (10, 8) where SIG transitions from a B.Cu track passing under
        // U_FRONT to an F.Cu track passing over U_BACK. Because U_FRONT and U_BACK are 4.0 mm wide,
        // the via is topologically trapped in the slot; stationary clearance allows only 2.2%
        // shrink (1.84 mm -> 1.80 mm) before jamming on the old code. With per-part rigid-body
        // stops, U_FRONT and U_BACK stop locally across the via while the four outer parts shrink
        // all the way to factor 0.65 (35% linear shrink, 57.75% area saved).
        let text = "(kicad_pcb
\t(version 20241229)
\t(general (thickness 1.6))
\t(layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal) (44 \"Edge.Cuts\" user) (46 \"B.CrtYd\" user) (47 \"F.CrtYd\" user))
\t(net 0 \"\")
\t(net 1 \"SIG\")
\t(net 2 \"FRONT_OBS\")
\t(net 3 \"BACK_OBS\")
\t(net 4 \"RING\")
\t(gr_line (start 0 0) (end 20 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e1\"))
\t(gr_line (start 20 0) (end 20 16) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e2\"))
\t(gr_line (start 20 16) (end 0 16) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e3\"))
\t(gr_line (start 0 16) (end 0 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e4\"))
\t(footprint \"U\" (layer \"F.Cu\") (uuid \"uf\") (at 10 7.08)
\t\t(property \"Reference\" \"U_FRONT\" (at 0 0 0) (layer \"F.SilkS\") (uuid \"rf\"))
\t\t(fp_rect (start -2.1 -0.5) (end 2.1 0.5) (stroke (width 0.05)) (layer \"F.CrtYd\") (uuid \"cf\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 4.0 0.8) (layers \"F.Cu\") (net 2 \"FRONT_OBS\") (uuid \"pf\"))
\t)
\t(footprint \"U\" (layer \"B.Cu\") (uuid \"ub\") (at 10 8.92)
\t\t(property \"Reference\" \"U_BACK\" (at 0 0 0) (layer \"B.SilkS\") (uuid \"rb\"))
\t\t(fp_rect (start -2.1 -0.5) (end 2.1 0.5) (stroke (width 0.05)) (layer \"B.CrtYd\") (uuid \"cb\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 4.0 0.8) (layers \"B.Cu\") (net 3 \"BACK_OBS\") (uuid \"pb\"))
\t)
\t(footprint \"R\" (layer \"B.Cu\") (uuid \"pn\") (at 10 2.5)
\t\t(property \"Reference\" \"P_NORTH\" (at 0 0 0) (layer \"B.SilkS\") (uuid \"rn\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"B.Cu\") (net 1 \"SIG\") (uuid \"pn1\"))
\t)
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"ps\") (at 10 13.5)
\t\t(property \"Reference\" \"P_SOUTH\" (at 0 0 0) (layer \"F.SilkS\") (uuid \"rs\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 1 \"SIG\") (uuid \"ps1\"))
\t)
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"pw\") (at 2.5 8.0)
\t\t(property \"Reference\" \"P_WEST\" (at 0 0 0) (layer \"F.SilkS\") (uuid \"rw\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 4 \"RING\") (uuid \"pw1\"))
\t)
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"pe\") (at 17.5 8.0)
\t\t(property \"Reference\" \"P_EAST\" (at 0 0 0) (layer \"F.SilkS\") (uuid \"re\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 4 \"RING\") (uuid \"pe1\"))
\t)
\t(via (at 10 8) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net 1) (uuid \"v1\"))
\t(segment (start 10.0 2.5) (end 10.0 8.0) (width 0.15) (layer \"B.Cu\") (net 1) (uuid \"s1\"))
\t(segment (start 10.0 8.0) (end 10.0 13.5) (width 0.15) (layer \"F.Cu\") (net 1) (uuid \"s2\"))
\t(segment (start 2.5 8.0) (end 2.5 1.5) (width 0.15) (layer \"F.Cu\") (net 4) (uuid \"s3\"))
\t(segment (start 2.5 1.5) (end 17.5 1.5) (width 0.15) (layer \"F.Cu\") (net 4) (uuid \"s4\"))
\t(segment (start 17.5 1.5) (end 17.5 8.0) (width 0.15) (layer \"F.Cu\") (net 4) (uuid \"s5\"))
)\n";
        let rules = Rules::parse("{}").unwrap();
        let rep = compact(
            text,
            rules.clone(),
            &CompactOptions {
                factor: 0.65,
                steps: Some(14),
                lock: HashSet::new(),
                uuid_seed: Some(11),
                verbose: false,
            },
        )
        .unwrap();
        assert_eq!(rep.steps_accepted, 14, "jam: {:?}", rep.jam);
        assert!((rep.achieved_factor - 0.65).abs() < 1e-9);
        assert!(
            rep.steps.iter().any(|s| s.part_stops > 0),
            "pinched central parts stopped locally while outer parts continued"
        );
        let (pcb, copper) = parse_board(&rep.text).unwrap();
        let fp = |r: &str| pcb.footprints.iter().find(|f| f.reference == r).unwrap();
        // Outer parts P_WEST (x = 2.5) and P_EAST (x = 17.5) shrank by 0.65 around x = 10:
        // span went from 15.0 mm down to ~9.75 mm.
        let outer_span_mm = (fp("P_EAST").at[0] - fp("P_WEST").at[0]) * 1e3;
        assert!(outer_span_mm < 10.0, "outer parts achieved 35% shrink: span={outer_span_mm} mm");
        // Central opposite-side pair U_FRONT and U_BACK kept >= 1.80 mm separation.
        let pinch_dy_mm = (fp("U_BACK").at[1] - fp("U_FRONT").at[1]) * 1e3;
        assert!(pinch_dy_mm >= 1.795, "opposite-side parts respected via clearance: dy={pinch_dy_mm}");

        let b = Board::new(&pcb, &copper, rules);
        assert!(check(&b).is_empty(), "zero DRC violations: {:?}", check(&b));
        assert!(connectivity_without_pours(&b).airlines.is_empty(), "all nets stayed connected");
    }

    #[test]
    fn compact_slides_pinched_through_via_along_its_track_so_front_and_back_parts_close() {
        // Through-via v1 (0.60 mm size, 0.30 mm drill) sits at (10, 8) between U_FRONT on F.Cu
        // (10, 6.90) and U_BACK on B.Cu (10, 9.10), while its net SIG continues east along a track
        // into open space toward R_EAST at (16, 8). Stationary via clearance would block U_FRONT
        // and U_BACK at dy = 1.80 mm (factor ~0.818); sliding the via east along its track lets
        // U_FRONT and U_BACK close all the way to factor 0.65 (dy = 1.430 mm).
        let text = "(kicad_pcb
\t(version 20241229)
\t(general (thickness 1.6))
\t(layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal) (44 \"Edge.Cuts\" user) (46 \"B.CrtYd\" user) (47 \"F.CrtYd\" user))
\t(net 0 \"\")
\t(net 1 \"SIG\")
\t(net 2 \"FRONT_OBS\")
\t(net 3 \"BACK_OBS\")
\t(gr_line (start 0 0) (end 20 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e1\"))
\t(gr_line (start 20 0) (end 20 16) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e2\"))
\t(gr_line (start 20 16) (end 0 16) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e3\"))
\t(gr_line (start 0 16) (end 0 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e4\"))
\t(footprint \"U\" (layer \"F.Cu\") (uuid \"uf\") (at 10 6.90)
\t\t(property \"Reference\" \"U_FRONT\" (at 0 0 0) (layer \"F.SilkS\") (uuid \"rf\"))
\t\t(fp_rect (start -0.8 -0.45) (end 0.8 0.45) (stroke (width 0.05)) (layer \"F.CrtYd\") (uuid \"cf\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 1.4 0.8) (layers \"F.Cu\") (net 2 \"FRONT_OBS\") (uuid \"pf\"))
\t)
\t(footprint \"U\" (layer \"B.Cu\") (uuid \"ub\") (at 10 9.10)
\t\t(property \"Reference\" \"U_BACK\" (at 0 0 0) (layer \"B.SilkS\") (uuid \"rb\"))
\t\t(fp_rect (start -0.8 -0.45) (end 0.8 0.45) (stroke (width 0.05)) (layer \"B.CrtYd\") (uuid \"cb\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 1.4 0.8) (layers \"B.Cu\") (net 3 \"BACK_OBS\") (uuid \"pb\"))
\t)
\t(footprint \"R\" (layer \"B.Cu\") (uuid \"rw\") (at 4 8)
\t\t(property \"Reference\" \"R_WEST\" (at 0 0 0) (layer \"B.SilkS\") (uuid \"rrw\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"B.Cu\") (net 1 \"SIG\") (uuid \"pw\"))
\t)
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"re\") (at 16 8)
\t\t(property \"Reference\" \"R_EAST\" (at 0 0 0) (layer \"F.SilkS\") (uuid \"rre\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 1 \"SIG\") (uuid \"pe\"))
\t)
\t(via (at 10 8) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net 1) (uuid \"v1\"))
\t(segment (start 4 8) (end 10 8) (width 0.15) (layer \"B.Cu\") (net 1) (uuid \"s1\"))
\t(segment (start 10 8) (end 16 8) (width 0.15) (layer \"F.Cu\") (net 1) (uuid \"s2\"))
)\n";
        let rules = Rules::parse("{}").unwrap();
        let rep = compact(
            text,
            rules.clone(),
            &CompactOptions {
                factor: 0.65,
                steps: Some(14),
                lock: HashSet::new(),
                uuid_seed: Some(12),
                verbose: false,
            },
        )
        .unwrap();
        assert_eq!(rep.steps_accepted, 14, "jam: {:?}", rep.jam);
        assert!(
            rep.steps.iter().any(|s| s.via_slides > 0),
            "slide_pinched_vias must slide the pinched via along its track"
        );
        let (pcb, copper) = parse_board(&rep.text).unwrap();
        let fp = |r: &str| pcb.footprints.iter().find(|f| f.reference == r).unwrap();
        let dy_mm = (fp("U_BACK").at[1] - fp("U_FRONT").at[1]) * 1e3;
        assert!(
            dy_mm < 1.50,
            "via escaped the throat so U_FRONT and U_BACK closed past 1.80 mm to {dy_mm:.3} mm"
        );
        assert_eq!(copper.vias.len(), 1);
        assert!(
            (copper.vias[0].at[0] * 1e3 - 10.0).abs() > 0.85,
            "via moved out of the x = 10 mm throat to x = {:.3} mm",
            copper.vias[0].at[0] * 1e3
        );
        let b = Board::new(&pcb, &copper, rules);
        assert!(check(&b).is_empty(), "zero DRC violations: {:?}", check(&b));
        assert!(connectivity_without_pours(&b).airlines.is_empty(), "SIG stayed connected");
    }

    #[test]
    fn compact_keeps_locked_parts_exact_and_produces_byte_identical_output() {
        let text = "(kicad_pcb
\t(version 20241229)
\t(general (thickness 1.6))
\t(layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal) (44 \"Edge.Cuts\" user) (47 \"F.CrtYd\" user))
\t(net 0 \"\")
\t(net 1 \"SIG\")
\t(net 2 \"OBS\")
\t(gr_line (start 0 0) (end 20 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e1\"))
\t(gr_line (start 20 0) (end 20 12) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e2\"))
\t(gr_line (start 20 12) (end 0 12) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e3\"))
\t(gr_line (start 0 12) (end 0 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e4\"))
\t(footprint \"J\" (layer \"F.Cu\") (uuid \"fj\") (at 5.2 6.0)
\t\t(property \"Reference\" \"J_LOCK\" (at 0 0 0) (layer \"F.SilkS\") (uuid \"rj\"))
\t\t(fp_rect (start -0.8 -0.8) (end 0.8 0.8) (stroke (width 0.05)) (layer \"F.CrtYd\") (uuid \"cj\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 1.0 1.0) (layers \"F.Cu\") (net 1 \"SIG\") (uuid \"pj\"))
\t)
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"fr1\") (at 3.0 6.0)
\t\t(property \"Reference\" \"R_NEAR\" (at 0 0 0) (layer \"F.SilkS\") (uuid \"rr1\"))
\t\t(fp_rect (start -0.7 -0.7) (end 0.7 0.7) (stroke (width 0.05)) (layer \"F.CrtYd\") (uuid \"cr1\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 2 \"OBS\") (uuid \"pr1\"))
\t)
\t(footprint \"R\" (layer \"F.Cu\") (uuid \"fr2\") (at 16.5 6.0)
\t\t(property \"Reference\" \"R_FAR\" (at 0 0 0) (layer \"F.SilkS\") (uuid \"rr2\"))
\t\t(fp_rect (start -0.7 -0.7) (end 0.7 0.7) (stroke (width 0.05)) (layer \"F.CrtYd\") (uuid \"cr2\"))
\t\t(pad \"1\" smd rect (at 0 0) (size 0.8 0.8) (layers \"F.Cu\") (net 1 \"SIG\") (uuid \"pr2\"))
\t)
\t(segment (start 5.2 6.0) (end 16.5 6.0) (width 0.2) (layer \"F.Cu\") (net 1) (uuid \"s1\"))
)\n";
        let (orig_pcb, _) = parse_board(text).unwrap();
        let rules = Rules::parse("{}").unwrap();
        let mut lock = HashSet::new();
        lock.insert("J_LOCK".to_string());
        let opts = CompactOptions {
            factor: 0.75,
            steps: Some(10),
            lock,
            uuid_seed: Some(99),
            verbose: false,
        };
        let rep1 = compact(text, rules.clone(), &opts).unwrap();
        let rep2 = compact(text, rules.clone(), &opts).unwrap();
        assert_eq!(rep1.text, rep2.text, "compaction output must be byte-identical");
        assert_eq!(rep1.steps_accepted, 10, "jam: {:?}", rep1.jam);

        let (pcb, copper) = parse_board(&rep1.text).unwrap();
        let fp_at = |p: &Pcb, r: &str| p.footprints.iter().find(|f| f.reference == r).unwrap().at;
        assert_eq!(
            fp_at(&pcb, "J_LOCK"),
            fp_at(&orig_pcb, "J_LOCK"),
            "locked footprint J_LOCK must not move by 1 nm"
        );
        // Board centre x = 10.0 mm, R_FAR starts at 16.5 mm, factor = 0.75 => 10 + 0.75 * 6.5 = 14.875 mm.
        assert!(
            (fp_at(&pcb, "R_FAR")[0] - 14.875e-3).abs() < 5e-6,
            "unlocked R_FAR moved inward to 14.875 mm: x={}",
            fp_at(&pcb, "R_FAR")[0]
        );
        // R_NEAR starts at 3.0 mm (nominal 0.75x = 4.75 mm) and stops at <= 3.70 mm against locked J_LOCK (5.2 mm).
        assert!(
            fp_at(&pcb, "R_NEAR")[0] > 3.5e-3 && fp_at(&pcb, "R_NEAR")[0] <= 3.705e-3,
            "R_NEAR stopped cleanly against locked J_LOCK: x={}",
            fp_at(&pcb, "R_NEAR")[0]
        );
        let b = Board::new(&pcb, &copper, rules);
        assert!(check(&b).is_empty());
        assert!(connectivity_without_pours(&b).airlines.is_empty());
    }
}
