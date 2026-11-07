//! A row of 0.4 mm-pitch pins whose only room is each pin's own centre line, as on freeDSPx's
//! VQFN-32: 0.2 mm tracks, 0.1778 mm clearance and mask openings 0.2 mm past each 0.2 mm pin
//! leave a track leaving a pin 11 um either side of that line, and on its openings' edges, which
//! no cell centre lies on. Every pin escapes, along its line, and the escapes break no rule.

use board::*;
use ::grid::*;

/// Seven oval pins of U1 at x 10, y 8.8 to 11.2, each wired to its own pad of J1 10 mm left, and
/// U1's exposed pad 0.2 mm inside them, as a QFN's, so every pin but the row's ends leaves left.
fn board() -> Board {
    let mut pins = String::new();
    let mut pads = String::new();
    for k in 0..7 {
        let y = -1.2 + 0.4 * k as f64;
        pins += &format!(r#"(pad "{n}" smd oval (at 0 {y:.1}) (size 0.8 0.2) (layers "F.Cu" "F.Mask") (net {n} "S{n}") (uuid "00000000-0000-4000-8000-0000000001{n:02}"))"#, n = k + 1);
        pads += &format!(r#"(pad "{n}" smd rect (at 0 {y:.1}) (size 1 0.25) (layers "F.Cu" "F.Mask") (net {n} "S{n}") (uuid "00000000-0000-4000-8000-0000000002{n:02}"))"#, n = k + 1, y = 2.0 * (k as f64 - 3.0));
    }
    let nets: String = (1..=7).map(|n| format!(r#"(net {n} "S{n}")"#)).collect();
    let text = format!(
        r#"(kicad_pcb (version 20241229) (generator "pcbnew") (general (thickness 1.6))
          (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (1 "F.Mask" user) (3 "B.Mask" user) (25 "Edge.Cuts" user))
          (setup (pad_to_mask_clearance 0.2))
          (net 0 "") {nets}
          (footprint "t:U" (layer "F.Cu") (uuid "11111111-1111-4111-8111-111111111110") (at 10 10)
            (property "Reference" "U1" (at 0 -3 0) (layer "F.SilkS") (uuid "11111111-1111-4111-8111-111111111119"))
            {pins}
            (pad "33" smd rect (at 2.1 0) (size 3 4) (layers "F.Cu" "F.Mask") (uuid "00000000-0000-4000-8000-000000000133")))
          (footprint "t:J" (layer "F.Cu") (uuid "22222222-2222-4222-8222-222222222220") (at 0 10)
            (property "Reference" "J1" (at 0 -8 0) (layer "F.SilkS") (uuid "22222222-2222-4222-8222-222222222229"))
            {pads})
          (gr_rect (start -5 0) (end 15 20) (layer "Edge.Cuts") (width 0.1)))"#
    );
    let (pcb, copper) = kicad_pcb::parse_board(&text).unwrap();
    let rules = kicad_pcb::Rules::parse(
        r#"{"board": {"design_settings": {"rules": {"min_clearance": 0.0, "min_track_width": 0.2, "min_via_diameter": 0.6, "min_through_hole_diameter": 0.3}}},
            "net_settings": {"classes": [{"name": "Default", "clearance": 0.1778, "track_width": 0.2, "via_diameter": 0.8, "via_drill": 0.3}]}}"#,
    )
    .unwrap();
    Board::new(&pcb, &copper, rules)
}

#[test]
fn pins_with_room_only_on_their_centre_lines_escape_along_them() {
    let mut b = board();
    assert!(check(&b).is_empty(), "the bare row breaks no rule: {:?}", check(&b));
    let u1 = b.footprints.iter().position(|f| f.reference == "U1").unwrap();
    let stats = prefanout(&mut b, &PrefanoutOptions { parts: vec![u1], guard_mouths: true, ..Default::default() }, &mut |_| {});
    assert_eq!(stats.failed, 0, "every pin escapes: {:?}", stats.failed_pads);
    assert_eq!(stats.escaped, 7, "{stats:?}");
    assert!(check(&b).is_empty(), "the escapes break no rule: {:?}", check(&b));
}
