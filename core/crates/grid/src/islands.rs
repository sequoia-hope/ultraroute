//! Strips a zone keeps as laid copper ([`board::Board::laid_islands`]) stitched at their ends: a
//! via at each end that lies over copper of the strip's net on another layer, as the reference
//! layouts such strips come from have, so the strip carries its current along its length and
//! not only joins its net somewhere (easy2350's 1V1 spokes, from the ring under the RP2350 to its
//! DVDD pins). The vias are laid before routing, at the designer's spots, and locked: no later
//! stage moves or prunes them.

use board::{check_item, Board, Item, ItemId, Kind, Nm, Point, Shape};

use crate::grid::Profile;
use crate::report::{tell, Log};

/// Spacing of the points sampled across an island to find its middle line.
const STEP: Nm = 25_000;
/// How far in from its end along the strip a stitch may stand.
const END_REACH: Nm = 1_000_000;
/// Most points of the middle line kept for finding its two ends.
const SPINE_MAX: usize = 2_000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct IslandStats {
    /// Ends of laid strips over copper of their net on another layer, and how many now hold a via.
    pub ends: usize,
    pub stitched: usize,
    pub laid: Vec<ItemId>,
}

/// The island's middle line: the sampled points no neighbour of which lies deeper in from its
/// edge by half a step, the ridge of its depth, which runs down the middle of a strip whatever
/// its width does along it (easy2350's pin-6 spoke widens at its pin end).
fn spine(shape: &Shape) -> Vec<Point> {
    let b = shape.bbox();
    let (w, h) = (((b.max.x - b.min.x) / STEP + 1) as usize, ((b.max.y - b.min.y) / STEP + 1) as usize);
    let at = |i: usize, j: usize| Point::new(b.min.x + STEP / 2 + i as Nm * STEP, b.min.y + STEP / 2 + j as Nm * STEP);
    let depth: Vec<f64> = (0..w * h)
        .map(|k| {
            let p = at(k % w, k / w);
            if shape.contains(p) {
                shape.depth(p)
            } else {
                -1.0
            }
        })
        .collect();
    let mut out = Vec::new();
    for j in 0..h {
        for i in 0..w {
            let d = depth[j * w + i];
            if d <= 0.0 {
                continue;
            }
            let ridge = (-1..=1i64).all(|dj| {
                (-1..=1i64).all(|di| {
                    let (x, y) = (i as i64 + di, j as i64 + dj);
                    x < 0 || y < 0 || x >= w as i64 || y >= h as i64 || depth[y as usize * w + x as usize] < d + STEP as f64 / 2.0
                })
            });
            if ridge {
                out.push(at(i, j));
            }
        }
    }
    if out.len() > SPINE_MAX {
        let every = out.len().div_ceil(SPINE_MAX);
        out = out.into_iter().step_by(every).collect();
    }
    out
}

/// The two points of `spine` furthest apart.
fn ends(spine: &[Point]) -> Option<(Point, Point)> {
    let mut best: Option<(f64, Point, Point)> = None;
    for (i, &p) in spine.iter().enumerate() {
        for &q in &spine[i + 1..] {
            let d = p.distance(q);
            if best.is_none_or(|b| d > b.0) {
                best = Some((d, p, q));
            }
        }
    }
    best.map(|b| (b.1, b.2)).or_else(|| spine.first().map(|&p| (p, p)))
}

/// The copper of `net` on layers other than `layer` that holds `p`, so a via there joins it. A
/// plane is left out: it lies under every strip of its net, and joins them by the vias the
/// router drops to it wherever they are needed.
fn own_copper_under(board: &Board, island: ItemId, net: u32, layer: usize, p: Point) -> Vec<ItemId> {
    let at = board::BBox::of([p]);
    (0..board.layers.len())
        .filter(|&l| l != layer)
        .flat_map(|l| board.near(l, at).into_iter().filter(move |&o| o != island && board.item(o).is_some_and(|it| it.net == net && it.is_copper() && it.on(l) && !board.is_plane(it) && it.shape.contains(p))))
        .collect()
}

/// Whether a via of the island's net already stands on it within `reach` of `end`, or joins it to
/// one of `under` that is a laid strip too: one via joins two strips, however many of their ends
/// overlap (a stub over a spoke shares the spoke's via). A pour under both of a strip's ends
/// still takes a via at each, so the strip carries its current end to end.
fn stitched(board: &Board, island: &Item, layer: usize, end: Point, reach: Nm, under: &[ItemId]) -> bool {
    let via_at = |v: &Item| match v.shape {
        Shape::Capsule { a, .. } if v.kind == Kind::Via && v.net == island.net => Some(a),
        _ => None,
    };
    let near_end = board.near(layer, board::BBox::of([end]).grow(reach)).into_iter().filter_map(|v| board.item(v)).filter_map(via_at).any(|a| a.distance(end) <= reach as f64 && island.shape.contains(a));
    near_end
        || under.iter().filter(|&&o| board.must_join(o)).filter_map(|&o| board.item(o)).any(|o| {
            let b = o.shape.bbox();
            (0..board.layers.len()).any(|l| board.near(l, b).into_iter().filter_map(|v| board.item(v)).filter_map(via_at).any(|a| island.shape.contains(a) && o.shape.contains(a)))
        })
}

/// Lays a locked via at each end of every laid strip that lies over copper of its net on
/// another layer, nearest the end that lays cleanly.
pub fn stitch_islands(board: &mut Board, log: &Option<Log>) -> IslandStats {
    let mut stats = IslandStats::default();
    // The longest strips first: a spoke's ends are the reference's via spots, which the shorter
    // stubs over them then share.
    let mut islands: Vec<(f64, ItemId, Vec<Point>, Point, Point)> = board
        .laid_islands
        .iter()
        .filter_map(|&id| {
            let line = spine(&board.item(id)?.shape);
            let (e1, e2) = ends(&line)?;
            Some((e1.distance(e2), id, line, e1, e2))
        })
        .collect();
    islands.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, id, line, e1, e2) in islands {
        let Some(island) = board.item(id).cloned() else { continue };
        let layer = island.layers.trailing_zeros() as usize;
        // A short strip's two ends share one stitch.
        let reach = END_REACH.min((e1.distance(e2) / 2.0) as Nm);
        let profile = Profile::for_net(board, island.net);
        for end in [e1, e2] {
            let mut sites: Vec<(Point, Vec<ItemId>)> =
                line.iter().copied().filter(|&p| p.distance(end) <= reach as f64).map(|p| (p, own_copper_under(board, id, island.net, layer, p))).filter(|s| !s.1.is_empty()).collect();
            sites.sort_by(|p, q| p.0.distance(end).total_cmp(&q.0.distance(end)));
            if sites.is_empty() {
                continue;
            }
            stats.ends += 1;
            let mut under: Vec<ItemId> = sites.iter().flat_map(|s| s.1.iter().copied()).collect();
            under.sort_unstable();
            under.dedup();
            if stitched(board, &island, layer, end, reach, &under) {
                stats.stitched += 1;
                continue;
            }
            for (p, _) in sites {
                let via = board.insert(Item {
                    kind: Kind::Via,
                    net: island.net,
                    layers: (1u64 << board.layers.len()) - 1,
                    shape: Shape::Capsule { a: p, b: p, r: profile.via_radius },
                    hole: Some((p, profile.drill_radius)),
                    locked: true,
                    span: None,
                });
                if check_item(board, via).is_empty() {
                    tell(log, "islands", || format!("{} stitched at ({:.3}, {:.3}) mm", board.nets.get(island.net as usize).map_or("", |n| n.as_str()), p.x as f64 * 1e-6, p.y as f64 * 1e-6), None);
                    stats.stitched += 1;
                    stats.laid.push(via);
                    break;
                }
                board.discard(via);
            }
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 0.6 mm strip of A on B.Cu from x 10 to 14 mm, which a custom rule keeps other nets off,
    /// so laid: a pour of A over its left end on F.Cu (the ring), and a laid stub of A on F.Cu
    /// from x 13 to 16 over its right end, which reaches back over the strip's body.
    #[test]
    fn a_laid_strip_gets_a_via_at_each_end_over_its_nets_copper_and_no_more() {
        let zone = |layer: &str, x0: f64, x1: f64, keep: bool| {
            let mode = if keep { "(island_removal_mode 1)" } else { "" };
            let name = match (layer, keep) {
                ("B.Cu", _) => "strip",
                (_, true) => "stub",
                _ => "ring",
            };
            format!(
                "(zone (net 1) (net_name \"A\") (layer \"{layer}\") (name \"{name}\") (hatch edge 0.5) (connect_pads (clearance 0.2)) (min_thickness 0.2)
                   (fill yes (thermal_gap 0.5) (thermal_bridge_width 0.5) {mode})
                   (polygon (pts (xy {x0} 9.7) (xy {x1} 9.7) (xy {x1} 10.3) (xy {x0} 10.3)))
                   (filled_polygon (layer \"{layer}\") (pts (xy {x0} 9.7) (xy {x1} 9.7) (xy {x1} 10.3) (xy {x0} 10.3))))"
            )
        };
        let text = format!(
            "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal)) (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
             (gr_rect (start 0 0) (end 30 20) (layer \"Edge.Cuts\") (width 0.1)) {} {} {}
             (footprint \"R\" (layer \"F.Cu\") (at 25 15) (property \"Reference\" \"R1\") (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net 2 \"B\"))))",
            zone("B.Cu", 10.0, 14.0, true),
            zone("F.Cu", 7.0, 10.8, false),
            zone("F.Cu", 13.0, 16.0, true)
        );
        let (pcb, copper) = kicad_pcb::parse_board(&text).unwrap();
        // The strips are kept other nets' tracks and vias off by name, their own net's vias let in.
        let mut rules = kicad_pcb::Rules::parse("{}").unwrap();
        rules.apply_custom("(version 1)\n(rule \"keep the strips\" (condition \"(A.intersectsArea('strip') || A.intersectsArea('stub')) && A.NetName != 'A'\") (constraint disallow track via))", "test.kicad_dru");
        let mut board = Board::new(&pcb, &copper, rules);
        assert_eq!(board.laid_islands.len(), 2);
        let stats = stitch_islands(&mut board, &None);
        let at: Vec<Point> = stats.laid.iter().filter_map(|&v| match board.item(v)?.shape {
            Shape::Capsule { a, .. } => Some(a),
            _ => None,
        }).collect();
        assert_eq!(at.len(), 2, "one via at each of the strip's ends, the stub's own sharing one: {at:?}");
        assert!(at.iter().any(|p| p.x < 10_800_000) && at.iter().any(|p| p.x > 13_000_000), "{at:?}");
        assert!(at.iter().all(|p| (p.y - 10_000_000).abs() <= STEP), "on the strip's middle line: {at:?}");
        assert!(stats.laid.iter().all(|&v| board.item(v).unwrap().locked));
    }
}
