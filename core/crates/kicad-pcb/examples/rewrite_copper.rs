//! Takes every track and via out of a board and writes them back in, to check the writer against
//! KiCad: `cargo run --example rewrite_copper -- in.kicad_pcb out.kicad_pcb`.

use kicad_pcb::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let text = std::fs::read_to_string(&args[1]).expect("read the board");
    let (_, copper) = parse_board(&text).expect("parse the board");
    let edit = Edit {
        remove: copper.tracks.iter().map(|t| t.span).chain(copper.vias.iter().map(|v| v.span)).collect(),
        tracks: copper
            .tracks
            .iter()
            .map(|t| NewTrack { start: t.start, end: t.end, width: t.width, layer: t.layer.clone(), net: t.net })
            .collect(),
        vias: copper
            .vias
            .iter()
            .map(|v| NewVia { at: v.at, size: v.size, drill: v.drill, layers: v.layers.clone(), net: v.net })
            .collect(),
        replace: Vec::new(),
        uuid_seed: None,
    };
    std::fs::write(&args[2], apply(&text, &edit)).expect("write the board");
    println!("{} tracks and {} vias rewritten", edit.tracks.len(), edit.vias.len());
}
