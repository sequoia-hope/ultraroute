//! Copper and solder-mask text outlines against KiCad 9's `PCB_TEXT::TransformTextToPolySet`,
//! read by `kicad-pcb` with `kicad-text` shaping them.
//!
//! `tests/fixtures/text_v9.kicad_pcb` holds multi-line, rotated, mirrored, justified, italic,
//! overbarred, accented/Greek, footprint-placed and knocked-out texts saved by pcbnew 9;
//! `tests/golden/text_v9.json` holds the polygons pcbnew 9 builds from them.

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Deserialize)]
struct GoldenPoly {
    outer: Vec<[f64; 2]>,
    holes: Vec<Vec<[f64; 2]>>,
}

#[derive(Deserialize)]
struct GoldenText {
    owner: String,
    layer: String,
    text: String,
    knockout: bool,
    bbox_m: [f64; 4],
    polygons: Vec<GoldenPoly>,
}

fn dir(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn point_in_ring(p: [f64; 2], ring: &[[f64; 2]]) -> bool {
    let mut odd = false;
    for i in 0..ring.len() {
        let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
        if (a[1] > p[1]) != (b[1] > p[1])
            && p[0] < a[0] + (p[1] - a[1]) * (b[0] - a[0]) / (b[1] - a[1])
        {
            odd = !odd;
        }
    }
    odd
}

fn dist_to_segment(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2
    }
    .clamp(0.0, 1.0);
    (p[0] - (a[0] + t * dx)).hypot(p[1] - (a[1] + t * dy))
}

fn dist_to_ring(p: [f64; 2], ring: &[[f64; 2]]) -> f64 {
    (0..ring.len())
        .map(|i| dist_to_segment(p, ring[i], ring[(i + 1) % ring.len()]))
        .fold(f64::INFINITY, f64::min)
}

fn in_ours(p: [f64; 2], polys: &[Vec<[f64; 2]>]) -> bool {
    polys.iter().any(|ring| point_in_ring(p, ring))
}

fn dist_to_ours(p: [f64; 2], polys: &[Vec<[f64; 2]>]) -> f64 {
    polys.iter().map(|ring| dist_to_ring(p, ring)).fold(f64::INFINITY, f64::min)
}

fn in_kicad(p: [f64; 2], polys: &[GoldenPoly]) -> bool {
    polys
        .iter()
        .any(|g| point_in_ring(p, &g.outer) && !g.holes.iter().any(|h| point_in_ring(p, h)))
}

fn dist_to_kicad(p: [f64; 2], polys: &[GoldenPoly]) -> f64 {
    polys
        .iter()
        .flat_map(|g| std::iter::once(&g.outer).chain(g.holes.iter()))
        .map(|ring| dist_to_ring(p, ring))
        .fold(f64::INFINITY, f64::min)
}

fn bbox_of(polys: &[Vec<[f64; 2]>]) -> [f64; 4] {
    let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
    let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for p in polys.iter().flatten() {
        x0 = x0.min(p[0]);
        y0 = y0.min(p[1]);
        x1 = x1.max(p[0]);
        y1 = y1.max(p[1]);
    }
    [x0, y0, x1, y1]
}

#[test]
fn copper_and_mask_text_polygons_match_kicad_stroke_outlines() {
    let pcb_text = std::fs::read_to_string(dir("tests/fixtures/text_v9.kicad_pcb")).unwrap();
    let golden_text = std::fs::read_to_string(dir("tests/golden/text_v9.json")).unwrap();
    let golden: Vec<GoldenText> = serde_json::from_str(&golden_text).unwrap();

    // Through the MIT reader, which runs this crate's program for the strokes.
    let helper = std::path::Path::new(env!("CARGO_BIN_EXE_kicad-text"));
    let (pcb, copper) = kicad_pcb::parse_board_with(&pcb_text, kicad_pcb::text::Shaper::Helper(helper, kicad_pcb::text::HELPER_TIMEOUT)).unwrap();
    assert!(copper.warnings.is_empty(), "{:?}", copper.warnings);
    assert_eq!(copper.text_note, Some(format!("8 text item(s) stroked by {}", helper.display())));

    // Board graphics come first in file order, then footprint graphics, with mask items split into
    // `mask_graphics`. Match each golden entry by (owner, layer, bbox centre).
    let mut all: Vec<(&str, &kicad_pcb::copper::CopperGraphic)> = Vec::new();
    for g in copper.graphics.iter().chain(copper.mask_graphics.iter()) {
        let owner = g.footprint.map_or("board", |i| pcb.footprints[i].reference.as_str());
        all.push((owner, g));
    }
    assert_eq!(all.len(), golden.len(), "hidden reference property must stay out");

    for ref_item in &golden {
        let label = format!("{} {:?} on {}", ref_item.owner, ref_item.text, ref_item.layer);
        let (_, ours) = all
            .iter()
            .find(|(owner, g)| {
                let bb = bbox_of(&g.polygons);
                *owner == ref_item.owner
                    && g.layer == ref_item.layer
                    && ((bb[0] + bb[2]) - (ref_item.bbox_m[0] + ref_item.bbox_m[2])).abs() < 1e-3
                    && ((bb[1] + bb[3]) - (ref_item.bbox_m[1] + ref_item.bbox_m[3])).abs() < 1e-3
            })
            .unwrap_or_else(|| panic!("no parsed graphic matching {label}"));

        let bb = bbox_of(&ours.polygons);
        for k in 0..4 {
            let err = (bb[k] - ref_item.bbox_m[k]).abs();
            assert!(err < 5e-6, "{label}: bbox[{k}] differs by {:.2} um ({bb:?} vs {:?})", err * 1e6, ref_item.bbox_m);
        }

        if ref_item.knockout {
            // Knockout text is modelled as one solid rectangle matching KiCad's outer box
            // (KiCad's polygon set also emits separate contours for enclosed glyph counters).
            assert_eq!(ours.polygons.len(), 1, "{label}: knockout must be a single box");
            assert_eq!(ours.polygons[0].len(), 4, "{label}: knockout box has four corners");
            let outer_box = ref_item
                .polygons
                .iter()
                .find(|p| p.outer.len() == 4)
                .unwrap_or_else(|| panic!("{label}: expected a four-corner outer knockout box"));
            for &pt in &outer_box.outer {
                let d = dist_to_ours(pt, &ours.polygons);
                assert!(d < 5e-6, "{label}: KiCad knockout corner {pt:?} is {:.2} um off our box", d * 1e6);
            }
            for &pt in &ours.polygons[0] {
                let d = dist_to_ring(pt, &outer_box.outer);
                assert!(d < 5e-6, "{label}: our knockout corner {pt:?} is {:.2} um off KiCad's", d * 1e6);
            }
            continue;
        }

        // 1. Every KiCad stroke contour vertex (outer and inner hole) lies inside or within 10 um
        //    of our capsule union.
        for gp in &ref_item.polygons {
            for &pt in gp.outer.iter().chain(gp.holes.iter().flatten()) {
                if !in_ours(pt, &ours.polygons) {
                    let d = dist_to_ours(pt, &ours.polygons);
                    assert!(d < 10e-6, "{label}: KiCad vertex {pt:?} outside ours by {:.2} um", d * 1e6);
                }
            }
        }

        // 2. Every vertex of our capsules lies inside KiCad's stroked shape or within 10 um of its
        //    boundary.
        for &pt in ours.polygons.iter().flatten() {
            if !in_kicad(pt, &ref_item.polygons) {
                let d = dist_to_kicad(pt, &ref_item.polygons);
                assert!(d < 10e-6, "{label}: our vertex {pt:?} outside KiCad's by {:.2} um", d * 1e6);
            }
        }

        // 3. Sample across the bounding box (0.15 mm grid) so empty regions beside shorter lines or
        //    between rotated strokes are verified clear in both shapes.
        let step = 0.15e-3;
        let nx = ((bb[2] - bb[0]) / step).ceil() as usize;
        let ny = ((bb[3] - bb[1]) / step).ceil() as usize;
        let mut clear_samples = 0usize;
        for ix in 0..=nx {
            for iy in 0..=ny {
                let p = [bb[0] + ix as f64 * step, bb[1] + iy as f64 * step];
                let (a, b) = (in_ours(p, &ours.polygons), in_kicad(p, &ref_item.polygons));
                if !a && !b {
                    clear_samples += 1;
                } else if a && !b {
                    let d = dist_to_kicad(p, &ref_item.polygons);
                    assert!(d < 10e-6, "{label}: sample {p:?} inside ours but {:.2} um from KiCad", d * 1e6);
                } else if b && !a {
                    let d = dist_to_ours(p, &ours.polygons);
                    assert!(d < 10e-6, "{label}: sample {p:?} inside KiCad but {:.2} um from ours", d * 1e6);
                }
            }
        }
        assert!(clear_samples > 0, "{label}: strokes must leave empty space inside the text block");
    }
}
