//! A harmonic displacement field with rigid inclusions on a regular grid, after the rubber and
//! pcbmorph warp fields: inside an inclusion the field is that inclusion's vector, elsewhere it
//! is harmonic, and it falls to zero on the border of a domain reaching past the board. Copper
//! carried by it flows smoothly around moving parts and stays put against still ones.

use board::{BBox, Point, Shape};

const UNOWNED: i32 = -1;
const CG_TOLERANCE: f64 = 1e-6;
const CG_MAX_ITERATIONS: usize = 4000;

pub struct WarpField {
    x0: f64,
    y0: f64,
    h: f64,
    nx: usize,
    ny: usize,
    owner: Vec<i32>,
    vectors: Vec<(f64, f64)>,
    pending: Vec<(Vec<Shape>, bool)>,
    u: Vec<(f64, f64)>,
}

impl WarpField {
    /// A field over `bounds` and `margin` beyond, in cells `h` across, all in nanometres.
    pub fn new(bounds: BBox, h: f64, margin: f64) -> WarpField {
        let x0 = bounds.min.x as f64 - margin;
        let y0 = bounds.min.y as f64 - margin;
        let nx = ((bounds.max.x as f64 + margin - x0) / h).ceil() as usize + 1;
        let ny = ((bounds.max.y as f64 + margin - y0) / h).ceil() as usize + 1;
        WarpField { x0, y0, h, nx: nx.max(3), ny: ny.max(3), owner: vec![UNOWNED; nx.max(3) * ny.max(3)], vectors: Vec::new(), pending: Vec::new(), u: vec![(0.0, 0.0); nx.max(3) * ny.max(3)] }
    }

    /// Adds an inclusion moving by `vector`: the cells under `shapes` and a one-cell halo, and
    /// with `with_hull` the space their convex hull encloses too. Every inclusion's own shapes
    /// claim cells before any hull does, so a part's hull never takes the pads of a part sitting
    /// inside it; among equals, earlier inclusions win.
    pub fn add_inclusion(&mut self, shapes: &[Shape], vector: (f64, f64), with_hull: bool) {
        self.vectors.push(vector);
        self.pending.push((shapes.to_vec(), with_hull));
    }

    fn claim_territory(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        for (index, (shapes, _)) in pending.iter().enumerate() {
            let mut cells = Vec::new();
            for shape in shapes {
                self.cells_inside(shape, 1, &mut cells);
            }
            self.claim(index, cells);
        }
        for (index, (shapes, with_hull)) in pending.iter().enumerate() {
            if !with_hull || shapes.len() < 2 {
                continue;
            }
            let hull = convex_hull(shapes.iter().flat_map(corners).collect());
            if hull.len() < 3 {
                continue;
            }
            let (lo, hi) = self.window(bounding(&hull), 0);
            let mut cells = Vec::new();
            for j in lo.1..=hi.1 {
                for i in lo.0..=hi.0 {
                    if point_in_convex(&hull, self.cell_center(i, j)) {
                        cells.push(j * self.nx + i);
                    }
                }
            }
            self.claim(index, cells);
        }
    }

    fn claim(&mut self, index: usize, cells: Vec<usize>) {
        let index = i32::try_from(index).expect("fewer inclusions than i32::MAX");
        for cell in cells {
            if self.owner[cell] == UNOWNED {
                self.owner[cell] = index;
            }
        }
    }

    /// Solves for the harmonic field: owned cells hold their inclusion's vector, the domain border
    /// holds zero, everything else is the discrete Laplace solution, by conjugate gradients on the
    /// reduced, symmetric positive definite system.
    pub fn solve(&mut self) {
        self.claim_territory();
        for (cell, owner) in self.owner.iter().enumerate() {
            if *owner >= 0 {
                self.u[cell] = self.vectors[*owner as usize];
            }
        }
        for component in 0..2 {
            self.solve_component(component);
        }
    }

    /// The displacement at `p`, bilinear between the four surrounding cell centres.
    pub fn displacement(&self, p: Point) -> (f64, f64) {
        let fx = ((p.x as f64 - self.x0) / self.h).clamp(0.0, (self.nx - 1) as f64);
        let fy = ((p.y as f64 - self.y0) / self.h).clamp(0.0, (self.ny - 1) as f64);
        let (i, j) = ((fx.floor() as usize).min(self.nx - 2), (fy.floor() as usize).min(self.ny - 2));
        let (tx, ty) = (fx - i as f64, fy - j as f64);
        let at = |i: usize, j: usize| self.u[j * self.nx + i];
        let (a, b, c, d) = (at(i, j), at(i + 1, j), at(i, j + 1), at(i + 1, j + 1));
        let lerp = |p: f64, q: f64, r: f64, s: f64| p * (1.0 - tx) * (1.0 - ty) + q * tx * (1.0 - ty) + r * (1.0 - tx) * ty + s * tx * ty;
        (lerp(a.0, b.0, c.0, d.0), lerp(a.1, b.1, c.1, d.1))
    }

    fn free(&self, i: usize, j: usize) -> bool {
        i > 0 && j > 0 && i + 1 < self.nx && j + 1 < self.ny && self.owner[j * self.nx + i] < 0
    }

    fn solve_component(&mut self, component: usize) {
        let (nx, ny) = (self.nx, self.ny);
        let value = |u: &(f64, f64)| if component == 0 { u.0 } else { u.1 };
        let free: Vec<usize> = (0..nx * ny).filter(|&c| self.free(c % nx, c / nx)).collect();
        if free.is_empty() {
            return;
        }
        let mut slot = vec![usize::MAX; nx * ny];
        for (k, &c) in free.iter().enumerate() {
            slot[c] = k;
        }
        let neighbours = |c: usize| [c - 1, c + 1, c - nx, c + nx];
        // A x = b with A = 4I - (free neighbours), b = the sum of fixed neighbours' values.
        let mut b = vec![0.0; free.len()];
        for (k, &c) in free.iter().enumerate() {
            for n in neighbours(c) {
                if slot[n] == usize::MAX {
                    b[k] += value(&self.u[n]);
                }
            }
        }
        let apply = |x: &[f64], out: &mut [f64]| {
            for (k, &c) in free.iter().enumerate() {
                let mut sum = 4.0 * x[k];
                for n in neighbours(c) {
                    if slot[n] != usize::MAX {
                        sum -= x[slot[n]];
                    }
                }
                out[k] = sum;
            }
        };
        let mut x = vec![0.0; free.len()];
        let mut r = b.clone();
        let mut p = r.clone();
        let mut ap = vec![0.0; free.len()];
        let mut rr: f64 = r.iter().map(|v| v * v).sum();
        let target = CG_TOLERANCE * CG_TOLERANCE * b.iter().map(|v| v * v).sum::<f64>().max(1e-30);
        for _ in 0..CG_MAX_ITERATIONS {
            if rr <= target {
                break;
            }
            apply(&p, &mut ap);
            let alpha = rr / p.iter().zip(&ap).map(|(a, b)| a * b).sum::<f64>();
            for k in 0..x.len() {
                x[k] += alpha * p[k];
                r[k] -= alpha * ap[k];
            }
            let next: f64 = r.iter().map(|v| v * v).sum();
            let beta = next / rr;
            rr = next;
            for k in 0..p.len() {
                p[k] = r[k] + beta * p[k];
            }
        }
        for (k, &c) in free.iter().enumerate() {
            if component == 0 {
                self.u[c].0 = x[k];
            } else {
                self.u[c].1 = x[k];
            }
        }
    }

    fn cell_center(&self, i: usize, j: usize) -> (f64, f64) {
        (self.x0 + i as f64 * self.h, self.y0 + j as f64 * self.h)
    }

    fn window(&self, (x0, y0, x1, y1): (f64, f64, f64, f64), halo: usize) -> ((usize, usize), (usize, usize)) {
        let cx = |v: f64| ((v - self.x0) / self.h).clamp(0.0, (self.nx - 1) as f64);
        let cy = |v: f64| ((v - self.y0) / self.h).clamp(0.0, (self.ny - 1) as f64);
        let lo = ((cx(x0).floor() as usize).saturating_sub(halo), (cy(y0).floor() as usize).saturating_sub(halo));
        let hi = ((cx(x1).ceil() as usize + halo).min(self.nx - 1), (cy(y1).ceil() as usize + halo).min(self.ny - 1));
        (lo, hi)
    }

    fn cells_inside(&self, shape: &Shape, halo: usize, out: &mut Vec<usize>) {
        let b = shape.bbox();
        let (lo, hi) = self.window((b.min.x as f64, b.min.y as f64, b.max.x as f64, b.max.y as f64), halo + 1);
        let reach = self.h * (halo as f64 + 0.5);
        for j in lo.1..=hi.1 {
            for i in lo.0..=hi.0 {
                let (x, y) = self.cell_center(i, j);
                let at = Shape::Capsule { a: Point::new(x.round() as i64, y.round() as i64), b: Point::new(x.round() as i64, y.round() as i64), r: 0 };
                if shape.gap(&at) <= reach {
                    out.push(j * self.nx + i);
                }
            }
        }
    }
}

/// A shape's outline points: a capsule by its ends pushed out by its radius, a polygon by its own.
fn corners(shape: &Shape) -> Vec<(f64, f64)> {
    match shape {
        Shape::Capsule { a, b, r } => {
            let r = *r as f64;
            [a, b].iter().flat_map(|p| [(p.x as f64 - r, p.y as f64 - r), (p.x as f64 + r, p.y as f64 - r), (p.x as f64 - r, p.y as f64 + r), (p.x as f64 + r, p.y as f64 + r)]).collect()
        }
        Shape::Polygon(p) => p.iter().map(|p| (p.x as f64, p.y as f64)).collect(),
    }
}

fn bounding(points: &[(f64, f64)]) -> (f64, f64, f64, f64) {
    points.iter().fold((f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY), |(a, b, c, d), p| (a.min(p.0), b.min(p.1), c.max(p.0), d.max(p.1)))
}

fn convex_hull(mut points: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    points.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    points.dedup();
    if points.len() < 3 {
        return points;
    }
    let cross = |o: (f64, f64), a: (f64, f64), b: (f64, f64)| (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0);
    let mut hull: Vec<(f64, f64)> = Vec::with_capacity(points.len() * 2);
    for pass in 0..2 {
        let start = hull.len();
        let ordered: Vec<(f64, f64)> = if pass == 0 { points.clone() } else { points.iter().rev().copied().collect() };
        for p in ordered {
            while hull.len() >= start + 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
                hull.pop();
            }
            hull.push(p);
        }
        hull.pop();
    }
    hull
}

fn point_in_convex(hull: &[(f64, f64)], p: (f64, f64)) -> bool {
    (0..hull.len()).all(|i| {
        let (a, b) = (hull[i], hull[(i + 1) % hull.len()]);
        (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0) >= 0.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: i64, y: i64, r: i64) -> Shape {
        Shape::Polygon(vec![Point::new(x - r, y - r), Point::new(x + r, y - r), Point::new(x + r, y + r), Point::new(x - r, y + r)])
    }

    #[test]
    fn inclusions_move_rigidly_and_the_field_fades_between_them() {
        let bounds = BBox { min: Point::new(0, 0), max: Point::new(1000, 1000) };
        let mut field = WarpField::new(bounds, 10.0, 100.0);
        field.add_inclusion(&[square(300, 500, 40)], (20.0, 0.0), false);
        field.add_inclusion(&[square(700, 500, 40)], (0.0, 0.0), false);
        field.solve();
        let at = |x: i64, y: i64| field.displacement(Point::new(x, y));
        assert!((at(300, 500).0 - 20.0).abs() < 1e-9, "a moving inclusion is rigid");
        assert!(at(700, 500).0.abs() < 1e-9, "a still inclusion stays");
        let mid = at(500, 500).0;
        assert!(mid > 1.0 && mid < 19.0, "the gap between them stretches smoothly: {mid}");
        assert!(at(380, 500).0 > at(600, 500).0, "monotone between them");
        assert!(at(-90, 500).0.abs() < 1.0, "fades to zero at the domain border");
    }

    #[test]
    fn a_hull_carries_the_space_between_a_parts_pads() {
        let bounds = BBox { min: Point::new(0, 0), max: Point::new(1000, 1000) };
        let mut field = WarpField::new(bounds, 10.0, 100.0);
        field.add_inclusion(&[square(400, 500, 20), square(600, 500, 20)], (0.0, 30.0), true);
        field.solve();
        assert!((field.displacement(Point::new(500, 500)).1 - 30.0).abs() < 1e-9);
    }
}
