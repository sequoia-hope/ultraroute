//! An obstacle's outline as a polygon, and the distances a wire needs from it. A pour's outline
//! has thousands of edges and covers most of the board, so a long outline keeps its edges in an
//! index by place: a question about one spot reads only the edges near it.

use std::collections::HashMap;

use board::{Nm, Point, Shape};

use crate::funnel::Pt;

/// Side of an edge index cell, in nanometres.
const CELL: f64 = 250_000.0;
/// Outlines with fewer edges than this are read whole.
const INDEXED: usize = 48;
/// How far a polygon standing in for a round end may stand outside it. The polygon is drawn
/// round the circle, never inside it, so distances to it are never longer than the true ones.
const ROUND_ERROR: f64 = 2_000.0;

#[derive(Clone)]
pub struct Outline {
    pub points: Vec<Pt>,
    /// An outline edge of the board is a line with no inside; everything else is solid.
    pub solid: bool,
    index: Option<EdgeIndex>,
}

/// Edge numbers by the cells they pass through, and by the rows of cells they span, which is
/// what a ray cast along a row needs.
#[derive(Clone)]
struct EdgeIndex {
    cells: HashMap<(i64, i64), Vec<u32>>,
    rows: HashMap<i64, Vec<u32>>,
}

fn cell(v: f64) -> i64 {
    (v / CELL).floor() as i64
}

pub fn pt(p: Point) -> Pt {
    Pt::new(p.x as f64, p.y as f64)
}

/// The nanometre point nearest `p`.
pub fn round(p: Pt) -> Point {
    Point::new(p.x.round() as Nm, p.y.round() as Nm)
}

impl Outline {
    pub fn new(points: Vec<Pt>, solid: bool) -> Outline {
        let mut outline = Outline { points, solid, index: None };
        if outline.edge_count() >= INDEXED {
            let mut index = EdgeIndex { cells: HashMap::new(), rows: HashMap::new() };
            for k in 0..outline.edge_count() {
                let (a, b) = outline.edge(k);
                let (x0, x1) = (cell(a.x.min(b.x)), cell(a.x.max(b.x)));
                let (y0, y1) = (cell(a.y.min(b.y)), cell(a.y.max(b.y)));
                for y in y0..=y1 {
                    index.rows.entry(y).or_default().push(k as u32);
                    for x in x0..=x1 {
                        index.cells.entry((x, y)).or_default().push(k as u32);
                    }
                }
            }
            outline.index = Some(index);
        }
        outline
    }

    /// A shape as a polygon that contains it: a capsule's round ends are drawn as polygons round
    /// their circles, within `ROUND_ERROR` of them.
    pub fn of(shape: &Shape) -> Outline {
        let points = match shape {
            Shape::Polygon(p) => p.iter().copied().map(pt).collect(),
            Shape::Capsule { a, b, r } if *r <= 0 => {
                if a == b {
                    vec![pt(*a)]
                } else {
                    vec![pt(*a), pt(*b)]
                }
            }
            Shape::Capsule { a, b, r } => {
                let r = *r as f64;
                let half_step = (r / (r + ROUND_ERROR)).acos();
                let cap = |centre: Point, from: f64, sweep: f64| -> Vec<Pt> {
                    let steps = (sweep / (2.0 * half_step)).ceil().max(4.0) as usize;
                    let outer = r / (sweep / steps as f64 / 2.0).cos();
                    (0..=steps)
                        .map(|k| {
                            let angle = from + sweep * k as f64 / steps as f64;
                            Pt::new(centre.x as f64 + outer * angle.cos(), centre.y as f64 + outer * angle.sin())
                        })
                        .collect()
                };
                if a == b {
                    let mut ring = cap(*a, 0.0, std::f64::consts::TAU);
                    ring.pop();
                    ring
                } else {
                    let along = ((b.y - a.y) as f64).atan2((b.x - a.x) as f64);
                    let half = std::f64::consts::FRAC_PI_2;
                    let mut ring = cap(*b, along - half, std::f64::consts::PI);
                    ring.extend(cap(*a, along + half, std::f64::consts::PI));
                    ring
                }
            }
        };
        Outline::new(points, true)
    }

    /// An open outline's last point does not join its first.
    pub fn edge_count(&self) -> usize {
        if self.solid || self.points.len() < 2 {
            self.points.len()
        } else {
            self.points.len() - 1
        }
    }

    pub fn edge(&self, k: usize) -> (Pt, Pt) {
        (self.points[k], self.points[(k + 1) % self.points.len()])
    }

    pub fn extent(&self) -> (Pt, Pt) {
        let mut lo = self.points[0];
        let mut hi = self.points[0];
        for p in &self.points {
            lo = Pt::new(lo.x.min(p.x), lo.y.min(p.y));
            hi = Pt::new(hi.x.max(p.x), hi.y.max(p.y));
        }
        (lo, hi)
    }

    pub fn centroid(&self) -> Pt {
        let n = self.points.len() as f64;
        Pt::new(self.points.iter().map(|p| p.x).sum::<f64>() / n, self.points.iter().map(|p| p.y).sum::<f64>() / n)
    }

    /// The edges that may come within the box `lo`–`hi`, each once.
    fn edges_near(&self, lo: Pt, hi: Pt) -> Vec<u32> {
        let Some(index) = &self.index else { return (0..self.edge_count() as u32).collect() };
        let mut out = Vec::new();
        for y in cell(lo.y)..=cell(hi.y) {
            for x in cell(lo.x)..=cell(hi.x) {
                if let Some(found) = index.cells.get(&(x, y)) {
                    out.extend(found);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Even-odd, by a ray cast along the row of `p`; never inside an open outline.
    pub fn inside(&self, p: Pt) -> bool {
        if !self.solid {
            return false;
        }
        let all: Vec<u32>;
        let edges: &[u32] = match &self.index {
            Some(index) => index.rows.get(&cell(p.y)).map_or(&[], Vec::as_slice),
            None => {
                all = (0..self.edge_count() as u32).collect();
                &all
            }
        };
        let mut odd = false;
        for &k in edges {
            let (a, b) = self.edge(k as usize);
            if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
                odd = !odd;
            }
        }
        odd
    }

    /// The distance from `p`, when it is at most `limit`; otherwise some distance beyond it.
    pub fn distance_within(&self, p: Pt, limit: f64) -> f64 {
        if self.inside(p) {
            return 0.0;
        }
        let reach = Pt::new(limit, limit);
        let lo = Pt::new(p.x - reach.x, p.y - reach.y);
        let hi = Pt::new(p.x + reach.x, p.y + reach.y);
        if self.points.len() == 1 {
            return p.distance(self.points[0]);
        }
        self.edges_near(lo, hi).into_iter().map(|k| self.edge(k as usize)).map(|(a, b)| foot(a, b, p).distance(p)).fold(f64::INFINITY, f64::min)
    }

    /// The distance from segment `a`-`b` (0 when they meet), with the nearest point on each,
    /// when it is at most `limit`; otherwise some distance beyond it.
    pub fn segment_within(&self, a: Pt, b: Pt, limit: f64) -> (f64, Pt, Pt) {
        if self.inside(a) {
            return (0.0, a, a);
        }
        if self.points.len() == 1 {
            let f = foot(a, b, self.points[0]);
            return (f.distance(self.points[0]), f, self.points[0]);
        }
        let lo = Pt::new(a.x.min(b.x) - limit, a.y.min(b.y) - limit);
        let hi = Pt::new(a.x.max(b.x) + limit, a.y.max(b.y) + limit);
        let mut best = (f64::INFINITY, a, a);
        for k in self.edges_near(lo, hi) {
            let (c, d) = self.edge(k as usize);
            if let Some(x) = crossing(a, b, c, d) {
                return (0.0, x, x);
            }
            for (p, s0, s1, p_on_ab) in [(a, c, d, false), (b, c, d, false), (c, a, b, true), (d, a, b, true)] {
                let f = foot(s0, s1, p);
                let dist = f.distance(p);
                if dist < best.0 {
                    best = if p_on_ab { (dist, f, p) } else { (dist, p, f) };
                }
            }
        }
        best
    }

    /// Points along the outline, its corners among them, no further apart than `spacing`.
    pub fn samples(&self, spacing: f64) -> Vec<Pt> {
        let mut out = Vec::new();
        for k in 0..self.edge_count() {
            let (a, b) = self.edge(k);
            let n = (a.distance(b) / spacing).ceil().max(1.0) as usize;
            for i in 0..n {
                let t = i as f64 / n as f64;
                out.push(Pt::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t));
            }
        }
        if !self.solid {
            out.extend(self.points.last());
        }
        out
    }
}

fn foot(a: Pt, b: Pt, p: Pt) -> Pt {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 { 0.0 } else { (((p.x - a.x) * dx + (p.y - a.y) * dy) / len2).clamp(0.0, 1.0) };
    Pt::new(a.x + t * dx, a.y + t * dy)
}

fn crossing(a: Pt, b: Pt, c: Pt, d: Pt) -> Option<Pt> {
    let r = (b.x - a.x, b.y - a.y);
    let s = (d.x - c.x, d.y - c.y);
    let denom = r.0 * s.1 - r.1 * s.0;
    if denom == 0.0 {
        return None;
    }
    let t = ((c.x - a.x) * s.1 - (c.y - a.y) * s.0) / denom;
    let u = ((c.x - a.x) * r.1 - (c.y - a.y) * r.0) / denom;
    ((0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u)).then(|| Pt::new(a.x + t * r.0, a.y + t * r.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> Pt {
        Pt::new(x, y)
    }

    fn square(half: f64) -> Outline {
        Outline::new(vec![p(-half, -half), p(half, -half), p(half, half), p(-half, half)], true)
    }

    /// A square of `n` edges a side, big enough to be indexed.
    fn fine_square(half: f64, n: usize) -> Outline {
        let corners = [p(-half, -half), p(half, -half), p(half, half), p(-half, half)];
        let mut points = Vec::new();
        for k in 0..4 {
            let (a, b) = (corners[k], corners[(k + 1) % 4]);
            for i in 0..n {
                let t = i as f64 / n as f64;
                points.push(p(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t));
            }
        }
        Outline::new(points, true)
    }

    #[test]
    fn a_round_outline_holds_its_circle_within_the_error() {
        let r = 300_000;
        let ring = Outline::of(&Shape::Capsule { a: Point::new(0, 0), b: Point::new(0, 0), r });
        assert!(ring.edge_count() >= 8);
        for k in 0..ring.edge_count() {
            let (a, b) = ring.edge(k);
            let d = foot(a, b, p(0.0, 0.0)).distance(p(0.0, 0.0));
            assert!(d >= r as f64 - 1.0 && d <= r as f64 + ROUND_ERROR, "edge {k} at {d}");
        }
    }

    #[test]
    fn a_capsule_outline_holds_its_whole_length() {
        let ring = Outline::of(&Shape::Capsule { a: Point::new(0, 0), b: Point::new(1_000_000, 0), r: 100_000 });
        for x in [0.0, 500_000.0, 1_000_000.0, 1_090_000.0, -90_000.0] {
            assert!(ring.inside(p(x, 0.0)), "{x}");
        }
        assert!(ring.inside(p(500_000.0, 99_000.0)));
        assert!(!ring.inside(p(500_000.0, 110_000.0)));
        assert!(!ring.inside(p(1_110_000.0, 0.0)));
    }

    #[test]
    fn samples_are_close_enough_and_keep_the_corners() {
        let s = square(1000.0).samples(300.0);
        for c in &square(1000.0).points {
            assert!(s.contains(c));
        }
        for k in 0..s.len() {
            assert!(s[k].distance(s[(k + 1) % s.len()]) <= 300.0 + 1e-9);
        }
        let open = Outline::new(vec![p(0.0, 0.0), p(1000.0, 0.0)], false).samples(300.0);
        assert_eq!((open.first(), open.last()), (Some(&p(0.0, 0.0)), Some(&p(1000.0, 0.0))));
        assert_eq!(open.len(), 5);
    }

    #[test]
    fn a_segment_measures_to_a_polygon_and_to_a_line() {
        let (d, on_seg, on_poly) = square(1.0).segment_within(p(-5.0, 3.0), p(5.0, 3.0), 10.0);
        assert_eq!(d, 2.0);
        assert_eq!(on_seg.y, 3.0);
        assert_eq!(on_poly.y, 1.0);
        assert_eq!(square(1.0).segment_within(p(-5.0, 0.0), p(5.0, 0.0), 10.0).0, 0.0);
        // Inside a solid polygon is no distance; beside an open line it is.
        assert_eq!(square(1.0).distance_within(p(0.0, 0.0), 10.0), 0.0);
        assert_eq!(Outline::new(vec![p(-1.0, -1.0), p(1.0, -1.0)], false).distance_within(p(0.0, 0.0), 10.0), 1.0);
    }

    #[test]
    fn an_indexed_outline_answers_as_a_plain_one_would() {
        let half = 2_000_000.0;
        let fine = fine_square(half, 40);
        assert!(fine.index.is_some());
        let plain = Outline { index: None, ..fine.clone() };
        let spots = [p(0.0, 0.0), p(1_999_000.0, 5.0), p(2_100_000.0, 0.0), p(-2_300_000.0, 1_000_000.0), p(123_456.0, -1_999_999.0)];
        for s in spots {
            assert_eq!(fine.inside(s), plain.inside(s), "{s:?}");
            for limit in [50_000.0, 400_000.0] {
                let (a, b) = (fine.distance_within(s, limit), plain.distance_within(s, limit));
                if b <= limit {
                    assert_eq!(a, b, "{s:?} {limit}");
                } else {
                    assert!(a > limit, "{s:?} {limit}");
                }
            }
        }
        let (a, b) = (p(-3_000_000.0, 2_200_000.0), p(3_000_000.0, 2_200_000.0));
        assert_eq!(fine.segment_within(a, b, 300_000.0).0, plain.segment_within(a, b, 300_000.0).0);
        assert!(fine.segment_within(a, b, 100_000.0).0 > 100_000.0);
    }
}
