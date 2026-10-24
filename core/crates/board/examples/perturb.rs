//! Writes altered copies of a board for checking the model against KiCad's DRC:
//! `cargo run --example perturb -- in.kicad_pcb out_dir`.

use kicad_pcb::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let text = std::fs::read_to_string(&args[1]).expect("read the board");
    let out = std::path::Path::new(&args[2]);
    let (_, copper) = parse_board(&text).expect("parse the board");
    let track = |t: &Track, by: [f64; 2]| NewTrack {
        start: [t.start[0] + by[0], t.start[1] + by[1]],
        end: [t.end[0] + by[0], t.end[1] + by[1]],
        width: t.width,
        layer: t.layer.clone(),
        net: t.net,
    };
    let all_copper: Vec<Span> = copper.tracks.iter().map(|t| t.span).chain(copper.vias.iter().map(|v| v.span)).collect();
    let variants = [
        ("stripped", Edit { remove: all_copper, ..Edit::default() }),
        ("half", Edit { remove: copper.tracks.iter().step_by(2).map(|t| t.span).collect(), ..Edit::default() }),
        (
            "shifted",
            Edit {
                remove: copper.tracks.iter().step_by(40).map(|t| t.span).collect(),
                tracks: copper.tracks.iter().step_by(40).map(|t| track(t, [0.1e-3, 0.0])).collect(),
                vias: Vec::new(),
            },
        ),
    ];
    for (name, edit) in variants {
        std::fs::write(out.join(format!("{name}.kicad_pcb")), apply(&text, &edit)).expect("write");
        println!("{name}");
    }
}
