//! Routes a board's open connections and writes the result beside it:
//! `cargo run --release --example route_board -- in.kicad_pcb out.kicad_pcb [--strip] [--seconds N]`.
//! `--strip` first takes out every track and via that is not locked.

use std::time::Instant;

use board::{check, connectivity, Board, Kind};
use grid::{edits, route_open, CpuSolver, Options};
use kicad_pcb::{apply, Edit};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (input, output) = (&args[1], &args[2]);
    let strip = args.iter().any(|a| a == "--strip");
    let seconds = args.iter().position(|a| a == "--seconds").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok());
    let (mut b, text) = Board::load(input.as_ref()).expect("load");
    let mut removed = Vec::new();
    if strip {
        let loose: Vec<_> = b.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked).map(|(id, _)| id).collect();
        for id in loose {
            if let Some(it) = b.remove(id) {
                removed.extend(it.span);
            }
        }
    }
    removed.sort_by_key(|s| s.start);
    removed.dedup();
    println!("{} open before", connectivity(&b).airlines.len());
    let t = Instant::now();
    let options = Options { max_seconds: seconds, pours_block: args.iter().any(|a| a == "--pours-block"), ..Options::default() };
    let stats = route_open(&mut b, &mut CpuSolver, &options, &mut |_| {});
    println!("routed {} failed {} (no terminal {}, no path {}, refused {}) vias {} in {:.1?}", stats.routed, stats.failed, stats.no_terminal, stats.no_path, stats.refused, stats.vias, t.elapsed());
    println!("{} open after, {} violations", connectivity(&b).airlines.len(), check(&b).len());
    let (tracks, vias) = edits(&b, &stats.laid);
    std::fs::write(output, apply(&text, &Edit { remove: removed, tracks, vias })).expect("write");
}
