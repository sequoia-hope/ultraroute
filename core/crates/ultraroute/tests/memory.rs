//! A run is sized before it allocates its grids: over its memory cap it gives up threads, then
//! routes coarser, and a board still over is refused with both figures, the input written back
//! as the output, as a crash leaves it.

use std::path::PathBuf;
use std::process::Command;

use board::Board;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name)
}

fn args(extra: &[&str]) -> ultraroute::Args {
    let mut args: Vec<String> = ["route", "in.kicad_pcb", "-o", "out.kicad_pcb"].map(String::from).to_vec();
    args.extend(extra.iter().map(|s| s.to_string()));
    ultraroute::parse(&args).unwrap()
}

#[test]
fn a_board_over_the_cap_is_refused_and_its_input_written_back() {
    let dir = std::env::temp_dir().join(format!("ultraroute-memory-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("out.kicad_pcb");
    let started = std::time::Instant::now();
    let run = Command::new(env!("CARGO_BIN_EXE_ultraroute"))
        .args(["route", fixture("bluebuzz-rm2.kicad_pcb").to_str().unwrap(), "-o", out.to_str().unwrap(), "--negotiate", "--max-memory", "16"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert_eq!(run.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("over the 16 MB allowed by --max-memory"), "{stderr}");
    assert!(started.elapsed().as_secs() < 30, "refused before routing, not after");
    let input = std::fs::read_to_string(fixture("bluebuzz-rm2.kicad_pcb")).unwrap();
    assert_eq!(std::fs::read_to_string(&out).unwrap(), input, "the input comes back as the output");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn over_the_cap_a_jacobi_run_gives_up_threads_before_it_coarsens() {
    let (board, _) = Board::load(&fixture("earring.kicad_pcb")).unwrap();
    let one = grid::memory_estimate(&board, grid::Options::default().cell, 1);
    let eight = grid::memory_estimate(&board, grid::Options::default().cell, 8);
    assert!(eight.bytes > one.bytes);

    // Room for one thread but not eight: threads go, the cell stays.
    let cap_mb = one.mb() + 1;
    let mut a = args(&["--negotiate-jacobi", "--threads", "8", "--max-memory", &cap_mb.to_string()]);
    let lines = ultraroute::fit_memory(&mut a, &board).unwrap();
    assert!(a.threads.unwrap() < 8 && a.cell.is_none(), "{lines:?}");
    assert!(grid::memory_estimate(&board, grid::Options::default().cell, a.threads.unwrap()).mb() <= cap_mb);

    // Room for nothing at the default cell: one thread, and a coarser cell.
    let coarse = grid::memory_estimate(&board, 100_000, 1);
    let mut a = args(&["--negotiate-jacobi", "--threads", "8", "--max-memory", &coarse.mb().to_string()]);
    let lines = ultraroute::fit_memory(&mut a, &board).unwrap();
    assert_eq!(a.threads, Some(1), "{lines:?}");
    assert!(a.cell.unwrap() > grid::Options::default().cell, "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("to fit")), "{lines:?}");

    // Gauss-Seidel, the default, searches one connection at a time: its threads cost no whole
    // board each, so the same cap leaves them as asked.
    let mut a = args(&["--threads", "8", "--max-memory", &cap_mb.to_string()]);
    let lines = ultraroute::fit_memory(&mut a, &board).unwrap();
    assert_eq!(a.threads, Some(8), "{lines:?}");

    // Within the cap nothing changes.
    let mut a = args(&["--threads", "2", "--max-memory", "100000"]);
    ultraroute::fit_memory(&mut a, &board).unwrap();
    assert_eq!((a.threads, a.cell), (Some(2), None));
}
