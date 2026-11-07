//! `--narrow-signals`: two signals share a 1 mm gap in a wall on a one-layer board. Their class
//! asks 0.3 mm, the board allows 0.15 mm, the clearance is 0.2 mm: two tracks fit through at
//! 0.15 mm (0.9 mm), not one at 0.3 mm beside one at 0.15 mm (1.05 mm). Routed at their minimum
//! both pass, and outside the gap they take their class's width after.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use board::{connectivity, Board, Kind, Shape};

/// A and B from the left to the right, through the 1 mm gap between two netless pads at x 10.
const BOARD: &str = r#"(kicad_pcb (version 20241229) (generator "pcbnew") (generator_version "9.0")
(general (thickness 1.6) (legacy_teardrops no))
(layers (0 "F.Cu" signal) (1 "F.Mask" user) (25 "Edge.Cuts" user))
(setup (pad_to_mask_clearance 0))
(net 0 "") (net 1 "A") (net 2 "B")
(footprint "t:J1" (layer "F.Cu") (uuid "11111111-1111-4111-8111-111111111110") (at 3 10)
 (property "Reference" "J1" (at 0 -2 0) (layer "F.SilkS") (uuid "11111111-1111-4111-8111-111111111119") (effects (font (size 1 1) (thickness 0.15))))
 (pad "1" smd rect (at 0 -1.5) (size 1 1) (layers "F.Cu" "F.Mask") (net 1 "A") (uuid "11111111-1111-4111-8111-111111111111"))
 (pad "2" smd rect (at 0 1.5) (size 1 1) (layers "F.Cu" "F.Mask") (net 2 "B") (uuid "11111111-1111-4111-8111-111111111112")))
(footprint "t:J2" (layer "F.Cu") (uuid "22222222-2222-4222-8222-222222222220") (at 17 10)
 (property "Reference" "J2" (at 0 -2 0) (layer "F.SilkS") (uuid "22222222-2222-4222-8222-222222222229") (effects (font (size 1 1) (thickness 0.15))))
 (pad "1" smd rect (at 0 -1.5) (size 1 1) (layers "F.Cu" "F.Mask") (net 1 "A") (uuid "22222222-2222-4222-8222-222222222221"))
 (pad "2" smd rect (at 0 1.5) (size 1 1) (layers "F.Cu" "F.Mask") (net 2 "B") (uuid "22222222-2222-4222-8222-222222222222")))
(footprint "t:W" (layer "F.Cu") (uuid "33333333-3333-4333-8333-333333333330") (at 10 10)
 (property "Reference" "W1" (at 0 -2 0) (layer "F.SilkS") (uuid "33333333-3333-4333-8333-333333333339") (effects (font (size 1 1) (thickness 0.15))))
 (pad "" smd rect (at 0 -5.25) (size 2 9.5) (layers "F.Cu") (uuid "33333333-3333-4333-8333-333333333331"))
 (pad "" smd rect (at 0 5.25) (size 2 9.5) (layers "F.Cu") (uuid "33333333-3333-4333-8333-333333333332")))
(gr_rect (start 0 1) (end 20 19) (stroke (width 0.1) (type default)) (fill no) (layer "Edge.Cuts") (uuid "44444444-4444-4444-8444-444444444444"))
)
"#;

const PROJECT: &str = r#"{"board": {"design_settings": {"rules": {"min_clearance": 0.2, "min_track_width": 0.15, "min_via_diameter": 0.6, "min_through_hole_diameter": 0.3, "min_copper_edge_clearance": 0}}},
 "net_settings": {"classes": [{"name": "Default", "clearance": 0.2, "track_width": 0.3, "via_diameter": 0.6, "via_drill": 0.3}]}}"#;

/// The board routed with `flags` after the default recipe: the routed board, and each track's
/// width and whether it lies in the gap's column (x 9 to 11 mm).
fn route(dir: &Path, flags: &[&str]) -> (Board, Vec<(i64, bool)>) {
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
    let tracks = routed.items().filter(|(_, it)| it.kind == Kind::Track).filter_map(|(_, it)| match it.shape {
        Shape::Capsule { a, b, r } => Some((2 * r, a.x.max(b.x) > 9_000_000 && a.x.min(b.x) < 11_000_000)),
        _ => None,
    }).collect();
    (routed, tracks)
}

fn open(board: &Board) -> usize {
    connectivity(board).airlines.len()
}

#[test]
fn signals_at_their_minimum_share_a_gap_and_widen_outside_it() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("narrow-signals");
    // At the class's width only one of the two gets through.
    let (wide, _) = route(&dir.join("wide"), &["--no-retries"]);
    assert_eq!(open(&wide), 1, "one at 0.3 mm leaves no room for the other");
    let (narrow, tracks) = route(&dir.join("narrow"), &["--no-retries", "--narrow-signals"]);
    assert_eq!(open(&narrow), 0, "{tracks:?}");
    assert!(tracks.iter().filter(|t| t.1).all(|t| t.0 < 300_000), "in the gap at the minimum: {tracks:?}");
    assert!(tracks.iter().any(|t| !t.1 && t.0 == 300_000), "widened to the class outside the gap: {tracks:?}");
    assert!(board::check(&narrow).is_empty(), "{:?}", board::check(&narrow));
    // The recipe's retries come to it when connections stay open.
    let (retried, _) = route(&dir.join("retried"), &[]);
    assert_eq!(open(&retried), 0);
}
