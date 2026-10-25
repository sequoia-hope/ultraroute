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
    let mut solver: Box<dyn grid::FieldSolver> = Box::new(CpuSolver);
    #[cfg(feature = "gpu")]
    if args.iter().any(|a| a == "--gpu") {
        match grid::GpuSolver::new() {
            Some(gpu) => {
                println!("field solver on {}", gpu.adapter_name);
                solver = Box::new(gpu);
            }
            None => println!("no GPU; solving on the CPU"),
        }
    }
    let mut laid = Vec::new();
    if args.iter().any(|a| a == "--negotiate") {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get() / 2).max(1);
        let n = grid::route_negotiated(
            &mut b,
            solver.as_mut(),
            &grid::NegotiationOptions { max_seconds: seconds.map(|s: f64| s / 2.0), threads, pours_block: options.pours_block, ..Default::default() },
            &mut |_| {},
            &mut |_, _| {},
        );
        println!(
            "negotiated {} of {} connections in {} rounds ({} still colliding, {} without a path), {} vias, {:.1?}",
            n.committed, n.connections, n.rounds, n.conflicted, n.unroutable, n.vias, t.elapsed()
        );
        laid.extend(n.laid);
    }
    let stats = route_open(&mut b, solver.as_mut(), &options, &mut |_| {});
    laid.extend(stats.laid.iter().copied());
    if args.iter().any(|a| a == "--rip-up") {
        let r = grid::rip_up(
            &mut b,
            solver.as_mut(),
            &grid::RipOptions { max_seconds: seconds.map(|s: f64| s / 2.0), pours_block: options.pours_block, ..Default::default() },
            &mut laid,
            &mut |_| {},
        );
        println!("rip-up: {} attempts, {} kept, {} ripped, {:.1?}", r.attempts, r.kept, r.ripped, t.elapsed());
    }
    println!("routed {} failed {} (no terminal {}, no path {}, refused {}) vias {} in {:.1?}", stats.routed, stats.failed, stats.no_terminal, stats.no_path, stats.refused, stats.vias, t.elapsed());
    println!("{} open after, {} violations", connectivity(&b).airlines.len(), check(&b).len());
    if args.iter().any(|a| a == "--why") {
        let name = |id| match b.item(id).map(|it| &it.kind) {
            Some(Kind::Pad { footprint, number, .. }) => format!("{}-{number}", b.footprints[*footprint].reference),
            Some(k) => format!("{k:?}").split([' ', '{']).next().unwrap_or("").to_string(),
            None => "gone".to_string(),
        };
        for (from, to, why) in &stats.failures {
            let net = b.item(*from).map_or(0, |it| it.net);
            println!("  {} {} - {}: {why}", b.net_name(net), name(*from), name(*to));
        }
    }
    let (tracks, vias) = edits(&b, &laid);
    std::fs::write(output, apply(&text, &Edit { remove: removed, tracks, vias, ..Edit::default() })).expect("write");
}
