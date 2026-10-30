//! A wide net into a fine-pitch pin: a 1 mm power net reaches the middle pin of a 0.5 mm pitch
//! row by a leg necked to the pin's width. Within its class that is the rules' own neck; under a
//! custom rule that asks 1 mm everywhere it is a forced neck, laid all the same, as a designer
//! would, and reported with the KiCad rule that allows it.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use board::{connectivity, Board, Kind, Shape};

/// Pins A, B, VBAT, C, D at 0.5 mm pitch, 0.25 mm wide, and VBAT's own pad 6 mm below.
const BOARD: &str = r#"(kicad_pcb (version 20241229) (generator "pcbnew") (generator_version "9.0")
(general (thickness 1.6) (legacy_teardrops no))
(layers (0 "F.Cu" signal) (2 "B.Cu" signal) (1 "F.Mask" user) (3 "B.Mask" user) (25 "Edge.Cuts" user) (31 "F.CrtYd" user "F.Courtyard"))
(setup (pad_to_mask_clearance 0))
(net 0 "") (net 1 "VBAT") (net 2 "A") (net 3 "B") (net 4 "C") (net 5 "D")
(footprint "t:U1" (layer "F.Cu") (uuid "11111111-1111-4111-8111-111111111110") (at 10 10)
 (property "Reference" "U1" (at 0 -2 0) (layer "F.SilkS") (uuid "11111111-1111-4111-8111-111111111119") (effects (font (size 1 1) (thickness 0.15))))
 (pad "1" smd rect (at -1 0) (size 0.25 0.8) (layers "F.Cu" "F.Mask") (net 2 "A") (uuid "00000000-0000-4000-8000-000000000001"))
 (pad "2" smd rect (at -0.5 0) (size 0.25 0.8) (layers "F.Cu" "F.Mask") (net 3 "B") (uuid "00000000-0000-4000-8000-000000000002"))
 (pad "3" smd rect (at 0 0) (size 0.25 0.8) (layers "F.Cu" "F.Mask") (net 1 "VBAT") (uuid "00000000-0000-4000-8000-000000000003"))
 (pad "4" smd rect (at 0.5 0) (size 0.25 0.8) (layers "F.Cu" "F.Mask") (net 4 "C") (uuid "00000000-0000-4000-8000-000000000004"))
 (pad "5" smd rect (at 1 0) (size 0.25 0.8) (layers "F.Cu" "F.Mask") (net 5 "D") (uuid "00000000-0000-4000-8000-000000000005"))
 (fp_rect (start -1.5 -0.75) (end 1.5 0.75) (stroke (width 0.05) (type solid)) (fill no) (layer "F.CrtYd") (uuid "44444444-4444-4444-8444-444444444444")))
(footprint "t:J1" (layer "F.Cu") (uuid "22222222-2222-4222-8222-222222222220") (at 10 16)
 (property "Reference" "J1" (at 0 -2 0) (layer "F.SilkS") (uuid "22222222-2222-4222-8222-222222222229") (effects (font (size 1 1) (thickness 0.15))))
 (pad "1" smd rect (at 0 0) (size 2 2) (layers "F.Cu" "F.Mask") (net 1 "VBAT") (uuid "22222222-2222-4222-8222-222222222221")))
(gr_rect (start 2 2) (end 18 22) (stroke (width 0.1) (type default)) (fill no) (layer "Edge.Cuts") (uuid "33333333-3333-4333-8333-333333333333"))
)
"#;

/// Signals at 0.2 mm; VBAT's class at 1 mm.
const PROJECT: &str = r#"{"board": {"design_settings": {"rules": {"min_clearance": 0.15, "min_track_width": 0.15, "min_via_diameter": 0.6, "min_through_hole_diameter": 0.3, "min_copper_edge_clearance": 0.3}}},
 "net_settings": {"classes": [
  {"name": "Default", "clearance": 0.15, "track_width": 0.2, "via_diameter": 0.6, "via_drill": 0.3},
  {"name": "POWER", "clearance": 0.2, "track_width": 1.0, "via_diameter": 0.8, "via_drill": 0.4, "priority": 0}],
  "netclass_patterns": [{"netclass": "POWER", "pattern": "VBAT"}]}}"#;

const RULE: &str = "(version 1)\n(rule \"VBAT current width\"\n  (condition \"A.NetName == 'VBAT'\")\n  (constraint track_width (min 1mm)))\n";

/// The board routed, its custom rules `dru` if any, read back with every track as the router's.
fn route(dir: &Path, dru: Option<&str>) -> (Board, Vec<board::ItemId>) {
    std::fs::create_dir_all(dir).unwrap();
    let pcb = dir.join("b.kicad_pcb");
    std::fs::write(&pcb, BOARD).unwrap();
    std::fs::write(dir.join("b.kicad_pro"), PROJECT).unwrap();
    if let Some(dru) = dru {
        std::fs::write(dir.join("b.kicad_dru"), dru).unwrap();
    }
    let args: Vec<String> = ["route", pcb.to_str().unwrap(), "-o", "unused.kicad_pcb", "--uuid-seed", "1"].map(String::from).to_vec();
    let a = ultraroute::parse(&args).unwrap();
    let (board, text) = Board::load(&pcb).unwrap();
    let out = ultraroute::run(&a, board, &text, None, Arc::new(AtomicBool::new(false)));
    std::fs::write(&pcb, out).unwrap();
    let (routed, _) = Board::load(&pcb).unwrap();
    let tracks = routed.items().filter(|(_, it)| it.kind == Kind::Track).map(|(id, _)| id).collect();
    std::fs::remove_dir_all(dir).unwrap();
    (routed, tracks)
}

/// The narrowest track width, in millimetres.
fn narrowest(board: &Board, tracks: &[board::ItemId]) -> f64 {
    tracks
        .iter()
        .filter_map(|id| match board.item(*id)?.shape {
            Shape::Capsule { r, .. } => Some(2.0 * r as f64 / 1e6),
            _ => None,
        })
        .fold(f64::INFINITY, f64::min)
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ultraroute-neck-{name}-{}", std::process::id()))
}

#[test]
fn a_power_net_necks_to_its_pin_within_its_class() {
    let (board, tracks) = route(&scratch("class"), None);
    assert!(connectivity(&board).airlines.is_empty(), "VBAT is joined");
    let w = narrowest(&board, &tracks);
    assert!((0.25..0.26).contains(&w), "the neck is the pin's width, {w} mm");
    assert!(grid::necks::report(&board, &tracks).is_empty(), "the rules allow it: nothing to report");
}

#[test]
fn a_width_rule_the_pin_cannot_meet_is_necked_and_reported() {
    let (board, tracks) = route(&scratch("rule"), Some(RULE));
    assert!(connectivity(&board).airlines.is_empty(), "VBAT is joined, not left open");
    let w = narrowest(&board, &tracks);
    assert!((0.25..0.26).contains(&w), "no thinner than the pin, {w} mm");
    let report = grid::necks::report(&board, &tracks);
    assert_eq!(report.len(), 2, "{report:?}");
    assert!(report[0].starts_with("VBAT 0.25 mm wide") && report[0].contains("into U1-3") && report[0].contains("1 mm its width rule"), "{report:?}");
    assert!(report[1].contains("A.intersectsCourtyard('U1')") && report[1].contains("(min 0.25mm)"), "{report:?}");
}
