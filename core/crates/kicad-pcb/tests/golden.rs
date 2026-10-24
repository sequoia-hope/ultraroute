//! Spec oracles O1 (parser golden), O2 (exact board area), O4 (loud
//! outline failures) and the failure-mode table of §6, on authored
//! fixtures (`tests/fixtures/*.kicad_pcb`, every one written for this
//! repo — no KiCad-library content).
//!
//! Regenerate the goldens deliberately with `UPDATE_GOLDEN=1 cargo test -p
//! kicad-pcb --test golden`, then read the diff.

use std::f64::consts::PI;
use std::path::PathBuf;

use kicad_pcb::*;
use serde::Serialize;

fn fixture(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[derive(Serialize)]
struct Golden<'a> {
    pcb: &'a Pcb,
    loops: Result<OutlineLoops, OutlineError>,
}

fn check_golden(name: &str) -> (Pcb, Result<OutlineLoops, OutlineError>) {
    let pcb = parse_kicad_pcb(&fixture(&format!("{name}.kicad_pcb"))).unwrap();
    let loops = pcb.outline_loops();
    let golden = Golden {
        pcb: &pcb,
        loops: loops.clone(),
    };
    let actual = serde_json::to_string_pretty(&golden).unwrap() + "\n";
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.json"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (run with UPDATE_GOLDEN=1)", path.display()));
    if expected != actual {
        // Byte-exact: float_roundtrip makes the JSON a faithful f64 image.
        let mismatch = expected
            .lines()
            .zip(actual.lines())
            .position(|(a, b)| a != b)
            .map(|i| i + 1);
        panic!(
            "{} differs from the parser output (first differing line: {:?}); run with UPDATE_GOLDEN=1 and read the diff",
            path.display(),
            mismatch
        );
    }
    (pcb, loops)
}

const MM2: f64 = 1e-6;

// ── O1 / O2: the four good boards ─────────────────────────────────────

#[test]
fn rect_v8_golden_and_values() {
    let (pcb, loops) = check_golden("rect_v8");
    assert_eq!(pcb.version, 20240108);
    assert_eq!(pcb.generator.as_deref(), Some("pcbnew"));
    // Stackup copper+core = 0.035 + 1.53 + 0.035 = 1.6 mm, equal to general.
    assert!((pcb.thickness_m - 1.6e-3).abs() < 1e-18);
    assert_eq!(pcb.thickness_general_m, Some(1.6e-3));
    assert!((pcb.thickness_stackup_m.unwrap() - 1.6e-3).abs() < 1e-18);
    assert_eq!(pcb.title_block.title, "Waffle test board");
    assert_eq!(pcb.title_block.rev, "A");
    assert_eq!(
        pcb.title_block.comments,
        vec!["authored fixture, no library content", "second comment"]
    );
    assert_eq!(pcb.copper_layers, 2);
    assert_eq!(pcb.nets.len(), 3);
    assert_eq!(pcb.nets[1].name, "GND");
    // Only the Edge.Cuts lines; the silkscreen gr_line is not outline.
    assert_eq!(pcb.outline.len(), 4);
    assert!(pcb.outline.iter().all(|p| p.footprint.is_none()));

    let loops = loops.unwrap();
    assert!(loops.holes.is_empty());
    assert!((loops.net_area_m2() - 1500.0 * MM2).abs() < 1e-15);

    assert_eq!(pcb.footprints.len(), 3);
    let r1 = &pcb.footprints[0];
    assert_eq!(r1.reference, "R1");
    assert_eq!(r1.value, "10k");
    assert_eq!(r1.footprint, "Resistor_SMD:R_0603_1608Metric");
    assert_eq!(r1.datasheet, "https://example.invalid/r.pdf");
    assert_eq!(r1.side, Side::Front);
    assert_eq!(r1.rotation_deg, 90.0);
    assert_eq!(r1.attrs, vec!["smd"]);
    // Pad 1 local (−0.825, 0) rotated +90° in the Y-down frame lands
    // BELOW the origin on screen: (20, 15.825).
    let p1 = &r1.pads[0];
    assert!((p1.position[0] - 20e-3).abs() < 1e-15);
    assert!(
        (p1.position[1] - 15.825e-3).abs() < 1e-15,
        "{:?}",
        p1.position
    );
    assert_eq!(p1.rotation_deg, 180.0);
    assert_eq!(p1.kind, PadKind::Smd);
    assert_eq!(p1.net.as_ref().unwrap().name, "GND");
    assert!(!p1.is_unconnected_hole());
    assert_eq!(r1.models.len(), 1);
    assert!(r1.models[0].path.starts_with("${KICAD8_3DMODEL_DIR}"));

    let c1 = &pcb.footprints[1];
    assert_eq!(c1.side, Side::Back);
    assert_eq!(c1.models[0].offset_m, [1e-3, 2e-3, 0.5e-3]);
    assert_eq!(c1.models[0].rotate_deg, [0.0, 0.0, 90.0]);
    assert!(!c1.models[0].hidden);

    let h1 = &pcb.footprints[2];
    assert!(h1.is_mounting_hole_footprint());
    assert_eq!(h1.pads[0].kind, PadKind::NpThruHole);
    assert!(h1.pads[0].is_unconnected_hole());
    assert_eq!(h1.pads[0].drill, Some([3.2e-3, 3.2e-3]));
    assert_eq!(h1.pads[0].position, [5e-3, 5e-3]);

    // One warning for the three unknown forms, none for the known ones.
    assert_eq!(pcb.warnings.len(), 1, "{:?}", pcb.warnings);
    assert_eq!(
        pcb.warnings[0],
        "skipped 3 unknown top-level forms (x_future_form ×2, y_other_form ×1)"
    );
}

#[test]
fn holes_v6_golden_and_values() {
    let (pcb, loops) = check_golden("holes_v6");
    assert_eq!(pcb.version, 20211014);
    assert_eq!(pcb.thickness_m, 1.2e-3);
    assert_eq!(pcb.thickness_stackup_m, None);
    assert_eq!(pcb.copper_layers, 2);
    // KiCad 6: tstamp + fp_text.
    let h1 = &pcb.footprints[0];
    assert_eq!(h1.uuid, "1b2c3d4e-0000-4000-8000-000000000001");
    assert_eq!(h1.reference, "H1");
    assert_eq!(h1.value, "MountingHole");
    assert_eq!(h1.footprint, "MountingHole:MountingHole_3.2mm");
    let j1 = &pcb.footprints[1];
    assert_eq!(j1.reference, "J1");
    assert_eq!(j1.rotation_deg, 270.0);
    // Pad 2 local (0, 2.54) rotated 270°: x' = y·sin270 = −2.54 ⇒ (17.46, 12.5).
    let p2 = &j1.pads[1];
    assert!(
        (p2.position[0] - 17.46e-3).abs() < 1e-15,
        "{:?}",
        p2.position
    );
    assert!(
        (p2.position[1] - 12.5e-3).abs() < 1e-15,
        "{:?}",
        p2.position
    );
    assert_eq!(p2.drill, Some([1e-3, 1.2e-3]));
    assert!(p2.is_unconnected_hole(), "a plated hole with no net");
    assert!(!j1.pads[0].is_unconnected_hole());
    assert!(j1.models[0].path.ends_with(".wrl"));

    let loops = loops.unwrap();
    assert_eq!(loops.holes.len(), 2);
    let expect = (40.0 * 25.0 - 2.0 * PI * 1.6 * 1.6) * MM2;
    assert!((loops.net_area_m2() - expect).abs() < 1e-15);
    assert!(pcb.warnings.is_empty(), "{:?}", pcb.warnings);
}

#[test]
fn rounded_v9_golden_and_values() {
    let (pcb, loops) = check_golden("rounded_v9");
    assert_eq!(pcb.version, 20241229);
    assert_eq!(pcb.copper_layers, 4);
    // 8 board primitives + 4 from the gr_poly (2 lines + arc + closing
    // line) + 4 footprint-level slot primitives.
    assert_eq!(pcb.outline.len(), 16);
    let slot: Vec<_> = pcb
        .outline
        .iter()
        .filter(|p| p.footprint.as_deref() == Some("2c3d4e5f-0000-4000-8000-000000000001"))
        .collect();
    assert_eq!(slot.len(), 4);
    // Slot local (−2,−1) at (30,20) rotated 90° ⇒ (30 + y, 20 − x) = (29, 22).
    match &slot[0].shape {
        OutlineShape::Line { start, .. } => {
            assert!((start[0] - 29e-3).abs() < 1e-15 && (start[1] - 22e-3).abs() < 1e-15);
        }
        s => panic!("{s:?}"),
    }
    match &slot[1].shape {
        OutlineShape::Arc { mid, .. } => {
            assert!((mid[0] - 30e-3).abs() < 1e-15 && (mid[1] - 17e-3).abs() < 1e-15);
        }
        s => panic!("{s:?}"),
    }

    let loops = loops.unwrap();
    assert_eq!(loops.outer.segments.len(), 8);
    assert_eq!(loops.holes.len(), 2);
    // Outer: 60×40 minus four r=5 corner squares plus their quarter discs.
    let outer = (60.0 * 40.0 - (4.0 - PI) * 25.0) * MM2;
    assert!(
        (loops.outer.area_m2() - outer).abs() < 1e-14,
        "{}",
        loops.outer.area_m2()
    );
    let slot_a = (4.0 * 2.0 + PI) * MM2;
    let poly_a = (25.0 + 0.5 * PI * 2.5 * 2.5) * MM2;
    let net = outer - slot_a - poly_a;
    assert!(
        (loops.net_area_m2() - net).abs() < 1e-14,
        "{}",
        loops.net_area_m2()
    );
    // Every corner arc reports r = 5 mm exactly from the circumcentre.
    for s in &loops.outer.segments {
        if let Segment::Arc { radius, .. } = s {
            assert!((radius - 5e-3).abs() < 1e-15, "{radius}");
        }
    }
    let q1 = &pcb.footprints[1];
    assert_eq!(q1.attrs, vec!["smd", "dnp"]);
    assert!(q1.models[0].hidden);
    assert!(pcb.warnings.is_empty(), "{:?}", pcb.warnings);
}

#[test]
fn disc_golden_and_values() {
    let (pcb, loops) = check_golden("disc");
    let loops = loops.unwrap();
    assert!(loops.holes.is_empty());
    assert!(matches!(
        loops.outer.segments[0],
        Segment::Circle { radius, .. } if (radius - 20e-3).abs() < 1e-15
    ));
    assert!((loops.net_area_m2() - PI * 400.0 * MM2).abs() < 1e-15);
    assert_eq!(pcb.thickness_m, 1.0e-3);
}

// ── O4 / §6: loud failures ────────────────────────────────────────────

#[test]
fn gap_is_reported_with_its_size() {
    let pcb = parse_kicad_pcb(&fixture("gap.kicad_pcb")).unwrap();
    match pcb.outline_loops().unwrap_err() {
        OutlineError::NotClosed { gap_m, at } => {
            assert!((gap_m - 1e-5).abs() < 1e-12, "{gap_m}");
            assert!((at[0]).abs() < 1e-15 && at[1] <= 1e-5 + 1e-15, "{at:?}");
        }
        e => panic!("{e:?}"),
    }
}

#[test]
fn panel_is_multiple_outlines() {
    let pcb = parse_kicad_pcb(&fixture("panel.kicad_pcb")).unwrap();
    assert_eq!(
        pcb.outline_loops().unwrap_err(),
        OutlineError::MultipleBoardOutlines { count: 2 }
    );
}

#[test]
fn collinear_arc_is_read_as_a_line_with_a_warning() {
    let pcb = parse_kicad_pcb(&fixture("collinear_arc.kicad_pcb")).unwrap();
    assert_eq!(pcb.warnings.len(), 1);
    assert!(pcb.warnings[0].contains("collinear"), "{:?}", pcb.warnings);
    assert!(pcb
        .outline
        .iter()
        .all(|p| matches!(p.shape, OutlineShape::Line { .. })));
    let loops = pcb.outline_loops().unwrap();
    assert!((loops.net_area_m2() - 1500.0 * MM2).abs() < 1e-15);
}

#[test]
fn legacy_v5_is_unsupported_version() {
    assert_eq!(
        parse_kicad_pcb(&fixture("legacy_v5.kicad_pcb")).unwrap_err(),
        KicadParse::UnsupportedVersion {
            found: 20171130,
            min: MIN_VERSION
        }
    );
}

#[test]
fn duplicate_uuid_is_refused() {
    assert_eq!(
        parse_kicad_pcb(&fixture("dup_uuid.kicad_pcb")).unwrap_err(),
        KicadParse::DuplicateFootprintUuid {
            uuid: "71829304-0000-4000-8000-000000000001".into()
        }
    );
}

#[test]
fn missing_thickness_is_refused() {
    assert_eq!(
        parse_kicad_pcb(&fixture("no_thickness.kicad_pcb")).unwrap_err(),
        KicadParse::MissingThickness
    );
}

#[test]
fn not_a_board_is_refused() {
    assert_eq!(
        parse_kicad_pcb("(kicad_sch (version 20231120))").unwrap_err(),
        KicadParse::NotAKicadPcb {
            found: "kicad_sch".into()
        }
    );
}

#[test]
fn syntax_error_carries_line_and_column() {
    let text = "(kicad_pcb\n  (version 20240108)\n  (general (thickness 1.6))\n  (gr_line (start 0 x) (end 1 1) (layer \"Edge.Cuts\"))\n)";
    match parse_kicad_pcb(text).unwrap_err() {
        KicadParse::Syntax {
            line,
            col,
            expected,
        } => {
            assert_eq!(line, 4);
            assert_eq!(col, 21);
            assert_eq!(expected, "number");
        }
        e => panic!("{e:?}"),
    }
}

#[test]
fn footprint_off_the_copper_layers_is_refused() {
    let text = "(kicad_pcb (version 20240108) (general (thickness 1.6))\n (footprint \"X:Y\" (layer \"F.SilkS\") (uuid \"u\") (at 0 0)))";
    match parse_kicad_pcb(text).unwrap_err() {
        KicadParse::Syntax { line, expected, .. } => {
            assert_eq!(line, 2);
            assert!(expected.contains("F.Cu"));
        }
        e => panic!("{e:?}"),
    }
}

#[test]
fn stackup_disagreeing_with_general_warns_and_uses_the_stackup() {
    let text = "(kicad_pcb (version 20240108) (general (thickness 1.6))\n (setup (stackup (layer \"F.Cu\" (type \"copper\") (thickness 0.035)) (layer \"d\" (type \"core\") (thickness 0.8)) (layer \"B.Cu\" (type \"copper\") (thickness 0.035)))))";
    let pcb = parse_kicad_pcb(text).unwrap();
    assert!((pcb.thickness_m - 0.87e-3).abs() < 1e-18);
    assert_eq!(pcb.warnings.len(), 1);
    assert!(pcb.warnings[0].contains("differs"));
    assert_eq!(pcb.outline_loops().unwrap_err(), OutlineError::Empty);
}

/// The re-sync fixture (spec O7, C5): `rect_v8` one revision on — the
/// outline grown to 60 × 30, R1 moved and rotated, C1's value changed, R2
/// (same shape) and U1 (a new shape) added, H1 deleted.
#[test]
fn rect_v8_resync_golden_and_values() {
    let (pcb, loops) = check_golden("rect_v8_resync");
    assert_eq!(pcb.title_block.rev, "B");
    let refs: Vec<&str> = pcb
        .footprints
        .iter()
        .map(|f| f.reference.as_str())
        .collect();
    assert_eq!(refs, ["R1", "C1", "R2", "U1"]);
    let r1 = &pcb.footprints[0];
    assert_eq!(r1.at, [0.025, 0.012]);
    assert_eq!(r1.rotation_deg, 45.0);
    assert_eq!(pcb.footprints[1].value, "220n");
    let loops = loops.unwrap();
    assert!((loops.outer.signed_area_m2.abs() - 1800.0e-6).abs() < 1e-15);
    assert!(loops.holes.is_empty());
}
