//! A board's custom rules (`.kicad_dru`) as the checker keeps them: clearances by object type,
//! the last matching rule winning even when lower, and `disallow` by net, layer and named area.

use board::*;

const RULES: &str = r#"{"board":{"design_settings":{"rules":{"min_clearance":0.2,"min_copper_edge_clearance":0.3}}}}"#;

fn board(body: &str, custom: &str) -> Board {
    let text = format!(
        "(kicad_pcb (version 20240108) (generator \"pcbnew\") (general (thickness 1.6))
         (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))
         (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
         (gr_rect (start 0 0) (end 30 30) (layer \"Edge.Cuts\") (width 0.1))
         {body})"
    );
    let (pcb, copper) = kicad_pcb::parse_board(&text).unwrap();
    let mut rules = kicad_pcb::Rules::parse(RULES).unwrap();
    if !custom.is_empty() {
        rules.apply_custom(custom, "test.kicad_dru");
    }
    Board::new(&pcb, &copper, rules)
}

/// A 0.2 mm track of `net` on `layer` from (x, 2) to (x, 28).
fn track(b: &mut Board, net: u32, layer: u64, x: f64) -> ItemId {
    b.insert(Item {
        kind: Kind::Track,
        net,
        layers: layer,
        shape: Shape::Capsule { a: Point::new(nm(x * 1e-3), nm(2e-3)), b: Point::new(nm(x * 1e-3), nm(28e-3)), r: nm(0.1e-3) },
        hole: None,
        locked: false,
        span: None,
    })
}

fn clearance_hits(b: &Board, id: ItemId) -> Vec<Nm> {
    check_item(b, id).iter().filter(|v| v.rule == Rule::Clearance).map(|v| v.required).collect()
}

#[test]
fn a_clearance_rule_by_object_type_holds_both_ways_round() {
    // A through-hole pad of net A at x = 10, 1.6 mm across; a track of B 0.25 mm from its copper.
    let pad = "(footprint \"P\" (layer \"F.Cu\") (at 10 15) (property \"Reference\" \"J1\")
                 (pad \"1\" thru_hole circle (at 0 0) (size 1.6 1.6) (drill 0.8) (layers \"*.Cu\") (net 1 \"A\")))";
    let x = 10.0 + 0.8 + 0.25 + 0.1;
    let mut plain = board(pad, "");
    let t = track(&mut plain, 2, 1, x);
    assert!(clearance_hits(&plain, t).is_empty(), "the class's 0.2 mm is kept");
    // Written as JLCPCB's template writes it: the pad is A, the track B; the checker asks with
    // the track first and still finds it.
    let mut ruled = board(pad, "(version 1)\n(rule \"Pad to Track\" (constraint clearance (min 0.3mm)) (condition \"A.isPlated() && B.Type == 'track'\"))");
    let t = track(&mut ruled, 2, 1, x);
    assert_eq!(clearance_hits(&ruled, t), [nm(0.3e-3)]);
    assert!(!ruled.rules.warnings.iter().any(|w| w.contains("test.kicad_dru")), "{:?}", ruled.rules.warnings);
}

#[test]
fn the_last_rule_that_holds_wins_even_below_the_class() {
    let mut b = board("", "(version 1)\n(rule \"wide\" (constraint clearance (min 0.5mm)))\n(rule \"narrow\" (constraint clearance (min 0.1mm)))");
    let a = track(&mut b, 1, 1, 10.0);
    // 0.15 mm apart: under the class's 0.2 mm, over the last rule's 0.1 mm, as KiCad judges it.
    let _ = track(&mut b, 2, 1, 10.0 + 0.2 + 0.15);
    assert!(clearance_hits(&b, a).is_empty());
    let mut b = board("", "(version 1)\n(rule \"narrow\" (constraint clearance (min 0.1mm)))\n(rule \"wide\" (constraint clearance (min 0.5mm)))");
    let a = track(&mut b, 1, 1, 10.0);
    let _ = track(&mut b, 2, 1, 10.0 + 0.2 + 0.15);
    assert_eq!(clearance_hits(&b, a), [nm(0.5e-3)]);
}

#[test]
fn a_disallow_rule_forbids_a_nets_tracks_on_a_layer() {
    let rule = "(version 1)\n(rule \"B on the back\" (condition \"A.NetName == 'B' && A.Layer != 'B.Cu'\") (constraint disallow track via))";
    let mut b = board("", rule);
    let front = track(&mut b, 2, 1, 10.0);
    let back = track(&mut b, 2, 2, 20.0);
    let other = track(&mut b, 1, 1, 5.0);
    let keepouts = |b: &Board, id| check_item(b, id).iter().filter(|v| v.rule == Rule::Keepout).count();
    assert_eq!(keepouts(&b, front), 1);
    assert_eq!(keepouts(&b, back), 0);
    assert_eq!(keepouts(&b, other), 0);
}

#[test]
fn a_disallow_rule_by_area_forbids_only_inside_it() {
    let area = "(zone (net 0) (net_name \"\") (layer \"F.Cu\") (name \"antenna\") (hatch edge 0.5)
                  (connect_pads (clearance 0)) (min_thickness 0.25)
                  (keepout (tracks allowed) (vias allowed) (pads allowed) (copperpour allowed) (footprints allowed))
                  (fill (thermal_gap 0.5) (thermal_bridge_width 0.5))
                  (polygon (pts (xy 5 5) (xy 15 5) (xy 15 15) (xy 5 15))))";
    let rule = "(version 1)\n(rule \"keep out\" (condition \"A.intersectsArea('antenna') && A.NetName != 'A'\") (constraint disallow track))";
    let mut b = board(area, rule);
    assert_eq!(b.areas.len(), 1);
    let through = track(&mut b, 2, 1, 10.0);
    let beside = track(&mut b, 2, 1, 20.0);
    let own = track(&mut b, 1, 1, 12.0);
    let keepouts = |b: &Board, id| check_item(b, id).iter().filter(|v| v.rule == Rule::Keepout).count();
    assert_eq!(keepouts(&b, through), 1);
    assert_eq!(keepouts(&b, beside), 0);
    assert_eq!(keepouts(&b, own), 0, "the rule spares net A");
}

#[test]
fn rules_the_router_does_not_keep_are_said_by_constraint() {
    let b = board(
        "",
        "(version 1)\n(rule \"clock\" (condition \"A.NetName == 'A'\") (constraint length (max 5mm)) (constraint via_count (max 0)))\n(rule \"silk\" (constraint silk_clearance (min 0.15mm)))\n(rule \"bad\" (condition \"A.Type == \") (constraint clearance (min 1mm)))",
    );
    let custom: Vec<&String> = b.rules.warnings.iter().filter(|w| w.contains("test.kicad_dru")).collect();
    assert_eq!(custom.len(), 2, "{custom:?}");
    assert!(custom.iter().any(|w| w.contains("\"clock\": length, via_count checked after routing")), "{custom:?}");
    assert!(custom.iter().any(|w| w.contains("bad") && w.contains("not applied")), "{custom:?}");
}

/// A 1 mm SMD pad of net A at (10, 10) with its own clearance, and a track of net B whose copper
/// is `gap` mm from the pad's right edge, on a board whose minimum clearance is `min`.
fn pad_and_track(own: f64, gap: f64, min: f64, custom: &str) -> (Board, ItemId) {
    let text = format!(
        "(kicad_pcb (version 20240108) (generator \"pcbnew\") (general (thickness 1.6))
         (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))
         (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
         (gr_rect (start 0 0) (end 30 30) (layer \"Edge.Cuts\") (width 0.1))
         (footprint \"R\" (layer \"F.Cu\") (at 10 10) (property \"Reference\" \"R1\")
           (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net 1 \"A\") (clearance {own}))))"
    );
    let (pcb, copper) = kicad_pcb::parse_board(&text).unwrap();
    let mut rules = kicad_pcb::Rules::parse(&format!(r#"{{"board":{{"design_settings":{{"rules":{{"min_clearance":{min}}}}}}},"net_settings":{{"classes":[{{"name":"Default","clearance":0.2,"track_width":0.2}}]}}}}"#)).unwrap();
    if !custom.is_empty() {
        rules.apply_custom(custom, "test.kicad_dru");
    }
    let mut b = Board::new(&pcb, &copper, rules);
    let t = track(&mut b, 2, 1, 10.5 + gap + 0.1);
    (b, t)
}

/// The clearance at the pad as KiCad judges it, and as the router keeps it.
fn at_pad(b: &Board, t: ItemId) -> (Nm, Nm) {
    let (pad, p) = b.items().find(|(_, it)| matches!(it.kind, Kind::Pad { .. })).unwrap();
    let (x, y) = (Thing::Item(b, pad, p), Thing::Item(b, t, b.item(t).unwrap()));
    (b.clearance_between(&x, &y, 0), b.clearance_kept(&x, &y, 0))
}

/// kicad-cli 9.0.8 on the same boards: a pad's own clearance stands in for the net classes and
/// the custom rules, lower or higher, and only the board's minimum holds under it. The router
/// keeps the wider of that and what the classes and rules ask, which KiCad always accepts.
#[test]
fn a_pads_own_clearance_outranks_the_classes_and_the_rules() {
    let (b, t) = pad_and_track(0.1, 0.15, 0.05, "");
    assert_eq!(at_pad(&b, t), (nm(0.1e-3), nm(0.2e-3)), "0.1 mm of its own below the class's 0.2 mm");
    let (b, t) = pad_and_track(0.1, 0.15, 0.2, "");
    assert_eq!(at_pad(&b, t).0, nm(0.2e-3), "the board's minimum holds");
    let (b, t) = pad_and_track(0.1, 0.15, 0.05, "(version 1)\n(rule hi (constraint clearance (min 0.3mm)))");
    assert_eq!(at_pad(&b, t), (nm(0.1e-3), nm(0.3e-3)), "over a custom rule's 0.3 mm in KiCad");
    let (b, t) = pad_and_track(0.4, 0.25, 0.05, "(version 1)\n(rule lo (constraint clearance (min 0.1mm)))");
    assert_eq!(clearance_hits(&b, t), [nm(0.4e-3)], "a custom rule's 0.1 mm does not lower it");
}

#[test]
fn a_nets_length_and_vias_over_a_rules_maximum_are_reported() {
    let mut b = board("", "(version 1)\n(rule \"short\" (condition \"A.NetName == 'B'\") (constraint length (max 20mm)) (constraint via_count (max 0)))");
    assert!(b.rule_limits().is_empty());
    track(&mut b, 2, 1, 10.0);
    b.insert(Item { kind: Kind::Via, net: 2, layers: 3, shape: Shape::Capsule { a: Point::new(nm(10e-3), nm(2e-3)), b: Point::new(nm(10e-3), nm(2e-3)), r: nm(0.3e-3) }, hole: Some((Point::new(nm(10e-3), nm(2e-3)), nm(0.15e-3))), locked: false, span: None });
    let found: Vec<String> = b.rule_limits().into_iter().map(|(line, _)| line).collect();
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].starts_with("length: B is 26.00 mm"), "{found:?}");
    assert!(found[1].starts_with("vias: B has 1"), "{found:?}");
    track(&mut b, 1, 1, 20.0);
    assert_eq!(b.rule_limits().len(), 2, "net A has no limit");
}
