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
//! The width is a preference ([`board::Board::widths`]): a net whose class is as wide keeps its
//! class's, a connection that finds no way at it routes at its class's, and once connections
//! are left open for rip-up, every net routes at its class's and the wide tracks laid so far are
//! narrowed to it ([`narrow`]).

use std::collections::{HashMap, HashSet};

use board::{Airline, Board, Kind, Nm, Shape};

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
    ["SENSE", "MEAS", "MON", "ADC", "DET", "_EN", "_ON", "_FB", "_PG", "SNS"].iter().any(|w| u.contains(w))
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

/// Prefers every net [`why`] finds, and that has no preference yet (a planner's comes first),
/// at `width`; returns them, by name, with why.
pub fn prefer(board: &mut Board, width: Nm) -> Vec<(String, &'static str)> {
    let mut refs: Vec<Vec<&str>> = vec![Vec::new(); board.nets.len()];
    for (_, it) in board.items() {
        if let Kind::Pad { footprint, .. } = &it.kind {
            if let (Some(fp), Some(list)) = (board.footprints.get(*footprint), refs.get_mut(it.net as usize)) {
                list.push(fp.reference.as_str());
            }
        }
    }
    let found: Vec<(u32, &'static str)> = (1..board.nets.len() as u32).filter(|n| !board.widths.contains_key(n)).filter_map(|n| why(board, n, &refs[n as usize]).map(|w| (n, w))).collect();
    let mut out = Vec::new();
    for (net, why) in found {
        board.widths.insert(net, width);
        out.push((board.net_name(net).to_string(), why));
    }
    out
}

/// Narrows each unlocked track of a net in `widths` that is wider than its net's class's to
/// that width, on the same centre line, which keeps its clearances and its joins at its ends;
/// a net with more connections open after (a join in a wide track's side) gets its tracks back.
/// For when the room the widths took is wanted: `board.widths` must no longer hold the nets, so
/// that [`grid::Profile::for_net`] gives the class's width. Returns the tracks narrowed.
pub fn narrow(board: &mut Board, widths: &HashMap<u32, Nm>, open: impl Fn(&Board) -> Vec<Airline>) -> usize {
    let class: HashMap<u32, Nm> = widths.keys().map(|&n| (n, grid::Profile::for_net(board, n).half_width)).collect();
    let wide: Vec<_> = board.items().filter(|(_, it)| it.kind == Kind::Track && !it.locked && matches!(it.shape, Shape::Capsule { r, .. } if class.get(&it.net).is_some_and(|&hw| r > hw))).map(|(id, it)| (id, it.clone())).collect();
    if wide.is_empty() {
        return 0;
    }
    let count = |airlines: Vec<Airline>| airlines.into_iter().fold(HashMap::new(), |mut m: HashMap<u32, usize>, a| {
        *m.entry(a.net).or_default() += 1;
        m
    });
    let before = count(open(board));
    for (id, it) in &wide {
        let mut thin = board.remove(*id).expect("a track on the board");
        if let Shape::Capsule { r, .. } = &mut thin.shape {
            *r = class[&it.net];
        }
        board.restore(*id, thin);
    }
    let after = count(open(board));
    let apart: HashSet<u32> = after.iter().filter(|(n, &k)| k > before.get(n).copied().unwrap_or(0)).map(|(&n, _)| n).collect();
    let mut narrowed = 0;
    for (id, it) in wide {
        if apart.contains(&it.net) {
            board.remove(id);
            board.restore(id, it);
        } else {
            narrowed += 1;
        }
    }
    narrowed
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
        for n in ["BATT_MEASURE", "BATT_MEASURE_ON", "VBUS_SENSE", "VIN_EN", "VDD_PG"] {
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
    fn narrowing_keeps_every_join_and_gives_back_a_net_it_would_part() {
        // N1: a 0.4 mm track pad to pad. N2: the same, and a 0.15 mm stub from a third pad ending
        // 0.2 mm off its centre line, inside the 0.4 mm copper but not the class's 0.15 mm.
        let src = format!(
            r#"(kicad_pcb (version 20241229) (generator "pcbnew") (general (thickness 1.6))
          (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (25 "Edge.Cuts" user))
          (net 0 "") (net 1 "N1") (net 2 "N2")
          {} {} {} {} {}
          {} {} {}
          (gr_rect (start 0 0) (end 20 20) (layer "Edge.Cuts") (width 0.1)))"#,
            pad("J1", 5.0, 10.0, 1, 1),
            pad("J2", 15.0, 10.0, 1, 2),
            pad("K1", 5.0, 5.0, 2, 3),
            pad("K2", 15.0, 5.0, 2, 4),
            pad("K3", 10.0, 2.0, 2, 5),
            track((5.0, 10.0), (15.0, 10.0), 0.4, 1, 6),
            track((5.0, 5.0), (15.0, 5.0), 0.4, 2, 7),
            track((10.0, 2.0), (10.0, 4.8), 0.15, 2, 8),
        );
        let (pcb, copper) = kicad_pcb::parse_board(&src).unwrap();
        let rules = r#"{"board":{"design_settings":{"rules":{"min_clearance":0.12,"min_track_width":0.1}}},
          "net_settings":{"classes":[{"name":"Default","clearance":0.12,"track_width":0.15,"via_diameter":0.6,"via_drill":0.3}]}}"#;
        let mut b = Board::new(&pcb, &copper, kicad_pcb::Rules::parse(rules).unwrap());
        let open = |b: &Board| board::connectivity(b).airlines;
        assert!(open(&b).is_empty(), "joined before");
        let widths = HashMap::from([(1, 400_000), (2, 400_000)]);
        assert_eq!(narrow(&mut b, &widths, open), 1, "N1's track, not N2's");
        assert!(open(&b).is_empty(), "joined after");
        let widths_of = |net: u32| b.items().filter(|(_, it)| it.kind == Kind::Track && it.net == net).map(|(_, it)| match it.shape {
            Shape::Capsule { r, .. } => 2 * r,
            _ => 0,
        }).max();
        assert_eq!(widths_of(1), Some(150_000));
        assert_eq!(widths_of(2), Some(400_000));
    }
}
