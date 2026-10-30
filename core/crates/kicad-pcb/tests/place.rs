//! `place::apply` against pcbnew 9: every footprint of three real boards moved, turned to right
//! and odd angles and flipped either way, in two rounds, must come out byte for byte as pcbnew
//! saved it (`tests/golden/place/gen.py` made the recipes and hashes). And the small cases by
//! hand: what a flip writes, what is left alone, what is refused.

use std::path::PathBuf;

use kicad_pcb::place::{apply, PlaceError, Placement};
use kicad_pcb::sexpr;
use kicad_pcb::Side;

fn dir(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn recipe(name: &str) -> Vec<Placement> {
    let text = std::fs::read_to_string(dir("tests/golden/place").join(format!("{name}.json"))).unwrap();
    let moves: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    moves
        .iter()
        .map(|m| Placement {
            reference: m["ref"].as_str().unwrap().to_string(),
            at: [m["x"].as_f64().unwrap() * 1e-3, m["y"].as_f64().unwrap() * 1e-3],
            rotation_deg: m["rotation"].as_f64().unwrap(),
            side: if m["side"] == "back" { Side::Back } else { Side::Front },
        })
        .collect()
}

/// FNV-1a over the board's footprints, in file order, as `gen.py` hashes them.
fn footprints_hash(text: &str) -> String {
    let root = sexpr::parse(text).unwrap();
    let blocks: Vec<&str> = root.find_all("footprint").map(|f| &text[f.pos().start..f.pos().end]).collect();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in blocks.join("\n").bytes() {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[test]
fn every_footprint_comes_out_as_pcbnew_saves_it() {
    let hashes: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir("tests/golden/place/hashes.json")).unwrap()).unwrap();
    for board in ["earring", "bluebuzz-rm2", "earring-locked"] {
        let mut text = std::fs::read_to_string(dir("../../fixtures").join(format!("{board}.kicad_pcb"))).unwrap();
        for round in 1..=2 {
            let name = format!("{board}.r{round}");
            text = apply(&text, &recipe(&name)).unwrap();
            assert_eq!(footprints_hash(&text), hashes[&name].as_str().unwrap(), "{name}: the footprints differ from pcbnew's; run gen.py's round by hand and diff");
        }
    }
}

const BOARD: &str = "(kicad_pcb
\t(version 20241229)
\t(generator \"pcbnew\")
\t(layers
\t\t(0 \"F.Cu\" signal)
\t\t(2 \"B.Cu\" signal)
\t\t(5 \"F.SilkS\" user \"F.Silkscreen\")
\t\t(7 \"B.SilkS\" user \"B.Silkscreen\")
\t\t(25 \"Edge.Cuts\" user)
\t)
\t(net 0 \"\")
\t(net 1 \"A\")
\t(footprint \"t:R\"
\t\t(layer \"F.Cu\")
\t\t(uuid \"11111111-1111-4111-8111-111111111111\")
\t\t(at 10 20)
\t\t(property \"Reference\" \"R1\"
\t\t\t(at 0 -1.5 0)
\t\t\t(layer \"F.SilkS\")
\t\t\t(uuid \"11111111-1111-4111-8111-111111111112\")
\t\t\t(effects
\t\t\t\t(font
\t\t\t\t\t(size 1 1)
\t\t\t\t\t(thickness 0.15)
\t\t\t\t)
\t\t\t)
\t\t)
\t\t(fp_rect
\t\t\t(start -1 -0.5)
\t\t\t(end 1 0.5)
\t\t\t(stroke
\t\t\t\t(width 0.1)
\t\t\t\t(type solid)
\t\t\t)
\t\t\t(fill no)
\t\t\t(layer \"F.SilkS\")
\t\t\t(uuid \"11111111-1111-4111-8111-111111111113\")
\t\t)
\t\t(pad \"1\" smd rect
\t\t\t(at -0.5 0.25)
\t\t\t(size 0.5 0.5)
\t\t\t(layers \"F.Cu\" \"F.Mask\" \"F.Paste\")
\t\t\t(net 1 \"A\")
\t\t\t(uuid \"11111111-1111-4111-8111-111111111114\")
\t\t)
\t)
\t(gr_rect
\t\t(start 0 0)
\t\t(end 30 30)
\t\t(stroke
\t\t\t(width 0.05)
\t\t\t(type default)
\t\t)
\t\t(fill no)
\t\t(layer \"Edge.Cuts\")
\t\t(uuid \"33333333-3333-4333-8333-333333333333\")
\t)
)
";

fn r1(at: [f64; 2], rotation_deg: f64, side: Side) -> Placement {
    Placement { reference: "R1".into(), at: [at[0] * 1e-3, at[1] * 1e-3], rotation_deg, side }
}

#[test]
fn a_part_left_where_it_is_changes_nothing() {
    assert_eq!(apply(BOARD, &[]).unwrap(), BOARD);
    assert_eq!(apply(BOARD, &[r1([10.0, 20.0], 0.0, Side::Front)]).unwrap(), BOARD);
}

#[test]
fn a_move_changes_only_the_footprints_position() {
    let out = apply(BOARD, &[r1([12.5, 7.0], 0.0, Side::Front)]).unwrap();
    assert_eq!(out, BOARD.replace("(at 10 20)", "(at 12.5 7)"));
}

#[test]
fn a_flip_swaps_layers_mirrors_points_and_mirrors_the_text() {
    let out = apply(BOARD, &[r1([10.0, 20.0], 0.0, Side::Back)]).unwrap();
    assert!(out.contains("\t\t(layer \"B.Cu\")\n"), "{out}");
    assert!(out.contains("(at 0 1.5 180)\n\t\t\t(layer \"B.SilkS\")"), "{out}");
    assert!(out.contains("\t\t\t\t)\n\t\t\t\t(justify mirror)\n\t\t\t)"), "{out}");
    assert!(out.contains("(start -1 0.5)\n\t\t\t(end 1 -0.5)"), "{out}");
    assert!(out.contains("(at -0.5 -0.25)\n\t\t\t(size 0.5 0.5)\n\t\t\t(layers \"B.Cu\" \"B.Mask\" \"B.Paste\")"), "{out}");
    // Flipped back, it is the board it was.
    assert_eq!(apply(&out, &[r1([10.0, 20.0], 0.0, Side::Front)]).unwrap(), BOARD);
}

#[test]
fn a_rectangle_turned_off_the_right_angles_becomes_a_polygon() {
    let out = apply(BOARD, &[r1([10.0, 20.0], 30.0, Side::Front)]).unwrap();
    assert!(out.contains("(at 10 20 30)"), "{out}");
    assert!(out.contains("(fp_poly\n\t\t\t(pts\n\t\t\t\t(xy -1 -0.5) (xy 1 -0.5) (xy 1 0.5) (xy -1 0.5)\n\t\t\t)\n\t\t\t(stroke"), "{out}");
    assert!(out.contains("(at -0.5 0.25 30)"), "{out}");
    assert!(out.contains("(at 0 -1.5 30)"), "{out}");
}

#[test]
fn an_unknown_part_is_refused() {
    let p = Placement { reference: "U9".into(), ..r1([0.0, 0.0], 0.0, Side::Front) };
    assert_eq!(apply(BOARD, &[p]), Err(PlaceError::NoSuchPart("U9".into())));
}
