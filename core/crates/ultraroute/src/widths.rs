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
//! are left open for rip-up, every net routes at its class's.

use board::{Board, Kind, Nm};

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
}
