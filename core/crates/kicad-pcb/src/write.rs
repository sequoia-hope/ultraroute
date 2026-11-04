//! Routed copper back into a `.kicad_pcb`: items are removed by their spans and new tracks and
//! vias added before the board's closing paren, in KiCad 9's own layout. Every other byte of the
//! file is kept as it was.

use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::copper::Span;
use crate::sexpr::Node;
use crate::KicadParse;

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

/// Poured rectangles `[x0, y0, x1, y1]` in metres to write into a zone's `(filled_polygon …)` on
/// `layer`, indexed in `Copper::zones` order.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneFill {
    pub zone: usize,
    pub layer: String,
    pub rects: Vec<[f64; 4]>,
}

/// Per-zone point warp `(zone_index, point_m) -> new_point_m`.
pub type ZonePointMap<'a> = dyn Fn(usize, [f64; 2]) -> [f64; 2] + 'a;

/// Spans to remove and `(span, replacement)` edits produced by [`scale_edits`].
pub type ScaleEdits = (Vec<Span>, Vec<(Span, String)>);

/// Spans to drop and replace in `text` so the board outline, board-level drawings and board-level
/// zone outlines scale by `factor` about `centre` (metres, or by `zone_map` per top-level zone
/// index when given), each footprint `(fp, orig_at, new_at)` moves to `new_at` (carrying any zones
/// drawn inside it), stale zone fills are dropped, and any `zone_fills` are written in their place.
pub fn scale_edits(
    text: &str,
    centre: [f64; 2],
    factor: f64,
    footprints: &[(usize, [f64; 2], [f64; 2])],
    zone_map: Option<&ZonePointMap<'_>>,
    zone_fills: &[ZoneFill],
) -> Result<ScaleEdits, KicadParse> {
    let root = crate::sexpr::parse(text)?;
    let scale_pt = |p: [f64; 2]| [centre[0] + factor * (p[0] - centre[0]), centre[1] + factor * (p[1] - centre[1])];
    let mut fp_moves = std::collections::HashMap::new();
    for &(fp, orig, to) in footprints {
        if (to[0] - orig[0]).abs() > 0.25e-9 || (to[1] - orig[1]).abs() > 0.25e-9 {
            fp_moves.insert(fp, (orig, to));
        }
    }
    let scale_changed = (factor - 1.0).abs() > 1e-12;
    let mut remove = Vec::new();
    let mut replace = Vec::new();
    let mut fp_idx = 0usize;
    let mut zone_idx = 0usize;

    for child in root.args() {
        match child.head() {
            Some("gr_line" | "gr_arc" | "gr_circle" | "gr_rect" | "gr_poly" | "gr_curve")
                if scale_changed =>
            {
                rewrite_graphic_points(child, &scale_pt, &mut replace)?;
            }
            Some("gr_text" | "gr_text_box") if scale_changed => {
                if let Some(at) = child.find("at") {
                    replace_xy(at, &scale_pt, &mut replace)?;
                }
                for tag in ["start", "end"] {
                    if let Some(pt) = child.find(tag) {
                        replace_xy(pt, &scale_pt, &mut replace)?;
                    }
                }
            }
            Some("zone") => {
                let z = zone_idx;
                zone_idx += 1;
                if let Some(zf) = zone_map {
                    rewrite_zone_polygons(child, &|p| zf(z, p), &mut replace)?;
                } else if scale_changed {
                    rewrite_zone_polygons(child, &scale_pt, &mut replace)?;
                }
                drop_and_fill_zone(text, child, z, zone_fills, &mut remove, &mut replace);
            }
            Some("footprint") => {
                let fp = fp_idx;
                fp_idx += 1;
                let moved = fp_moves.get(&fp).copied();
                if let Some((_, to)) = moved {
                    if let Some(at) = child.find("at") {
                        replace_xy(at, &|_| to, &mut replace)?;
                    }
                }
                for znode in child.find_all("zone") {
                    let z = zone_idx;
                    zone_idx += 1;
                    if let Some((orig, to)) = moved {
                        let (dx, dy) = (to[0] - orig[0], to[1] - orig[1]);
                        rewrite_zone_polygons(znode, &|p| [p[0] + dx, p[1] + dy], &mut replace)?;
                    }
                    if moved.is_some() || scale_changed || zone_fills.iter().any(|f| f.zone == z) {
                        drop_and_fill_zone(text, znode, z, zone_fills, &mut remove, &mut replace);
                    }
                }
            }
            _ => {}
        }
    }
    Ok((remove, replace))
}

fn replace_xy(node: &Node, map: &dyn Fn([f64; 2]) -> [f64; 2], out: &mut Vec<(Span, String)>) -> Result<(), KicadParse> {
    let args = node.args();
    if args.len() < 2 {
        return Ok(());
    }
    let x = args[0].number()? * 1e-3;
    let y = args[1].number()? * 1e-3;
    let [nx, ny] = map([x, y]);
    out.push((Span { start: args[0].pos().start, end: args[1].pos().end }, format!("{} {}", mm(nx), mm(ny))));
    Ok(())
}

fn rewrite_pts_list(pts: &Node, map: &dyn Fn([f64; 2]) -> [f64; 2], out: &mut Vec<(Span, String)>) -> Result<(), KicadParse> {
    for item in pts.args() {
        match item.head() {
            Some("xy") => replace_xy(item, map, out)?,
            Some("arc") => {
                for tag in ["start", "mid", "end"] {
                    if let Some(pt) = item.find(tag) {
                        replace_xy(pt, map, out)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn rewrite_graphic_points(node: &Node, map: &dyn Fn([f64; 2]) -> [f64; 2], out: &mut Vec<(Span, String)>) -> Result<(), KicadParse> {
    for tag in ["start", "mid", "end", "center"] {
        if let Some(pt) = node.find(tag) {
            replace_xy(pt, map, out)?;
        }
    }
    if let Some(pts) = node.find("pts") {
        rewrite_pts_list(pts, map, out)?;
    }
    Ok(())
}

fn rewrite_zone_polygons(zone: &Node, map: &dyn Fn([f64; 2]) -> [f64; 2], out: &mut Vec<(Span, String)>) -> Result<(), KicadParse> {
    for poly in zone.find_all("polygon") {
        if let Some(pts) = poly.find("pts") {
            rewrite_pts_list(pts, map, out)?;
        }
    }
    Ok(())
}

fn drop_and_fill_zone(
    text: &str,
    zone: &Node,
    zone_idx: usize,
    zone_fills: &[ZoneFill],
    remove: &mut Vec<Span>,
    replace: &mut Vec<(Span, String)>,
) {
    for tag in ["filled_polygon", "fill_segments"] {
        for f in zone.find_all(tag) {
            remove.push(f.pos().into());
        }
    }
    let mut filled = String::new();
    for zf in zone_fills.iter().filter(|f| f.zone == zone_idx) {
        for &[x0, y0, x1, y1] in &zf.rects {
            filled.push_str(&format!(
                "\t\t(filled_polygon\n\t\t\t(layer \"{}\")\n\t\t\t(pts\n\t\t\t\t(xy {} {}) (xy {} {}) (xy {} {}) (xy {} {})\n\t\t\t)\n\t\t)\n",
                zf.layer,
                mm(x0),
                mm(y0),
                mm(x1),
                mm(y0),
                mm(x1),
                mm(y1),
                mm(x0),
                mm(y1)
            ));
        }
    }
    if filled.is_empty() {
        return;
    }
    let end = zone.pos().end;
    let line_start = text[..end - 1].rfind('\n').map_or(0, |i| i + 1);
    if line_start > zone.pos().start && text[line_start..end - 1].trim().is_empty() {
        replace.push((Span { start: line_start, end }, format!("{filled}{}", &text[line_start..end])));
    } else {
        replace.push((Span { start: end - 1, end }, format!("\n{filled}\t)")));
    }
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

    #[test]
    fn scale_edits_rewrites_outline_zones_drawings_and_footprints() {
        let text = "(kicad_pcb\n\t(version 20241229)\n\t(general (thickness 1.6))\n\t(layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))\n\t(net 0 \"\")\n\t(net 1 \"GND\")\n\t(gr_line (start 0 0) (end 20 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e1\"))\n\t(gr_arc (start 20 0) (mid 22 5) (end 20 10) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e2\"))\n\t(gr_line (start 20 10) (end 0 10) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e3\"))\n\t(gr_line (start 0 10) (end 0 0) (stroke (width 0.1)) (layer \"Edge.Cuts\") (uuid \"e4\"))\n\t(gr_text \"REV1\" (at 2 2 90) (layer \"F.Cu\") (uuid \"t1\"))\n\t(footprint \"R\" (locked yes) (layer \"F.Cu\") (uuid \"f1\") (at 4 2 45)\n\t\t(pad \"1\" smd rect (at 0 0 45) (size 1 1) (layers \"F.Cu\") (net 1 \"GND\") (uuid \"p1\"))\n\t\t(zone (net 0) (net_name \"\") (layer \"F.Cu\") (uuid \"fz1\")\n\t\t\t(keepout (tracks not_allowed) (vias not_allowed) (pads allowed) (copperpour not_allowed) (footprints allowed))\n\t\t\t(polygon (pts (xy 3 1) (xy 5 1) (xy 5 3) (xy 3 3)))\n\t\t)\n\t)\n\t(zone (net 1) (net_name \"GND\") (layer \"F.Cu\") (uuid \"z1\")\n\t\t(polygon (pts (xy 0 0) (xy 20 0) (xy 20 10) (xy 0 10)))\n\t\t(filled_polygon (layer \"F.Cu\") (pts (xy 1 1) (xy 2 1) (xy 2 2) (xy 1 2)))\n\t)\n)\n";
        let centre = [10e-3, 5e-3];
        let factor = 1.5;
        let orig = [4e-3, 2e-3];
        let to = [centre[0] + factor * (orig[0] - centre[0]), centre[1] + factor * (orig[1] - centre[1])];
        let fills = vec![ZoneFill { zone: 1, layer: "F.Cu".into(), rects: vec![[2e-3, 2e-3, 3e-3, 3e-3]] }];
        let (remove, replace) = scale_edits(text, centre, factor, &[(0, orig, to)], None, &fills).unwrap();
        let out = apply(text, &Edit { remove, replace, ..Edit::default() });
        let (pcb, copper) = crate::parse_board(&out).unwrap();
        assert!((pcb.footprints[0].at[0] - 1e-3).abs() < 1e-9 && (pcb.footprints[0].at[1] - 0.5e-3).abs() < 1e-9, "{out}");
        assert!((pcb.footprints[0].rotation_deg - 45.0).abs() < 1e-9, "rotation preserved: {out}");
        assert!(out.contains("(gr_text \"REV1\" (at -2 0.5 90)"), "text scaled with angle kept: {out}");
        assert_eq!(copper.zones.len(), 2);
        assert!(copper.zones[0].keepout.is_some());
        assert!((copper.zones[0].outline[0][0] - 0.0).abs() < 1e-9, "footprint zone translated by dx=-3mm: {out}");
        assert!((copper.zones[1].outline[0][0] - -5e-3).abs() < 1e-9, "board zone scaled about centre: {out}");
        assert_eq!(copper.zones[1].filled.len(), 1, "stale fill replaced by carved rect: {out}");
        assert!(pcb.outline_loops().is_ok(), "scaled outline chains cleanly: {out}");
    }
}
