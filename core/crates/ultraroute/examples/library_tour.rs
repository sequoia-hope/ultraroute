//! The core as a library, end to end: read a KiCad board, route it with a short pipeline of the
//! grid's stages, bend the routed copper with a repulsor field and repair what that breaks, check
//! the result, and write the routed board. docs/library.md explains each step.
//!
//! `cargo run --release -p ultraroute --example library_tour -- IN.kicad_pcb OUT.kicad_pcb [SECONDS]`
//!
//! The `.kicad_pro` beside IN supplies the rules. Every unlocked track and via is taken out
//! first, so the board is routed from its pads. SECONDS (default 20) bounds the routing stages.

use std::path::Path;
use std::time::Instant;

use board::{check_item, connectivity_without_pours, BBox, Board, ItemId, Kind, Point};
use kicad_pcb::{apply, Edit, Rules, Span};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(input), Some(output)) = (args.first(), args.get(1)) else {
        eprintln!("usage: library_tour IN.kicad_pcb OUT.kicad_pcb [SECONDS]");
        std::process::exit(2);
    };
    let seconds: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20.0);
    let started = Instant::now();

    // 1. Read. The text is kept: writing back edits it in place, by the spans the parse noted.
    // `Board::load(path)` does these four lines in one.
    let text = std::fs::read_to_string(input).expect("read the board");
    let (pcb, copper) = kicad_pcb::parse_board(&text).expect("parse the board");
    let rules = Rules::read(&Path::new(input).with_extension("kicad_pro")).expect("read the project's rules");
    let mut board = Board::new(&pcb, &copper, rules);
    for w in &board.rules.warnings {
        eprintln!("warning: {w}");
    }
    println!("{}: {} layers, {} nets, {} parts", input, board.layers.len(), board.nets.len() - 1, board.footprints.len());

    // 2. Strip. Unlocked tracks and vias come out of the model, and their spans go on the list
    // of text to delete from the file. Anything taken out of the board must be handled this way,
    // or the old copper stays in the written file beside the new.
    let mut removed: Vec<Span> = Vec::new();
    let loose: Vec<ItemId> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked).map(|(id, _)| id).collect();
    for id in loose {
        if let Some(it) = board.remove(id) {
            removed.extend(it.span);
        }
    }
    // Pours are refilled by KiCad round new copper, so they join nothing yet: count as the
    // router does by default (`pours_block: false`).
    let open = |b: &Board| connectivity_without_pours(b).airlines.len();
    println!("stripped: {} connections open", open(&board));

    // 3. Route. `laid` is the router's own copper: what later stages may rip, move and tighten,
    // and what is written back at the end. The routers that lay new copper return it in their
    // stats; the stages that rework copper take `laid` and keep it current.
    let mut solver = grid::CpuSolver;
    let mut laid: Vec<ItemId> = Vec::new();
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get() / 2).max(1);

    let n = grid::route_negotiated(
        &mut board,
        &mut solver,
        &grid::NegotiationOptions { max_seconds: Some(seconds * 0.5), threads, ..Default::default() },
        &mut |_| {},    // sees the board after each connection committed
        &mut |_, _| {}, // sees each round's trial paths
    );
    laid.extend(n.laid);
    println!("negotiate: {} of {} connections in {} rounds, {} vias, {} open", n.committed, n.connections, n.rounds, n.vias, open(&board));

    let s = grid::route_open(&mut board, &mut solver, &grid::Options { max_seconds: Some(seconds * 0.2), ..Default::default() }, &mut |_| {});
    laid.extend(s.laid.iter().copied());
    println!("route_open: {} routed, {} failed, {} open", s.routed, s.failed, open(&board));

    let r = grid::rip_up(&mut board, &mut solver, &grid::RipOptions { max_seconds: Some(seconds * 0.3), ..Default::default() }, &mut laid, &mut |_| {});
    println!("rip_up: {} tries, {} kept, {} open", r.attempts, r.kept, open(&board));

    // 4. Morph. A repulsor pole at the middle of the routed copper pushes every unlocked track
    // and via away from it: 0.15 mm at one radius (1 mm) out, nothing at the centre, fading
    // beyond. Pads and locked copper hold still and joints stay joined. Copper the move pushed
    // into something is ripped and routed again, and the whole move is undone if it leaves more
    // open than before: the snapshot pattern every trial move in the CLI uses.
    let before = open(&board);
    let snapshot = (board.clone(), laid.clone(), removed.len());
    let centre = laid.iter().filter_map(|id| board.bbox(*id)).reduce(BBox::union).map(|b| Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2));
    if let Some(centre) = centre {
        let pole = morph::Pole { center: centre, strength: 150_000.0, radius: 1_000_000.0 };
        let (moved, spans) = morph::shift_poles(&mut board, &mut laid, &[pole], 150_000.0);
        removed.extend(spans);
        let (ripped, spans) = grid::rip_broken(&mut board, &mut laid, false);
        removed.extend(spans);
        let again = grid::route_open(&mut board, &mut solver, &grid::Options { max_seconds: Some(5.0), ..Default::default() }, &mut |_| {});
        laid.extend(again.laid.iter().copied());
        let after = open(&board);
        println!(
            "repulsor at ({:.2}, {:.2}) mm: {moved} pieces moved, {ripped} ripped, {} rerouted, {before} -> {after} open{}",
            centre.x as f64 * 1e-6,
            centre.y as f64 * 1e-6,
            again.routed,
            if after > before { ", undone" } else { "" }
        );
        if after > before {
            (board, laid) = (snapshot.0, snapshot.1);
            removed.truncate(snapshot.2);
        }
    }

    // 5. Tidy: straighten the router's tracks, then drop doubled tracks and copper leading
    // nowhere. Both may take out file copper, so their spans join `removed`.
    let t = grid::pull_tight(&mut board, &mut laid);
    println!("pull_tight: {} -> {} segments, {:.1} -> {:.1} mm", t.segments_before, t.segments_after, t.length_before, t.length_after);
    let (doubled, spans) = grid::drop_duplicates(&mut board, &mut laid);
    removed.extend(spans);
    let (pruned, spans) = grid::prune_dangling(&mut board, &mut laid);
    removed.extend(spans);
    println!("cleanup: {doubled} doubled, {pruned} dangling taken out");

    // 6. Check. Every piece the router keeps was checked as it was laid; checking again here is
    // what an embedding program would do after its own edits. Open connections are listed.
    let broken: usize = laid.iter().map(|&id| check_item(&board, id).len()).sum();
    let left = connectivity_without_pours(&board).airlines;
    println!("check: {} pieces of routed copper, {broken} violations, {} open", laid.len(), left.len());
    for a in left.iter().take(8) {
        println!(
            "  open: {} from ({:.2}, {:.2}) to ({:.2}, {:.2}) mm",
            board.net_name(a.net),
            a.from.x as f64 * 1e-6,
            a.from.y as f64 * 1e-6,
            a.to.x as f64 * 1e-6,
            a.to.y as f64 * 1e-6
        );
    }

    // 7. Write. The router's copper becomes new segments and vias appended to the file; the
    // spans collected above are deleted. Every other byte of the file is kept.
    let (tracks, vias) = grid::edits(&board, &laid);
    let routed = apply(&text, &Edit { remove: removed, replace: Vec::new(), tracks, vias });
    std::fs::write(output, routed).expect("write the routed board");
    println!("wrote {output}, {:.1} s", started.elapsed().as_secs_f64());
}
