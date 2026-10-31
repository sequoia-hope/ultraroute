//! Room for a pour's thermal spokes, as KiCad 9.0.8's filler and DRC treat them (probed with
//! kicad-cli on boards like these, 2026-10-31): a pad the pour joins with reliefs keeps as many
//! spokes free of other nets' copper as `min_resolved_spokes` asks (2 unless a rule says), and a
//! spoke is lost to a track whose clearance leaves the pour past the gap less than the zone's
//! minimum width deep.

use board::*;

const RULES: &str = r#"{"board":{"design_settings":{"rules":{"min_clearance":0.2,"min_copper_edge_clearance":0.3}}}}"#;

/// Net A's zone over F.Cu, with a 0.5 mm thermal gap and spokes, 0.25 mm minimum width and
/// 0.2 mm clearance, unfilled; `pad` and `extra` inside it.
fn board(pad: &str, connect: &str, extra: &str, custom: &str) -> Board {
    let text = format!(
        "(kicad_pcb (version 20240108) (generator \"pcbnew\") (general (thickness 1.6))
         (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))
         (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
         (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1))
         (footprint \"P\" (layer \"F.Cu\") (at 10 10) (property \"Reference\" \"U1\") {pad})
         (zone (net 1) (net_name \"A\") (layer \"F.Cu\") (hatch edge 0.5) (connect_pads {connect} (clearance 0.2)) (min_thickness 0.25)
           (fill yes (thermal_gap 0.5) (thermal_bridge_width 0.5))
           (polygon (pts (xy 1 1) (xy 19 1) (xy 19 19) (xy 1 19))))
         {extra})"
    );
    let (pcb, copper) = kicad_pcb::parse_board(&text).unwrap();
    let mut rules = kicad_pcb::Rules::parse(RULES).unwrap();
    if !custom.is_empty() {
        rules.apply_custom(custom, "test.kicad_dru");
    }
    Board::new(&pcb, &copper, rules)
}

const RECT: &str = "(pad \"1\" smd rect (at 0 0) (size 1 0.6) (layers \"F.Cu\") (net 1 \"A\"))";
const FOUR: &str = "(version 1) (rule spokes (constraint min_resolved_spokes 4))";

/// The far ends of the spokes kept, in millimetres, rounded to the micron.
fn tips(b: &Board) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = b
        .items()
        .filter_map(|(_, it)| match (&it.kind, &it.shape) {
            (Kind::Spoke { .. }, Shape::Polygon(p)) => Some((((p[1].x + p[2].x) / 2) as f64 / 1e6, ((p[1].y + p[2].y) / 2) as f64 / 1e6)),
            _ => None,
        })
        .map(|(x, y)| ((x * 1e3).round() / 1e3, (y * 1e3).round() / 1e3))
        .collect();
    out.sort_by(|a, b| a.partial_cmp(b).unwrap());
    out
}

/// A 0.2 mm track of net B from (x, 2) to (x, 18), laid by the router.
fn track(b: &mut Board, x: f64) -> ItemId {
    b.insert(Item {
        kind: Kind::Track,
        net: 2,
        layers: 1,
        shape: Shape::Capsule { a: Point::new(nm(x * 1e-3), nm(2e-3)), b: Point::new(nm(x * 1e-3), nm(18e-3)), r: nm(0.1e-3) },
        hole: None,
        locked: false,
        span: None,
    })
}

#[test]
fn a_relief_pad_keeps_room_for_two_spokes_unless_a_rule_asks_more() {
    assert_eq!(tips(&board(RECT, "", "", "")).len(), 2);
    // Along the pad's axes, past its edge, the 0.5 mm gap and a 0.25 mm land of pour.
    assert_eq!(tips(&board(RECT, "", "", FOUR)), vec![(8.75, 10.0), (10.0, 8.95), (10.0, 11.05), (11.25, 10.0)]);
    assert_eq!(tips(&board(RECT, "", "", "(version 1) (rule one (constraint min_resolved_spokes 1))")).len(), 1);
}

#[test]
fn a_solid_or_unjoined_pad_keeps_none() {
    assert!(tips(&board(RECT, "yes", "", "")).is_empty(), "the zone joins pads solidly");
    assert!(tips(&board(RECT, "no", "", "")).is_empty(), "the zone joins no pad");
    assert!(tips(&board(RECT, "thru_hole_only", "", "")).is_empty(), "reliefs only for through-hole pads");
    let solid = "(pad \"1\" smd rect (at 0 0) (size 1 0.6) (layers \"F.Cu\") (net 1 \"A\") (zone_connect 2))";
    assert!(tips(&board(solid, "", "", "")).is_empty(), "the pad's own connection is solid");
    let tht = "(pad \"1\" thru_hole circle (at 0 0) (size 1.6 1.6) (drill 0.8) (layers \"*.Cu\") (net 1 \"A\"))";
    assert_eq!(tips(&board(tht, "thru_hole_only", "", "")).len(), 2);
}

#[test]
fn a_round_pads_spokes_run_at_45_degrees_and_a_rotated_pads_along_its_axes() {
    let round = "(pad \"1\" smd circle (at 0 0) (size 1 1) (layers \"F.Cu\") (net 1 \"A\"))";
    let r = 0.5 + 0.5 + 0.25;
    let d = ((r / 2f64.sqrt()) * 1e3).round() / 1e3;
    assert_eq!(tips(&board(round, "", "", FOUR)), vec![(10.0 - d, 10.0 - d), (10.0 - d, 10.0 + d), (10.0 + d, 10.0 - d), (10.0 + d, 10.0 + d)]);
    let turned = "(pad \"1\" smd rect (at 0 0 90) (size 1 0.6) (layers \"F.Cu\") (net 1 \"A\"))";
    assert_eq!(tips(&board(turned, "", "", FOUR)), vec![(8.95, 10.0), (10.0, 8.75), (10.0, 11.25), (11.05, 10.0)]);
}

#[test]
fn spokes_that_other_copper_or_the_zones_edge_would_cut_are_not_kept() {
    // Net B's pad 0.6 mm right of U1's: the +x spoke has no room.
    let neighbour = "(footprint \"P\" (layer \"F.Cu\") (at 11.3 10) (property \"Reference\" \"U2\")
                       (pad \"1\" smd rect (at 0 0) (size 0.4 0.6) (layers \"F.Cu\") (net 2 \"B\")))";
    assert_eq!(tips(&board(RECT, "", neighbour, FOUR)), vec![(8.75, 10.0), (10.0, 8.95), (10.0, 11.05)]);
    let b = board(RECT, "", neighbour, FOUR);
    assert!(b.warnings.iter().any(|w| w.contains("thermal spokes: 1 of 1")), "{:?}", b.warnings);
    // Two are enough by default: no warning.
    assert!(!board(RECT, "", neighbour, "").warnings.iter().any(|w| w.contains("thermal")));
}

#[test]
fn new_copper_of_another_net_keeps_clear_of_a_spoke_as_kicad_judges_it() {
    let mut b = board(RECT, "", "", FOUR);
    // The +x spoke's land ends at x = 11.25: KiCad keeps the spoke for a 0.2 mm track at
    // x = 11.55 (0.2 mm clearance) and loses it at 11.5.
    let near = track(&mut b, 11.5);
    let hits: Vec<Violation> = check_item(&b, near).into_iter().filter(|v| v.rule == Rule::Clearance).collect();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(matches!(b.item(hits[0].b.unwrap()).unwrap().kind, Kind::Spoke { .. }));
    b.remove(near);
    let clear = track(&mut b, 11.56);
    assert!(check_item(&b, clear).is_empty(), "{:?}", check_item(&b, clear));
}

#[test]
fn a_spoke_joins_nothing() {
    // A pad of net A far outside the zone, and a track of A ending inside U1's +x spoke: the
    // spoke is room, not copper, so the track reaches nothing.
    let other = "(footprint \"P\" (layer \"F.Cu\") (at 10 10) (property \"Reference\" \"U3\")
                   (pad \"2\" smd rect (at 9 9) (size 0.5 0.5) (layers \"F.Cu\") (net 1 \"A\")))";
    let mut b = board(RECT, "", other, FOUR);
    let before = connectivity_without_pours(&b).airlines.len();
    b.insert(Item {
        kind: Kind::Track,
        net: 1,
        layers: 1,
        shape: Shape::Capsule { a: Point::new(nm(11.1e-3), nm(10e-3)), b: Point::new(nm(19e-3), nm(19e-3)), r: nm(0.1e-3) },
        hole: None,
        locked: false,
        span: None,
    });
    assert_eq!(connectivity_without_pours(&b).airlines.len(), before, "U1's pad and U3's are no nearer joined");
}

#[test]
fn a_spoke_that_lands_in_another_pads_relief_is_not_kept() {
    // A second pad of net A 0.8 mm above U1's: the pour leaves its gap round it, so U1's -y
    // spoke (and its +y one toward U1) would land where there is no pour.
    let above = "(footprint \"P\" (layer \"F.Cu\") (at 10 9.2) (property \"Reference\" \"U2\")
                   (pad \"1\" smd rect (at 0 0) (size 1 0.6) (layers \"F.Cu\") (net 1 \"A\")))";
    let tips = tips(&board(RECT, "", above, FOUR));
    assert!(!tips.contains(&(10.0, 8.95)) && !tips.contains(&(10.0, 10.25)), "{tips:?}");
    assert!(tips.contains(&(11.25, 10.0)) && tips.contains(&(10.0, 11.05)), "{tips:?}");
}
