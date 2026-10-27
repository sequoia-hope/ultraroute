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
