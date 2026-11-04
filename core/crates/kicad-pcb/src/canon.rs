//! One order for a board, whatever order its file has. KiCad sorts footprints by uuid and writes
//! other items as they fall, so the same board can come in many orders; everything downstream
//! takes indices and net numbers from what it reads, and breaks ties by them. So what is read is
//! put in an order built from what each thing is (net name, layer, geometry, reference and pad
//! number), never from where it stood or from a uuid. Nets are numbered by name, 0 staying the
//! unconnected net; [`Pcb::file_nets`] keeps each one's number in the file for the writer. Spans
//! still point into the text as it was, so what the writer keeps stays where it is.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

use crate::copper::{ArcTrack, Copper, CopperGraphic, PadCopper, Span, Track, Via, Zone};
use crate::model::{Footprint, Net, OutlinePrimitive, OutlineShape, Pad, Pcb};

/// Numbers in order, by value; `total_cmp` so that every float has a place.
fn floats(a: impl IntoIterator<Item = f64>, b: impl IntoIterator<Item = f64>) -> Ordering {
    let (mut a, mut b) = (a.into_iter(), b.into_iter());
    loop {
        match (a.next(), b.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match x.total_cmp(&y) {
                Ordering::Equal => {}
                other => return other,
            },
        }
    }
}

fn points(a: &[[f64; 2]], b: &[[f64; 2]]) -> Ordering {
    floats(a.iter().flatten().copied(), b.iter().flatten().copied())
}

/// Lists of polygons, the shorter list first, then polygon by polygon.
fn polygons<'a>(a: impl ExactSizeIterator<Item = &'a Vec<[f64; 2]>>, b: impl ExactSizeIterator<Item = &'a Vec<[f64; 2]>>) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.zip(b).map(|(p, q)| points(p, q)).find(|o| o.is_ne()).unwrap_or(Ordering::Equal))
}

/// The last word on two things that agree on every field looked at: all the rest of them. Only
/// ties get here, so a board pays for this hardly at all.
fn rest<T: Debug>(a: &T, b: &T) -> Ordering {
    format!("{a:?}").cmp(&format!("{b:?}"))
}

/// Numbers the nets by name. Each number any item uses gets a place, one the table lacks as a net
/// with no name.
fn number_nets(pcb: &mut Pcb, copper: &mut Copper) {
    let table: BTreeMap<u32, String> = pcb.nets.iter().map(|n| (n.number, n.name.clone())).collect();
    let mut used: BTreeSet<u32> = table.keys().copied().collect();
    used.extend(pcb.footprints.iter().flat_map(|f| &f.pads).filter_map(|p| p.net.as_ref().map(|n| n.number)));
    used.extend(copper.tracks.iter().map(|t| t.net));
    used.extend(copper.arcs.iter().map(|t| t.net));
    used.extend(copper.vias.iter().map(|v| v.net));
    used.extend(copper.zones.iter().map(|z| z.net));
    used.extend(copper.graphics.iter().chain(&copper.mask_graphics).map(|g| g.net));
    used.remove(&0);
    let name = |n: &u32| table.get(n).map_or("", String::as_str);
    let mut order: Vec<u32> = used.into_iter().collect();
    // Two numbers with one name, which KiCad does not write, are told apart by their numbers: all
    // the file says of them.
    order.sort_by(|a, b| name(a).cmp(name(b)).then(a.cmp(b)));
    order.insert(0, 0);
    let number: BTreeMap<u32, u32> = order.iter().enumerate().map(|(k, n)| (*n, k as u32)).collect();
    pcb.nets = order.iter().enumerate().map(|(k, n)| Net { number: k as u32, name: name(n).to_string() }).collect();
    let new = |n: &mut u32| *n = number[n];
    pcb.footprints.iter_mut().flat_map(|f| &mut f.pads).filter_map(|p| p.net.as_mut()).for_each(|n| new(&mut n.number));
    copper.tracks.iter_mut().for_each(|t| new(&mut t.net));
    copper.arcs.iter_mut().for_each(|t| new(&mut t.net));
    copper.vias.iter_mut().for_each(|v| new(&mut v.net));
    copper.zones.iter_mut().for_each(|z| new(&mut z.net));
    copper.graphics.iter_mut().chain(&mut copper.mask_graphics).for_each(|g| new(&mut g.net));
    pcb.file_nets = order;
}

type PadWith = (Pad, Option<PadCopper>);

fn by_pad(a: &PadWith, b: &PadWith) -> Ordering {
    let key = |p: &Pad| [p.position[0], p.position[1], p.size[0], p.size[1], p.rotation_deg];
    a.0.number.cmp(&b.0.number).then_with(|| floats(key(&a.0), key(&b.0))).then_with(|| {
        let bare = |p: &PadWith| (p.0.clone(), p.1.clone().map(|c| PadCopper { footprint: 0, pad: 0, ..c }));
        rest(&bare(a), &bare(b))
    })
}

fn by_footprint(a: &Footprint, b: &Footprint) -> Ordering {
    let key = |f: &Footprint| [f.at[0], f.at[1], f.rotation_deg];
    (&a.reference, &a.library_id).cmp(&(&b.reference, &b.library_id)).then_with(|| floats(key(a), key(b))).then_with(|| {
        let bare = |f: &Footprint| Footprint { uuid: String::new(), ..f.clone() };
        rest(&bare(a), &bare(b))
    })
}

/// Each footprint's pads in order, then the footprints, and all that goes by their indices.
fn order_footprints(pcb: &mut Pcb, copper: &mut Copper) {
    let mut coppers: Vec<Vec<Option<PadCopper>>> = pcb.footprints.iter().map(|f| vec![None; f.pads.len()]).collect();
    for pad in copper.pads.drain(..) {
        let (f, p) = (pad.footprint, pad.pad);
        coppers[f][p] = Some(pad);
    }
    for (f, mine) in pcb.footprints.iter_mut().zip(&mut coppers) {
        let mut pads: Vec<PadWith> = std::mem::take(&mut f.pads).into_iter().zip(std::mem::take(mine)).collect();
        pads.sort_by(by_pad);
        (f.pads, *mine) = pads.into_iter().unzip();
    }
    let mut order: Vec<usize> = (0..pcb.footprints.len()).collect();
    order.sort_by(|&a, &b| by_footprint(&pcb.footprints[a], &pcb.footprints[b]));
    let mut place = vec![0; order.len()];
    for (k, &f) in order.iter().enumerate() {
        place[f] = k;
    }
    let mut old: Vec<Option<Footprint>> = std::mem::take(&mut pcb.footprints).into_iter().map(Some).collect();
    pcb.footprints = order.iter().filter_map(|&f| old[f].take()).collect();
    copper.locked_footprints = order.iter().map(|&f| copper.locked_footprints[f]).collect();
    copper.footprint_at = order.iter().map(|&f| copper.footprint_at[f]).collect();
    copper.courtyards = order.iter().map(|&f| copper.courtyards[f].take()).collect();
    for (k, &f) in order.iter().enumerate() {
        for (p, pad) in std::mem::take(&mut coppers[f]).into_iter().enumerate() {
            copper.pads.extend(pad.map(|pad| PadCopper { footprint: k, pad: p, ..pad }));
        }
    }
    let new = |f: &mut Option<usize>| *f = f.map(|f| place[f]);
    copper.zones.iter_mut().for_each(|z| new(&mut z.footprint));
    copper.graphics.iter_mut().chain(&mut copper.mask_graphics).for_each(|g| new(&mut g.footprint));
}

fn by_track(a: &Track, b: &Track) -> Ordering {
    let key = |t: &Track| [t.start[0], t.start[1], t.end[0], t.end[1], t.width];
    (a.net, &a.layer).cmp(&(b.net, &b.layer)).then_with(|| floats(key(a), key(b))).then(a.locked.cmp(&b.locked))
}

fn by_arc(a: &ArcTrack, b: &ArcTrack) -> Ordering {
    let key = |t: &ArcTrack| [t.start[0], t.start[1], t.mid[0], t.mid[1], t.end[0], t.end[1], t.width];
    (a.net, &a.layer).cmp(&(b.net, &b.layer)).then_with(|| floats(key(a), key(b))).then(a.locked.cmp(&b.locked))
}

fn by_via(a: &Via, b: &Via) -> Ordering {
    let key = |v: &Via| [v.at[0], v.at[1], v.size, v.drill];
    (a.net, &a.layers).cmp(&(b.net, &b.layers)).then_with(|| floats(key(a), key(b))).then(a.locked.cmp(&b.locked))
}

fn by_zone(a: &Zone, b: &Zone) -> Ordering {
    (a.net, a.priority, &a.layers, &a.name, a.footprint)
        .cmp(&(b.net, b.priority, &b.layers, &b.name, b.footprint))
        .then_with(|| points(&a.outline, &b.outline))
        .then_with(|| polygons(a.cutouts.iter(), b.cutouts.iter()))
        .then_with(|| a.filled.iter().map(|f| &f.0).cmp(b.filled.iter().map(|f| &f.0)))
        .then_with(|| polygons(a.filled.iter().map(|f| &f.1), b.filled.iter().map(|f| &f.1)))
        .then_with(|| {
            let bare = |z: &Zone| Zone {
                uuid: String::new(),
                span: Span { start: 0, end: 0 },
                fill_spans: Vec::new(),
                outline: Vec::new(),
                cutouts: Vec::new(),
                filled: Vec::new(),
                ..z.clone()
            };
            rest(&bare(a), &bare(b))
        })
}

fn by_graphic(a: &CopperGraphic, b: &CopperGraphic) -> Ordering {
    (&a.layer, a.net, a.footprint).cmp(&(&b.layer, b.net, b.footprint)).then_with(|| polygons(a.polygons.iter(), b.polygons.iter()))
}

fn by_primitive(a: &OutlinePrimitive, b: &OutlinePrimitive) -> Ordering {
    let key = |p: &OutlinePrimitive| match p.shape {
        OutlineShape::Line { start, end } => (0, vec![start[0], start[1], end[0], end[1]]),
        OutlineShape::Arc { start, mid, end } => (1, vec![start[0], start[1], mid[0], mid[1], end[0], end[1]]),
        OutlineShape::Circle { center, radius } => (2, vec![center[0], center[1], radius]),
    };
    let (x, y) = (key(a), key(b));
    x.0.cmp(&y.0).then_with(|| floats(x.1, y.1)).then_with(|| a.width.total_cmp(&b.width))
}

/// Puts what [`crate::parse_board`] read into the one order. Things it leaves tied are the same
/// in all but their uuids and their place in the text, so either may come first.
pub(crate) fn canonical(pcb: &mut Pcb, copper: &mut Copper) {
    number_nets(pcb, copper);
    order_footprints(pcb, copper);
    copper.tracks.sort_by(by_track);
    copper.arcs.sort_by(by_arc);
    copper.vias.sort_by(by_via);
    for z in &mut copper.zones {
        let mut fills: Vec<_> = std::mem::take(&mut z.filled).into_iter().zip(std::mem::take(&mut z.fill_spans)).collect();
        fills.sort_by(|a, b| a.0 .0.cmp(&b.0 .0).then_with(|| points(&a.0 .1, &b.0 .1)));
        (z.filled, z.fill_spans) = fills.into_iter().unzip();
    }
    copper.zones.sort_by(by_zone);
    copper.graphics.sort_by(by_graphic);
    copper.mask_graphics.sort_by(by_graphic);
    pcb.outline.sort_by(by_primitive);
    pcb.margin.sort_by(by_primitive);
    copper.groups.sort_by(|a, b| a.name.cmp(&b.name));
    pcb.warnings.sort();
    copper.warnings.sort();
}

#[cfg(test)]
mod tests {
    use crate::parse_board;

    const BOARD: &str = "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal))
        (net 0 \"\") (net 5 \"B\") (net 2 \"A\")
        (footprint \"p\" (layer \"F.Cu\") (uuid \"u2\") (at 5 5) (property \"Reference\" \"R2\")
          (pad \"2\" smd rect (at 1 0) (size 1 1) (layers \"F.Cu\") (net 2 \"A\"))
          (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net 5 \"B\"))
          (fp_rect (start 0 0) (end 1 1) (stroke (width 0.1) (type solid)) (fill yes) (layer \"F.Cu\")))
        (footprint \"p\" (layer \"F.Cu\") (uuid \"u1\") (at 9 9) (property \"Reference\" \"R1\")
          (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net 2 \"A\")))
        (segment (start 3 3) (end 4 4) (width 0.2) (layer \"F.Cu\") (net 5) (uuid \"s1\"))
        (segment (start 1 1) (end 2 2) (width 0.2) (layer \"F.Cu\") (net 2) (uuid \"s2\"))
        (zone (net 5) (net_name \"B\") (layer \"F.Cu\") (uuid \"z1\") (hatch edge 0.5) (connect_pads (clearance 0.3)) (min_thickness 0.2)
          (polygon (pts (xy 0 0) (xy 9 0) (xy 9 9))))
        (zone (net 2) (net_name \"A\") (layer \"F.Cu\") (uuid \"z2\") (hatch edge 0.5) (connect_pads (clearance 0.3)) (min_thickness 0.2)
          (polygon (pts (xy 0 0) (xy 8 0) (xy 8 8)))))";

    #[test]
    fn nets_go_by_name_and_remember_their_file_numbers() {
        let (pcb, copper) = parse_board(BOARD).unwrap();
        let names: Vec<(u32, &str)> = pcb.nets.iter().map(|n| (n.number, n.name.as_str())).collect();
        assert_eq!(names, [(0, ""), (1, "A"), (2, "B")]);
        assert_eq!(pcb.file_nets, [0, 2, 5]);
        assert_eq!((pcb.file_net(1), pcb.file_net(2), pcb.file_net(9)), (2, 5, 9));
        assert_eq!(copper.tracks[0].net, 1, "the track of A, numbered 2 in the file");
        assert_eq!(copper.zones[0].net_name, "A");
        assert_eq!(copper.zones[0].net, 1);
    }

    #[test]
    fn footprints_and_pads_go_by_reference_and_number_and_what_points_at_them_follows() {
        let (pcb, copper) = parse_board(BOARD).unwrap();
        let parts: Vec<&str> = pcb.footprints.iter().map(|f| f.reference.as_str()).collect();
        assert_eq!(parts, ["R1", "R2"]);
        let pads: Vec<&str> = pcb.footprints[1].pads.iter().map(|p| p.number.as_str()).collect();
        assert_eq!(pads, ["1", "2"]);
        assert_eq!(pcb.footprints[1].pads[0].net.as_ref().unwrap().number, 2, "pad 1 of R2 is on B");
        for pad in &copper.pads {
            let model = &pcb.footprints[pad.footprint].pads[pad.pad];
            let [x, _] = pcb.footprints[pad.footprint].at;
            assert!((model.position[0] - x).abs() < 2e-3, "a pad's copper stays with its pad");
        }
        assert_eq!(copper.graphics[0].footprint, Some(1), "R2's drawn copper");
        assert_eq!(copper.footprint_at[0].map(|s| &BOARD[s.start..s.end]), Some("(at 9 9)"));
    }

    #[test]
    fn items_keep_the_spans_of_their_own_text() {
        let (_, copper) = parse_board(BOARD).unwrap();
        for t in &copper.tracks {
            let text = &BOARD[t.span.start..t.span.end];
            assert!(text.contains(if t.net == 1 { "(start 1 1)" } else { "(start 3 3)" }), "{text}");
        }
    }
}
