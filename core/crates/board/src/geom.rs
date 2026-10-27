//! Shapes in integer nanometres and the distances between them. Coordinates are exact; distances
//! are computed in `f64`, which holds every nanometre on a board exactly.

pub type Nm = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Point {
    pub x: Nm,
    pub y: Nm,
}

impl Point {
    pub const fn new(x: Nm, y: Nm) -> Point {
        Point { x, y }
    }

    pub fn from_m(p: [f64; 2]) -> Point {
        Point { x: nm(p[0]), y: nm(p[1]) }
    }

    pub fn distance(self, o: Point) -> f64 {
        ((self.x - o.x) as f64).hypot((self.y - o.y) as f64)
    }
}

pub fn nm(metres: f64) -> Nm {
    (metres * 1e9).round() as Nm
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BBox {
    pub min: Point,
    pub max: Point,
}

impl BBox {
    pub fn of(points: impl IntoIterator<Item = Point>) -> BBox {
        let mut b = BBox { min: Point::new(Nm::MAX, Nm::MAX), max: Point::new(Nm::MIN, Nm::MIN) };
        for p in points {
            b.min = Point::new(b.min.x.min(p.x), b.min.y.min(p.y));
            b.max = Point::new(b.max.x.max(p.x), b.max.y.max(p.y));
        }
        b
    }

    pub fn grow(self, by: Nm) -> BBox {
        BBox { min: Point::new(self.min.x - by, self.min.y - by), max: Point::new(self.max.x + by, self.max.y + by) }
    }

    pub fn overlaps(self, o: BBox) -> bool {
        self.min.x <= o.max.x && o.min.x <= self.max.x && self.min.y <= o.max.y && o.min.y <= self.max.y
    }

    pub fn contains(self, p: Point) -> bool {
        p.x >= self.min.x && p.x <= self.max.x && p.y >= self.min.y && p.y <= self.max.y
    }

    pub fn union(self, o: BBox) -> BBox {
        BBox {
            min: Point::new(self.min.x.min(o.min.x), self.min.y.min(o.min.y)),
            max: Point::new(self.max.x.max(o.max.x), self.max.y.max(o.max.y)),
        }
    }
}

/// A shape on one copper layer.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// Every point within `r` of the segment `a`–`b`: tracks, round pads, vias (`a == b`).
    Capsule { a: Point, b: Point, r: Nm },
    /// A filled polygon, closed implicitly. May be concave.
    Polygon(Vec<Point>),
}

impl Shape {
    pub fn bbox(&self) -> BBox {
        match self {
            Shape::Capsule { a, b, r } => BBox::of([*a, *b]).grow(*r),
            Shape::Polygon(p) => BBox::of(p.iter().copied()),
        }
    }

    pub fn translated(&self, by: Point) -> Shape {
        let m = |p: &Point| Point::new(p.x + by.x, p.y + by.y);
        match self {
            Shape::Capsule { a, b, r } => Shape::Capsule { a: m(a), b: m(b), r: *r },
            Shape::Polygon(p) => Shape::Polygon(p.iter().map(m).collect()),
        }
    }

    /// The gap between two shapes; 0 when they touch or overlap.
    pub fn gap(&self, o: &Shape) -> f64 {
        match (self, o) {
            (Shape::Capsule { a, b, r }, Shape::Capsule { a: c, b: d, r: s }) => {
                (segment_segment(*a, *b, *c, *d) - (*r + *s) as f64).max(0.0)
            }
            (Shape::Capsule { a, b, r }, Shape::Polygon(p)) | (Shape::Polygon(p), Shape::Capsule { a, b, r }) => {
                (segment_polygon(*a, *b, p) - *r as f64).max(0.0)
            }
            (Shape::Polygon(p), Shape::Polygon(q)) => polygon_polygon(p, q),
        }
    }

    /// The gap when it is at most `limit`; otherwise some value above `limit`. Polygon edges
    /// too far away to matter are skipped, which is what makes large pours cheap.
    pub fn gap_within(&self, o: &Shape, limit: f64) -> f64 {
        let reach = limit.ceil() as Nm + 1;
        match (self, o) {
            (Shape::Capsule { .. }, Shape::Capsule { .. }) => self.gap(o),
            (Shape::Capsule { a, b, r }, Shape::Polygon(p)) | (Shape::Polygon(p), Shape::Capsule { a, b, r }) => {
                if p.is_empty() {
                    return f64::INFINITY;
                }
                let zone = BBox::of([*a, *b]).grow(*r + reach);
                if !BBox::of(p.iter().copied()).overlaps(zone) {
                    return limit + 1.0;
                }
                if inside(*a, p) || (a != b && inside(*b, p)) {
                    return 0.0;
                }
                let d = edges(p)
                    .filter(|(c, e)| BBox::of([*c, *e]).overlaps(zone))
                    .map(|(c, e)| segment_segment(*a, *b, c, e))
                    .fold(f64::INFINITY, f64::min);
                (d - *r as f64).max(0.0)
            }
            (Shape::Polygon(p), Shape::Polygon(q)) => {
                if p.is_empty() || q.is_empty() {
                    return f64::INFINITY;
                }
                let (bp, bq) = (BBox::of(p.iter().copied()).grow(reach), BBox::of(q.iter().copied()).grow(reach));
                if !bp.overlaps(bq) {
                    return limit + 1.0;
                }
                if inside(p[0], q) || inside(q[0], p) {
                    return 0.0;
                }
                let near_p: Vec<(Point, Point, BBox)> =
                    edges(p).map(|(c, e)| (c, e, BBox::of([c, e]).grow(reach))).filter(|(_, _, b)| b.overlaps(bq)).collect();
                let near_q: Vec<(Point, Point, BBox)> =
                    edges(q).map(|(c, e)| (c, e, BBox::of([c, e]))).filter(|(_, _, b)| b.overlaps(bp)).collect();
                let mut best = limit + 1.0;
                for (a, b, ba) in &near_p {
                    for (c, d, bc) in &near_q {
                        if ba.overlaps(*bc) {
                            best = best.min(segment_segment(*a, *b, *c, *d));
                            if best == 0.0 {
                                return 0.0;
                            }
                        }
                    }
                }
                best
            }
        }
    }

    pub fn contains(&self, pt: Point) -> bool {
        match self {
            Shape::Capsule { a, b, r } => point_segment(pt, *a, *b) <= *r as f64,
            Shape::Polygon(p) => BBox::of(p.iter().copied()).contains(pt) && inside(pt, p),
        }
    }

    /// How far inside the shape a point lies, to its nearest edge; 0 outside.
    pub fn depth(&self, pt: Point) -> f64 {
        match self {
            Shape::Capsule { a, b, r } => (*r as f64 - point_segment(pt, *a, *b)).max(0.0),
            Shape::Polygon(p) if self.contains(pt) => edges(p).map(|(c, d)| point_segment(pt, c, d)).fold(f64::INFINITY, f64::min),
            Shape::Polygon(_) => 0.0,
        }
    }
}

pub fn point_segment(p: Point, a: Point, b: Point) -> f64 {
    let (dx, dy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
    let (px, py) = ((p.x - a.x) as f64, (p.y - a.y) as f64);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 { 0.0 } else { ((px * dx + py * dy) / len2).clamp(0.0, 1.0) };
    (px - t * dx).hypot(py - t * dy)
}

fn cross(o: Point, a: Point, b: Point) -> i128 {
    (a.x - o.x) as i128 * (b.y - o.y) as i128 - (a.y - o.y) as i128 * (b.x - o.x) as i128
}

/// Whether the closed segments `a`–`b` and `c`–`d` share a point; exact in integers.
pub fn segments_cross(a: Point, b: Point, c: Point, d: Point) -> bool {
    let (d1, d2, d3, d4) = (cross(c, d, a), cross(c, d, b), cross(a, b, c), cross(a, b, d));
    if ((d1 > 0 && d2 < 0) || (d1 < 0 && d2 > 0)) && ((d3 > 0 && d4 < 0) || (d3 < 0 && d4 > 0)) {
        return true;
    }
    let on = |p: Point, q: Point, r: Point| {
        cross(p, q, r) == 0 && r.x >= p.x.min(q.x) && r.x <= p.x.max(q.x) && r.y >= p.y.min(q.y) && r.y <= p.y.max(q.y)
    };
    on(c, d, a) || on(c, d, b) || on(a, b, c) || on(a, b, d)
}

pub fn segment_segment(a: Point, b: Point, c: Point, d: Point) -> f64 {
    if segments_cross(a, b, c, d) {
        return 0.0;
    }
    point_segment(a, c, d).min(point_segment(b, c, d)).min(point_segment(c, a, b)).min(point_segment(d, a, b))
}

/// Even-odd point in polygon; points on the boundary may land either side.
pub fn inside(p: Point, poly: &[Point]) -> bool {
    let mut odd = false;
    let n = poly.len();
    for i in 0..n {
        let (a, b) = (poly[i], poly[(i + 1) % n]);
        if (a.y > p.y) != (b.y > p.y) {
            let x = a.x as f64 + (p.y - a.y) as f64 * (b.x - a.x) as f64 / (b.y - a.y) as f64;
            if (p.x as f64) < x {
                odd = !odd;
            }
        }
    }
    odd
}

fn edges(poly: &[Point]) -> impl Iterator<Item = (Point, Point)> + '_ {
    (0..poly.len()).map(move |i| (poly[i], poly[(i + 1) % poly.len()]))
}

pub fn segment_polygon(a: Point, b: Point, poly: &[Point]) -> f64 {
    if poly.is_empty() {
        return f64::INFINITY;
    }
    if inside(a, poly) || (a != b && inside(b, poly)) {
        return 0.0;
    }
    edges(poly).map(|(c, d)| segment_segment(a, b, c, d)).fold(f64::INFINITY, f64::min)
}

pub fn polygon_polygon(p: &[Point], q: &[Point]) -> f64 {
    if p.is_empty() || q.is_empty() {
        return f64::INFINITY;
    }
    if inside(p[0], q) || inside(q[0], p) {
        return 0.0;
    }
    let mut best = f64::INFINITY;
    for (a, b) in edges(p) {
        for (c, d) in edges(q) {
            best = best.min(segment_segment(a, b, c, d));
            if best == 0.0 {
                return 0.0;
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn p(x: Nm, y: Nm) -> Point {
        Point::new(x, y)
    }

    fn square(x: Nm, y: Nm, half: Nm) -> Vec<Point> {
        vec![p(x - half, y - half), p(x + half, y - half), p(x + half, y + half), p(x - half, y + half)]
    }

    #[test]
    fn parallel_capsules_keep_the_gap_between_their_edges() {
        let a = Shape::Capsule { a: p(0, 0), b: p(1000, 0), r: 100 };
        let b = Shape::Capsule { a: p(0, 500), b: p(1000, 500), r: 100 };
        assert_eq!(a.gap(&b), 300.0);
    }

    #[test]
    fn crossing_and_overlapping_shapes_have_no_gap() {
        let a = Shape::Capsule { a: p(0, 0), b: p(1000, 1000), r: 1 };
        let b = Shape::Capsule { a: p(0, 1000), b: p(1000, 0), r: 1 };
        assert_eq!(a.gap(&b), 0.0);
        let sq = Shape::Polygon(square(0, 0, 500));
        let inner = Shape::Capsule { a: p(-10, 0), b: p(10, 0), r: 1 };
        assert_eq!(sq.gap(&inner), 0.0);
        assert_eq!(inner.gap(&sq), 0.0);
    }

    #[test]
    fn a_capsule_beside_a_polygon_measures_to_its_nearest_edge() {
        let sq = Shape::Polygon(square(0, 0, 500));
        let via = Shape::Capsule { a: p(900, 0), b: p(900, 0), r: 100 };
        assert_eq!(sq.gap(&via), 300.0);
    }

    #[test]
    fn polygons_apart_and_nested() {
        let a = Shape::Polygon(square(0, 0, 500));
        let b = Shape::Polygon(square(2000, 0, 500));
        assert_eq!(a.gap(&b), 1000.0);
        let c = Shape::Polygon(square(0, 0, 100));
        assert_eq!(a.gap(&c), 0.0);
    }

    #[test]
    fn a_bounded_gap_agrees_with_the_exact_one_within_its_limit() {
        let shapes = [
            Shape::Polygon(square(0, 0, 500)),
            Shape::Polygon(square(1700, 300, 400)),
            Shape::Capsule { a: p(-2000, 900), b: p(2000, 900), r: 100 },
            Shape::Capsule { a: p(700, 0), b: p(700, 0), r: 150 },
            Shape::Polygon(square(0, 0, 50)),
        ];
        for x in &shapes {
            for y in &shapes {
                let exact = x.gap(y);
                for limit in [0.0, 100.0, 1000.0, 5000.0] {
                    let bounded = x.gap_within(y, limit);
                    if exact <= limit {
                        assert_eq!(bounded, exact, "{x:?} {y:?} {limit}");
                    } else {
                        assert!(bounded > limit, "{x:?} {y:?} {limit}: {bounded} vs {exact}");
                    }
                }
            }
        }
    }

    #[test]
    fn collinear_touching_segments_cross() {
        assert!(segments_cross(p(0, 0), p(10, 0), p(10, 0), p(20, 0)));
        assert!(!segments_cross(p(0, 0), p(10, 0), p(11, 0), p(20, 0)));
    }
}
