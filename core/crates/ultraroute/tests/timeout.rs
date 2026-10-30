//! `--timeout` bounds the whole run: the benchmark's recipe on a whole board, stripped bare, returns
//! a board that reads back within a little more than the time it was given.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use board::Board;

fn routed_within(timeout: f64, limit: f64) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/bluebuzz-rm2.kicad_pcb");
    let mut args: Vec<String> = ["route", path.to_str().unwrap(), "-o", "unused.kicad_pcb", "--strip-all", "--seed", "0", "--threads", "1"].map(String::from).to_vec();
    args.extend(["--recipe".to_string(), "default".to_string()]);
    args.extend(["--timeout".to_string(), timeout.to_string()]);
    let a = ultraroute::parse(&args).unwrap();
    let (board, text) = Board::load(&path).unwrap();
    let t = std::time::Instant::now();
    let out = ultraroute::run(&a, board, &text, None, Arc::new(AtomicBool::new(false)));
    let took = t.elapsed().as_secs_f64();
    assert!(took < limit, "--timeout {timeout} took {took:.1} s");
    kicad_pcb::parse_board(&out).expect("the routed board reads back");
}

#[test]
fn five_seconds_is_kept() {
    routed_within(5.0, 8.0);
}

#[test]
fn one_second_is_kept() {
    routed_within(1.0, 4.0);
}
