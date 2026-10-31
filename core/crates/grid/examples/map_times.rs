//! `map_times BOARD`: how long a whole-board map takes to make for each class, stripped bare as
//! the benchmark strips it, and what in it takes the time.
use std::path::PathBuf;
use std::time::Instant;

use board::{Board, Kind};
use grid::{ObstacleMap, Profile};

fn main() {
    let path = PathBuf::from(std::env::args().nth(1).expect("usage: map_times BOARD"));
    let t = Instant::now();
    let mut board = Board::load(&path).unwrap().0;
    println!("load {:.2} s", t.elapsed().as_secs_f64());
    let strip: Vec<_> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked).map(|(id, _)| id).collect();
    for id in strip {
        board.remove(id);
    }
    let mut seen = std::collections::HashSet::new();
    for net in 1..board.nets.len() as u32 {
        let p = Profile::for_net(&board, net);
        if !seen.insert(p.class.clone()) {
            continue;
        }
        let t = Instant::now();
        let m = ObstacleMap::new(&board, p.clone(), 50_000, false);
        println!("class {:12} {:.2} s ({}x{})", p.class, t.elapsed().as_secs_f64(), m.nx, m.ny);
    }
}
