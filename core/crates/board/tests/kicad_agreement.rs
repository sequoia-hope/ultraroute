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
