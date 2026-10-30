//! `--place` moves a part, and the zones and rule areas it carries with it: KiCad writes those in
//! the board's frame, not the part's.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use board::Board;

const BOARD: &str = r#"(kicad_pcb (version 20241229) (generator "pcbnew") (generator_version "9.0") (general (thickness 1.6) (legacy_teardrops no))
 (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (25 "Edge.Cuts" user))
 (net 0 "") (net 1 "A")
 (footprint "t:KEEP" (layer "F.Cu") (uuid "11111111-1111-4111-8111-111111111111") (at 10 10)
  (property "Reference" "U1" (at 0 -3 0) (layer "F.SilkS") (uuid "11111111-1111-4111-8111-111111111112") (effects (font (size 1 1) (thickness 0.15))))
  (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "A") (uuid "11111111-1111-4111-8111-111111111113"))
  (zone (net 0) (net_name "") (layers "F.Cu") (uuid "11111111-1111-4111-8111-111111111114") (name "no vias") (hatch edge 0.5)
   (connect_pads (clearance 0)) (min_thickness 0.25)
   (keepout (tracks allowed) (vias not_allowed) (pads allowed) (copperpour allowed) (footprints allowed))
   (fill (thermal_gap 0.5) (thermal_bridge_width 0.5))
   (polygon (pts (xy 9 9) (xy 11 9) (xy 11 11) (xy 9 11)))))
 (gr_rect (start 0 0) (end 30 30) (stroke (width 0.1) (type default)) (fill no) (layer "Edge.Cuts") (uuid "33333333-3333-4333-8333-333333333333"))
)
"#;

#[test]
fn a_placed_part_takes_its_zones_with_it() {
    let dir = std::env::temp_dir().join(format!("ultraroute-place-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pcb = dir.join("b.kicad_pcb");
    std::fs::write(&pcb, BOARD).unwrap();
    std::fs::write(dir.join("b.kicad_pro"), "{}").unwrap();
    // --place takes y pointing up: KiCad's (15, 20) is 15,-20.
    let args: Vec<String> = ["route", pcb.to_str().unwrap(), "-o", "unused.kicad_pcb", "--no-route", "--place", "U1=15,-20"].map(String::from).to_vec();
    let a = ultraroute::parse(&args).unwrap();
    let (board, text) = Board::load(&pcb).unwrap();
    let out = ultraroute::run(&a, board, &text, None, Arc::new(AtomicBool::new(false)));
    std::fs::remove_dir_all(&dir).unwrap();
    let (pcb, copper) = kicad_pcb::parse_board(&out).unwrap();
    let at = pcb.footprints[0].at;
    assert!((at[0] - 15e-3).abs() < 1e-9 && (at[1] - 20e-3).abs() < 1e-9, "the part is at {at:?}");
    let zone = copper.zones.iter().find(|z| z.name == "no vias").expect("the zone is still there");
    // It moved by (5, 10) mm with the part.
    let xs: Vec<f64> = zone.outline.iter().map(|p| p[0] * 1e3).collect();
    let ys: Vec<f64> = zone.outline.iter().map(|p| p[1] * 1e3).collect();
    let near = |v: &[f64], lo: f64, hi: f64| (v.iter().cloned().fold(f64::INFINITY, f64::min) - lo).abs() < 1e-6 && (v.iter().cloned().fold(f64::NEG_INFINITY, f64::max) - hi).abs() < 1e-6;
    assert!(near(&xs, 14.0, 16.0) && near(&ys, 19.0, 21.0), "the zone is at x {xs:?}, y {ys:?}");
}
