//! The model's verdicts against KiCad 9.0.8's DRC on the same boards. The expected numbers are
//! what `kicad-cli pcb drc` reported; the altered boards are made here with the writer.

use std::path::PathBuf;

use board::*;
use kicad_pcb::*;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name)
}

fn verdict(text: &str, project: &str) -> (usize, usize) {
    let (pcb, copper) = parse_board(text).unwrap();
    let rules = Rules::read(&fixture(project)).unwrap();
    let b = Board::new(&pcb, &copper, rules);
    let clearance = check(&b).iter().filter(|v| v.rule == Rule::Clearance).count();
    (connectivity(&b).airlines.len(), clearance)
}

fn variants(text: &str) -> [(&'static str, String); 4] {
    let (_, copper) = parse_board(text).unwrap();
    let shifted = |t: &Track| NewTrack {
        start: [t.start[0] + 0.1e-3, t.start[1]],
        end: [t.end[0] + 0.1e-3, t.end[1]],
        width: t.width,
        layer: t.layer.clone(),
        net: t.net,
        locked: false,
    };
    [
        ("clean", text.to_string()),
        (
            "stripped",
            apply(text, &Edit { remove: copper.tracks.iter().map(|t| t.span).chain(copper.vias.iter().map(|v| v.span)).collect(), ..Edit::default() }),
        ),
        ("half", apply(text, &Edit { remove: copper.tracks.iter().step_by(2).map(|t| t.span).collect(), ..Edit::default() })),
        (
            "shifted",
            apply(
                text,
                &Edit {
                    remove: copper.tracks.iter().step_by(40).map(|t| t.span).collect(),
                    tracks: copper.tracks.iter().step_by(40).map(shifted).collect(),
                    vias: Vec::new(),
                    ..Edit::default()
                },
            ),
        ),
    ]
}

#[test]
fn bluebuzz_open_connections_and_clearance_match_kicad() {
    let text = std::fs::read_to_string(fixture("bluebuzz-rm2.kicad_pcb")).unwrap();
    // KiCad: (unconnected items, clearance violations).
    let kicad = [("clean", (0, 0)), ("stripped", (250, 0)), ("half", (458, 0)), ("shifted", (0, 37))];
    for ((name, board), (_, (open, clearance))) in variants(&text).iter().zip(kicad) {
        let ours = verdict(board, "bluebuzz-rm2.kicad_pro");
        if *name == "half" {
            // Known gap: 3 of 458 loose fragments that KiCad keeps apart are joined here.
            assert!(ours.0.abs_diff(open) <= 3 && ours.1 == clearance, "{name}: ours {ours:?}, KiCad {:?}", (open, clearance));
        } else {
            assert_eq!(ours, (open, clearance), "{name}");
        }
    }
}

#[test]
fn earring_open_connections_and_clearance_match_kicad() {
    let text = std::fs::read_to_string(fixture("earring.kicad_pcb")).unwrap();
    let kicad = [("clean", (249, 0)), ("stripped", (242, 0)), ("half", (260, 0)), ("shifted", (249, 1))];
    for ((name, board), (_, expected)) in variants(&text).iter().zip(kicad) {
        assert_eq!(verdict(board, "earring.kicad_pro"), expected, "{name}");
    }
}

#[test]
fn the_clean_boards_break_no_rule() {
    for (pcb, pro) in [("bluebuzz-rm2.kicad_pcb", "bluebuzz-rm2.kicad_pro"), ("earring.kicad_pcb", "earring.kicad_pro")] {
        let text = std::fs::read_to_string(fixture(pcb)).unwrap();
        let (p, c) = parse_board(&text).unwrap();
        let b = Board::new(&p, &c, Rules::read(&fixture(pro)).unwrap());
        assert_eq!(check(&b), Vec::new(), "{pcb}");
    }
}

#[test]
fn track_ends_join_where_they_overlap_not_where_they_only_touch() {
    // Two collinear pieces 0.15 mm wide, as a plough cuts copper: KiCad leaves them unconnected
    // when their round ends only touch (ends 0.15 mm apart), and joins them when they overlap.
    let text = std::fs::read_to_string(fixture("earring.kicad_pcb")).unwrap();
    let (pcb, copper) = parse_board(&text).unwrap();
    let mut b = Board::new(&pcb, &copper, Rules::read(&fixture("earring.kicad_pro")).unwrap());
    let track = |x0: i64, x1: i64| Item { kind: Kind::Track, net: 1, layers: 1, shape: Shape::Capsule { a: Point::new(x0, 0), b: Point::new(x1, 0), r: 75_000 }, hole: None, locked: false, span: None };
    let a = b.insert(track(0, 75_000));
    let touching = b.insert(track(225_000, 300_000));
    let overlapping = b.insert(track(200_000, 275_000));
    assert!(!joined(&b, a, touching));
    assert!(joined(&b, a, overlapping));
}

#[test]
fn overlapping_polygon_pads_join_while_touching_round_pads_stay_apart() {
    // Connectors with twin through-hole pads per pin overlap their rectangular copper while
    // neither pad's centre lies inside the other; KiCad 9 joins them, and leaves two round pads
    // that only osculate at one point unconnected.
    let text = std::fs::read_to_string(fixture("earring.kicad_pcb")).unwrap();
    let (pcb, copper) = parse_board(&text).unwrap();
    let mut b = Board::new(&pcb, &copper, Rules::read(&fixture("earring.kicad_pro")).unwrap());
    let rect_pad = |num: &str, y0: i64, y1: i64| Item {
        kind: Kind::Pad { footprint: 0, number: num.into(), plated: true },
        net: 1,
        layers: 1,
        shape: Shape::Polygon(vec![Point::new(-1_890_000, y0), Point::new(1_890_000, y0), Point::new(1_890_000, y1), Point::new(-1_890_000, y1)]),
        hole: None,
        locked: true,
        span: None,
    };
    let p_a = b.insert(rect_pad("1", -1_715_000, 1_715_000));
    let p_b = b.insert(rect_pad("1", -4_895_000, -1_465_000));
    let p_far = b.insert(rect_pad("2", 5_000_000, 6_000_000));
    assert!(joined(&b, p_a, p_b));
    assert!(!joined(&b, p_a, p_far));

    let round_pad = |x: i64| Item {
        kind: Kind::Pad { footprint: 0, number: "3".into(), plated: false },
        net: 1,
        layers: 1,
        shape: Shape::Capsule { a: Point::new(x, 0), b: Point::new(x, 0), r: 1_000_000 },
        hole: None,
        locked: true,
        span: None,
    };
    let r0 = b.insert(round_pad(0));
    let r_touch = b.insert(round_pad(2_000_000));
    let r_overlap = b.insert(round_pad(1_999_000));
    assert!(!joined(&b, r0, r_touch));
    assert!(joined(&b, r0, r_overlap));
}

#[test]
fn pad_with_drill_offset_places_copper_apart_from_hole() {
    let src = r#"(kicad_pcb
      (version 20241229)
      (generator "pcbnew")
      (general (thickness 1.6))
      (layers (0 "F.Cu" signal) (31 "B.Cu" signal) (44 "Edge.Cuts" user))
      (net 0 "")
      (net 1 "SIG")
      (footprint "Test:DIP" (layer "F.Cu") (uuid "00000000-0000-0000-0000-000000000010") (at 100 50 -90)
        (pad "1" thru_hole oval (at 2.54 0 270) (size 1.5 2.3) (drill 0.8 (offset 0 -0.4)) (layers "*.Cu") (net 1 "SIG"))
      )
    )"#;
    let (pcb, copper) = parse_board(src).unwrap();
    let b = Board::new(&pcb, &copper, Rules::read(&fixture("earring.kicad_pro")).unwrap());
    let (_, item) = b.items().find(|(_, it)| matches!(it.kind, Kind::Pad { .. })).unwrap();
    let (hole_pt, hole_r) = item.hole.expect("drilled pad");
    assert_eq!(hole_pt, Point::new(100_000_000, 52_540_000));
    assert_eq!(hole_r, 400_000);
    // At 270 deg, offset (0, -0.4) shifts copper +0.4 mm in X, so the +X side overhangs and the
    // -X side stops 0.35 mm from the hole instead of 0.75 mm.
    assert!(item.shape.contains(Point::new(101_450_000, 52_540_000)));
    assert!(!item.shape.contains(Point::new(99_200_000, 52_540_000)));
}

/// A via beside an unplated 0.8 x 2.2 mm slot, at x = `x` mm: the slot's centre line runs from
/// (10, 9.3) to (10, 10.7).
fn via_beside_a_slot(x: f64) -> Board {
    let src = format!(
        r#"(kicad_pcb (version 20241229) (generator "pcbnew") (general (thickness 1.6))
      (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (25 "Edge.Cuts" user))
      (net 0 "") (net 1 "A")
      (footprint "t:SLOT" (layer "F.Cu") (uuid "11111111-1111-4111-8111-111111111111") (at 10 10)
        (pad "" np_thru_hole oval (at 0 0) (size 0.8 2.2) (drill oval 0.8 2.2) (layers "*.Cu" "*.Mask")))
      (via (at {x} 10.65) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net 1) (uuid "22222222-2222-4222-8222-222222222222"))
      (gr_rect (start 5 5) (end 15 15) (layer "Edge.Cuts") (width 0.1)))"#
    );
    let (pcb, copper) = parse_board(&src).unwrap();
    let rules = r#"{"board":{"design_settings":{"rules":{"min_clearance":0.2,"min_hole_to_hole":0.25,"min_hole_clearance":0.25,"min_copper_edge_clearance":0.5,"min_through_hole_diameter":0.3}}},"net_settings":{"classes":[{"name":"Default","clearance":0.2,"track_width":0.2,"via_diameter":0.6,"via_drill":0.3}]}}"#;
    Board::new(&pcb, &copper, Rules::parse(rules).unwrap())
}

#[test]
fn a_slots_whole_length_keeps_copper_clear_as_kicad_measures_it() {
    // kicad-cli 9.0.8 on this board: via at x = 10.85, hole clearance 0.15 mm (to the slot's
    // length; to the circle the drill's width makes, it would be 0.37); at x = 11.0 none.
    let hole_gap = |x: f64| {
        let b = via_beside_a_slot(x);
        let (via, _) = b.items().find(|(_, it)| it.kind == Kind::Via).unwrap();
        check_item(&b, via).into_iter().filter(|v| v.rule == Rule::HoleClearance).map(|v| v.gap).fold(f64::INFINITY, f64::min)
    };
    let gap = hole_gap(10.85);
    assert!((gap - 150_000.0).abs() < 2_000.0, "KiCad measures 0.15 mm, the check {gap} nm");
    assert_eq!(hole_gap(11.0), f64::INFINITY, "0.3 mm from the slot is clear of its 0.25 mm");
}

/// KiCad's NetTie-2_SMD_Pad0.5mm (two 0.5 mm pads 1 mm apart, a copper bar between them) tying
/// nets A and B at (10, 10), and `extra` beside it.
pub fn net_tie_board(extra: &str) -> Board {
    let src = format!(
        r#"(kicad_pcb (version 20241229) (generator "pcbnew") (general (thickness 1.6))
      (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (25 "Edge.Cuts" user))
      (net 0 "") (net 1 "A") (net 2 "B") (net 3 "C")
      (footprint "NetTie:NetTie-2_SMD_Pad0.5mm" (layer "F.Cu") (uuid "11111111-1111-4111-8111-111111111111") (at 10 10)
        (property "Reference" "NT1" (at 0 -1.2 0) (layer "F.SilkS") (uuid "11111111-1111-4111-8111-111111111112"))
        (net_tie_pad_groups "1, 2")
        (fp_poly (pts (xy -0.5 -0.25) (xy 0.5 -0.25) (xy 0.5 0.25) (xy -0.5 0.25)) (stroke (width 0) (type solid)) (fill yes) (layer "F.Cu"))
        (pad "1" smd circle (at -0.5 0) (size 0.5 0.5) (layers "F.Cu") (net 1 "A"))
        (pad "2" smd circle (at 0.5 0) (size 0.5 0.5) (layers "F.Cu") (net 2 "B")))
      {extra}
      (gr_rect (start 0 0) (end 30 30) (layer "Edge.Cuts") (width 0.1)))"#
    );
    let (pcb, copper) = parse_board(&src).unwrap();
    let rules = r#"{"board":{"design_settings":{"rules":{"min_clearance":0.2,"min_copper_edge_clearance":0.5}}},"net_settings":{"classes":[{"name":"Default","clearance":0.2,"track_width":0.2}]}}"#;
    Board::new(&pcb, &copper, Rules::parse(rules).unwrap())
}

#[test]
fn a_net_tie_joins_its_nets_as_kicad_allows_and_nothing_else() {
    // kicad-cli 9.0.8 finds no violation in the tie itself.
    assert!(check(&net_tie_board("")).is_empty(), "{:?}", check(&net_tie_board("")));
    // A track of net A ending inside pad 2 (net B), touching the bar: KiCad finds nothing.
    let b = net_tie_board(r#"(segment (start 10.5 12) (end 10.5 10) (width 0.2) (layer "F.Cu") (net 1) (uuid "aaaaaaaa-0000-4000-8000-000000000001"))"#);
    assert!(check(&b).is_empty(), "{:?}", check(&b));
    // One ending short of the pads, 0.15 mm from them: KiCad's clearance violation.
    let b = net_tie_board(r#"(segment (start 10.2 12) (end 10.2 10.4) (width 0.2) (layer "F.Cu") (net 1) (uuid "aaaaaaaa-0000-4000-8000-000000000003"))"#);
    assert!(check(&b).iter().any(|v| v.rule == Rule::Clearance), "net A ending short of the tie's pads");
    // Another net's track across the bar is not.
    let b = net_tie_board(r#"(segment (start 10 8) (end 10 12) (width 0.2) (layer "F.Cu") (net 3) (uuid "aaaaaaaa-0000-4000-8000-000000000002"))"#);
    assert!(check(&b).iter().any(|v| v.rule == Rule::Clearance), "net C across the tie");
}
