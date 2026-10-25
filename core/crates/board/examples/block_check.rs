//! `block_check BOARD.kicad_pcb`: each block's pours, and the copper of other nets that comes
//! within clearance of them, which a refill would cut out of the plane.

use std::path::PathBuf;

use board::{check_item, Board, Kind};

fn main() {
    let path = PathBuf::from(std::env::args().nth(1).expect("usage: block_check BOARD.kicad_pcb"));
    let (board, _) = Board::load(&path).unwrap_or_else(|e| panic!("{e}"));
    if board.blocks.is_empty() {
        println!("no blocks (KiCad groups) on {}", path.display());
    }
    let mut total = 0;
    for block in &board.blocks {
        let parts: Vec<&str> = block.footprints.iter().map(|&f| board.footprints[f].reference.as_str()).collect();
        println!("{}: parts {}, {} zones", block.name, parts.join(" "), block.zones.len());
        for (id, item) in board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via)) {
            for v in check_item(&board, id) {
                let Some(pour) = v.b.and_then(|b| board.item(b)) else { continue };
                let Kind::Pour { zone, kept: true } = pour.kind else { continue };
                if !block.zones.contains(&zone) {
                    continue;
                }
                total += 1;
                let at = item.shape.bbox().min;
                println!(
                    "  {:?} of {} at {:.2}, {:.2} mm into {} on {}",
                    item.kind,
                    board.nets[item.net as usize],
                    at.x as f64 * 1e-6,
                    at.y as f64 * 1e-6,
                    board.nets[pour.net as usize],
                    v.layer.map_or("?", |l| board.layers[l].as_str())
                );
            }
        }
    }
    println!("{total} intrusions");
}
