//! What the command says of what it was given and cannot use: a part to lock that is not on the
//! board warns; a live file it cannot write stops it before it routes.

use std::path::PathBuf;
use std::process::Command;

fn earring() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pcb").to_str().unwrap().to_string()
}

fn out_path(name: &str) -> String {
    std::env::temp_dir().join(format!("ultraroute-cli-{name}-{}.kicad_pcb", std::process::id())).to_str().unwrap().to_string()
}

#[test]
fn an_unknown_part_to_lock_is_warned_of() {
    let out = out_path("lock");
    let run = Command::new(env!("CARGO_BIN_EXE_ultraroute")).args(["route", &earring(), "-o", &out, "--no-route", "--lock", "NOPE9"]).output().unwrap();
    let _ = std::fs::remove_file(&out);
    assert!(run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("--lock NOPE9: no such part"), "{}", String::from_utf8_lossy(&run.stderr));
}

#[test]
fn a_live_file_that_cannot_be_written_stops_the_run() {
    let out = out_path("live");
    let run = Command::new(env!("CARGO_BIN_EXE_ultraroute")).args(["route", &earring(), "-o", &out, "--no-route", "--live", "/nonexistent-dir/live.jsonl"]).output().unwrap();
    let _ = std::fs::remove_file(&out);
    assert_eq!(run.status.code(), Some(1), "{}", String::from_utf8_lossy(&run.stderr));
    assert!(String::from_utf8_lossy(&run.stderr).contains("--live /nonexistent-dir/live.jsonl"));
}
