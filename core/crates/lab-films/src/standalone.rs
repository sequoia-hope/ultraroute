//! The films of what is not a stage of a run: a part moved by the copper flow, and the zone fill;
//! and the drawing helpers the stage films share.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use board::{BBox, Board, Nm, Point};
use serde_json::{json, Value};

use crate::film::*;
use crate::{prepared, Cli, USAGE};

pub(crate) fn parse_xy(s: &str) -> Point {
    let (x, y) = s.split_once(',').unwrap_or_else(|| panic!("expected X,Y in mm, got {s}"));
    Point::new(nm_mm(x.trim().parse().expect("X mm")), nm_mm(y.trim().parse().expect("Y mm")))
}


// --- 3. Flow -------------------------------------------------------------------------------

pub(crate) fn run_flow(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 3, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let (ref_name, delta_str) = pos[2].split_once('=').expect("expected REF=DX,DY in mm");
    let delta = parse_xy(delta_str);
    let steps: usize = cli.flag("--steps").map_or(16, |s| s.parse().expect("--steps")).max(1);
    let do_rip = cli.has("--rip");
    let reroute_s: Option<f64> = cli.flag("--reroute").map(|s| s.parse().expect("--reroute"));
    let pours_block = cli.pours_block();
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));

    let (mut board, _) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    let fp = board
        .footprints
        .iter()
        .position(|f| f.reference == ref_name)
        .unwrap_or_else(|| panic!("no footprint {ref_name}"));
    let mut soft = prepared(cli, &mut board, board_path).unwrap_or_else(|| adopt_soft(&mut board));
    let c0 = part_centre(&board, fp);
    let window = cli.crop_override().unwrap_or_else(|| BBox::of([c0, Point::new(c0.x + delta.x, c0.y + delta.y)]).grow(5_500_000));

    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "flow",
            out_path: &out_path,
            board_path,
            cropped: true,
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| format!("Harmonic copper flow moving {ref_name}")),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| format!("shift ({:+.2}, {:+.2}) mm across {steps} warp steps", mm(delta.x as f64), mm(delta.y as f64))),
        },
        &board,
        window,
        pours_block,
    );

    let full_moves = HashMap::from([(fp, delta)]);
    let full_warp = morph::warp_for(&board, &full_moves);
    let init_arrows = sample_arrows(window, 22, |p| full_warp.displacement(p));
    let init_copper = fb.snapshot_copper(&board);
    let open0 = open_airlines(&board, pours_block).len();

    fb.push_frame(
        &board,
        format!("Initial board: {ref_name} about to move ({:+.2}, {:+.2}) mm; harmonic warp field shown", mm(delta.x as f64), mm(delta.y as f64)),
        FrameOverlays {
            initial: true,
            important: true,
            arrows: init_arrows,
            stats: vec![
                ("part".into(), json!(ref_name)),
                ("step".into(), json!(format!("0 / {steps}"))),
                ("open".into(), json!(open0)),
            ],
            ..Default::default()
        },
    );

    let mut moved_so_far = Point::new(0, 0);
    for step in 1..=steps {
        let target = Point::new(
            (delta.x as f64 * step as f64 / steps as f64).round() as Nm,
            (delta.y as f64 * step as f64 / steps as f64).round() as Nm,
        );
        let step_by = Point::new(target.x - moved_so_far.x, target.y - moved_so_far.y);
        let prev_offset = moved_so_far;
        moved_so_far = target;

        let step_moves = HashMap::from([(fp, step_by)]);
        let warp = morph::warp_for(&board, &step_moves);
        // Scale step arrows by `steps` so the visual vector field stays legible at every frame.
        let arrows = sample_arrows(window, 22, |p| {
            let (ux, uy) = warp.displacement(p);
            (ux * steps as f64, uy * steps as f64)
        });
        let f_stats = morph::flow(&mut board, &step_moves, &mut soft);
        let now_copper = fb.snapshot_copper(&board);
        let (gone_init, came_init) = diff_pools(&init_copper, &now_copper);
        let clashes = count_violations(&board, pours_block);
        let open_now = open_airlines(&board, pours_block).len();

        fb.push_frame(
            &board,
            format!(
                "Step {step}/{steps}: {ref_name} at ({:+.2}, {:+.2}) mm, {} copper pieces flowed, {clashes} rule clashes, {open_now} open",
                mm(target.x as f64),
                mm(target.y as f64),
                f_stats.flowed
            ),
            FrameOverlays {
                important: step == steps,
                moved_from: gone_init,
                moved_to: came_init,
                moved_parts: vec![json!({
                    "fp": fp,
                    "ref": ref_name,
                    "from": if step == steps { [0.0, 0.0] } else { [mm(prev_offset.x as f64), mm(prev_offset.y as f64)] },
                    "to": [mm(target.x as f64), mm(target.y as f64)],
                    "status": "moved",
                })],
                arrows,
                stats: vec![
                    ("step".into(), json!(format!("{step} / {steps}"))),
                    ("flowed".into(), json!(f_stats.flowed)),
                    ("clashes".into(), json!(clashes)),
                    ("open".into(), json!(open_now)),
                ],
                ..Default::default()
            },
        );
    }

    if !do_rip && reroute_s.is_none() {
        let after = fb.snapshot_copper(&board);
        let (init_gone, init_came) = diff_pools(&init_copper, &after);
        let clashes = count_violations(&board, pours_block);
        let open_now = open_airlines(&board, pours_block).len();
        fb.push_frame(
            &board,
            format!(
                "Harmonic flow complete: {ref_name} shifted ({:+.2}, {:+.2}) mm, {} copper pieces stretched ({clashes} clashes, {open0} -> {open_now} open)",
                mm(delta.x as f64),
                mm(delta.y as f64),
                init_came.len()
            ),
            FrameOverlays {
                important: true,
                summary: true,
                moved_from: init_gone,
                moved_to: init_came.clone(),
                moved_parts: vec![json!({
                    "fp": fp,
                    "ref": ref_name,
                    "from": [0.0, 0.0],
                    "to": [mm(delta.x as f64), mm(delta.y as f64)],
                    "status": "kept",
                })],
                stats: vec![
                    ("part".into(), json!(ref_name)),
                    ("flowed".into(), json!(init_came.len())),
                    ("clashes".into(), json!(clashes)),
                    ("open".into(), json!(open_now)),
                ],
                ..Default::default()
            },
        );
    }

    if do_rip {
        let (ripped_items, _) = grid::rip_broken_items(&mut board, &mut soft, pours_block);
        let ripped = fb.intern_items(&board, &ripped_items);
        let open_now = open_airlines(&board, pours_block).len();
        fb.push_frame(
            &board,
            format!("Ripped {} pieces pushed into obstacles; {open_now} open", ripped_items.len()),
            FrameOverlays {
                important: true,
                ripped,
                stats: vec![("ripped".into(), json!(ripped_items.len())), ("open".into(), json!(open_now))],
                ..Default::default()
            },
        );
    }
    if let Some(secs) = reroute_s {
        let before = fb.snapshot_copper(&board);
        let r = grid::route_open(
            &mut board,
            &mut grid::CpuSolver,
            &grid::Options { deadline: grid::Deadline::max_seconds(Some(secs)), pours_block, ..Default::default() },
            &mut |_| {},
        );
        soft.extend(r.laid);
        let after = fb.snapshot_copper(&board);
        let (_, laid) = diff_pools(&before, &after);
        let laid_set: HashSet<u32> = laid.iter().copied().collect();
        let (init_gone, init_came) = diff_pools(&init_copper, &after);
        let moved_to: Vec<u32> = init_came.into_iter().filter(|id| !laid_set.contains(id)).collect();
        let flowed_cnt = moved_to.len();
        let open_now = open_airlines(&board, pours_block).len();
        fb.push_frame(
            &board,
            format!("Rerouted {} connections ({} failed); {open0} -> {open_now} open", r.routed, r.failed),
            FrameOverlays {
                important: true,
                summary: true,
                moved_from: init_gone,
                moved_to,
                laid,
                moved_parts: vec![json!({
                    "fp": fp,
                    "ref": ref_name,
                    "from": [0.0, 0.0],
                    "to": [mm(delta.x as f64), mm(delta.y as f64)],
                    "status": "kept",
                })],
                stats: vec![
                    ("flowed".into(), json!(flowed_cnt)),
                    ("rerouted".into(), json!(r.routed)),
                    ("open".into(), json!(open_now)),
                ],
                ..Default::default()
            },
        );
    }
    fb.write(&out_path);
}


pub(crate) fn build_heatmap(rv: &grid::RoundView, window: BBox) -> Value {
    let g = rv.grid;
    let bin = 4usize; // 4x4 grid cells = 0.2 mm tiles at 0.05 mm cell size
    let bnx = g.nx.div_ceil(bin);
    let bny = g.ny.div_ceil(bin);
    let cells_per_layer = g.nx * g.ny;
    let layers = g.layers.len();
    let step_nm = g.h * bin as Nm;
    let mut cells = Vec::new();
    for bj in 0..bny {
        for bi in 0..bnx {
            let p = Point::new(g.x0 + (bi * bin) as Nm * g.h, g.y0 + (bj * bin) as Nm * g.h);
            if !window.grow(step_nm).contains(p) {
                continue;
            }
            let mut max_over = 0u16;
            let mut max_hist = 0u32;
            for l in 0..layers {
                for dj in 0..bin {
                    let j = bj * bin + dj;
                    if j >= g.ny {
                        continue;
                    }
                    for di in 0..bin {
                        let i = bi * bin + di;
                        if i >= g.nx {
                            continue;
                        }
                        let idx = l * cells_per_layer + j * g.nx + i;
                        max_over = max_over.max(rv.overlap[idx]);
                        max_hist = max_hist.max(rv.history[idx]);
                    }
                }
            }
            if max_over > 1 || max_hist > 0 {
                cells.push(json!([bi, bj, max_over, max_hist]));
            }
        }
    }
    json!({
        "x0": mm(g.x0 as f64),
        "y0": mm(g.y0 as f64),
        "step": mm(step_nm as f64),
        "nx": bnx,
        "ny": bny,
        "cells": cells,
    })
}


pub(crate) fn focus_json(board: &Board, focus: Option<&grid::Focus>, failed: bool) -> Value {
    match focus {
        Some(grid::Focus::Connection { net, from, to }) if from == to => json!({"net": net, "at": xy(*from), "failed": failed}),
        Some(grid::Focus::Connection { net, from, to }) => json!({"net": net, "from": xy(*from), "to": xy(*to), "failed": failed}),
        Some(grid::Focus::Part(fp)) => json!({"at": xy(part_centre(board, *fp)), "failed": failed}),
        _ => Value::Null,
    }
}

// --- 18. Fill ------------------------------------------------------------------------------

pub(crate) fn run_fill(cli: &Cli) {
    let pos = cli.positional();
    assert!(pos.len() >= 2, "{USAGE}");
    let (board_path, out_path) = (&pos[0], PathBuf::from(&pos[1]));
    let pours_block = cli.pours_block();
    let max_frames = cli.flag("--max-frames").map_or(200, |s| s.parse().expect("--max-frames"));

    let (mut board, text) = Board::load(Path::new(board_path)).unwrap_or_else(|e| panic!("{e}"));
    prepared(cli, &mut board, board_path);
    let (_, copper) = kicad_pcb::parse_board(&text).unwrap_or_else(|e| panic!("{board_path}: {e}"));
    let window = cli.crop_override().unwrap_or_else(|| board_bounds(&board));
    let mut fb = FilmBuilder::new(
        FilmSpec {
            feature: "fill",
            out_path: &out_path,
            board_path,
            cropped: cli.crop_override().is_some(),
            max_frames,
            title: cli.flag("--title").unwrap_or_else(|| "Zone fills as KiCad would pour them".to_string()),
            subtitle: cli.flag("--subtitle").unwrap_or_else(|| format!("board::fill_zones on a {} µm raster, highest priority first", board::fill::STEP / 1000)),
        },
        &board,
        window,
        pours_block,
    );
    // The fills the file holds are stale once the router has moved copper; only fresh ones show.
    fb.pours.clear();

    let started = Instant::now();
    let fills = board::fill_zones(&board, &copper);
    let secs = started.elapsed().as_secs_f64();
    let zone_name = |z: usize| {
        let zone = &copper.zones[z];
        if zone.name.is_empty() { format!("zone {z}") } else { zone.name.clone() }
    };
    let area = |f: &board::Fill| f.rects.iter().map(|r| (r[2] - r[0]) as f64 * (r[3] - r[1]) as f64).sum::<f64>() * 1e-12;
    fb.extra.insert(
        "fills".into(),
        json!(fills
            .iter()
            .map(|f| json!({
                "zone": f.zone,
                "name": zone_name(f.zone),
                "net": f.net,
                "layer": f.layer,
                "r": f.rects.iter().flat_map(|r| [mm(r[0] as f64), mm(r[1] as f64), mm(r[2] as f64), mm(r[3] as f64)]).collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>()),
    );

    let open0 = open_airlines(&board, pours_block).len();
    let zones = fills.iter().map(|f| f.zone).collect::<HashSet<_>>().len();
    fb.push_frame(
        &board,
        format!("Routed board, zones unpoured: {zones} zones to fill on {} layer pours; {open0} open without pours", fills.len()),
        FrameOverlays {
            initial: true,
            important: true,
            stats: vec![("zones".into(), json!(zones)), ("poured".into(), json!(0)), ("open".into(), json!(open0))],
            ..Default::default()
        },
    );
    for (k, f) in fills.iter().enumerate() {
        fb.push_frame(
            &board,
            format!(
                "Poured {} ({}) on {}: {:.1} mm² in {} rectangles",
                zone_name(f.zone),
                board.net_name(f.net),
                board.layers[f.layer],
                area(f),
                f.rects.len()
            ),
            FrameOverlays {
                important: true,
                fills: (0..=k).collect(),
                fill_new: Some(k),
                stats: vec![
                    ("zone".into(), json!(zone_name(f.zone))),
                    ("layer".into(), json!(board.layers[f.layer])),
                    ("mm²".into(), json!((area(f) * 10.0).round() / 10.0)),
                    ("poured".into(), json!(format!("{} / {}", k + 1, fills.len()))),
                ],
                ..Default::default()
            },
        );
    }
    let total: f64 = fills.iter().map(area).sum();
    fb.push_frame(
        &board,
        format!("All {} pours filled in {secs:.2} s: {total:.0} mm² of copper over {zones} zones", fills.len()),
        FrameOverlays {
            important: true,
            summary: true,
            fills: (0..fills.len()).collect(),
            stats: vec![
                ("pours".into(), json!(fills.len())),
                ("mm²".into(), json!(total.round())),
                ("seconds".into(), json!((secs * 100.0).round() / 100.0)),
                ("open".into(), json!(open0)),
            ],
            ..Default::default()
        },
    );
    fb.write(&out_path);
}

