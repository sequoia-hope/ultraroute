//! `--power-width MM`: the nets that carry supply current, found by their names and parts, are
//! preferred at a width, as designers widen them. Of the nets PCBench's designers widened (half
//! as wide again as the board's usual track), these rules find the ones holding two thirds of
//! the widened length; the designers' usual width for a widened supply rail is 0.5 mm.
//!
//! - a power rail: a name like `+3V3`, `-12V`, `5V`, `3.3V`, or beginning `VCC`, `VDD`, `VIN`,
//!   `VBAT`, `VBUS`, `VSYS`, `VOUT`, `PWR`, ... (the last part of a hierarchical name);
//! - a ground: a name beginning `GND`, `AGND`, `PGND`, `VSS`, `GROUND`, `EARTH`, or `0V`;
//! - a switch node: a net joining an inductor (`L1`) and an IC (`U1`, `IC1`), as a regulator's
//!   switch pin, its inductor and its catch diode are.
//!
//! Not a net whose name says it senses, enables or measures (`VBUS_SENSE`, `BATT_MEASURE`).
//!
//! The width is an optimum, KiCad's `opt` ([`board::Board::opt_widths`]): the nets route at
//! their class's width, and [`widen`] takes it after where the routing left room. A guess from a
//! name never costs a connection; a width a net must have is a rule (a class, a custom rule's
//! `min`, a planner's `min_mm`).

use std::collections::HashMap;

use board::{Board, ItemId, Kind, Nm, Shape};

/// Why a net is taken to carry current, or `None`.
pub fn why(board: &Board, net: u32, refs: &[&str]) -> Option<&'static str> {
    let name = board.net_name(net);
    let last = name.rsplit('/').next().unwrap_or(name).to_ascii_uppercase();
    if is_ground(&last) {
        return Some("ground");
    }
    // A supply's sense, enable or measuring net carries none: BATT_MEASURE, VBUS_SENSE, VIN_EN.
    if is_sensing(&last) {
        return None;
    }
    if is_power(&last) {
        return Some("power");
    }
    let has = |pre: &[&str]| refs.iter().any(|r| pre.iter().any(|p| r.strip_prefix(p).is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))));
    if has(&["L"]) && has(&["U", "IC"]) {
        return Some("switch node");
    }
    None
}

fn is_sensing(u: &str) -> bool {
    // An enable may run the word on: PWREN, VBUSEN.
    u.ends_with("EN") || ["SENSE", "MEAS", "MON", "ADC", "DET", "_EN", "_ON", "_FB", "_PG", "SNS"].iter().any(|w| u.contains(w))
}

fn is_ground(u: &str) -> bool {
    u == "0V" || ["GND", "AGND", "DGND", "PGND", "SGND", "CGND", "VSS", "GROUND", "EARTH"].iter().any(|p| u.starts_with(p))
}

fn is_power(u: &str) -> bool {
    let b = u.as_bytes();
    // +3V3, -12V, +5VA
    if b.len() > 1 && (b[0] == b'+' || b[0] == b'-') && b[1].is_ascii_digit() {
        return true;
    }
    // 5V, 3V3, 3.3V, 12V0: digits, then a V or a point, digits, and a V somewhere.
    if b.first().is_some_and(u8::is_ascii_digit) && u.contains('V') && u.bytes().all(|c| c.is_ascii_digit() || c == b'V' || c == b'.') {
        return true;
    }
    const PREFIXES: [&str; 21] =
        ["VCC", "VDD", "VEE", "VIN", "VBAT", "VBUS", "VSYS", "VMOT", "VPP", "VREG", "VOUT", "PWR", "POWER", "BATT", "AVDD", "DVDD", "IOVDD", "VAA", "VLED", "VUSB", "V_"];
    PREFIXES.iter().any(|p| u.starts_with(p))
}

/// Gives every net [`why`] finds, and that has no optimum yet (a planner's or a custom rule's
/// comes first), `width` as its optimum; returns them, by name, with why.
pub fn prefer(board: &mut Board, width: Nm) -> Vec<(String, &'static str)> {
    let mut refs: Vec<Vec<&str>> = vec![Vec::new(); board.nets.len()];
    for (_, it) in board.items() {
        if let Kind::Pad { footprint, .. } = &it.kind {
            if let (Some(fp), Some(list)) = (board.footprints.get(*footprint), refs.get_mut(it.net as usize)) {
                list.push(fp.reference.as_str());
            }
        }
    }
    let found: Vec<(u32, &'static str)> = (1..board.nets.len() as u32).filter(|n| !board.opt_widths.contains_key(n)).filter_map(|n| why(board, n, &refs[n as usize]).map(|w| (n, w))).collect();
    let mut out = Vec::new();
    for (net, why) in found {
        board.opt_widths.insert(net, width);
        out.push((board.net_name(net).to_string(), why));
    }
    out
}

/// Widens each unlocked track of a net in `widths` narrower than its preferred width to that
/// width, or else halfway to it, on the same centre line, wherever the wider copper breaks no
/// rule: room the routing left, taken without moving anything, so no connection is lost to it.
/// Returns the tracks widened.
pub fn widen(board: &mut Board, widths: &[(u32, Nm)], pours_block: bool) -> usize {
    let want: HashMap<u32, Nm> = widths.iter().map(|&(n, w)| (n, w / 2)).collect();
    let thin: Vec<ItemId> = board.items().filter(|(_, it)| it.kind == Kind::Track && !it.locked && matches!(it.shape, Shape::Capsule { r, .. } if want.get(&it.net).is_some_and(|&hw| r < hw))).map(|(id, _)| id).collect();
    let check = |b: &Board, id| if pours_block { board::check_item_with_pours(b, id) } else { board::check_item(b, id) };
    let mut widened = 0;
    for id in thin {
        if !check(board, id).is_empty() {
            continue;
        }
        let it = board.item(id).expect("a track on the board").clone();
        let Shape::Capsule { r: was, .. } = it.shape else { continue };
        let hw = want[&it.net];
        for r in [hw, (was + hw) / 2] {
            let mut wide = board.remove(id).expect("a track on the board");
            if let Shape::Capsule { r: s, .. } = &mut wide.shape {
                *s = r;
            }
            board.restore(id, wide);
            if check(board, id).is_empty() {
                widened += 1;
                break;
            }
            board.remove(id);
            board.restore(id, it.clone());
        }
    }
    widened
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for n in ["+3V3", "-12V", "5V", "3V3", "3.3V", "VCC", "VDD_IO", "VBUS", "VIN", "V_BAT", "PWR_5V"] {
            assert!(is_power(n), "{n}");
        }
        for n in ["GND", "AGND", "PGND", "VSS", "GNDA", "0V", "EARTH"] {
            assert!(is_ground(n), "{n}");
        }
        for n in ["NET-(R1-PAD1)", "SDA", "LED5", "D+", "VREF_SENSE_X1"] {
            assert!(!is_ground(n), "{n}");
        }
        for n in ["NET-(R1-PAD1)", "SDA", "LED5", "D+", "12C", "VSYNC", "V5_SENSE"] {
            assert!(!is_power(n), "{n}");
        }
        for n in ["BATT_MEASURE", "BATT_MEASURE_ON", "VBUS_SENSE", "VIN_EN", "VDD_PG", "PWREN"] {
            assert!(is_sensing(n), "{n}");
        }
        for n in ["VBAT", "+3V3", "VDD_IO"] {
            assert!(!is_sensing(n), "{n}");
        }
    }

    fn pad(r: &str, x: f64, y: f64, net: u32, k: u32) -> String {
        format!(
            r#"(footprint "t:P" (layer "F.Cu") (uuid "{k:08}-2222-4222-8222-222222222221") (at {x} {y})
            (property "Reference" "{r}" (at 0 -2 0) (layer "F.SilkS") (uuid "{k:08}-2222-4222-8222-222222222222"))
            (pad "1" smd rect (at 0 0) (size 0.8 0.8) (layers "F.Cu") (net {net} "N{net}")))"#
        )
    }

    fn track(a: (f64, f64), b: (f64, f64), width: f64, net: u32, k: u32) -> String {
        format!(r#"(segment (start {} {}) (end {} {}) (width {width}) (layer "F.Cu") (net {net}) (uuid "{k:08}-5555-4555-8555-555555555555"))"#, a.0, a.1, b.0, b.1)
    }

    #[test]
    fn widening_takes_the_room_there_is_and_no_more() {
        // N1 alone, N2 0.36 mm from N3's track: 0.4 mm wide it would come 0.085 mm from it, under
        // the 0.12 mm clearance; halfway, 0.275 mm, it keeps 0.1475 mm.
        let src = format!(
            r#"(kicad_pcb (version 20241229) (generator "pcbnew") (general (thickness 1.6))
          (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (25 "Edge.Cuts" user))
          (net 0 "") (net 1 "N1") (net 2 "N2") (net 3 "N3")
          {} {} {} {} {}
          (gr_rect (start 0 0) (end 20 20) (layer "Edge.Cuts") (width 0.1)))"#,
            pad("J1", 5.0, 10.0, 1, 1),
            pad("J2", 15.0, 10.0, 1, 2),
            track((5.0, 10.0), (15.0, 10.0), 0.15, 1, 3),
            track((6.0, 5.0), (14.0, 5.0), 0.15, 2, 4),
            track((6.0, 5.36), (14.0, 5.36), 0.15, 3, 5),
        );
        let (pcb, copper) = kicad_pcb::parse_board(&src).unwrap();
        let rules = r#"{"board":{"design_settings":{"rules":{"min_clearance":0.12,"min_track_width":0.1}}},
          "net_settings":{"classes":[{"name":"Default","clearance":0.12,"track_width":0.15,"via_diameter":0.6,"via_drill":0.3}]}}"#;
        let mut b = Board::new(&pcb, &copper, kicad_pcb::Rules::parse(rules).unwrap());
        assert_eq!(widen(&mut b, &[(1, 400_000), (2, 400_000)], false), 2);
        let width_of = |net: u32| b.items().find(|(_, it)| it.kind == Kind::Track && it.net == net).map(|(_, it)| match it.shape {
            Shape::Capsule { r, .. } => 2 * r,
            _ => 0,
        });
        assert_eq!(width_of(1), Some(400_000));
        assert_eq!(width_of(2), Some(275_000));
        assert_eq!(width_of(3), Some(150_000), "not preferred, not widened");
        assert!(board::check(&b).is_empty(), "{:?}", board::check(&b));
    }
}
