//! The reader on two real KiCad 9 boards (core/fixtures, see SOURCE.txt).

use std::path::PathBuf;

use kicad_pcb::*;

fn board(name: &str) -> (Pcb, Copper, String) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let (pcb, copper) = parse_board(&text).unwrap();
    (pcb, copper, text)
}

fn count(text: &str, form: &str) -> usize {
    text.lines().filter(|l| l.strip_prefix('\t').is_some_and(|l| l.starts_with(&format!("({form}")) && !l.starts_with(&format!("({form}_")))).count()
}

#[test]
fn every_track_via_and_zone_is_read() {
    for name in ["bluebuzz-rm2.kicad_pcb", "earring.kicad_pcb"] {
        let (pcb, copper, text) = board(name);
        assert_eq!(copper.tracks.len(), count(&text, "segment"), "{name}");
        assert_eq!(copper.arcs.len(), count(&text, "arc"), "{name}");
        assert_eq!(copper.vias.len(), count(&text, "via"), "{name}");
        assert_eq!(copper.zones.iter().filter(|z| z.footprint.is_none()).count(), count(&text, "zone"), "{name}");
        let pads: usize = pcb.footprints.iter().map(|f| f.pads.len()).sum();
        assert_eq!(copper.pads.len(), pads, "{name}");
        assert_eq!(copper.locked_footprints.len(), pcb.footprints.len());
    }
}

#[test]
fn copper_layers_run_front_to_back() {
    let (_, copper, _) = board("bluebuzz-rm2.kicad_pcb");
    let names: Vec<&str> = copper.layers.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, ["F.Cu", "In1.Cu", "In2.Cu", "In3.Cu", "In4.Cu", "B.Cu"]);
    assert_eq!(copper.expand(&["*.Cu".to_string()]).len(), 6);
    assert_eq!(copper.expand(&["F.Cu".to_string(), "F.Mask".to_string()]), ["F.Cu"]);
}

#[test]
fn spans_cover_exactly_their_item() {
    let (_, copper, text) = board("bluebuzz-rm2.kicad_pcb");
    for t in copper.tracks.iter().take(50) {
        let s = &text[t.span.start..t.span.end];
        assert!(s.starts_with("(segment") && s.ends_with(')') && s.contains(&t.uuid), "{s}");
    }
    for v in copper.vias.iter().take(50) {
        assert!(text[v.span.start..v.span.end].starts_with("(via"));
    }
}

fn inside(outline: &Outline, p: [f64; 2], slack: f64) -> bool {
    match outline {
        Outline::Circle { centre, radius } => (p[0] - centre[0]).hypot(p[1] - centre[1]) <= radius + slack,
        Outline::Polygons(polys) => polys.iter().any(|poly| {
            let mut odd = false;
            for i in 0..poly.len() {
                let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
                if (a[1] > p[1]) != (b[1] > p[1]) && p[0] < a[0] + (p[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0]) {
                    odd = !odd;
                }
            }
            odd || (0..poly.len()).any(|i| segment_distance(p, poly[i], poly[(i + 1) % poly.len()]) <= slack)
        }),
    }
}

fn segment_distance(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 { 0.0 } else { (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0) };
    (p[0] - a[0] - t * dx).hypot(p[1] - a[1] - t * dy)
}

/// Where a track ends on a pad of its own net, its copper reaches into that pad's outline: the
/// pad placement and rotation agree with where the designer's copper actually meets the pads.
#[test]
fn tracks_end_inside_the_pads_they_meet() {
    for name in ["bluebuzz-rm2.kicad_pcb", "earring.kicad_pcb"] {
        let (pcb, copper, _) = board(name);
        let pads: Vec<(&PadCopper, u32)> = copper
            .pads
            .iter()
            .filter_map(|p| Some((p, pcb.footprints[p.footprint].pads[p.pad].net.as_ref()?.number)))
            .collect();
        let near = |p: [f64; 2], net: u32| {
            pads.iter().filter(|(_, n)| *n == net).map(|(pad, _)| pad).find(|pad| {
                let at = pcb.footprints[pad.footprint].pads[pad.pad].position;
                (at[0] - p[0]).hypot(at[1] - p[1]) < 0.3e-3
            })
        };
        let (mut checked, mut inside_count) = (0, 0);
        for t in &copper.tracks {
            for end in [t.start, t.end] {
                if let Some(pad) = near(end, t.net) {
                    checked += 1;
                    inside_count += inside(&pad.outline, end, t.width / 2.0) as usize;
                }
            }
        }
        assert!(checked > 0, "{name}: {checked}");
        assert_eq!(inside_count, checked, "{name}");
    }
}

#[test]
fn rule_areas_say_what_they_forbid() {
    let (_, copper, _) = board("bluebuzz-rm2.kicad_pcb");
    let keepouts: Vec<&Zone> = copper.zones.iter().filter(|z| z.keepout.is_some()).collect();
    assert!(!keepouts.is_empty());
    assert!(keepouts.iter().any(|z| z.keepout.unwrap().tracks));
    let poured: Vec<&Zone> = copper.zones.iter().filter(|z| !z.filled.is_empty()).collect();
    assert!(!poured.is_empty());
}

#[test]
fn custom_pads_carry_their_primitives() {
    let (_, copper, _) = board("earring.kicad_pcb");
    let custom: Vec<&PadCopper> = copper.pads.iter().filter(|p| p.shape == PadShape::Custom).collect();
    assert_eq!(custom.len(), 5);
    for pad in custom {
        let Outline::Polygons(polys) = &pad.outline else { panic!() };
        assert!(polys.len() >= 2 && polys[1..].iter().all(|p| p.len() >= 3), "{polys:?}");
    }
}

fn key(start: [f64; 2], end: [f64; 2], width: f64, layer: &str, net: u32) -> String {
    let nm = |v: f64| (v * 1e9).round() as i64;
    format!("{} {} {} {} {} {layer} {net}", nm(start[0]), nm(start[1]), nm(end[0]), nm(end[1]), nm(width))
}

/// Every track and via taken out and written back in gives the same copper, and nothing else
/// in the file moves.
#[test]
fn copper_written_back_reads_the_same() {
    let (_, copper, text) = board("bluebuzz-rm2.kicad_pcb");
    let edit = Edit {
        remove: copper.tracks.iter().map(|t| t.span).chain(copper.vias.iter().map(|v| v.span)).collect(),
        tracks: copper
            .tracks
            .iter()
            .map(|t| NewTrack { start: t.start, end: t.end, width: t.width, layer: t.layer.clone(), net: t.net })
            .collect(),
        vias: copper
            .vias
            .iter()
            .map(|v| NewVia { at: v.at, size: v.size, drill: v.drill, layers: v.layers.clone(), net: v.net })
            .collect(),
        replace: Vec::new(),
    };
    let written = apply(&text, &edit);
    let (_, again) = parse_board(&written).unwrap();
    let tracks = |c: &Copper| {
        let mut k: Vec<String> = c.tracks.iter().map(|t| key(t.start, t.end, t.width, &t.layer, t.net)).collect();
        k.sort();
        k
    };
    let vias = |c: &Copper| {
        let mut k: Vec<String> = c.vias.iter().map(|v| key(v.at, v.at, v.size, &v.layers.join(","), v.net)).collect();
        k.sort();
        k
    };
    assert_eq!(tracks(&copper), tracks(&again));
    assert_eq!(vias(&copper), vias(&again));
    let removed = apply(&text, &Edit { remove: edit.remove.clone(), ..Edit::default() });
    let close = removed.rfind(')').unwrap();
    assert!(written.starts_with(&removed[..close]));
    assert!(written.ends_with(&removed[close..]));
    assert_eq!(removed.matches("\n\t(segment\n").count() + removed.matches("\n\t(via\n").count(), 0);
}

/// Courtyards as read hold the pads they surround; a few library footprints draw theirs a
/// hair inside their own pads (the earring's USON-8), so 95%, not all, are asked for.
#[test]
fn most_footprints_have_a_courtyard_around_their_pads() {
    for name in ["bluebuzz-rm2.kicad_pcb", "earring.kicad_pcb"] {
        let (pcb, copper, _) = board(name);
        let with: Vec<usize> = (0..pcb.footprints.len()).filter(|&i| copper.courtyards[i].is_some()).collect();
        assert!(with.len() * 10 >= pcb.footprints.len() * 8, "{name}: {} of {}", with.len(), pcb.footprints.len());
        let (mut pads, mut inside) = (0, 0);
        for i in with {
            let yard = copper.courtyards[i].as_ref().unwrap();
            for pad in &pcb.footprints[i].pads {
                let p = pad.position;
                let mut odd = false;
                for k in 0..yard.len() {
                    let (a, b) = (yard[k], yard[(k + 1) % yard.len()]);
                    if (a[1] > p[1]) != (b[1] > p[1]) && p[0] < a[0] + (p[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0]) {
                        odd = !odd;
                    }
                }
                let on_edge = (0..yard.len()).any(|k| segment_distance(p, yard[k], yard[(k + 1) % yard.len()]) < 1e-9);
                pads += 1;
                inside += (odd || on_edge) as usize;
            }
        }
        assert!(inside * 100 >= pads * 95, "{name}: {inside} of {pads} pads inside their courtyards");
    }
}
