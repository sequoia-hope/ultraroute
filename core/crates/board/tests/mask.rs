//! Other nets' copper keeps out of a pad's solder mask opening, where solder would bridge it to
//! the pad; KiCad's DRC calls that a solder mask bridge.

use board::*;

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
    let mut b = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
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
    Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap())
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
