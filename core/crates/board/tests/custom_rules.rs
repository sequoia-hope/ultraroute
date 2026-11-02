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
    // A via with copper on F.Cu alone counts nothing, as KiCad 9 counts it.
    assert!(found[0].starts_with("length: B is 26.00 mm"), "{found:?}");
    assert!(found[1].starts_with("vias: B has 1"), "{found:?}");
    track(&mut b, 1, 1, 20.0);
    assert_eq!(b.rule_limits().len(), 2, "net A has no limit");
}

#[test]
fn a_net_a_rule_allows_no_vias_gets_none() {
    let mut b = board("", "(version 1)\n(rule \"crystal\" (condition \"A.NetName == 'B'\") (constraint via_count (max 0)))");
    assert!(b.no_vias(2) && !b.no_vias(1));
    let via = |b: &mut Board, net: u32| b.insert(Item { kind: Kind::Via, net, layers: 3, shape: Shape::Capsule { a: Point::new(nm(10e-3), nm(5e-3)), b: Point::new(nm(10e-3), nm(5e-3)), r: nm(0.3e-3) }, hole: Some((Point::new(nm(10e-3), nm(5e-3)), nm(0.15e-3))), locked: false, span: None });
    let v = via(&mut b, 2);
    assert!(check_item(&b, v).iter().any(|v| v.rule == Rule::Keepout), "net B's via is refused");
    b.discard(v);
    let v = via(&mut b, 1);
    assert!(check_item(&b, v).is_empty(), "net A's via stands");
}

#[test]
fn a_length_limit_shorter_than_the_pads_lie_apart_is_warned_of() {
    let pads = "(footprint \"R\" (layer \"F.Cu\") (at 10 15) (property \"Reference\" \"R1\")
                  (pad \"1\" smd rect (at -2 0) (size 1 1) (layers \"F.Cu\") (net 2 \"B\"))
                  (pad \"2\" smd rect (at 2 0) (size 1 1) (layers \"F.Cu\") (net 2 \"B\")))";
    // The pads' edges are 3 mm apart.
    let short = board(pads, "(version 1)\n(rule \"tight\" (condition \"A.NetName == 'B'\") (constraint length (max 2mm)))");
    assert!(short.warnings.iter().any(|w| w.starts_with("length: B's pads lie 3.00 mm apart")), "{:?}", short.warnings);
    let room = board(pads, "(version 1)\n(rule \"loose\" (condition \"A.NetName == 'B'\") (constraint length (max 4mm)))");
    assert!(!room.warnings.iter().any(|w| w.starts_with("length:")), "{:?}", room.warnings);
}

#[test]
fn a_via_joining_both_outer_layers_counts_the_boards_height() {
    let mut b = board("", "(version 1)\n(rule \"short\" (condition \"A.NetName == 'B'\") (constraint length (max 20mm)))");
    track(&mut b, 2, 1, 10.0);
    let at = Point::new(nm(10e-3), nm(2e-3));
    b.insert(Item { kind: Kind::Via, net: 2, layers: 3, shape: Shape::Capsule { a: at, b: at, r: nm(0.3e-3) }, hole: Some((at, nm(0.15e-3))), locked: false, span: None });
    assert!((b.net_length(2) - 26.0).abs() < 1e-6, "on F.Cu only: {}", b.net_length(2));
    b.insert(Item { kind: Kind::Track, net: 2, layers: 2, shape: Shape::Capsule { a: at, b: Point::new(nm(15e-3), nm(2e-3)), r: nm(0.1e-3) }, hole: None, locked: false, span: None });
    // 26 + 5 mm of track and the via's 1.565 mm: a 1.6 mm board less 35 um of copper.
    assert!((b.net_length(2) - 32.565).abs() < 1e-6, "{}", b.net_length(2));
}

#[test]
fn a_joint_is_held_to_the_widest_of_track_width_minimum_connection_and_connection_width() {
    let b = board("", "");
    assert_eq!(b.min_joint_of(1), 0, "KiCad checks no connection width unless a board sets one");
    let b = board("", "(version 1)\n(rule \"supply\" (condition \"A.NetName == 'A'\") (constraint connection_width (min 0.25mm)))");
    assert_eq!(b.min_joint_of(1), nm(0.25e-3));
    assert_eq!(b.min_joint_of(2), 0, "the rule is A's");
    assert!(b.rules.warnings.iter().any(|w| w.contains("\"supply\": connection_width checked after routing")), "{:?}", b.rules.warnings);
    let mut rules = kicad_pcb::Rules::parse(r#"{"board":{"design_settings":{"rules":{"min_track_width":0.1,"min_connection":0.15}}}}"#).unwrap();
    assert!((rules.min_connection - 0.15e-3).abs() < 1e-12);
    rules.min_connection = 0.05e-3;
    let (pcb, copper) = kicad_pcb::parse_board("(kicad_pcb (version 20240108) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\") (net 1 \"A\"))").unwrap();
    assert_eq!(Board::new(&pcb, &copper, rules).min_joint_of(1), nm(0.1e-3), "never under the narrowest track");
}

#[test]
fn a_physical_hole_clearance_keeps_a_via_out_of_its_own_nets_smd_pad() {
    // easy2350's "no via hole in an SMD pad": a fan-out via drilled in a pad of its own net,
    // which KiCad flags, while the via's own track still joins it.
    let pads = "(footprint \"R\" (layer \"F.Cu\") (at 10 15) (property \"Reference\" \"R1\")
                  (pad \"1\" smd rect (at 0 0) (size 0.6 0.6) (layers \"F.Cu\") (net 1 \"A\"))
                  (pad \"2\" smd rect (at 3 0) (size 0.6 0.6) (layers \"F.Cu\") (net 1 \"A\")))";
    let rule = "(version 1)\n(rule \"no via hole in an SMD pad\" (condition \"A.Type == 'Via' && B.Type == 'Pad' && B.Pad_Type == 'SMD' && !(B.memberOfFootprint('R1') && B.Pad_Number == '2')\") (constraint physical_hole_clearance (min 0.1mm)))";
    let via = |b: &mut Board, x: f64| {
        b.insert(Item {
            kind: Kind::Via,
            net: 1,
            layers: u64::MAX >> (64 - b.layers.len()),
            shape: Shape::Capsule { a: Point::new(nm(x * 1e-3), nm(15e-3)), b: Point::new(nm(x * 1e-3), nm(15e-3)), r: nm(0.3e-3) },
            hole: Some((Point::new(nm(x * 1e-3), nm(15e-3)), nm(0.15e-3))),
            locked: false,
            span: None,
        })
    };
    let holes = |b: &Board, id: ItemId| check_item(b, id).iter().filter(|v| v.rule == Rule::HoleClearance).count();
    let mut plain = board(pads, "");
    let v = via(&mut plain, 10.0);
    assert_eq!(holes(&plain, v), 0, "no rule: a via of the pad's net may sit in it");
    let mut ruled = board(pads, rule);
    let v = via(&mut ruled, 10.0);
    assert_eq!(holes(&ruled, v), 1, "the rule keeps the drill out of its own net's pad");
    let v = via(&mut ruled, 13.0);
    assert_eq!(holes(&ruled, v), 0, "the excepted pad takes it");
    let v = via(&mut ruled, 11.0);
    let t = ruled.insert(Item { kind: Kind::Track, net: 1, layers: 1, shape: Shape::Capsule { a: Point::new(nm(10e-3), nm(15e-3)), b: Point::new(nm(11e-3), nm(15e-3)), r: nm(0.1e-3) }, hole: None, locked: false, span: None });
    assert_eq!(holes(&ruled, v), 0, "beside the pad, joined by its track");
    assert!(check_item(&ruled, t).is_empty());
}

#[test]
fn a_physical_hole_clearance_allows_no_slack() {
    // KiCad measures physical clearances exactly: easy2350's GND via 0.19988 mm from C26's
    // rounded corner was flagged under a 0.2 mm rule, though inside the DRC's 0.5 um epsilon.
    let pads = "(footprint \"R\" (layer \"F.Cu\") (at 10 15) (property \"Reference\" \"R1\")
                  (pad \"1\" smd rect (at 0 0) (size 0.6 0.6) (layers \"F.Cu\") (net 1 \"A\")))";
    let rule = "(version 1)\n(rule \"no via hole in an SMD pad\" (condition \"A.Type == 'Via' && B.Type == 'Pad' && B.Pad_Type == 'SMD'\") (constraint physical_hole_clearance (min 0.1mm)))";
    let mut b = board(pads, rule);
    // The pad's edge at x 10.3 mm, the hole's radius 0.15 mm: its centre must be 10.55 mm out.
    let mut short = |x: f64| {
        let at = Point::new(nm(x * 1e-3), nm(15e-3));
        let v = b.insert(Item { kind: Kind::Via, net: 1, layers: u64::MAX >> (64 - b.layers.len()), shape: Shape::Capsule { a: at, b: at, r: nm(0.3e-3) }, hole: Some((at, nm(0.15e-3))), locked: false, span: None });
        let n = check_item(&b, v).iter().filter(|v| v.rule == Rule::HoleClearance).count();
        b.remove(v);
        n
    };
    assert_eq!(short(10.5498), 1, "0.2 um short is short");
    assert_eq!(short(10.5501), 0);
}

#[test]
fn a_copper_zone_a_rule_keeps_other_nets_off_is_laid() {
    // easy2350's way of protecting its 1V1 strips: a rule disallowing other nets' tracks and
    // vias in the named copper zone (`A.intersectsArea`), and a pad of B so another net exists.
    let body = "(footprint \"R\" (layer \"F.Cu\") (at 25 25) (property \"Reference\" \"R1\") (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net 2 \"B\")))
                (zone (net 1) (net_name \"A\") (layer \"B.Cu\") (name \"strip A\") (hatch edge 0.5) (connect_pads (clearance 0.2)) (min_thickness 0.25)
                  (fill yes (thermal_gap 0.5) (thermal_bridge_width 0.5) (island_removal_mode 1))
                  (polygon (pts (xy 10 20) (xy 15 20) (xy 15 20.6) (xy 10 20.6)))
                  (filled_polygon (layer \"B.Cu\") (pts (xy 10 20) (xy 15 20) (xy 15 20.6) (xy 10 20.6))))";
    let strip = |b: &Board| b.items().find(|(_, it)| matches!(it.kind, Kind::Pour { .. })).map(|(id, _)| id).unwrap();
    let open = board(body, "");
    assert!(!open.is_laid(strip(&open)), "no rule: a pour");
    let rule = "(version 1)\n(rule \"keep strip A\" (condition \"A.intersectsArea('strip A') && A.NetName != 'A'\") (constraint disallow track via))";
    let kept = board(body, rule);
    assert!(kept.is_laid(strip(&kept)) && kept.must_join(strip(&kept)));
    // A rule that lets B's vias in protects nothing: a via of B would cut the fill.
    let tracks_only = "(version 1)\n(rule \"keep strip A\" (condition \"A.intersectsArea('strip A') && A.NetName != 'A'\") (constraint disallow track))";
    let half = board(body, tracks_only);
    assert!(!half.is_laid(strip(&half)));
}
