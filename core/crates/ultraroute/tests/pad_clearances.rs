//! `--pad-clearances`: a pin of a 0.4 mm-pitch row whose footprint narrows the clearance to its
//! pads to 0.1 mm, where the class asks 0.2 mm. KiCad keeps the footprint's: a 0.2 mm track leaves
//! the middle pin between its neighbours 0.175 mm from each. Keeping the wider clearance, as the
//! router does by default, boxes the pin in; with the footprint's it routes, and KiCad's
//! clearances hold.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use board::{connectivity, Board};

/// U1's pins 1 to 3 at x 9.6, 10 and 10.4, 0.25 mm wide and 0.74 mm long; pin 2 (A) goes to J1
/// 5 mm below. Pins 1 and 3 are nets of their own with nowhere to go.
const BOARD: &str = r#"(kicad_pcb (version 20241229) (generator "pcbnew") (generator_version "9.0")
(general (thickness 1.6) (legacy_teardrops no))
(layers (0 "F.Cu" signal) (1 "F.Mask" user) (25 "Edge.Cuts" user))
(setup (pad_to_mask_clearance 0))
(net 0 "") (net 1 "A") (net 2 "B") (net 3 "C")
(footprint "t:U1" (layer "F.Cu") (uuid "11111111-1111-4111-8111-111111111110") (at 10 10)
 (property "Reference" "U1" (at 0 -2 0) (layer "F.SilkS") (uuid "11111111-1111-4111-8111-111111111119") (effects (font (size 1 1) (thickness 0.15))))
 (clearance 0.1)
 (pad "1" smd rect (at -0.4 0) (size 0.25 0.74) (layers "F.Cu" "F.Mask") (net 2 "B") (uuid "11111111-1111-4111-8111-111111111111"))
 (pad "2" smd rect (at 0 0) (size 0.25 0.74) (layers "F.Cu" "F.Mask") (net 1 "A") (uuid "11111111-1111-4111-8111-111111111112"))
 (pad "3" smd rect (at 0.4 0) (size 0.25 0.74) (layers "F.Cu" "F.Mask") (net 3 "C") (uuid "11111111-1111-4111-8111-111111111113")))
(footprint "t:J1" (layer "F.Cu") (uuid "22222222-2222-4222-8222-222222222220") (at 10 15)
 (property "Reference" "J1" (at 0 -2 0) (layer "F.SilkS") (uuid "22222222-2222-4222-8222-222222222229") (effects (font (size 1 1) (thickness 0.15))))
 (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu" "F.Mask") (net 1 "A") (uuid "22222222-2222-4222-8222-222222222221")))
(gr_rect (start 5 5) (end 15 18) (stroke (width 0.1) (type default)) (fill no) (layer "Edge.Cuts") (uuid "44444444-4444-4444-8444-444444444444"))
)
"#;

const PROJECT: &str = r#"{"board": {"design_settings": {"rules": {"min_clearance": 0, "min_track_width": 0.2, "min_via_diameter": 0.6, "min_through_hole_diameter": 0.3, "min_copper_edge_clearance": 0}}},
 "net_settings": {"classes": [{"name": "Default", "clearance": 0.2, "track_width": 0.2, "via_diameter": 0.6, "via_drill": 0.3}]}}"#;

/// The board routed with `flags` after the default recipe.
fn route(dir: &Path, flags: &[&str]) -> Board {
    std::fs::create_dir_all(dir).unwrap();
    let pcb = dir.join("b.kicad_pcb");
    std::fs::write(&pcb, BOARD).unwrap();
    std::fs::write(dir.join("b.kicad_pro"), PROJECT).unwrap();
    let mut args: Vec<String> = ["route", pcb.to_str().unwrap(), "-o", "unused.kicad_pcb", "--uuid-seed", "1", "--recipe", "default", "--threads", "2"].map(String::from).to_vec();
    args.extend(flags.iter().map(|f| f.to_string()));
    let a = ultraroute::parse(&args).unwrap();
    let (board, text) = Board::load(&pcb).unwrap();
    let out = ultraroute::route_retrying(&a, board, &text, None, Arc::new(AtomicBool::new(false)));
    std::fs::write(&pcb, out.text).unwrap();
    let (routed, _) = Board::load(&pcb).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    routed
}

fn open(board: &Board) -> usize {
    connectivity(board).airlines.len()
}

#[test]
fn a_pin_leaves_between_its_neighbours_at_its_footprints_clearance() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pad-clearances");
    // At the class's 0.2 mm the pin is boxed in.
    let kept = route(&dir.join("kept"), &["--no-retries"]);
    assert_eq!(open(&kept), 1, "0.175 mm to the neighbours is under the class's 0.2 mm");
    let mut exact = route(&dir.join("exact"), &["--no-retries", "--pad-clearances"]);
    assert_eq!(open(&exact), 0);
    // KiCad's clearances: the footprint's 0.1 mm to its pads.
    exact.pad_clearances_exact = true;
    assert!(board::check(&exact).is_empty(), "{:?}", board::check(&exact));
    // The class's 0.2 mm, which the default keeps, is what the route gives up.
    exact.pad_clearances_exact = false;
    assert!(!board::check(&exact).is_empty(), "the route passes nearer than the class asks");
    // The recipe's retries come to it when connections stay open.
    let retried = route(&dir.join("retried"), &[]);
    assert_eq!(open(&retried), 0);
}
