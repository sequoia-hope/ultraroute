//! Edge.Cuts primitives → closed loops (spec §3 rows O1–O6).
//!
//! Endpoints are welded at [`OUTLINE_WELD_M`] and nothing wider: a gap is
//! reported with its size, never closed (P9/P10). Every welded endpoint
//! must have exactly two segments; one is an open outline, three or more
//! is a branch. Loops are then nested by containment: exactly one
//! outermost loop is the board, loops directly inside it are cutouts, and
//! anything deeper (an island inside a hole) is refused.

use serde::{Deserialize, Serialize};

use crate::model::{OutlinePrimitive, OutlineShape};

/// Endpoint weld distance: the project's `TAU_MODEL` (1e-7 m). KiCad
/// writes coordinates to 1e-6 mm = 1e-9 m, so file precision is well
/// inside it; a drawn gap is not.
pub const OUTLINE_WELD_M: f64 = 1e-7;

#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize, Deserialize)]
pub enum OutlineError {
    #[error("no Edge.Cuts primitives")]
    Empty,
    #[error("outline not closed: gap of {gap_m} m at ({}, {})", at[0], at[1])]
    NotClosed { gap_m: f64, at: [f64; 2] },
    #[error("outline branches at ({}, {}): {degree} segments meet", at[0], at[1])]
    Branching { at: [f64; 2], degree: usize },
    #[error("{count} separate board outlines (a panel?)")]
    MultipleBoardOutlines { count: usize },
    #[error("outline loop nested {depth} deep (an island inside a cutout)")]
    Nested { depth: usize },
}

/// One traversed segment of a loop, in walk order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Segment {
    Line {
        start: [f64; 2],
        end: [f64; 2],
    },
    Arc {
        start: [f64; 2],
        end: [f64; 2],
        center: [f64; 2],
        radius: f64,
        /// Sweep from `start` to `end` goes counter-clockwise in the
        /// coordinate sense of the frame the points are in (angle
        /// increasing). In the file's Y-down frame that is clockwise on
        /// screen; the Y flip of the derive step reverses it.
        ccw: bool,
    },
    Circle {
        center: [f64; 2],
        radius: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Loop {
    pub segments: Vec<Segment>,
    /// Shoelace area of the loop with circular-segment corrections, in
    /// the coordinate sense of the input frame (positive = angle-increasing
    /// traversal). A lone circle is positive.
    pub signed_area_m2: f64,
}

impl Loop {
    pub fn area_m2(&self) -> f64 {
        self.signed_area_m2.abs()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutlineLoops {
    pub outer: Loop,
    pub holes: Vec<Loop>,
}

impl OutlineLoops {
    /// Board area: outer minus every cutout.
    pub fn net_area_m2(&self) -> f64 {
        self.outer.area_m2() - self.holes.iter().map(Loop::area_m2).sum::<f64>()
    }
}

/// Circumcentre of three points, or `None` when they are collinear (the
/// triangle's doubled area is below `1e-12` of the squared span).
pub fn circumcenter(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> Option<[f64; 2]> {
    let bx = b[0] - a[0];
    let by = b[1] - a[1];
    let cx = c[0] - a[0];
    let cy = c[1] - a[1];
    let d = 2.0 * (bx * cy - by * cx);
    let span2 = (bx * bx + by * by).max(cx * cx + cy * cy);
    if span2 == 0.0 || d.abs() <= 1e-12 * span2 {
        return None;
    }
    let b2 = bx * bx + by * by;
    let c2 = cx * cx + cy * cy;
    let ux = (cy * b2 - by * c2) / d;
    let uy = (bx * c2 - cx * b2) / d;
    Some([a[0] + ux, a[1] + uy])
}

/// Chain the primitives into loops.
pub fn chain(prims: &[OutlinePrimitive]) -> Result<OutlineLoops, OutlineError> {
    if prims.is_empty() {
        return Err(OutlineError::Empty);
    }
    // Directed open segments with their endpoints, plus standalone circles.
    struct Open {
        seg: Segment,
        a: [f64; 2],
        b: [f64; 2],
    }
    let mut opens: Vec<Open> = Vec::new();
    let mut loops: Vec<Loop> = Vec::new();
    for p in prims {
        match &p.shape {
            OutlineShape::Line { start, end } => opens.push(Open {
                seg: Segment::Line {
                    start: *start,
                    end: *end,
                },
                a: *start,
                b: *end,
            }),
            OutlineShape::Arc { start, mid, end } => {
                let center = circumcenter(*start, *mid, *end)
                    .expect("reader converts collinear arcs to lines");
                let radius = dist(center, *start);
                let ccw = arc_is_ccw(center, *start, *mid, *end);
                opens.push(Open {
                    seg: Segment::Arc {
                        start: *start,
                        end: *end,
                        center,
                        radius,
                        ccw,
                    },
                    a: *start,
                    b: *end,
                });
            }
            OutlineShape::Circle { center, radius } => loops.push(Loop {
                segments: vec![Segment::Circle {
                    center: *center,
                    radius: *radius,
                }],
                signed_area_m2: std::f64::consts::PI * radius * radius,
            }),
        }
    }

    // Weld endpoints: node id per distinct point within OUTLINE_WELD_M.
    let mut nodes: Vec<[f64; 2]> = Vec::new();
    let mut node_of = |p: [f64; 2]| -> usize {
        if let Some(i) = nodes.iter().position(|q| dist(*q, p) <= OUTLINE_WELD_M) {
            i
        } else {
            nodes.push(p);
            nodes.len() - 1
        }
    };
    let ends: Vec<(usize, usize)> = opens.iter().map(|o| (node_of(o.a), node_of(o.b))).collect();
    let mut degree = vec![0usize; nodes.len()];
    let mut incident: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (i, (a, b)) in ends.iter().enumerate() {
        degree[*a] += 1;
        degree[*b] += 1;
        incident[*a].push(i);
        incident[*b].push(i);
    }
    for (i, d) in degree.iter().enumerate() {
        if *d == 1 {
            let at = nodes[i];
            let gap = nodes
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, q)| dist(*q, at))
                .fold(f64::INFINITY, f64::min);
            return Err(OutlineError::NotClosed { gap_m: gap, at });
        }
        if *d > 2 {
            return Err(OutlineError::Branching {
                at: nodes[i],
                degree: *d,
            });
        }
    }

    // Walk loops.
    let mut used = vec![false; opens.len()];
    for start in 0..opens.len() {
        if used[start] {
            continue;
        }
        let mut segs = Vec::new();
        let mut cur = start;
        let mut cur_from = ends[start].0;
        loop {
            used[cur] = true;
            let (a, b) = ends[cur];
            let forward = a == cur_from;
            let seg = if forward {
                opens[cur].seg.clone()
            } else {
                reverse(&opens[cur].seg)
            };
            segs.push(seg);
            let next_node = if forward { b } else { a };
            let next = incident[next_node].iter().copied().find(|s| !used[*s]);
            match next {
                Some(s) => {
                    cur = s;
                    cur_from = next_node;
                }
                None => break,
            }
        }
        let signed_area_m2 = signed_area(&segs);
        loops.push(Loop {
            segments: segs,
            signed_area_m2,
        });
    }

    // Nesting by containment of a representative point.
    let polys: Vec<Vec<[f64; 2]>> = loops.iter().map(sample).collect();
    let mut depth = vec![0usize; loops.len()];
    for (i, l) in loops.iter().enumerate() {
        let probe = probe_point(l);
        for (j, poly) in polys.iter().enumerate() {
            if i != j && point_in_polygon(probe, poly) {
                depth[i] += 1;
            }
        }
    }
    let outers: Vec<usize> = (0..loops.len()).filter(|i| depth[*i] == 0).collect();
    if outers.len() != 1 {
        return Err(OutlineError::MultipleBoardOutlines {
            count: outers.len(),
        });
    }
    if let Some(d) = depth.iter().copied().find(|d| *d >= 2) {
        return Err(OutlineError::Nested { depth: d });
    }
    let outer_idx = outers[0];
    let mut holes = Vec::new();
    let mut outer = None;
    for (i, l) in loops.into_iter().enumerate() {
        if i == outer_idx {
            outer = Some(l);
        } else {
            holes.push(l);
        }
    }
    Ok(OutlineLoops {
        outer: outer.expect("one outer loop"),
        holes,
    })
}

fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

fn angle(center: [f64; 2], p: [f64; 2]) -> f64 {
    (p[1] - center[1]).atan2(p[0] - center[0])
}

/// The arc visits `mid` between `start` and `end`; it is counter-clockwise
/// (angle-increasing) iff the ccw angular distance to `mid` is shorter
/// than the ccw distance to `end`.
pub fn arc_is_ccw(center: [f64; 2], start: [f64; 2], mid: [f64; 2], end: [f64; 2]) -> bool {
    let tau = std::f64::consts::TAU;
    let a0 = angle(center, start);
    let dm = (angle(center, mid) - a0).rem_euclid(tau);
    let de = (angle(center, end) - a0).rem_euclid(tau);
    dm < de
}

fn reverse(s: &Segment) -> Segment {
    match s {
        Segment::Line { start, end } => Segment::Line {
            start: *end,
            end: *start,
        },
        Segment::Arc {
            start,
            end,
            center,
            radius,
            ccw,
        } => Segment::Arc {
            start: *end,
            end: *start,
            center: *center,
            radius: *radius,
            ccw: !ccw,
        },
        Segment::Circle { center, radius } => Segment::Circle {
            center: *center,
            radius: *radius,
        },
    }
}

/// Sweep angle of an arc in (0, 2π).
fn sweep(center: [f64; 2], start: [f64; 2], end: [f64; 2], ccw: bool) -> f64 {
    let tau = std::f64::consts::TAU;
    let d = (angle(center, end) - angle(center, start)).rem_euclid(tau);
    let d = if ccw { d } else { (tau - d).rem_euclid(tau) };
    if d == 0.0 {
        tau
    } else {
        d
    }
}

/// Shoelace over the chord polygon plus, per arc, the circular segment
/// `r²/2·(θ − sin θ)` signed by its sweep direction.
fn signed_area(segs: &[Segment]) -> f64 {
    let mut shoelace = 0.0;
    let mut bulge = 0.0;
    for s in segs {
        match s {
            Segment::Line { start, end } => {
                shoelace += start[0] * end[1] - end[0] * start[1];
            }
            Segment::Arc {
                start,
                end,
                center,
                radius,
                ccw,
            } => {
                shoelace += start[0] * end[1] - end[0] * start[1];
                let th = sweep(*center, *start, *end, *ccw);
                let seg = 0.5 * radius * radius * (th - th.sin());
                bulge += if *ccw { seg } else { -seg };
            }
            Segment::Circle { radius, .. } => {
                return std::f64::consts::PI * radius * radius;
            }
        }
    }
    0.5 * shoelace + bulge
}

/// Polyline sample of a loop for containment tests (32 steps per arc).
fn sample(l: &Loop) -> Vec<[f64; 2]> {
    let mut pts = Vec::new();
    for s in &l.segments {
        match s {
            Segment::Line { start, .. } => pts.push(*start),
            Segment::Arc {
                start,
                end,
                center,
                radius,
                ccw,
            } => {
                let a0 = angle(*center, *start);
                let th = sweep(*center, *start, *end, *ccw);
                let n = 32;
                for k in 0..n {
                    let t = a0 + if *ccw { 1.0 } else { -1.0 } * th * (k as f64 / n as f64);
                    pts.push([center[0] + radius * t.cos(), center[1] + radius * t.sin()]);
                }
            }
            Segment::Circle { center, radius } => {
                let n = 64;
                for k in 0..n {
                    let t = std::f64::consts::TAU * (k as f64 / n as f64);
                    pts.push([center[0] + radius * t.cos(), center[1] + radius * t.sin()]);
                }
            }
        }
    }
    pts
}

/// A point of the loop's boundary, nudged nowhere: containment of a
/// boundary point in ANOTHER loop is what nesting asks.
fn probe_point(l: &Loop) -> [f64; 2] {
    match &l.segments[0] {
        Segment::Line { start, .. } | Segment::Arc { start, .. } => *start,
        Segment::Circle { center, radius } => [center[0] + radius, center[1]],
    }
}

fn point_in_polygon(p: [f64; 2], poly: &[[f64; 2]]) -> bool {
    let mut inside = false;
    let n = poly.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (poly[i][0], poly[i][1]);
        let (xj, yj) = (poly[j][0], poly[j][1]);
        if (yi > p[1]) != (yj > p[1]) && p[0] < (xj - xi) * (p[1] - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(a: [f64; 2], b: [f64; 2]) -> OutlinePrimitive {
        OutlinePrimitive {
            shape: OutlineShape::Line { start: a, end: b },
            footprint: None,
        }
    }

    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<OutlinePrimitive> {
        vec![
            line([x0, y0], [x1, y0]),
            line([x1, y0], [x1, y1]),
            line([x1, y1], [x0, y1]),
            line([x0, y1], [x0, y0]),
        ]
    }

    #[test]
    fn circumcenter_of_a_right_isoceles_is_the_hypotenuse_midpoint() {
        let c = circumcenter([0.0, 0.0], [2.0, 0.0], [0.0, 2.0]).unwrap();
        assert!((c[0] - 1.0).abs() < 1e-15 && (c[1] - 1.0).abs() < 1e-15);
        assert!(circumcenter([0.0, 0.0], [1.0, 1.0], [2.0, 2.0]).is_none());
    }

    #[test]
    fn rectangle_is_one_loop_with_exact_area() {
        let l = chain(&rect(0.0, 0.0, 0.05, 0.03)).unwrap();
        assert_eq!(l.outer.segments.len(), 4);
        assert!(l.holes.is_empty());
        assert!((l.outer.area_m2() - 1.5e-3).abs() < 1e-18);
    }

    #[test]
    fn reversed_and_shuffled_segments_still_chain() {
        let mut prims = rect(0.0, 0.0, 0.05, 0.03);
        prims.swap(1, 3);
        if let OutlineShape::Line { start, end } = &mut prims[2].shape {
            std::mem::swap(start, end);
        }
        let l = chain(&prims).unwrap();
        assert!((l.outer.area_m2() - 1.5e-3).abs() < 1e-18);
    }

    #[test]
    fn a_gap_is_reported_with_its_size_not_closed() {
        let mut prims = rect(0.0, 0.0, 0.05, 0.03);
        prims[3].shape = OutlineShape::Line {
            start: [0.0, 0.03],
            end: [0.0, 1e-5],
        };
        match chain(&prims).unwrap_err() {
            OutlineError::NotClosed { gap_m, at } => {
                assert!((gap_m - 1e-5).abs() < 1e-15, "{gap_m}");
                assert!(at == [0.0, 1e-5] || at == [0.0, 0.0]);
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn a_gap_inside_the_weld_is_welded() {
        let mut prims = rect(0.0, 0.0, 0.05, 0.03);
        prims[3].shape = OutlineShape::Line {
            start: [0.0, 0.03],
            end: [0.0, 5e-8],
        };
        assert!(chain(&prims).is_ok());
    }

    #[test]
    fn holes_nest_under_the_outer_loop() {
        let mut prims = rect(0.0, 0.0, 0.05, 0.03);
        prims.extend(rect(0.01, 0.01, 0.02, 0.02));
        prims.push(OutlinePrimitive {
            shape: OutlineShape::Circle {
                center: [0.04, 0.015],
                radius: 0.002,
            },
            footprint: None,
        });
        let l = chain(&prims).unwrap();
        assert_eq!(l.holes.len(), 2);
        let expect = 1.5e-3 - 1e-4 - std::f64::consts::PI * 4e-6;
        assert!((l.net_area_m2() - expect).abs() < 1e-15);
    }

    #[test]
    fn two_disjoint_loops_are_a_panel() {
        let mut prims = rect(0.0, 0.0, 0.02, 0.01);
        prims.extend(rect(0.03, 0.0, 0.05, 0.01));
        assert_eq!(
            chain(&prims).unwrap_err(),
            OutlineError::MultipleBoardOutlines { count: 2 }
        );
    }

    #[test]
    fn an_island_inside_a_hole_is_refused() {
        let mut prims = rect(0.0, 0.0, 0.05, 0.05);
        prims.extend(rect(0.01, 0.01, 0.04, 0.04));
        prims.extend(rect(0.02, 0.02, 0.03, 0.03));
        assert_eq!(
            chain(&prims).unwrap_err(),
            OutlineError::Nested { depth: 2 }
        );
    }

    #[test]
    fn a_branching_vertex_is_refused() {
        let mut prims = rect(0.0, 0.0, 0.05, 0.03);
        prims.push(line([0.0, 0.0], [0.01, 0.01]));
        prims.push(line([0.01, 0.01], [0.05, 0.0]));
        assert!(matches!(
            chain(&prims).unwrap_err(),
            OutlineError::Branching { degree: 3, .. }
        ));
    }

    #[test]
    fn semicircle_area_is_half_a_disc() {
        // Diameter along X from (0,0) to (2,0), bulging to +Y (mid (1,1)).
        let prims = vec![
            OutlinePrimitive {
                shape: OutlineShape::Arc {
                    start: [0.0, 0.0],
                    mid: [1.0, 1.0],
                    end: [2.0, 0.0],
                },
                footprint: None,
            },
            line([2.0, 0.0], [0.0, 0.0]),
        ];
        let l = chain(&prims).unwrap();
        assert!((l.outer.area_m2() - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        // Start (0,0) → mid (1,1) → end (2,0) about (1,0): angle goes
        // π → π/2 → 0, decreasing ⇒ not ccw.
        assert!(matches!(
            l.outer.segments[0],
            Segment::Arc { ccw: false, .. }
        ));
    }
}
