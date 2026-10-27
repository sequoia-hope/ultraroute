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
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewVia {
    pub at: [f64; 2],
    pub size: f64,
    pub drill: f64,
    pub layers: [String; 2],
    pub net: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Edit {
    pub remove: Vec<Span>,
    /// Text put in place of a span, such as a moved footprint's `(at …)`.
    pub replace: Vec<(Span, String)>,
    pub tracks: Vec<NewTrack>,
    pub vias: Vec<NewVia>,
}

pub fn apply(text: &str, edit: &Edit) -> String {
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
            "\t(segment\n\t\t(start {} {})\n\t\t(end {} {})\n\t\t(width {})\n\t\t(layer \"{}\")\n\t\t(net {})\n\t\t(uuid \"{}\")\n\t)\n",
            mm(t.start[0]),
            mm(t.start[1]),
            mm(t.end[0]),
            mm(t.end[1]),
            mm(t.width),
            t.layer,
            net(t.net),
            uuid()
        ));
    }
    for v in &edit.vias {
        out.push_str(&format!(
            "\t(via\n\t\t(at {} {})\n\t\t(size {})\n\t\t(drill {})\n\t\t(layers \"{}\" \"{}\")\n\t\t(net {})\n\t\t(uuid \"{}\")\n\t)\n",
            mm(v.at[0]),
            mm(v.at[1]),
            mm(v.size),
            mm(v.drill),
            v.layers[0],
            v.layers[1],
            net(v.net),
            uuid()
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

/// A random version-4 UUID; the standard library's hasher seeds supply the randomness.
fn uuid() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    let a = h.finish();
    h.write_u64(a);
    let b = h.finish();
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
            tracks: vec![NewTrack { start: [1e-3, 2e-3], end: [3e-3, 2e-3], width: 0.2e-3, layer: "F.Cu".into(), net: 4 }],
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
        let edit = Edit { tracks: vec![NewTrack { start: [12e-3, 10e-3], end: [12e-3, 12e-3], width: 0.2e-3, layer: "F.Cu".into(), net: 2 }], ..Edit::default() };
        let out = apply(text, &edit);
        assert!(out.contains("(net \"VCC\")\n\t\t(uuid"), "{out}");
        let (again, _) = crate::parse_board(&out).unwrap();
        assert_eq!(again.nets.len(), 3);
    }
}
