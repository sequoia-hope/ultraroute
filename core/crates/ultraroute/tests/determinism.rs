//! A run's output depends on the board, the flags and the seeds alone: the benchmark recipe on the
//! earring, stripped bare, writes the same bytes twice, on one thread or four, and with a time
//! limit it never reaches as without one.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use board::Board;

/// The benchmark recipe of REMEDIATION.md §0.5.
const RECIPE: &str = "--negotiate --rip-up --tighten --prefanout --diff-pairs --shove-plough region --shove-reach 0.5 --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8";

fn route(extra: &[&str]) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pcb");
    let mut args: Vec<String> = ["route", path.to_str().unwrap(), "-o", "unused.kicad_pcb", "--strip-all", "--seed", "0", "--uuid-seed", "1"].map(String::from).to_vec();
    args.extend(RECIPE.split_whitespace().map(String::from));
    args.extend(extra.iter().map(|s| s.to_string()));
    let a = ultraroute::parse(&args).unwrap();
    let (board, text) = Board::load(&path).unwrap();
    ultraroute::run(&a, board, &text, None, Arc::new(AtomicBool::new(false)))
}

#[test]
fn the_same_input_routes_to_the_same_bytes() {
    let runs: Vec<(&str, &[&str])> = vec![
        ("one thread", &["--threads", "1"]),
        ("one thread again", &["--threads", "1"]),
        ("four threads", &["--threads", "4"]),
        ("a time limit it never reaches", &["--threads", "1", "--timeout", "240"]),
    ];
    // At once: each run is half a minute.
    let outs: Vec<(&str, String)> = std::thread::scope(|scope| {
        let handles: Vec<_> = runs.iter().map(|(name, extra)| (*name, scope.spawn(|| route(extra)))).collect();
        handles.into_iter().map(|(name, h)| (name, h.join().expect("no run panics"))).collect()
    });
    let (first, reference) = &outs[0];
    assert!(reference.contains("(segment"), "the earring is routed");
    for (name, out) in &outs[1..] {
        if out != reference {
            let line = reference.lines().zip(out.lines()).position(|(a, b)| a != b).unwrap_or(0);
            panic!("{name} differs from {first} at line {}: {:?} vs {:?}", line + 1, reference.lines().nth(line), out.lines().nth(line));
        }
    }
}
