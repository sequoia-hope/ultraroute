//! Other nets' copper keeps out of a pad's solder mask opening, where solder would bridge it to
//! the pad; KiCad's DRC calls that a solder mask bridge.

use board::*;

/// Rules asking no clearance, so what keeps copper away is the mask opening alone.
fn no_clearance() -> kicad_pcb::Rules {
    kicad_pcb::Rules::parse(r#"{"net_settings":{"classes":[{"name":"Default","clearance":0.0,"track_width":0.2}]}}"#).unwrap()
}

/// A board with one pad of net A whose mask opens `margin` mm beyond it (the board's
/// `pad_to_mask_clearance` when `None`), and a track of net B `gap` mm from the pad on `layer`.
fn board(margin: Option<f64>, gap: f64, layer: &str) -> (Board, ItemId) {
    let margin = margin.map_or(String::new(), |m| format!("(solder_mask_margin {m})"));
    let text = format!(
        "(kicad_pcb (version 20240108) (generator \"pcbnew\") (general (thickness 1.6))
         (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal) (38 \"B.Mask\" user) (39 \"F.Mask\" user) (44 \"Edge.Cuts\" user))
         (setup (pad_to_mask_clearance 0.2))
         (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
         (footprint \"R\" (layer \"F.Cu\") (uuid \"0b10c000-0000-4000-8000-0000000000aa\") (at 10 10) (property \"Reference\" \"R1\")
           (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\" \"F.Mask\") (net 1 \"A\") {margin}))
         (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1)))"
    );
    let (pcb, copper) = kicad_pcb::parse_board(&text).unwrap();
    let mut b = Board::new(&pcb, &copper, no_clearance());
    let x = nm(10.5e-3 + 0.1e-3 + gap * 1e-3);
    let l = b.layers.iter().position(|n| n == layer).unwrap();
    let track = b.insert(Item {
        kind: Kind::Track,
        net: 2,
        layers: 1 << l,
        shape: Shape::Capsule { a: Point::new(x, nm(5e-3)), b: Point::new(x, nm(15e-3)), r: nm(0.1e-3) },
        hole: None,
        locked: false,
        span: None,
    });
    (b, track)
}

#[test]
fn another_nets_track_keeps_out_of_a_pads_mask_opening() {
    let (b, t) = board(None, 0.15, "F.Cu");
    assert!(check_item(&b, t).iter().any(|v| v.rule == Rule::Clearance), "0.15 mm from the pad is inside its 0.2 mm opening");
    let (b, t) = board(None, 0.25, "F.Cu");
    assert!(check_item(&b, t).is_empty(), "outside the opening");
    let (b, t) = board(None, 0.15, "B.Cu");
    assert!(check_item(&b, t).is_empty(), "the pad opens no mask on the back");
}

#[test]
fn a_pads_own_mask_margin_overrides_the_boards() {
    let (b, t) = board(Some(0.05), 0.15, "F.Cu");
    assert!(check_item(&b, t).is_empty(), "the pad's opening reaches 0.05 mm");
    let (b, t) = board(Some(0.3), 0.25, "F.Cu");
    assert!(!check_item(&b, t).is_empty(), "the pad's opening reaches 0.3 mm");
}

/// A board with the given setup and drawings, and nothing else but net A's pad at (10, 10).
fn plain(setup: &str, drawn: &str) -> Board {
    let text = format!(
        "(kicad_pcb (version 20240108) (generator \"pcbnew\") (general (thickness 1.6))
         (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal) (38 \"B.Mask\" user) (39 \"F.Mask\" user) (44 \"Edge.Cuts\" user))
         (setup {setup})
         (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
         (footprint \"R\" (layer \"F.Cu\") (uuid \"0b10c000-0000-4000-8000-0000000000ab\") (at 10 10) (property \"Reference\" \"R1\")
           (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net 1 \"A\")))
         (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1))
         {drawn})"
    );
    let (pcb, copper) = kicad_pcb::parse_board(&text).unwrap();
    Board::new(&pcb, &copper, no_clearance())
}

fn via(b: &mut Board, net: u32, x: f64, y: f64) -> ItemId {
    let p = Point::new(nm(x * 1e-3), nm(y * 1e-3));
    b.insert(Item { kind: Kind::Via, net, layers: 3, shape: Shape::Capsule { a: p, b: p, r: nm(0.3e-3) }, hole: Some((p, nm(0.15e-3))), locked: false, span: None })
}

#[test]
fn bare_vias_keep_other_nets_out_of_their_openings_and_tented_ones_do_not() {
    for (setup, bare) in [("(pad_to_mask_clearance 0.2) (tenting none)", true), ("(pad_to_mask_clearance 0.2) (tenting front back)", false)] {
        let mut b = plain(setup, "");
        via(&mut b, 1, 5.0, 5.0);
        // 0.15 mm between the two vias' copper: inside a 0.2 mm opening.
        let v = via(&mut b, 2, 5.75, 5.0);
        assert_eq!(!check_item(&b, v).is_empty(), bare, "{setup}");
    }
}

#[test]
fn an_opening_drawn_in_the_mask_admits_only_the_net_it_bares() {
    let drawn = "(gr_rect (start 9 9) (end 14 11) (layer \"F.Mask\") (fill solid) (width 0))";
    let mut b = plain("(pad_to_mask_clearance 0)", drawn);
    assert_eq!(b.mask_areas.len(), 1);
    assert_eq!(b.mask_areas[0].2, 1, "it bares net A's pad");
    let track = |b: &mut Board, net| {
        b.insert(Item { kind: Kind::Track, net, layers: 1, shape: Shape::Capsule { a: Point::new(nm(12e-3), nm(5e-3)), b: Point::new(nm(12e-3), nm(15e-3)), r: nm(0.1e-3) }, hole: None, locked: false, span: None })
    };
    let a = track(&mut b, 1);
    assert!(check_item(&b, a).is_empty(), "net A may run in its own opening");
    let other = track(&mut b, 2);
    assert!(check_item(&b, other).iter().any(|v| v.rule == Rule::Keepout), "net B would be bared beside A");
}

/// A vertical track of `net` at `x` on the layer bit `layer`, from y 5 to 15.
fn track_at(b: &mut Board, net: u32, x: f64, layer: u64) -> ItemId {
    b.insert(Item { kind: Kind::Track, net, layers: layer, shape: Shape::Capsule { a: Point::new(nm(x * 1e-3), nm(5e-3)), b: Point::new(nm(x * 1e-3), nm(15e-3)), r: nm(0.1e-3) }, hole: None, locked: false, span: None })
}

#[test]
fn a_pads_opening_over_the_side_without_its_copper_bares_that_side() {
    // Net B's pad at (15, 10) has its copper on B.Cu and its 0.2 mm mask opening on the front.
    let pad = "(footprint \"P\" (layer \"F.Cu\") (uuid \"0b10c000-0000-4000-8000-0000000000ac\") (at 15 10) (property \"Reference\" \"P1\")
         (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"B.Cu\" \"F.Mask\") (net 2 \"B\")))";
    let mut b = plain("(pad_to_mask_clearance 0.2)", pad);
    // 0.15 mm from the pad's outline, within its opening, on F.Cu.
    let x = 15.5 + 0.1 + 0.15;
    let a = track_at(&mut b, 1, x, 1);
    assert!(!check_item(&b, a).is_empty(), "net A bared in B's front opening");
    b.remove(a);
    let own = track_at(&mut b, 2, x, 1);
    assert!(check_item(&b, own).is_empty(), "net B's own track may lie there");
    b.remove(own);
    let outside = track_at(&mut b, 1, 15.5 + 0.1 + 0.25, 1);
    assert!(check_item(&b, outside).is_empty(), "outside the opening");
}

#[test]
fn an_opening_drawn_in_the_mask_bares_the_pads_it_touches_not_only_those_it_covers() {
    // A stroke on F.Mask from A's pad (edge at 10.5) to B's (edge at 11.5), covering neither
    // centre: it bares both nets, so neither may run there.
    let drawn = "(footprint \"P\" (layer \"F.Cu\") (uuid \"0b10c000-0000-4000-8000-0000000000ad\") (at 12 10) (property \"Reference\" \"P2\")
         (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net 2 \"B\")))
         (gr_line (start 10.4 10) (end 11.6 10) (layer \"F.Mask\") (stroke (width 0.4) (type solid)))";
    let mut b = plain("(pad_to_mask_clearance 0)", drawn);
    assert_eq!(b.mask_areas.len(), 1);
    assert_eq!(b.mask_areas[0].2, 0, "it bares nets A and B");
    let a = track_at(&mut b, 1, 11.0, 1);
    assert!(check_item(&b, a).iter().any(|v| v.rule == Rule::Keepout));
}

#[test]
fn a_mask_only_pad_admits_the_one_net_of_the_pads_it_bares() {
    // A 4 mm opening with no copper of its own over net A's pad at (10, 10).
    let drawn = "(footprint \"M\" (layer \"F.Cu\") (uuid \"0b10c000-0000-4000-8000-0000000000ae\") (at 10 10) (property \"Reference\" \"M1\")
         (pad \"\" smd circle (at 0 0) (size 4 4) (layers \"F.Mask\")))";
    let mut b = plain("(pad_to_mask_clearance 0)", drawn);
    assert_eq!(b.mask_areas.iter().map(|a| a.2).collect::<Vec<_>>(), vec![1]);
    let own = track_at(&mut b, 1, 11.5, 1);
    assert!(check_item(&b, own).is_empty(), "net A may run in it");
    let other = track_at(&mut b, 2, 11.5, 1);
    assert!(check_item(&b, other).iter().any(|v| v.rule == Rule::Keepout), "net B would be bared there");
}

#[test]
fn two_bare_openings_keep_the_masks_narrowest_web_between_them() {
    // Openings reach 0.1 mm and the web is 0.1 mm: a bare via keeps its copper 0.3 mm from
    // another's, and from a pad's with a front opening.
    let setup = "(pad_to_mask_clearance 0.1) (solder_mask_min_width 0.1) (tenting none)";
    let pad = "(footprint \"P\" (layer \"F.Cu\") (uuid \"0b10c000-0000-4000-8000-0000000000af\") (at 15 10) (property \"Reference\" \"P3\")
         (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\" \"F.Mask\") (net 1 \"A\")))";
    for (gap, bridged) in [(0.25, true), (0.31, false)] {
        let mut b = plain(setup, pad);
        via(&mut b, 1, 5.0, 5.0);
        let v = via(&mut b, 2, 5.6 + gap, 5.0);
        assert_eq!(!check_item(&b, v).is_empty(), bridged, "via to via {gap}");
        let p = via(&mut b, 2, 15.5 + 0.3 + gap, 10.0);
        assert_eq!(!check_item(&b, p).is_empty(), bridged, "via to pad {gap}");
    }
    // A track has no opening: it keeps only the via's 0.1 mm.
    let mut b = plain(setup, "");
    via(&mut b, 1, 5.0, 5.0);
    let t = track_at(&mut b, 2, 5.0 + 0.3 + 0.1 + 0.15, 1);
    assert!(check_item(&b, t).is_empty());
}

#[test]
fn a_bare_vias_opening_keeps_clear_of_an_unplated_holes() {
    // An unplated 3.2 mm hole whose pad is the hole, its mask opening 0.2 mm wider all round
    // (1.8 mm radius), and vias left bare: a 2 mm via of another net opens to 1.2 mm round its
    // centre, so the two openings meet within 3.0 mm, as on digital_clock's Q17.
    let text = "(kicad_pcb (version 20241229) (generator \"pcbnew\") (general (thickness 1.6))
         (layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal) (1 \"F.Mask\" user) (3 \"B.Mask\" user) (25 \"Edge.Cuts\" user))
         (setup (pad_to_mask_clearance 0.2) (tenting))
         (net 0 \"\") (net 1 \"B\")
         (footprint \"H\" (layer \"F.Cu\") (uuid \"0b10c000-0000-4000-8000-0000000000ab\") (at 10 10) (property \"Reference\" \"H1\")
           (pad \"\" np_thru_hole circle (at 0 0) (size 3.2 3.2) (drill 3.2) (layers \"*.Cu\" \"*.Mask\")))
         (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1)))";
    let (pcb, copper) = kicad_pcb::parse_board(text).unwrap();
    let rules = kicad_pcb::Rules::parse(r#"{"board":{"design_settings":{"rules":{"min_hole_clearance":0.25}}},"net_settings":{"classes":[{"name":"Default","clearance":0.0,"track_width":0.2}]}}"#).unwrap();
    let via = |b: &mut Board, d: f64| {
        let at = Point::new(nm((10.0 + d) * 1e-3), nm(10e-3));
        b.insert(Item { kind: Kind::Via, net: 1, layers: 3, shape: Shape::Capsule { a: at, b: at, r: nm(1.0e-3) }, hole: Some((at, nm(0.4e-3))), locked: false, span: None })
    };
    let mut near = Board::new(&pcb, &copper, rules.clone());
    let v = via(&mut near, 2.86);
    assert!(!check_item(&near, v).is_empty(), "the openings meet: KiCad's solder mask bridge");
    let mut far = Board::new(&pcb, &copper, rules);
    let v = via(&mut far, 3.1);
    assert!(check_item(&far, v).is_empty(), "{:?}", check_item(&far, v));
}

#[test]
fn an_unattached_mask_opening_admits_any_one_net_and_keeps_a_second_out() {
    // A bare F.Mask stroke far from every pad: the first net's tracks may run through it, while a
    // second net's track in the same opening is refused as a solder mask bridge.
    let drawn = "(gr_line (start 3 10) (end 7 10) (layer \"F.Mask\") (stroke (width 0.6) (type solid)))";
    let mut b = plain("(pad_to_mask_clearance 0)", drawn);
    assert_eq!(b.mask_areas.len(), 1);
    assert!(b.mask_areas[0].2 >= ANY_ONE_NET, "unclaimed at load: {}", b.mask_areas[0].2);
    let a1 = track_at(&mut b, 1, 4.0, 1);
    let a2 = track_at(&mut b, 1, 6.0, 1);
    assert!(check_item(&b, a1).is_empty(), "one net may enter");
    assert!(check_item(&b, a2).is_empty(), "more copper of that same net may enter too");
    let other = track_at(&mut b, 2, 5.0, 1);
    assert!(check_item(&b, other).iter().any(|v| v.rule == Rule::Keepout), "a second net would bridge in the opening");
    assert!(check_item(&b, a1).iter().any(|v| v.rule == Rule::Keepout), "both nets see the conflict");
    b.remove(a1);
    b.remove(a2);
    assert!(check_item(&b, other).is_empty(), "with net A gone, net B alone may claim it");
}

#[test]
fn the_shapes_of_one_unattached_mask_drawing_share_one_net_claim() {
    // KiCad's DRC checks a drawing on F.Mask as one BOARD_ITEM even where its pieces do not touch,
    // as a frame's two sides or a text's glyphs; two drawings are two items.
    let frame = "(gr_rect (start 3 8) (end 7 12) (layer \"F.Mask\") (stroke (width 0.3) (type solid)) (fill none))";
    let mut b = plain("(pad_to_mask_clearance 0)", frame);
    assert_eq!(b.mask_areas.len(), 4, "the frame's four sides");
    assert!(b.mask_areas.iter().all(|a| a.2 == b.mask_areas[0].2 && a.2 >= ANY_ONE_NET), "one group");
    let a = track_at(&mut b, 1, 3.0, 1);
    assert!(check_item(&b, a).is_empty());
    let other = track_at(&mut b, 2, 7.0, 1);
    assert!(check_item(&b, other).iter().any(|v| v.rule == Rule::Keepout), "a second net on the far side bridges the one opening");
    let lines = "(gr_line (start 3 8) (end 3 12) (layer \"F.Mask\") (stroke (width 0.3) (type solid)))
         (gr_line (start 7 8) (end 7 12) (layer \"F.Mask\") (stroke (width 0.3) (type solid)))";
    let mut b = plain("(pad_to_mask_clearance 0)", lines);
    assert_ne!(b.mask_areas[0].2, b.mask_areas[1].2, "two groups");
    let a = track_at(&mut b, 1, 3.0, 1);
    let other = track_at(&mut b, 2, 7.0, 1);
    assert!(check_item(&b, a).is_empty() && check_item(&b, other).is_empty(), "each net has an opening of its own");
}

#[test]
fn an_unattached_mask_opening_in_an_unfilled_zone_admits_only_the_zones_net() {
    let drawn = "(gr_rect (start 3 9) (end 6 11) (layer \"F.Mask\") (fill solid) (width 0))
         (zone (net 1) (net_name \"A\") (layer \"F.Cu\") (hatch edge 0.5)
           (polygon (pts (xy 1 1) (xy 8 1) (xy 8 19) (xy 1 19))))";
    let mut b = plain("(pad_to_mask_clearance 0)", drawn);
    assert_eq!(b.mask_areas[0].2, 1, "refilling the zone will bare net A in the opening");
    let own = track_at(&mut b, 1, 4.5, 1);
    assert!(check_item(&b, own).is_empty());
    let other = track_at(&mut b, 2, 4.5, 1);
    assert!(check_item(&b, other).iter().any(|v| v.rule == Rule::Keepout));
}

#[test]
fn mask_frame_bridging_two_pad_openings_or_unnetted_copper_admits_no_net() {
    // Modelled on ottawa-badges-2016's JP11 solder jumper: two pads 0.2 mm apart with 0.15 mm
    // mask expansion, an F.Mask stroke just outside their copper that bridges both pad openings,
    // and a second F.Mask stroke touching pad 2 plus an unnetted F.Cu shorting bar.
    let text = "(kicad_pcb (version 20240108) (generator \"pcbnew\") (general (thickness 1.6))
         (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal) (38 \"B.Mask\" user) (39 \"F.Mask\" user) (44 \"Edge.Cuts\" user))
         (setup (pad_to_mask_clearance 0.15) (tenting none))
         (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
         (footprint \"JP\" (layer \"F.Cu\") (uuid \"0b10c000-0000-4000-8000-0000000000b1\") (at 10 10) (property \"Reference\" \"JP11\")
           (pad \"1\" smd rect (at 0 -0.4) (size 0.8 0.6) (layers \"F.Cu\" \"F.Mask\") (net 1 \"A\"))
           (pad \"2\" smd rect (at 0 0.4) (size 0.8 0.6) (layers \"F.Cu\" \"F.Mask\") (net 2 \"B\"))
           (fp_line (start 0 -0.3) (end 0 0.3) (layer \"F.Cu\") (stroke (width 0.15) (type solid)))
           (fp_line (start -0.508 -0.2) (end -0.508 0.2) (layer \"F.Mask\") (stroke (width 0.15) (type solid)))
           (fp_line (start -0.508 0.177) (end 0.508 0.177) (layer \"F.Mask\") (stroke (width 0.15) (type solid))))
         (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1)))";
    let (pcb, copper) = kicad_pcb::parse_board(text).unwrap();
    let mut b = Board::new(&pcb, &copper, no_clearance());
    assert_eq!(b.mask_areas.len(), 2);
    assert_eq!(b.mask_areas[0].2, 0, "left mask stroke bridges pad 1 and pad 2 openings");
    assert_eq!(b.mask_areas[1].2, 0, "lower mask stroke touches pad 2 and unnetted drawn copper");
    let v = via(&mut b, 2, 9.4125, 10.4875);
    assert!(check_item(&b, v).iter().any(|x| x.rule == Rule::Keepout), "bare via opening bridges into the jumper frame");
}
