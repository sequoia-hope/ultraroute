//! `why_blocked BOARD.kicad_pcb REF-PAD [CELL_MM]`: the routing grid around a pad, as the grid
//! router sees it for the pad's net: how far a path can get from the pad, a map of each layer
//! near it, and the copper that walls in what the pad can reach.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use board::{Board, Kind, Point, Shape};
use grid::{terminal_cells, ObstacleMap, Profile};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(path), Some(which)) = (args.first(), args.get(1)) else { panic!("usage: why_blocked BOARD.kicad_pcb REF-PAD [CELL_MM]") };
    let cell = args.get(2).map_or(50_000, |c| (c.parse::<f64>().expect("CELL_MM") * 1e6) as i64);
    let (mut board, _) = Board::load(&PathBuf::from(path)).unwrap_or_else(|e| panic!("{e}"));
    let (reference, number) = which.split_once('-').expect("REF-PAD");
    let f = board.footprints.iter().position(|f| f.reference == reference).expect("no such part");
    let (pad, net) = board
        .items()
        .find(|(_, it)| matches!(&it.kind, Kind::Pad { footprint, number: n, .. } if *footprint == f && n == number))
        .map(|(id, it)| (id, it.net))
        .expect("no such pad");
    let profile = Profile::for_net(&board, net);
    println!("{which}: net {} ({}), half width {} nm, clearance {} nm, cell {cell} nm", board.net_name(net), profile.class, profile.half_width, profile.clearance);
    let grid = ObstacleMap::new(&board, profile, cell, false).view(net);
    let sources = terminal_cells(&mut board, &grid, pad);
    println!("{} terminal cells", sources.len());
    // Flood the free cells from the terminals: steps on a layer, and vias where one may stand.
    let per = grid.nx * grid.ny;
    let mut seen = vec![false; grid.blocked.len()];
    let mut queue: VecDeque<usize> = sources.iter().copied().collect();
    for &s in &sources {
        seen[s] = true;
    }
    while let Some(n) = queue.pop_front() {
        let (l, c) = (n / per, n % per);
        let (i, j) = ((c % grid.nx) as i64, (c / grid.nx) as i64);
        let mut next = Vec::new();
        for (di, dj) in [(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)] {
            let (a, b) = (i + di, j + dj);
            if a >= 0 && b >= 0 && (a as usize) < grid.nx && (b as usize) < grid.ny {
                next.push(l * per + b as usize * grid.nx + a as usize);
            }
        }
        if grid.via.is_some() && !grid.via_blocked[c] {
            next.extend((0..grid.layers.len()).map(|k| k * per + c));
        }
        for m in next {
            if !seen[m] && !grid.blocked[m] {
                seen[m] = true;
                queue.push_back(m);
            }
        }
    }
    let reached = seen.iter().filter(|s| **s).count();
    println!("{reached} cells reachable from the pad");
    let centre = board.item(pad).map(|it| { let b = it.shape.bbox(); Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2) }).unwrap();
    let (ci, cj) = grid.cell_at(centre).expect("pad off the grid");
    let r = (700_000 / cell) as usize;
    for (k, &layer) in grid.layers.iter().enumerate() {
        println!("-- {} ('S' terminal, '+' reached, '#' blocked, '.' free but out of reach)", board.layers[layer]);
        for j in cj.saturating_sub(r)..(cj + r).min(grid.ny) {
            let row: String = (ci.saturating_sub(r)..(ci + r).min(grid.nx))
                .map(|i| {
                    let n = k * per + j * grid.nx + i;
                    if sources.contains(&n) { 'S' } else if seen[n] { '+' } else if grid.blocked[n] { '#' } else { '.' }
                })
                .collect();
            println!("{row}");
        }
    }
    // Everything within half a millimetre of the pad, on its layers.
    println!("-- near the pad:");
    let bb = board.item(pad).unwrap().shape.bbox().grow(500_000);
    let pad_shape = board.item(pad).unwrap().shape.clone();
    for &layer in &grid.layers {
        for id in board.near(layer, bb) {
            let Some(it) = board.item(id) else { continue };
            if id == pad || !it.on(layer) {
                continue;
            }
            let what = match &it.kind {
                Kind::Pad { footprint, number, .. } => format!("pad {}-{number}", board.footprints[*footprint].reference),
                Kind::Track => format!("track{}", if it.locked { " (locked)" } else { "" }),
                Kind::Via => "via".into(),
                Kind::Pour { kept, zone } => format!("{}pour (zone {zone})", if *kept { "kept " } else { "" }),
                Kind::Keepout { footprint, forbids } => format!("keepout of {} {:?}", footprint.map_or("the board".into(), |f| board.footprints[f].reference.clone()), forbids),
            };
            println!("  {:>8.3} mm  {what} {} on {}", it.shape.gap(&pad_shape) * 1e-6, board.net_name(it.net), board.layers[layer]);
        }
    }
    // What walls in the reached region: copper within a path's reach of a blocked cell next to it.
    let mut walls: BTreeMap<String, usize> = BTreeMap::new();
    let reach = (grid.half_width + 300_000) as f64;
    for n in (0..seen.len()).filter(|n| seen[*n]) {
        let (l, c) = (n / per, n % per);
        let (i, j) = (c % grid.nx, c / grid.nx);
        let at = grid.centre(i, j);
        let layer = grid.layers[l];
        let blocked_next = [(1i64, 0i64), (-1, 0), (0, 1), (0, -1)].iter().any(|(di, dj)| {
            let (a, b) = (i as i64 + di, j as i64 + dj);
            a >= 0 && b >= 0 && (a as usize) < grid.nx && (b as usize) < grid.ny && grid.blocked[l * per + b as usize * grid.nx + a as usize]
        });
        if !blocked_next {
            continue;
        }
        let probe = Shape::Capsule { a: at, b: at, r: grid.half_width };
        for id in board.near(layer, board::BBox::of([at]).grow(reach as i64 + 200_000)) {
            let Some(it) = board.item(id) else { continue };
            if it.net == net && net != 0 || !it.on(layer) {
                continue;
            }
            if it.shape.gap(&probe) < reach - grid.half_width as f64 {
                let what = match &it.kind {
                    Kind::Pad { footprint, number, .. } => format!("pad {}-{number} ({})", board.footprints[*footprint].reference, board.net_name(it.net)),
                    Kind::Track => format!("track {}{}", board.net_name(it.net), if it.locked { " (locked)" } else { "" }),
                    Kind::Via => format!("via {}", board.net_name(it.net)),
                    Kind::Pour { kept, .. } => format!("{}pour {}", if *kept { "kept " } else { "" }, board.net_name(it.net)),
                    Kind::Keepout { footprint, .. } => format!("keepout {}", footprint.map_or("board".into(), |f| board.footprints[f].reference.clone())),
                };
                *walls.entry(format!("{} on {}", what, board.layers[layer])).or_default() += 1;
            }
        }
    }
    println!("-- what walls it in (cell contacts):");
    let mut walls: Vec<_> = walls.into_iter().collect();
    walls.sort_by_key(|b| std::cmp::Reverse(b.1));
    for (w, n) in walls.iter().take(25) {
        println!("{n:5}  {w}");
    }
}
