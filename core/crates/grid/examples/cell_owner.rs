//! `cell_owner BOARD.kicad_pcb REF-PAD`: which items block the cells inside a pad, for the pad's
//! own net: each nearby item is left out of the map in turn, and the cells it frees are counted.

use std::path::PathBuf;

use board::{Board, Kind};
use grid::{ObstacleMap, Profile};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(path), Some(which)) = (args.first(), args.get(1)) else { panic!("usage: cell_owner BOARD.kicad_pcb REF-PAD") };
    let (board, _) = Board::load(&PathBuf::from(path)).unwrap_or_else(|e| panic!("{e}"));
    let (reference, number) = which.split_once('-').expect("REF-PAD");
    let f = board.footprints.iter().position(|f| f.reference == reference).expect("no such part");
    let pads: Vec<_> = board
        .items()
        .filter(|(_, it)| matches!(&it.kind, Kind::Pad { footprint, number: n, .. } if *footprint == f && n == number))
        .map(|(id, it)| (id, it.clone()))
        .collect();
    let net = pads[0].1.net;
    let profile = Profile::for_net(&board, net);
    let h = 50_000;
    let inside = |map: &ObstacleMap| -> (usize, usize) {
        let grid = map.view(net);
        let (mut free, mut all) = (0, 0);
        for (_, pad) in &pads {
            let bb = pad.shape.bbox();
            for layer in 0..board.layers.len() {
                if pad.layers & (1 << layer) == 0 {
                    continue;
                }
                let (Some(lo), Some(hi)) = (grid.cell_at(bb.min), grid.cell_at(bb.max)) else { continue };
                for j in lo.1..=hi.1 {
                    for i in lo.0..=hi.0 {
                        if pad.shape.contains(grid.centre(i, j)) {
                            all += 1;
                            free += usize::from(!grid.blocked[grid.index(layer, i, j)]);
                        }
                    }
                }
            }
        }
        (free, all)
    };
    let base = ObstacleMap::new(&board, profile.clone(), h, false);
    let (free, all) = inside(&base);
    println!("{which}: {} pieces, layers {:#b}; {free} of {all} cells inside it free", pads.len(), pads[0].1.layers);
    let bb = pads.iter().map(|(_, p)| p.shape.bbox()).reduce(|a, b| a.union(b)).unwrap();
    let reach = 2_000_000;
    for (id, it) in board.items() {
        let ib = it.shape.bbox();
        if ib.min.x > bb.max.x + reach || ib.max.x < bb.min.x - reach || ib.min.y > bb.max.y + reach || ib.max.y < bb.min.y - reach {
            continue;
        }
        let map = ObstacleMap::without(&board, profile.clone(), h, false, &|x| x == id);
        let (f2, _) = inside(&map);
        if f2 > free {
            println!("  without {:?} net {} layers {:#b}: {f2} free", it.kind, board.net_name(it.net), it.layers);
        }
    }
}
