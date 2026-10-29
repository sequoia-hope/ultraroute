//! Routes a board's open connections as rubber bands and reports what it did:
//! `cargo run --release --example route_board -- in.kicad_pcb [--strip] [--pours-block]
//! [--no-pours] [--seconds N] [--sample NM] [--rip-passes N] [--quiet]`.

use std::sync::Arc;
use std::time::Instant;

use board::{check, connectivity, connectivity_without_pours, Board, Kind};
use topo::{route_topological, Options};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|s| s.parse::<f64>().ok());
    let (mut b, _) = Board::load(args[1].as_ref()).expect("load");
    if flag("--strip") {
        let loose: Vec<_> = b.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked).map(|(id, _)| id).collect();
        for id in loose {
            b.remove(id);
        }
    }
    if flag("--no-pours") {
        let pours: Vec<_> = b.items().filter(|(_, it)| matches!(it.kind, Kind::Pour { .. })).map(|(id, _)| id).collect();
        for id in pours {
            b.remove(id);
        }
    }
    let pours_block = flag("--pours-block");
    let open = |b: &Board| if pours_block { connectivity(b) } else { connectivity_without_pours(b) }.airlines.len();
    let violations = check(&b).len();
    println!("{} open, {violations} violations before", open(&b));
    let t = Instant::now();
    let log = (!flag("--quiet")).then(|| {
        grid::Log(Arc::new(move |a: grid::Activity| println!("{:7.2} s  {:>10}  {}", t.elapsed().as_secs_f64(), a.phase, a.text)))
    });
    let defaults = Options::default();
    let options = Options {
        deadline: grid::Deadline::max_seconds(value("--seconds")),
        sample: value("--sample").map_or(defaults.sample, |v| v as i64),
        rip_passes: value("--rip-passes").map_or(defaults.rip_passes, |v| v as usize),
        pours_block,
        log,
        ..defaults
    };
    let stats = route_topological(&mut b, &options, &mut |_| {});
    println!(
        "{} of {} connections, {} without a channel, {} rejected, {} rip-ups, {} vias, {} pieces, {:.1} s",
        stats.routed,
        stats.connections,
        stats.no_channel,
        stats.rejected,
        stats.rip_ups,
        stats.vias,
        stats.laid.len(),
        t.elapsed().as_secs_f64()
    );
    let after = check(&b);
    let pour = |id: Option<board::ItemId>| id.and_then(|id| b.item(id)).is_some_and(|it| matches!(it.kind, Kind::Pour { .. }));
    let not_pours = after.iter().filter(|v| !pour(Some(v.a)) && !pour(v.b)).count();
    println!("{} open, {} violations after, {not_pours} of them not against pours", open(&b), after.len());
}
