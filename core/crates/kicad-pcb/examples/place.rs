//! `cargo run --example place -- BOARD MOVES.json OUT`: each part of MOVES (`[{"ref", "x", "y",
//! "rotation", "side"}]`, millimetres, KiCad's frame) placed on BOARD, written to OUT.

use kicad_pcb::place::{apply, Placement};
use kicad_pcb::Side;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, board, moves, out] = &args[..] else { panic!("place BOARD MOVES.json OUT") };
    let text = std::fs::read_to_string(board).unwrap();
    let moves: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(moves).unwrap()).unwrap();
    let placements: Vec<Placement> = moves
        .iter()
        .map(|m| Placement {
            reference: m["ref"].as_str().unwrap().to_string(),
            at: [m["x"].as_f64().unwrap() * 1e-3, m["y"].as_f64().unwrap() * 1e-3],
            rotation_deg: m["rotation"].as_f64().unwrap(),
            side: if m["side"] == "back" { Side::Back } else { Side::Front },
        })
        .collect();
    std::fs::write(out, apply(&text, &placements).unwrap()).unwrap();
}
