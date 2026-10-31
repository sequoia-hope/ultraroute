//! `spokes BOARD.kicad_pcb [REF]`: the thermal spokes the router keeps room for, per pad, with
//! their corners in millimetres; only footprint REF's where one is named.

use std::collections::BTreeMap;
use std::path::PathBuf;

use board::{Board, Kind, Shape};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().expect("usage: spokes BOARD.kicad_pcb [REF]"));
    let only = args.next();
    let (board, _) = Board::load(&path).unwrap_or_else(|e| panic!("{e}"));
    let mut per: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut total = 0;
    for (_, it) in board.items() {
        let Kind::Spoke { footprint, .. } = it.kind else { continue };
        total += 1;
        let reference = board.footprints[footprint].reference.clone();
        if only.as_ref().is_some_and(|r| *r != reference) {
            continue;
        }
        let Shape::Polygon(p) = &it.shape else { continue };
        let tip = ((p[1].x + p[2].x) as f64 / 2e6, (p[1].y + p[2].y) as f64 / 2e6);
        let layer = board.layers[it.layers.trailing_zeros() as usize].clone();
        per.entry((reference, board.net_name(it.net).to_string())).or_default().push(format!("{layer} tip ({:.3}, {:.3})", tip.0, tip.1));
    }
    for ((reference, net), spokes) in &per {
        println!("{reference} {net}: {}", spokes.join(", "));
    }
    println!("{total} spokes kept");
    for w in board.warnings.iter().filter(|w| w.contains("thermal")) {
        println!("warning: {w}");
    }
}
