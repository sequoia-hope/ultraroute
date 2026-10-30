//! The routing grid as a board's custom rules shape it: the sizes of a net's vias and tracks,
//! and the layers a rule keeps a net's tracks off.

use board::Board;
use grid::{ObstacleMap, Profile};

fn board(custom: &str) -> Board {
    let text = "(kicad_pcb (version 20240108) (generator \"pcbnew\") (general (thickness 1.6))
         (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))
         (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
         (gr_rect (start 0 0) (end 30 30) (layer \"Edge.Cuts\") (width 0.1)))";
    let (pcb, copper) = kicad_pcb::parse_board(text).unwrap();
    let mut rules = kicad_pcb::Rules::parse(
        r#"{"board":{"design_settings":{"rules":{"min_clearance":0.2,"min_track_width":0.15,"min_via_annular_width":0.1}}},"net_settings":{"classes":[{"name":"Default","clearance":0.2,"track_width":0.2,"via_diameter":0.6,"via_drill":0.3}]}}"#,
    )
    .unwrap();
    rules.apply_custom(custom, "test.kicad_dru");
    Board::new(&pcb, &copper, rules)
}

#[test]
fn via_rules_grow_a_nets_vias_and_never_shrink_them() {
    let b = board("(version 1)\n(rule \"B drill\" (condition \"A.Type == 'Via' && A.NetName == 'B'\") (constraint hole_size (min 0.4mm)))\n(rule \"small\" (constraint via_diameter (min 0.3mm)))");
    let (a, bb) = (Profile::for_net(&b, 1), Profile::for_net(&b, 2));
    assert_eq!((a.drill_radius, a.via_radius), (150_000, 300_000), "net A as its class has it");
    // 0.4 mm drill, and the ring of 0.1 mm round it: 0.6 mm wide.
    assert_eq!((bb.drill_radius, bb.via_radius), (200_000, 300_000));
}

#[test]
fn a_rule_keeping_a_net_off_a_layer_closes_that_layer_to_it() {
    let b = board("(version 1)\n(rule \"B on the back\" (condition \"A.NetName == 'B' && A.Layer != 'B.Cu'\") (constraint disallow track))");
    let map = ObstacleMap::new(&b, Profile::for_net(&b, 2), 250_000, false);
    let cells = map.nx * map.ny;
    let centre = cells / 2 + map.nx / 2;
    assert!(map.blocked_at(2, centre), "F.Cu is closed to B");
    assert!(!map.blocked_at(2, cells + centre), "B.Cu is open");
    let other = ObstacleMap::new(&b, Profile::for_net(&b, 1), 250_000, false);
    assert!(!other.blocked_at(1, centre), "and F.Cu open to A");
}
