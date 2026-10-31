//! Routed copper back into a `.kicad_pcb`: items are removed by their spans and new tracks and
//! vias added before the board's closing paren, in KiCad 9's own layout. Every other byte of the
//! file is kept as it was.

use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::copper::Span;

#[derive(Debug, Clone, PartialEq)]
pub struct NewTrack {
    pub start: [f64; 2],
    pub end: [f64; 2],
    pub width: f64,
    pub layer: String,
    pub net: u32,
    /// Written `(locked yes)`: copper of the board's that the router split or moved stays locked.
    pub locked: bool,
}

/// A track along a circle from `start` through `mid` to `end`, written as KiCad's `(arc …)`.
#[derive(Debug, Clone, PartialEq)]
pub struct NewArc {
    pub start: [f64; 2],
    pub mid: [f64; 2],
    pub end: [f64; 2],
    pub width: f64,
    pub layer: String,
    pub net: u32,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewVia {
    pub at: [f64; 2],
    pub size: f64,
    pub drill: f64,
    pub layers: [String; 2],
    pub net: u32,
    pub locked: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Edit {
    pub remove: Vec<Span>,
    /// Text put in place of a span, such as a moved footprint's `(at …)`.
    pub replace: Vec<(Span, String)>,
    pub tracks: Vec<NewTrack>,
    pub arcs: Vec<NewArc>,
    pub vias: Vec<NewVia>,
    /// The new items' uuids drawn from this seed, the same every run; random when `None`.
    pub uuid_seed: Option<u64>,
}

pub fn apply(text: &str, edit: &Edit) -> String {
    let mut uuids = UuidSource::new(edit.uuid_seed);
    let mut changes: Vec<(Span, Option<&str>)> =
        edit.remove.iter().map(|s| (*s, None)).chain(edit.replace.iter().map(|(s, t)| (*s, Some(t.as_str())))).collect();
    changes.sort_by_key(|c| c.0.start);
    let mut out = String::with_capacity(text.len());
    let mut kept = 0;
    for (span, replacement) in changes {
        if span.start < kept {
            continue;
        }
        if let Some(replacement) = replacement {
            out.push_str(&text[kept..span.start]);
            out.push_str(replacement);
            kept = span.end;
            continue;
        }
        // The whole line goes when the item is alone on it, as KiCad writes them.
        let line_start = text[..span.start].rfind('\n').map_or(0, |i| i + 1);
        let start = if text[line_start..span.start].trim().is_empty() { line_start } else { span.start };
        let end = if text[span.end..].starts_with('\n') { span.end + 1 } else { span.end };
        out.push_str(&text[kept..start]);
        kept = end;
    }
    let close = text.rfind(')').unwrap_or(text.len());
    if close < kept {
        return out;
    }
    out.push_str(&text[kept..close]);
    // A board that names its nets in place (KiCad 10) is written the same way, by the names its
    // nets were numbered from when read.
    let names = text
        .find("(net ")
        .filter(|&i| text[i + 5..].starts_with('"'))
        .and_then(|_| crate::sexpr::parse(text).ok())
        .map(|root| crate::sexpr::net_names(&root));
    let net = |n: u32| match &names {
        Some(names) => format!("\"{}\"", names.get(n as usize).map_or("", String::as_str).replace('\\', "\\\\").replace('"', "\\\"")),
        None => n.to_string(),
    };
    for t in &edit.tracks {
        out.push_str(&format!(
            "\t(segment\n\t\t(start {} {})\n\t\t(end {} {})\n\t\t(width {})\n{}\t\t(layer \"{}\")\n\t\t(net {})\n\t\t(uuid \"{}\")\n\t)\n",
            mm(t.start[0]),
            mm(t.start[1]),
            mm(t.end[0]),
            mm(t.end[1]),
            mm(t.width),
            if t.locked { "\t\t(locked yes)\n" } else { "" },
            t.layer,
            net(t.net),
            uuids.draw()
        ));
    }
    for t in &edit.arcs {
        out.push_str(&format!(
            "\t(arc\n\t\t(start {} {})\n\t\t(mid {} {})\n\t\t(end {} {})\n\t\t(width {})\n{}\t\t(layer \"{}\")\n\t\t(net {})\n\t\t(uuid \"{}\")\n\t)\n",
            mm(t.start[0]),
            mm(t.start[1]),
            mm(t.mid[0]),
            mm(t.mid[1]),
            mm(t.end[0]),
            mm(t.end[1]),
            mm(t.width),
            if t.locked { "\t\t(locked yes)\n" } else { "" },
            t.layer,
            net(t.net),
            uuids.draw()
        ));
    }
    for v in &edit.vias {
        out.push_str(&format!(
            "\t(via\n\t\t(at {} {})\n\t\t(size {})\n\t\t(drill {})\n\t\t(layers \"{}\" \"{}\")\n{}\t\t(net {})\n\t\t(uuid \"{}\")\n\t)\n",
            mm(v.at[0]),
            mm(v.at[1]),
            mm(v.size),
            mm(v.drill),
            v.layers[0],
            v.layers[1],
            if v.locked { "\t\t(locked yes)\n" } else { "" },
            net(v.net),
            uuids.draw()
        ));
    }
    out.push_str(&text[close..]);
    out
}

/// Metres as KiCad writes millimetres: up to six decimals (whole nanometres), no trailing zeros.
pub fn mm(m: f64) -> String {
    let s = format!("{:.6}", m * 1e3);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".to_string() } else { s.to_string() }
}

/// Where new items' uuids come from: random, or a sequence fixed by a seed.
pub enum UuidSource {
    Random,
    Seeded(u64),
}

impl UuidSource {
    pub fn new(seed: Option<u64>) -> UuidSource {
        // A zero state would stay zero.
        seed.map_or(UuidSource::Random, |s| UuidSource::Seeded(s ^ 0x9e37_79b9_7f4a_7c15 | 1))
    }

    /// The next version-4 uuid.
    pub fn draw(&mut self) -> String {
        let (a, b) = match self {
            UuidSource::Random => {
                static COUNTER: AtomicU64 = AtomicU64::new(0);
                let mut h = std::collections::hash_map::RandomState::new().build_hasher();
                h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
                let a = h.finish();
                h.write_u64(a);
                (a, h.finish())
            }
            UuidSource::Seeded(state) => {
                let mut draw = || {
                    // xorshift64*
                    *state ^= *state >> 12;
                    *state ^= *state << 25;
                    *state ^= *state >> 27;
                    state.wrapping_mul(0x2545_f491_4f6c_dd1d)
                };
                (draw(), draw())
            }
        };
        uuid_of(a, b)
    }
}

/// A random version-4 UUID; the standard library's hasher seeds supply the randomness.
#[cfg(test)]
fn uuid() -> String {
    UuidSource::Random.draw()
}

/// The version-4 uuid of 128 bits.
fn uuid_of(a: u64, b: u64) -> String {
    let bytes: Vec<u8> = a.to_le_bytes().into_iter().chain(b.to_le_bytes()).collect();
    let hex: String = bytes
        .iter()
        .enumerate()
        .map(|(i, byte)| match i {
            6 => format!("{:02x}", (byte & 0x0f) | 0x40),
            8 => format!("{:02x}", (byte & 0x3f) | 0x80),
            _ => format!("{byte:02x}"),
        })
        .collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn millimetres_keep_whole_nanometres_and_drop_trailing_zeros() {
        assert_eq!(mm(0.1e-3), "0.1");
        assert_eq!(mm(103.575e-3), "103.575");
        assert_eq!(mm(108.837499e-3), "108.837499");
        assert_eq!(mm(-0.0), "0");
        assert_eq!(mm(2e-3), "2");
    }

    #[test]
    fn uuids_are_version_4_and_differ() {
        let (a, b) = (uuid(), uuid());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
    }

    #[test]
    fn a_seed_fixes_the_uuids() {
        let draw = |seed| {
            let mut s = UuidSource::new(Some(seed));
            (s.draw(), s.draw())
        };
        assert_eq!(draw(1), draw(1));
        assert_ne!(draw(1), draw(2));
        assert_ne!(draw(1).0, draw(1).1);
        assert_eq!(&draw(0).0[14..15], "4");
    }

    #[test]
    fn locked_copper_is_written_locked() {
        let text = "(kicad_pcb\n\t(version 20241229)\n\t(net 0 \"\")\n\t(net 1 \"A\")\n)\n";
        let edit = Edit {
            tracks: vec![NewTrack { start: [1e-3, 2e-3], end: [3e-3, 2e-3], width: 0.2e-3, layer: "F.Cu".into(), net: 1, locked: true }],
            vias: vec![NewVia { at: [3e-3, 2e-3], size: 0.6e-3, drill: 0.3e-3, layers: ["F.Cu".into(), "B.Cu".into()], net: 1, locked: true }],
            ..Edit::default()
        };
        let out = apply(text, &edit);
        assert!(out.contains("(width 0.2)\n\t\t(locked yes)\n\t\t(layer \"F.Cu\")"), "{out}");
        assert!(out.contains("(layers \"F.Cu\" \"B.Cu\")\n\t\t(locked yes)\n\t\t(net 1)"), "{out}");
        let unlocked = apply(text, &Edit { tracks: vec![NewTrack { locked: false, ..edit.tracks[0].clone() }], ..Edit::default() });
        assert!(!unlocked.contains("locked"));
    }

    #[test]
    fn an_arc_is_read_back_as_written() {
        let text = "(kicad_pcb\n\t(version 20241229)\n\t(general (thickness 1.6))\n\t(layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal))\n\t(net 0 \"\")\n\t(net 1 \"A\")\n)\n";
        let arc = NewArc { start: [1e-3, 2e-3], mid: [1.5e-3, 2.5e-3], end: [2e-3, 2e-3], width: 0.25e-3, layer: "F.Cu".into(), net: 1, locked: false };
        let out = apply(text, &Edit { arcs: vec![arc.clone()], ..Edit::default() });
        let (_, copper) = crate::parse_board(&out).unwrap();
        let read = &copper.arcs[0];
        let close = |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).abs() < 1e-9 && (a[1] - b[1]).abs() < 1e-9;
        assert!(close(read.start, arc.start) && close(read.mid, arc.mid) && close(read.end, arc.end), "{out}");
        assert_eq!((read.width, read.layer.as_str(), read.net), (arc.width, "F.Cu", 1));
    }

    #[test]
    fn an_empty_edit_changes_nothing() {
        let text = "(kicad_pcb\n\t(version 20241229)\n)\n";
        assert_eq!(apply(text, &Edit::default()), text);
    }

    #[test]
    fn removed_items_take_their_lines_and_new_ones_go_before_the_close() {
        let text = "(kicad_pcb\n\t(a 1)\n\t(segment (x))\n\t(b 2)\n)\n";
        let start = text.find("(segment").unwrap();
        let end = start + "(segment (x))".len();
        let edit = Edit {
            remove: vec![Span { start, end }],
            tracks: vec![NewTrack { start: [1e-3, 2e-3], end: [3e-3, 2e-3], width: 0.2e-3, layer: "F.Cu".into(), net: 4, locked: false }],
            ..Edit::default()
        };
        let out = apply(text, &edit);
        assert!(out.starts_with("(kicad_pcb\n\t(a 1)\n\t(b 2)\n\t(segment\n\t\t(start 1 2)\n\t\t(end 3 2)\n\t\t(width 0.2)\n"));
        assert!(out.ends_with("\t)\n)\n"));
        crate::sexpr::parse(&out).unwrap();
    }

    #[test]
    fn a_board_that_names_its_nets_is_read_numbered_and_written_by_name() {
        // KiCad 10: no net table, each pad and track names its net.
        let text = "(kicad_pcb\n\t(version 20260206)\n\t(general (thickness 1.6))\n\t(layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal))\n\t(footprint \"R\" (layer \"F.Cu\") (uuid \"f1\") (at 10 10)\n\t\t(pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net \"GND\") (uuid \"p1\"))\n\t\t(pad \"2\" smd rect (at 2 0) (size 1 1) (layers \"F.Cu\") (net \"VCC\") (uuid \"p2\")))\n\t(segment (start 10 10) (end 10 12) (width 0.2) (layer \"F.Cu\") (net \"GND\") (uuid \"s1\"))\n)\n";
        let (pcb, copper) = crate::parse_board(text).unwrap();
        let names: Vec<&str> = pcb.nets.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["", "GND", "VCC"]);
        assert_eq!(pcb.footprints[0].pads.iter().map(|p| p.net.as_ref().unwrap().number).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(copper.tracks[0].net, 1);
        let edit = Edit { tracks: vec![NewTrack { start: [12e-3, 10e-3], end: [12e-3, 12e-3], width: 0.2e-3, layer: "F.Cu".into(), net: 2, locked: false }], ..Edit::default() };
        let out = apply(text, &edit);
        assert!(out.contains("(net \"VCC\")\n\t\t(uuid"), "{out}");
        let (again, _) = crate::parse_board(&out).unwrap();
        assert_eq!(again.nets.len(), 3);
    }
}
