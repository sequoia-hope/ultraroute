//! `fanout_trace ROUTED.kicad_pcb OUT.json [--parts auto|REF,...] [--reroute SECONDS] [--trials N] [--pours-block] [--plough warp|linear|rip|gentle|dipole] [--cascade N]`:
//! the escape pass, step by step, on a routed board, for the lab's fan-out views.
//!
//! For each fine-pitch part (or each named one), on its own copy of the board: the part's edges
//! and lanes, then the copper around it as routed, after combing, after ripping what combing left
//! breaking a rule, and after routing again. Then the lane clearing `--clear-lanes` does, pad by
//! pad, keeping a clearing only when it leaves fewer connections open; and the plough `--shove`
//! runs, every trapped connection round by round. Coordinates are in mm, as
//! in the board file.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use board::{connectivity, connectivity_without_pours, Airline, BBox, Board, Item, ItemId, Kind, Point, Shape};
use serde_json::{json, Value};

fn mm(v: f64) -> f64 {
    (v * 1e-6 * 1e4).round() / 1e4
}

fn xy(p: Point) -> [f64; 2] {
    [mm(p.x as f64), mm(p.y as f64)]
}

fn shape(s: &Shape) -> Value {
    match s {
        Shape::Capsule { a, b, r } => json!({"a": xy(*a), "b": xy(*b), "r": mm(*r as f64)}),
        Shape::Polygon(p) => json!({"poly": p.iter().map(|q| xy(*q)).collect::<Vec<_>>()}),
    }
}

fn layers(board: &Board, it: &Item) -> Vec<usize> {
    (0..board.layers.len()).filter(|l| it.on(*l)).collect()
}

fn copper(board: &Board, id: ItemId, it: &Item) -> Value {
    json!({"id": id, "net": it.net, "via": matches!(it.kind, Kind::Via), "layers": layers(board, it), "shape": shape(&it.shape)})
}

/// The tracks and vias in `window`, with their shapes, for telling one step from the next.
fn routed(board: &Board, window: BBox) -> Vec<(ItemId, Item)> {
    board
        .items()
        .filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && it.shape.bbox().overlaps(window))
        .map(|(id, it)| (id, it.clone()))
        .collect()
}

/// What left and what arrived between two takes of the same window.
fn diff(board_before: &Board, before: &[(ItemId, Item)], board_after: &Board, after: &[(ItemId, Item)]) -> (Vec<Value>, Vec<Value>) {
    let same = |a: &(ItemId, Item), b: &(ItemId, Item)| a.0 == b.0 && a.1.shape == b.1.shape && a.1.net == b.1.net;
    let gone = before.iter().filter(|a| !after.iter().any(|b| same(a, b))).map(|(id, it)| copper(board_before, *id, it)).collect();
    let came = after.iter().filter(|b| !before.iter().any(|a| same(a, b))).map(|(id, it)| copper(board_after, *id, it)).collect();
    (gone, came)
}

fn airline(board: &Board, a: &Airline) -> Value {
    json!({"net": a.net, "name": board.nets[a.net as usize], "from": xy(a.from), "to": xy(a.to), "items": [a.from_item, a.to_item]})
}

fn open(board: &Board, pours_block: bool) -> Vec<Airline> {
    if pours_block { connectivity(board) } else { connectivity_without_pours(board) }.airlines
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: fanout_trace ROUTED.kicad_pcb OUT.json [--parts auto|REF,...] [--reroute SECONDS] [--trials N] [--pours-block] [--plough warp|linear|rip|gentle|dipole] [--cascade N]";
    let (Some(input), Some(output)) = (args.first(), args.get(1)) else { panic!("{usage}") };
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let reroute: f64 = flag("--reroute").map_or(30.0, |s| s.parse().expect("--reroute SECONDS"));
    let trials: usize = flag("--trials").map_or(12, |s| s.parse().expect("--trials N"));
    let pours_block = args.iter().any(|a| a == "--pours-block");
    let (board, _) = Board::load(&PathBuf::from(input)).unwrap_or_else(|e| panic!("{e}"));
    let started = Instant::now();

    let fine = morph::fine_pitch(&board, 500_000);
    let parts: Vec<usize> = match flag("--parts").as_deref() {
        None | Some("auto") => fine.clone(),
        Some(list) => list.split(',').map(|r| board.footprints.iter().position(|f| f.reference == r).unwrap_or_else(|| panic!("no part {r}"))).collect(),
    };
    // The router's copper, as `--adopt` takes it: everything not locked may move.
    let soft: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked).map(|(id, _)| id).collect();
    let opts = morph::EscapeOptions::default();
    let route = |b: &mut Board, soft: &mut Vec<ItemId>, seconds: f64| {
        let s = grid::route_open(b, &mut grid::CpuSolver, &grid::Options { deadline: grid::Deadline::max_seconds(Some(seconds)), pours_block, ..Default::default() }, &mut |_| {});
        soft.extend(s.laid.iter().copied());
        s
    };

    let pads_of = |b: &Board, f: usize| -> Vec<(ItemId, Item)> {
        b.items().filter(|(_, it)| matches!(it.kind, Kind::Pad { footprint, .. } if footprint == f)).map(|(id, it)| (id, it.clone())).collect()
    };
    let mut out_parts = Vec::new();
    for &f in &parts {
        let pads = pads_of(&board, f);
        let Some(pad_box) = pads.iter().map(|(_, p)| p.shape.bbox()).reduce(BBox::union) else { continue };
        let window = pad_box.grow(opts.zone + 1_500_000);
        let edges = morph::edges(&board, f);
        let zone = opts.zone as f64;
        let edge_json: Vec<Value> = edges
            .iter()
            .map(|e| {
                let lanes: Vec<Value> = e.lanes.iter().flat_map(|(net, ts)| ts.iter().map(move |t| (*net, *t))).map(|(net, t)| {
                    let at = |d: f64| Point::new((e.along.0 * t + e.out.0 * (e.row + d)).round() as i64, (e.along.1 * t + e.out.1 * (e.row + d)).round() as i64);
                    json!({"net": net, "name": board.nets[net as usize], "t": mm(t), "from": xy(at(0.0)), "to": xy(at(zone))})
                }).collect();
                json!({"out": [e.out.0, e.out.1], "along": [e.along.0, e.along.1], "row": mm(e.row), "from": mm(e.from), "to": mm(e.to), "lanes": lanes})
            })
            .collect();
        let pad_json: Vec<Value> = pads
            .iter()
            .map(|(id, p)| {
                let Kind::Pad { number, .. } = &p.kind else { unreachable!() };
                json!({"id": id, "number": number, "net": p.net, "layers": layers(&board, p), "shape": shape(&p.shape)})
            })
            .collect();

        // The comb, on a copy of the board.
        let mut b = board.clone();
        let mut s = soft.clone();
        let mut stages = Vec::new();
        let t0 = started.elapsed().as_secs_f64();
        let take0 = routed(&b, window);
        let open0 = open(&b, pours_block);
        stages.push(json!({"name": "routed", "copper": take0.iter().map(|(id, it)| copper(&b, *id, it)).collect::<Vec<_>>(), "open": open0.iter().map(|a| airline(&b, a)).collect::<Vec<_>>()}));
        let before_comb = b.clone();
        let e = morph::comb(&mut b, f, &opts, &mut s);
        let take1 = routed(&b, window);
        let (gone, came) = diff(&before_comb, &take0, &b, &take1);
        let open1 = open(&b, pours_block);
        stages.push(json!({"name": "combed", "combed": e.combed, "pushed_out": e.pushed_out, "gone": gone, "came": came, "copper": take1.iter().map(|(id, it)| copper(&b, *id, it)).collect::<Vec<_>>(), "open": open1.iter().map(|a| airline(&b, a)).collect::<Vec<_>>()}));
        let before_rip = b.clone();
        let (ripped, _) = grid::rip_broken(&mut b, &mut s, pours_block);
        let take2 = routed(&b, window);
        let (gone, _) = diff(&before_rip, &take1, &b, &take2);
        let open2 = open(&b, pours_block);
        stages.push(json!({"name": "ripped", "ripped": ripped, "gone": gone, "copper": take2.iter().map(|(id, it)| copper(&b, *id, it)).collect::<Vec<_>>(), "open": open2.iter().map(|a| airline(&b, a)).collect::<Vec<_>>()}));
        let before_route = b.clone();
        let r = route(&mut b, &mut s, reroute);
        let take3 = routed(&b, window);
        let (gone, came) = diff(&before_route, &take2, &b, &take3);
        let open3 = open(&b, pours_block);
        stages.push(json!({"name": "rerouted", "routed": r.routed, "failed": r.failed, "gone": gone, "came": came, "copper": take3.iter().map(|(id, it)| copper(&b, *id, it)).collect::<Vec<_>>(), "open": open3.iter().map(|a| airline(&b, a)).collect::<Vec<_>>()}));
        let reference = board.footprints[f].reference.clone();
        eprintln!(
            "{reference}: {} lanes, {} combed, {} pushed out, {ripped} ripped, {} rerouted; open {} -> {} -> {} -> {} ({:.1} s)",
            e.lanes,
            e.combed,
            e.pushed_out,
            r.routed,
            open0.len(),
            open1.len(),
            open2.len(),
            open3.len(),
            started.elapsed().as_secs_f64() - t0
        );

        // Lane clearing, pad by pad, as `--clear-lanes` does it, on the routed board.
        let mut b = board.clone();
        let mut s = soft.clone();
        let mut done: HashSet<ItemId> = HashSet::new();
        let mut trial_json = Vec::new();
        while trial_json.len() < trials {
            let before_open = open(&b, pours_block);
            let Some(pad) = before_open.iter().flat_map(|c| [c.from_item, c.to_item]).find(|id| {
                !done.contains(id) && b.item(*id).is_some_and(|it| matches!(it.kind, Kind::Pad { footprint, .. } if footprint == f))
            }) else {
                break;
            };
            done.insert(pad);
            let snapshot = (b.clone(), s.clone());
            let take0 = routed(&b, window);
            let (moved, _) = morph::clear_lane(&mut b, f, pad, &opts, &mut s);
            let take1 = routed(&b, window);
            let (slid_from, slid_to) = diff(&snapshot.0, &take0, &b, &take1);
            let before_rip = b.clone();
            let (ripped, _) = grid::rip_broken(&mut b, &mut s, pours_block);
            let take2 = routed(&b, window);
            let (rip_gone, _) = diff(&before_rip, &take1, &b, &take2);
            let before_route = b.clone();
            let r = route(&mut b, &mut s, 20.0);
            let take3 = routed(&b, window);
            let (_, laid) = diff(&before_route, &take2, &b, &take3);
            let after_open = open(&b, pours_block);
            let kept = after_open.len() < before_open.len();
            let it = board.item(pad).cloned();
            let (number, net) = match &it {
                Some(Item { kind: Kind::Pad { number, .. }, net, .. }) => (number.clone(), *net),
                _ => (String::new(), 0),
            };
            let own_open = after_open.iter().any(|c| c.from_item == pad || c.to_item == pad);
            eprintln!(
                "  clear {reference}-{number} ({}): {moved} slid, {ripped} ripped, {} rerouted, open {} -> {}{}",
                board.nets[net as usize],
                r.routed,
                before_open.len(),
                after_open.len(),
                if kept { ", kept" } else { ", undone" }
            );
            trial_json.push(json!({
                "pad": pad, "number": number, "net": net, "name": board.nets[net as usize],
                "moved": moved, "ripped": ripped, "routed": r.routed, "failed": r.failed,
                "open_before": before_open.len(), "open_after": after_open.len(), "own_open": own_open, "kept": kept,
                "base": take0.iter().map(|(id, it)| copper(&snapshot.0, *id, it)).collect::<Vec<_>>(),
                "slid_from": slid_from, "slid_to": slid_to, "ripped_items": rip_gone, "laid": laid,
                "newly_open": after_open.iter().filter(|c| !before_open.iter().any(|o| o.from_item == c.from_item && o.to_item == c.to_item)).map(|a| airline(&b, a)).collect::<Vec<_>>(),
            }));
            if !kept {
                (b, s) = snapshot;
            }
        }

        out_parts.push(json!({
            "ref": reference,
            "fine": fine.contains(&f),
            "window": [xy(window.min), xy(window.max)],
            "pads": pad_json,
            "edges": edge_json,
            "stages": stages,
            "trials": trial_json,
        }));
    }

    // The plough: every trapped connection at a fine-pitch part, round by round, on one copy of
    // the board, as `--shove` runs it.
    let shove_json = {
        let mut b = board.clone();
        let mut s = soft.clone();
        let fine_set: HashSet<usize> = fine.iter().copied().collect();
        let plough = match flag("--plough").as_deref() {
            None | Some("warp") => grid::Plough::Warp,
            Some("linear") => grid::Plough::Linear,
            Some("rip") => grid::Plough::Rip,
            Some("gentle") => grid::Plough::Gentle,
            Some("dipole") => grid::Plough::Dipole,
            Some(other) => panic!("--plough {other}: warp, linear, rip, gentle or dipole"),
        };
        let cascade = flag("--cascade").map_or(grid::ShoveOptions::default().cascade, |s| s.parse().expect("--cascade N"));
        let opts = grid::ShoveOptions { parts: Some(fine_set), reroute_seconds: 20.0, pours_block, plough, cascade, ..Default::default() };
        let mut tries: Vec<Value> = Vec::new();
        let mut window = BBox::of([Point::new(0, 0)]);
        let mut last: (Board, Vec<(ItemId, Item)>) = (board.clone(), Vec::new());
        let t0 = Instant::now();
        let stats = grid::shove_escape(&mut b, &mut grid::CpuSolver, &opts, &mut s, &mut |now: &Board, event| {
            let open_here = open(now, pours_block);
            match event {
                grid::ShoveEvent::Start(t) => {
                    let at = now.item(t.pad).map_or(t.to, |it| { let bb = it.shape.bbox(); Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2) });
                    window = BBox::of([at, t.to]).grow(2_500_000);
                    let take = routed(now, window);
                    let (reference, number) = match now.item(t.pad).map(|it| &it.kind) {
                        Some(Kind::Pad { footprint, number, .. }) => (now.footprints[*footprint].reference.clone(), number.clone()),
                        _ => (String::new(), String::new()),
                    };
                    tries.push(json!({
                        "pad": t.pad, "ref": reference, "number": number, "net": t.net, "name": now.nets[t.net as usize],
                        "at": xy(at), "to": xy(t.to), "window": [xy(window.min), xy(window.max)],
                        "stages": [{"name": "trapped", "copper": take.iter().map(|(id, it)| copper(now, *id, it)).collect::<Vec<_>>(), "open": open_here.iter().map(|a| airline(now, a)).collect::<Vec<_>>()}],
                    }));
                    last = (now.clone(), take);
                }
                grid::ShoveEvent::Round(_, r) => {
                    let take = routed(now, window);
                    let (gone, came) = diff(&last.0, &last.1, now, &take);
                    let stage = json!({
                        "name": format!("round {}", tries.last().map_or(0, |t| t["stages"].as_array().map_or(0, Vec::len))),
                        "path": r.path.iter().map(|(l, p)| json!([l, xy(*p)[0], xy(*p)[1]])).collect::<Vec<_>>(),
                        "soft_cells": r.soft_cells, "crossed": r.crossed, "moved": r.moved, "ripped": r.ripped, "routed": r.routed,
                        "gone": gone, "came": came,
                        "ripped_items": r.ripped_items.iter().filter(|it| it.shape.bbox().overlaps(window)).map(|it| copper(now, usize::MAX, it)).collect::<Vec<_>>(),
                        "copper": take.iter().map(|(id, it)| copper(now, *id, it)).collect::<Vec<_>>(),
                        "open": open_here.iter().map(|a| airline(now, a)).collect::<Vec<_>>(),
                    });
                    if let Some(t) = tries.last_mut() { t["stages"].as_array_mut().unwrap().push(stage); }
                    last = (now.clone(), take);
                }
                grid::ShoveEvent::Done(t) => {
                    let take = routed(now, window);
                    let (gone, came) = diff(&last.0, &last.1, now, &take);
                    let stage = json!({
                        "name": "rerouted",
                        "gone": gone, "came": came,
                        "copper": take.iter().map(|(id, it)| copper(now, *id, it)).collect::<Vec<_>>(),
                        "open": open_here.iter().map(|a| airline(now, a)).collect::<Vec<_>>(),
                    });
                    if let Some(j) = tries.last_mut() {
                        j["stages"].as_array_mut().unwrap().push(stage);
                        j["routed"] = json!(t.routed);
                        j["kept"] = json!(t.kept);
                        j["open_before"] = json!(t.open_before);
                        j["open_after"] = json!(t.open_after);
                    }
                }
            }
        });
        let open_end = open(&b, pours_block).len();
        eprintln!(
            "shove: {} tried, {} routed out, {} kept; open {} -> {open_end} ({:.1} s)",
            stats.tries.len(),
            stats.tries.iter().filter(|t| t.routed).count(),
            stats.tries.iter().filter(|t| t.kept).count(),
            open(&board, pours_block).len(),
            t0.elapsed().as_secs_f64()
        );
        for t in &stats.tries {
            eprintln!("  {} {}: {} rounds, {}, open {} -> {}, {}", board.nets[t.net as usize], grid::item_name(&board, t.pad), t.rounds.len(), if t.routed { "routed" } else { "trapped" }, t.open_before, t.open_after, if t.kept { "kept" } else { "undone" });
        }
        json!({"tries": tries, "open_before": open(&board, pours_block).len(), "open_after": open_end, "options": {"reach": mm(opts.reach as f64), "rounds": opts.rounds, "margin": mm(opts.margin as f64), "plough": format!("{:?}", opts.plough).to_lowercase(), "cascade": opts.cascade}})
    };

    // The board around the parts: every pad, the pours and the outline, drawn under the copper.
    let mut windows: Vec<BBox> = parts.iter().filter_map(|&f| pads_of(&board, f).iter().map(|(_, p)| p.shape.bbox()).reduce(BBox::union)).map(|b| b.grow(opts.zone + 1_500_000)).collect();
    for t in shove_json["tries"].as_array().into_iter().flatten() {
        let w = &t["window"];
        let at = |k: usize| Point::new((w[k][0].as_f64().unwrap() * 1e6) as i64, (w[k][1].as_f64().unwrap() * 1e6) as i64);
        windows.push(BBox::of([at(0), at(1)]));
    }
    let near = |it: &Item| windows.iter().any(|w| it.shape.bbox().overlaps(*w));
    let pads: Vec<Value> = board
        .items()
        .filter(|(_, it)| matches!(it.kind, Kind::Pad { .. }) && near(it))
        .map(|(id, it)| {
            let Kind::Pad { footprint, number, .. } = &it.kind else { unreachable!() };
            json!({"id": id, "ref": board.footprints[*footprint].reference, "number": number, "net": it.net, "layers": layers(&board, it), "shape": shape(&it.shape)})
        })
        .collect();
    let pours: Vec<Value> = board
        .items()
        .filter_map(|(_, it)| match it.kind {
            Kind::Pour { zone, kept } if near(it) => Some(json!({"zone": zone, "kept": kept, "net": it.net, "layers": layers(&board, it), "shape": shape(&it.shape)})),
            _ => None,
        })
        .collect();
    let blocks: Vec<Value> = board.blocks.iter().map(|bl| json!({"name": bl.name, "parts": bl.footprints.iter().map(|&f| board.footprints[f].reference.clone()).collect::<Vec<_>>()})).collect();
    let trace = json!({
        "board": input,
        "layers": board.layers,
        "nets": board.nets,
        "blocks": blocks,
        "options": {"zone": mm(opts.zone as f64), "full": opts.full, "clear": mm(opts.clear as f64), "reroute": reroute, "pours_block": pours_block},
        "pads": pads,
        "pours": pours,
        "parts": out_parts,
        "shove": shove_json,
        "seconds": started.elapsed().as_secs_f64(),
    });
    std::fs::write(output, serde_json::to_string(&trace).unwrap()).unwrap_or_else(|e| panic!("{output}: {e}"));
    eprintln!("wrote {output}, {:.1} s", started.elapsed().as_secs_f64());
}
