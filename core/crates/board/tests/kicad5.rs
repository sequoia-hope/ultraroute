//! What older and hand-drawn boards carry that the router must see as KiCad does: copper drawn
//! rather than routed, a pad's copper moved off its hole, a pad's own clearance, and board edge
//! that does not close into the outline.

use board::*;

fn board(body: &str) -> Board {
    let text = format!(
        "(kicad_pcb (version 20240108) (generator \"pcbnew\") (general (thickness 1.6))
         (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))
         (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
         (gr_rect (start 0 0) (end 30 30) (layer \"Edge.Cuts\") (width 0.1))
         {body})"
    );
    let (pcb, copper) = kicad_pcb::parse_board(&text).unwrap();
    Board::new(&pcb, &copper, kicad_pcb::Rules::parse(r#"{"board":{"design_settings":{"rules":{"min_clearance":0.2,"min_copper_edge_clearance":0.3}}}}"#).unwrap())
}

/// A track of net B on F.Cu from (x, 2) to (x, 28).
fn track(b: &mut Board, x: f64) -> ItemId {
    b.insert(Item {
        kind: Kind::Track,
        net: 2,
        layers: 1,
        shape: Shape::Capsule { a: Point::new(nm(x * 1e-3), nm(2e-3)), b: Point::new(nm(x * 1e-3), nm(28e-3)), r: nm(0.1e-3) },
        hole: None,
        locked: false,
        span: None,
    })
}

#[test]
fn drawn_copper_and_copper_text_are_obstacles() {
    let mut b = board(
        "(gr_poly (pts (xy 10 10) (xy 12 10) (xy 12 12) (xy 10 12)) (layer \"F.Cu\") (width 0) (fill solid))
         (gr_text \"HELLO\" (at 20 15) (layer \"F.Cu\") (effects (font (size 1 1) (thickness 0.15))))",
    );
    assert_eq!(b.items().filter(|(_, it)| it.drawn()).count(), 2, "the polygon and the text's box");
    let t = track(&mut b, 11.0);
    assert!(!check_item(&b, t).is_empty(), "through the drawn polygon");
    let t = track(&mut b, 20.0);
    assert!(!check_item(&b, t).is_empty(), "through the text");
    let t = track(&mut b, 15.5);
    assert!(check_item(&b, t).is_empty(), "between them");
}

#[test]
fn a_drill_offset_moves_the_pads_copper_off_its_hole() {
    let b = board(
        "(footprint \"P\" (layer \"F.Cu\") (uuid \"0b10c000-0000-4000-8000-0000000000b1\") (at 15 15) (property \"Reference\" \"J1\")
           (pad \"1\" thru_hole circle (at 0 0) (size 1.5 1.5) (drill 0.8 (offset 1 0)) (layers \"*.Cu\") (net 1 \"A\")))",
    );
    let (_, pad) = b.items().find(|(_, it)| matches!(it.kind, Kind::Pad { .. })).unwrap();
    let bb = pad.shape.bbox();
    let centre = ((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2);
    assert!((centre.0 - nm(16e-3)).abs() < 1000 && (centre.1 - nm(15e-3)).abs() < 1000, "copper centred 1 mm from the hole: {centre:?}");
    assert_eq!(pad.hole.unwrap().0, Point::new(nm(15e-3), nm(15e-3)), "the hole stays put");
}

#[test]
fn a_pads_own_clearance_holds_where_wider_than_its_nets() {
    let mut b = board(
        "(footprint \"P\" (layer \"F.Cu\") (uuid \"0b10c000-0000-4000-8000-0000000000b2\") (at 15 15) (property \"Reference\" \"R1\")
           (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net 1 \"A\") (clearance 0.5)))",
    );
    // 0.3 mm from the pad's edge: clear of the board's 0.2 mm, not of the pad's 0.5 mm.
    let t = track(&mut b, 15.5 + 0.1 + 0.3);
    assert!(check_item(&b, t).iter().any(|v| v.rule == Rule::Clearance));
}

#[test]
fn an_edge_line_inside_the_board_keeps_copper_off_too() {
    let mut b = board("(gr_line (start 5 5) (end 5 25) (layer \"Edge.Cuts\") (width 0.1))");
    let t = track(&mut b, 5.0);
    assert!(!check_item(&b, t).is_empty(), "a slot drawn as an open line is board edge");
}

#[test]
fn an_edge_arc_keeps_copper_off_along_the_arc_not_its_other_half() {
    // The arc's upper half: from (5, 15) over (15, 5) to (25, 15), y down.
    let mut b = board("(gr_arc (start 5 15) (mid 15 5) (end 25 15) (layer \"Edge.Cuts\") (width 0.1))");
    let across = |b: &mut Board, y: f64| {
        b.insert(Item {
            kind: Kind::Track,
            net: 2,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(nm(12e-3), nm(y * 1e-3)), b: Point::new(nm(18e-3), nm(y * 1e-3)), r: nm(0.1e-3) },
            hole: None,
            locked: false,
            span: None,
        })
    };
    let on_arc = across(&mut b, 5.0);
    assert!(!check_item(&b, on_arc).is_empty(), "a track over the arc's top");
    let other_half = across(&mut b, 25.0);
    assert!(check_item(&b, other_half).is_empty(), "nothing where the circle's other half would be");
}

#[test]
fn an_unplated_slot_is_board_edge_to_copper_and_a_round_hole_is_not() {
    // 0.25 mm from the hole's edge: clear of the 0.2 mm clearance, not of the 0.3 mm edge rule.
    for (drill, edge) in [("(drill oval 2 2.6)", true), ("(drill 2)", false)] {
        let mut b = board(&format!(
            "(footprint \"H\" (layer \"F.Cu\") (uuid \"0b10c000-0000-4000-8000-0000000000b3\") (at 15 15) (property \"Reference\" \"H1\")
               (pad \"\" np_thru_hole oval (at 0 0) (size 2 2.6) {drill} (layers \"*.Cu\" \"*.Mask\")))"
        ));
        let t = track(&mut b, 15.0 + 1.0 + 0.1 + 0.25);
        assert_eq!(!check_item(&b, t).is_empty(), edge, "{drill}");
    }
}

#[test]
fn an_edge_arc_that_ends_where_it_starts_is_a_whole_circle() {
    // KiCad 5 left full circles as arcs from a point back to itself through the far side.
    let mut b = board("(gr_arc (start 15 10) (mid 15 20) (end 15 10) (layer \"Edge.Cuts\") (width 0.1))");
    let t = track(&mut b, 10.0);
    assert!(!check_item(&b, t).is_empty(), "the circle's left side crosses x = 10");
}

#[test]
fn a_pad_without_a_hole_on_every_layer_joins_on_its_side_alone_but_is_copper_on_all() {
    let mut b = board(
        "(footprint \"J\" (layer \"F.Cu\") (uuid \"0b10c000-0000-4000-8000-0000000000b4\") (at 15 15) (property \"Reference\" \"J1\")
           (pad \"1\" smd rect (at 0 0) (size 2 1) (layers \"*.Cu\" \"*.Mask\") (net 1 \"A\")))",
    );
    let (pad, it) = b.items().find(|(_, it)| matches!(it.kind, Kind::Pad { .. })).unwrap();
    assert_eq!(it.layers, 1, "the pad joins on F.Cu only");
    // KiCad still sees its copper on B.Cu: another net's track there shorts to it, and its own
    // net's track there is not connected to it.
    let back = |b: &mut Board, net| {
        b.insert(Item {
            kind: Kind::Track,
            net,
            layers: 2,
            shape: Shape::Capsule { a: Point::new(nm(15e-3), nm(10e-3)), b: Point::new(nm(15e-3), nm(20e-3)), r: nm(0.1e-3) },
            hole: None,
            locked: false,
            span: None,
        })
    };
    let other = back(&mut b, 2);
    assert!(check_item(&b, other).iter().any(|v| v.rule == Rule::Clearance), "net B's track over the pad on B.Cu");
    b.remove(other);
    let own = back(&mut b, 1);
    assert!(check_item(&b, own).is_empty(), "net A's own track may lie there");
    let c = connectivity(&b);
    assert_ne!(c.cluster[&pad], c.cluster[&own], "but it does not join the pad");
}

#[test]
fn an_unplated_slot_lies_along_its_pads_angle_on_the_board() {
    // The pad's angle in the file is on the board: 90° with the footprint's 90° in it, so the
    // slot, long along the pad's own y, lies along the board's x, from 14.1 to 15.9.
    let mut b = board(
        "(footprint \"S\" (layer \"F.Cu\") (uuid \"0b10c000-0000-4000-8000-0000000000b5\") (at 15 15 90) (property \"Reference\" \"SW1\")
           (pad \"\" np_thru_hole oval (at 0 0 90) (size 0.8 2.6) (drill oval 0.8 2.6) (layers \"*.Cu\" \"*.Mask\")))",
    );
    // 0.25 mm beyond the slot's end: clear of the 0.2 mm clearance, not of the 0.3 mm edge rule.
    let t = track(&mut b, 15.0 + 1.3 + 0.1 + 0.25);
    assert!(!check_item(&b, t).is_empty(), "the slot's end is board edge");
}

#[test]
fn a_margin_line_keeps_copper_off_its_stroke_and_an_edge_line_off_its_centre() {
    // 0.4 mm wide lines at x = 5; a track 0.35 mm from the centre line is 0.15 mm from the stroke.
    for (layer, near) in [("Margin", true), ("Edge.Cuts", false)] {
        let mut b = board(&format!("(gr_line (start 5 5) (end 5 25) (layer \"{layer}\") (stroke (width 0.4) (type solid)))"));
        let t = track(&mut b, 5.0 + 0.1 + 0.35);
        assert_eq!(!check_item(&b, t).is_empty(), near, "{layer}");
        let t = track(&mut b, 5.0 + 0.1 + 0.2 + 0.31);
        assert!(check_item(&b, t).is_empty(), "{layer}: 0.31 mm beyond the stroke");
    }
}

#[test]
fn an_edge_arc_keeps_copper_off_on_both_its_sides_to_the_micron() {
    // A half circle of radius 8 about (15, 15), y down; dots of copper 0.2985 mm off it, inside
    // and outside, all round: the arc's straight pieces must not pass farther from either.
    let mut b = board("(gr_arc (start 23 15) (mid 15 23) (end 7 15) (layer \"Edge.Cuts\") (width 0.1))");
    for k in 1..200 {
        let t = std::f64::consts::PI * k as f64 / 200.0;
        for r in [8.0 + 0.1 + 0.2985, 8.0 - 0.1 - 0.2985] {
            let p = Point::new(nm((15.0 + r * t.cos()) * 1e-3), nm((15.0 + r * t.sin()) * 1e-3));
            let dot = b.insert(Item { kind: Kind::Via, net: 2, layers: 1, shape: Shape::Capsule { a: p, b: p, r: nm(0.1e-3) }, hole: None, locked: false, span: None });
            assert!(check_item(&b, dot).iter().any(|v| v.rule == Rule::EdgeClearance), "at {t:.3} rad, radius {r}");
            b.remove(dot);
        }
    }
}

#[test]
fn copper_text_of_several_lines_is_as_tall_as_all_of_them() {
    // Three lines 1 mm high about (15, 15): the last is near y = 16.6, well below one line's box.
    let mut b = board("(gr_text \"ONE\\nTWO\\nTHREE\" (at 15 15) (layer \"F.Cu\") (effects (font (size 1 1) (thickness 0.15))))");
    let t = b.insert(Item {
        kind: Kind::Track,
        net: 2,
        layers: 1,
        shape: Shape::Capsule { a: Point::new(nm(10e-3), nm(16.8e-3)), b: Point::new(nm(20e-3), nm(16.8e-3)), r: nm(0.1e-3) },
        hole: None,
        locked: false,
        span: None,
    });
    assert!(!check_item(&b, t).is_empty(), "through the third line");
}

#[test]
fn an_edge_clearance_rule_holds_copper_further_from_edge_circles_only() {
    let text = "(kicad_pcb (version 20240108) (generator \"pcbnew\") (general (thickness 1.6))
         (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))
         (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
         (gr_rect (start 0 0) (end 30 30) (layer \"Edge.Cuts\") (width 0.1))
         (gr_circle (center 15 15) (end 16.5 15) (layer \"Edge.Cuts\") (width 0.1)))";
    let (pcb, copper) = kicad_pcb::parse_board(text).unwrap();
    let mut rules = kicad_pcb::Rules::parse(r#"{"board":{"design_settings":{"rules":{"min_clearance":0.2,"min_copper_edge_clearance":0.3}}}}"#).unwrap();
    let plain = Board::new(&pcb, &copper, rules.clone());
    rules.apply_custom("(rule \"cast hole\" (constraint edge_clearance (min 0.5mm)) (condition \"B.Layer == 'Edge.Cuts' && B.Shape == 'Circle'\"))", "test");
    let ruled = Board::new(&pcb, &copper, rules);
    // A 0.2 mm track whose edge is 0.4 mm from the 1.5 mm hole, and one 0.4 mm from the outline.
    for (mut b, hole_short) in [(plain, false), (ruled, true)] {
        let by_hole = track(&mut b, 15.0 + 1.5 + 0.4 + 0.1);
        let found = check_item(&b, by_hole);
        assert_eq!(found.iter().any(|v| v.rule == Rule::EdgeClearance && v.required == nm(0.5e-3)), hole_short, "{found:?}");
        let by_outline = track(&mut b, 0.4 + 0.1);
        assert!(check_item(&b, by_outline).is_empty(), "the outline keeps the board's 0.3 mm");
    }
}

#[test]
fn an_outline_that_does_not_close_is_reported() {
    let text = "(kicad_pcb (version 20240108) (generator \"pcbnew\") (general (thickness 1.6))
         (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))
         (net 0 \"\")
         (gr_line (start 0 0) (end 30 0) (layer \"Edge.Cuts\") (width 0.1))
         (gr_line (start 30 0) (end 30 30) (layer \"Edge.Cuts\") (width 0.1)))";
    let (pcb, copper) = kicad_pcb::parse_board(text).unwrap();
    let b = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
    assert!(b.outline.is_empty());
    assert!(b.warnings.iter().any(|w| w.contains("outline")), "{:?}", b.warnings);
    // A closed outline warns of nothing.
    assert!(!board("").warnings.iter().any(|w| w.contains("outline")));
}

#[test]
fn a_curved_edge_closes_the_outline_and_keeps_copper_clear() {
    // Three straight sides and a Bezier for the fourth, bulging to y = 37.5 at x = 15.
    let text = "(kicad_pcb (version 20240108) (generator \"pcbnew\") (general (thickness 1.6))
         (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))
         (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
         (gr_line (start 0 0) (end 30 0) (layer \"Edge.Cuts\") (width 0.1))
         (gr_line (start 30 0) (end 30 30) (layer \"Edge.Cuts\") (width 0.1))
         (gr_curve (pts (xy 30 30) (xy 20 40) (xy 10 40) (xy 0 30)) (layer \"Edge.Cuts\") (width 0.1))
         (gr_line (start 0 30) (end 0 0) (layer \"Edge.Cuts\") (width 0.1)))";
    let (pcb, copper) = kicad_pcb::parse_board(text).unwrap();
    let mut b = Board::new(&pcb, &copper, kicad_pcb::Rules::parse(r#"{"board":{"design_settings":{"rules":{"min_copper_edge_clearance":0.5}}}}"#).unwrap());
    assert_eq!(b.outline.len(), 1, "the curve closes the outline: {:?}", b.warnings);
    let at = |b: &mut Board, y: f64| {
        let id = b.insert(Item {
            kind: Kind::Track,
            net: 2,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(nm(14.5e-3), nm(y * 1e-3)), b: Point::new(nm(15.5e-3), nm(y * 1e-3)), r: nm(0.05e-3) },
            hole: None,
            locked: false,
            span: None,
        });
        let edge = check_item(b, id).iter().any(|v| v.rule == Rule::EdgeClearance);
        b.remove(id);
        edge
    };
    assert!(at(&mut b, 37.3), "0.15 mm inside the curve's bulge");
    assert!(!at(&mut b, 36.0), "1.45 mm inside it");
}

#[test]
fn copper_at_the_clearance_from_an_arc_track_is_judged_as_from_the_true_arc() {
    // A quarter arc of radius 5 mm round (10, 10), 0.2 mm wide, net A; clearance 0.2 mm.
    let mut b = board("(arc (start 15 10) (mid 13.5355339 13.5355339) (end 10 15) (width 0.2) (layer \"F.Cu\") (net 1) (uuid \"aaaaaaaa-0000-4000-8000-000000000001\"))");
    let c = Point::new(nm(10e-3), nm(10e-3));
    // A dot of copper of net B at `radius` mm from the centre, 37 degrees round: between chords.
    let breaks = |b: &mut Board, radius: f64| {
        let t = 37f64.to_radians();
        let p = Point::new(c.x + nm(radius * 1e-3 * t.cos()), c.y + nm(radius * 1e-3 * t.sin()));
        let id = b.insert(Item { kind: Kind::Track, net: 2, layers: 1, shape: Shape::Capsule { a: p, b: p, r: nm(0.05e-3) }, hole: None, locked: false, span: None });
        let hit = check_item(b, id).iter().any(|v| v.rule == Rule::Clearance);
        b.remove(id);
        hit
    };
    // Outside: the arc's edge at 5.1 mm, the dot's at radius - 0.05, so clear at 5.35.
    assert!(!breaks(&mut b, 5.35), "exactly the clearance outside the arc");
    assert!(breaks(&mut b, 5.349), "1 um closer outside");
    // Inside: the arc's edge at 4.9 mm, so clear at 4.65.
    assert!(!breaks(&mut b, 4.65), "exactly the clearance inside the arc");
    assert!(breaks(&mut b, 4.651), "1 um closer inside");
}

/// A rule area keeping tracks and vias off F.Cu over (2, 2)-(22, 22) but for a cutout over
/// (7, 7)-(17, 17), as KiCad writes one: the outline, then the cutout.
pub const KEEPOUT_WITH_CUTOUT: &str = "(zone (net 0) (net_name \"\") (layer \"F.Cu\") (uuid \"cccccccc-0000-4000-8000-000000000001\") (hatch edge 0.5)
    (connect_pads (clearance 0)) (min_thickness 0.25)
    (keepout (tracks not_allowed) (vias not_allowed) (pads allowed) (copperpour allowed) (footprints allowed))
    (fill (thermal_gap 0.5) (thermal_bridge_width 0.5))
    (polygon (pts (xy 2 2) (xy 22 2) (xy 22 22) (xy 2 22)))
    (polygon (pts (xy 7 7) (xy 17 7) (xy 17 17) (xy 7 17))))";

#[test]
fn a_keepouts_cutout_forbids_nothing() {
    let mut b = board(KEEPOUT_WITH_CUTOUT);
    let keepout_hit = |b: &mut Board, a: (f64, f64), z: (f64, f64)| {
        let id = b.insert(Item {
            kind: Kind::Track,
            net: 2,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(nm(a.0 * 1e-3), nm(a.1 * 1e-3)), b: Point::new(nm(z.0 * 1e-3), nm(z.1 * 1e-3)), r: nm(0.1e-3) },
            hole: None,
            locked: false,
            span: None,
        });
        let hit = check_item(b, id).iter().any(|v| v.rule == Rule::Keepout);
        b.remove(id);
        hit
    };
    assert!(!keepout_hit(&mut b, (10.0, 10.0), (14.0, 10.0)), "inside the cutout");
    assert!(keepout_hit(&mut b, (10.0, 10.0), (19.0, 10.0)), "out across the cutout's edge into the keepout");
    assert!(keepout_hit(&mut b, (3.0, 3.0), (5.0, 3.0)), "in the keepout");
}
