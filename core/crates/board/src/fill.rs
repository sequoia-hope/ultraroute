//! Zone fills as KiCad would pour them after routing, on a fine raster: each zone's outline
//! inside the board, less the clearance around other nets' copper, holes, the board edge, rule
//! areas and higher-priority pours, opened to the zone's minimum width, with islands that touch
//! none of the zone's own copper taken out. It is for showing the board, not for checking it:
//! edges are good to a raster step, and pads join their pour solidly with no thermal spokes.

use kicad_pcb::{Copper, Zone};

use crate::geom::{inside, nm, point_segment, BBox, Nm, Point, Shape};
use crate::model::{Board, Kind};

/// Side of a raster cell: 25 µm, a sixth of the narrowest pour the boards here allow.
pub const STEP: Nm = 25_000;
/// A raster larger than this many cells a side is not filled, so a stray coordinate cannot ask
/// for gigabytes.
const SIDE_MAX: i64 = 20_000;

/// One zone's copper on one layer, as rectangles `[x0, y0, x1, y1]` in nanometres.
#[derive(Debug, Clone, PartialEq)]
pub struct Fill {
    pub zone: usize,
    pub net: u32,
    pub layer: usize,
    pub rects: Vec<[Nm; 4]>,
}

/// Cells on a grid shared by every raster, so two rasters line up cell for cell.
#[derive(Clone)]
struct Raster {
    /// The first cell's column and row on the shared grid.
    i0: i64,
    j0: i64,
    w: usize,
    h: usize,
    cells: Vec<bool>,
}

impl Raster {
    fn over(b: BBox) -> Option<Raster> {
        let (i0, j0) = (b.min.x.div_euclid(STEP), b.min.y.div_euclid(STEP));
        let (i1, j1) = (b.max.x.div_euclid(STEP) + 1, b.max.y.div_euclid(STEP) + 1);
        if i1 - i0 > SIDE_MAX || j1 - j0 > SIDE_MAX || i1 <= i0 || j1 <= j0 {
            return None;
        }
        let (w, h) = ((i1 - i0) as usize, (j1 - j0) as usize);
        Some(Raster { i0, j0, w, h, cells: vec![false; w * h] })
    }

    fn center(&self, i: usize, j: usize) -> Point {
        Point::new((self.i0 + i as i64) * STEP + STEP / 2, (self.j0 + j as i64) * STEP + STEP / 2)
    }

    fn get(&self, i: i64, j: i64) -> bool {
        i >= 0 && j >= 0 && (i as usize) < self.w && (j as usize) < self.h && self.cells[j as usize * self.w + i as usize]
    }

    /// The cells whose centres may lie in `b`, as column and row ranges.
    fn span(&self, b: BBox) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
        let clamp = |v: i64, n: usize| v.clamp(0, n as i64) as usize;
        let cols = clamp(b.min.x.div_euclid(STEP) - self.i0, self.w)..clamp(b.max.x.div_euclid(STEP) + 1 - self.i0, self.w);
        let rows = clamp(b.min.y.div_euclid(STEP) - self.j0, self.h)..clamp(b.max.y.div_euclid(STEP) + 1 - self.j0, self.h);
        (cols, rows)
    }

    /// Sets every cell whose centre is inside `polys`, taken together even-odd, to `value`.
    fn scan(&mut self, polys: &[&[Point]], value: bool) {
        let mut xs = Vec::new();
        for j in 0..self.h {
            let y = self.center(0, j).y;
            xs.clear();
            for poly in polys {
                for k in 0..poly.len() {
                    let (a, b) = (poly[k], poly[(k + 1) % poly.len()]);
                    if (a.y > y) != (b.y > y) {
                        xs.push(a.x as f64 + (y - a.y) as f64 * (b.x - a.x) as f64 / (b.y - a.y) as f64);
                    }
                }
            }
            xs.sort_by(f64::total_cmp);
            for pair in xs.chunks_exact(2) {
                let from = ((pair[0] - STEP as f64 / 2.0) / STEP as f64).ceil() as i64 - self.i0;
                let to = ((pair[1] - STEP as f64 / 2.0) / STEP as f64).ceil() as i64 - self.i0;
                for i in from.max(0)..to.min(self.w as i64) {
                    self.cells[j * self.w + i as usize] = value;
                }
            }
        }
    }

    /// Clears the cells within `b` whose centres `near` picks out.
    fn clear(&mut self, b: BBox, near: impl Fn(Point) -> bool) {
        let (cols, rows) = self.span(b);
        for j in rows {
            for i in cols.clone() {
                let k = j * self.w + i;
                if self.cells[k] && near(self.center(i, j)) {
                    self.cells[k] = false;
                }
            }
        }
    }

    /// Whether a cell differs from one of its four neighbours, the raster's edge counting as empty.
    fn edge(&self, i: usize, j: usize) -> bool {
        let v = self.cells[j * self.w + i];
        let (i, j) = (i as i64, j as i64);
        [(i - 1, j), (i + 1, j), (i, j - 1), (i, j + 1)].iter().any(|&(x, y)| self.get(x, y) != v)
    }

    /// Opens the copper by `r`: what a disc of radius `r` cannot reach by sliding inside it goes.
    fn open(&mut self, r: Nm) {
        let reach = r / STEP;
        if reach == 0 {
            return;
        }
        let disc: Vec<(i64, i64)> = (-reach..=reach)
            .flat_map(|di| (-reach..=reach).map(move |dj| (di, dj)))
            .filter(|(di, dj)| (di * di + dj * dj) * STEP * STEP <= r * r)
            .collect();
        let before = self.cells.clone();
        let mut eroded = self.cells.clone();
        for j in 0..self.h {
            for i in 0..self.w {
                if !before[j * self.w + i] && self.edge(i, j) {
                    for &(di, dj) in &disc {
                        let (x, y) = (i as i64 + di, j as i64 + dj);
                        if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
                            eroded[y as usize * self.w + x as usize] = false;
                        }
                    }
                }
            }
        }
        // Copper on the raster's own edge borders nothing filled beyond it.
        for j in 0..self.h {
            for i in 0..self.w {
                if before[j * self.w + i] && (i == 0 || j == 0 || i + 1 == self.w || j + 1 == self.h) {
                    for &(di, dj) in &disc {
                        let (x, y) = (i as i64 + di, j as i64 + dj);
                        if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
                            eroded[y as usize * self.w + x as usize] = false;
                        }
                    }
                }
            }
        }
        self.cells = eroded;
        let eroded = self.clone();
        // Growing back from the edge of what is left reaches everything growing from all of it would.
        for j in 0..self.h {
            for i in 0..self.w {
                if eroded.cells[j * self.w + i] && eroded.edge(i, j) {
                    for &(di, dj) in &disc {
                        let (x, y) = (i as i64 + di, j as i64 + dj);
                        if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
                            let k = y as usize * self.w + x as usize;
                            self.cells[k] |= before[k];
                        }
                    }
                }
            }
        }
    }

    /// Keeps only the copper joined, side by side, to a seed cell.
    fn keep_seeded(&mut self, seeds: &[usize]) {
        let mut kept = vec![false; self.cells.len()];
        let mut stack: Vec<usize> = seeds.iter().copied().filter(|&k| self.cells[k]).collect();
        while let Some(k) = stack.pop() {
            if kept[k] || !self.cells[k] {
                continue;
            }
            kept[k] = true;
            let (i, j) = (k % self.w, k / self.w);
            if i > 0 {
                stack.push(k - 1);
            }
            if i + 1 < self.w {
                stack.push(k + 1);
            }
            if j > 0 {
                stack.push(k - self.w);
            }
            if j + 1 < self.h {
                stack.push(k + self.w);
            }
        }
        self.cells = kept;
    }

    /// The copper as rectangles: runs along each row, runs alike in successive rows merged.
    fn rects(&self) -> Vec<[Nm; 4]> {
        let mut done = Vec::new();
        // Open rectangles by (first column, end column): the row they started on.
        let mut open: Vec<(usize, usize, usize)> = Vec::new();
        for j in 0..=self.h {
            let mut runs = Vec::new();
            if j < self.h {
                let mut i = 0;
                while i < self.w {
                    if self.cells[j * self.w + i] {
                        let from = i;
                        while i < self.w && self.cells[j * self.w + i] {
                            i += 1;
                        }
                        runs.push((from, i));
                    }
                    i += 1;
                }
            }
            let mut still = Vec::with_capacity(runs.len());
            for (from, to, top) in open.drain(..) {
                if let Some(pos) = runs.iter().position(|&r| r == (from, to)) {
                    runs.swap_remove(pos);
                    still.push((from, to, top));
                } else {
                    let x = |i: usize| (self.i0 + i as i64) * STEP;
                    let y = |j: usize| (self.j0 + j as i64) * STEP;
                    done.push([x(from), y(top), x(to), y(j)]);
                }
            }
            still.extend(runs.into_iter().map(|(from, to)| (from, to, j)));
            open = still;
        }
        done
    }
}

/// The distance from `p` to a shape, 0 inside it.
fn distance(shape: &Shape, p: Point) -> f64 {
    match shape {
        Shape::Capsule { a, b, r } => (point_segment(p, *a, *b) - *r as f64).max(0.0),
        Shape::Polygon(q) if inside(p, q) => 0.0,
        Shape::Polygon(q) => (0..q.len()).map(|k| point_segment(p, q[k], q[(k + 1) % q.len()])).fold(f64::INFINITY, f64::min),
    }
}

/// Every zone that pours copper, filled, highest priority first.
pub fn fill_zones(board: &Board, copper: &Copper) -> Vec<Fill> {
    let mut order: Vec<usize> = (0..copper.zones.len()).filter(|&z| copper.zones[z].keepout.is_none()).collect();
    order.sort_by_key(|&z| std::cmp::Reverse(copper.zones[z].priority));
    let mut poured: Vec<(u32, usize, Raster)> = Vec::new();
    let mut fills = Vec::new();
    for z in order {
        let zone = &copper.zones[z];
        let layers: Vec<usize> =
            copper.expand(&zone.layers).iter().filter_map(|n| board.layers.iter().position(|l| l == n)).collect();
        for layer in layers {
            if let Some(r) = fill_one(board, zone, layer, &poured) {
                fills.push(Fill { zone: z, net: zone.net, layer, rects: r.rects() });
                poured.push((zone.net, layer, r));
            }
        }
    }
    fills
}

fn fill_one(board: &Board, zone: &Zone, layer: usize, poured: &[(u32, usize, Raster)]) -> Option<Raster> {
    let outline: Vec<Point> = zone.outline.iter().copied().map(Point::from_m).collect();
    if outline.len() < 3 {
        return None;
    }
    let mut b = BBox::of(outline.iter().copied());
    if !board.outline.is_empty() {
        let edge = BBox::of(board.outline.iter().flatten().copied());
        b = BBox { min: Point::new(b.min.x.max(edge.min.x), b.min.y.max(edge.min.y)), max: Point::new(b.max.x.min(edge.max.x), b.max.y.min(edge.max.y)) };
    }
    let mut r = Raster::over(b)?;
    r.scan(&[&outline], true);
    if !board.outline.is_empty() {
        let mut board_area = r.clone();
        board_area.cells.fill(false);
        board_area.scan(&board.outline.iter().map(Vec::as_slice).collect::<Vec<_>>(), true);
        for (c, inside) in r.cells.iter_mut().zip(&board_area.cells) {
            *c &= inside;
        }
        for &(a, e, eb, edge) in &board.outline_edges {
            if eb.grow(edge).overlaps(b) {
                r.clear(eb.grow(edge), |p| point_segment(p, a, e) < edge as f64);
            }
        }
    }
    let own = nm(zone.clearance);
    let net = zone.net;
    let mut seeds = Vec::new();
    for id in board.near(layer, b) {
        let Some(item) = board.item(id) else { continue };
        if !item.on(layer) {
            continue;
        }
        match &item.kind {
            Kind::Keepout { forbids, .. } => {
                if let (true, Shape::Polygon(p)) = (forbids.copperpour, &item.shape) {
                    r.scan(&[p], false);
                }
            }
            Kind::Pour { .. } => {}
            _ if net != 0 && item.net == net => {
                let (cols, rows) = r.span(item.shape.bbox());
                for j in rows {
                    for i in cols.clone() {
                        if item.shape.contains(r.center(i, j)) {
                            seeds.push(j * r.w + i);
                        }
                    }
                }
            }
            _ => {
                let c = own.max(board.clearance(net, item.net)) as f64;
                r.clear(item.shape.bbox().grow(c as Nm), |p| distance(&item.shape, p) < c);
            }
        }
    }
    let holes = board.layers.len();
    let hole_gap = nm(board.rules.min_hole_clearance);
    for id in board.near(holes, b) {
        let Some(item) = board.item(id) else { continue };
        let Some(Shape::Capsule { a, b: end, r: radius }) = board.hole_shape(id, item) else { continue };
        if net != 0 && item.net == net && item.on(layer) {
            continue;
        }
        let reach = radius + hole_gap.max(own).max(board.clearance(net, item.net));
        if a == end {
            r.clear(BBox::of([a]).grow(reach), |p| p.distance(a) < reach as f64);
        } else {
            // A slot: every point within reach of its centre line.
            let slot = Shape::Capsule { a, b: end, r: 0 };
            r.clear(BBox::of([a, end]).grow(reach), |p| slot.gap_within(&Shape::Capsule { a: p, b: p, r: 0 }, reach as f64) < reach as f64);
        }
    }
    for (other, l, done) in poured {
        if *l != layer || *other == net {
            continue;
        }
        let c = own.max(board.clearance(net, *other)) as f64;
        for j in 0..done.h {
            for i in 0..done.w {
                if !done.cells[j * done.w + i] {
                    continue;
                }
                let (x, y) = (done.i0 + i as i64 - r.i0, done.j0 + j as i64 - r.j0);
                if x >= 0 && y >= 0 && (x as usize) < r.w && (y as usize) < r.h {
                    r.cells[y as usize * r.w + x as usize] = false;
                }
                if done.edge(i, j) {
                    let p = done.center(i, j);
                    r.clear(BBox::of([p]).grow(c as Nm + STEP), |q| q.distance(p) < c + STEP as f64 / 2.0);
                }
            }
        }
    }
    r.open(nm(zone.min_thickness) / 2);
    if net != 0 {
        r.keep_seeded(&seeds);
    }
    r.cells.iter().any(|&c| c).then_some(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Item;

    fn board(text: &str) -> (Board, Copper) {
        let (pcb, copper) = kicad_pcb::parse_board(text).unwrap();
        let board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        (board, copper)
    }

    const ZONE: &str = r#"(kicad_pcb (version 20240101) (general (thickness 1.6))
        (layers (0 "F.Cu" signal) (31 "B.Cu" signal)) (net 0 "") (net 1 "GND") (net 2 "SIG")
        (zone (net 1) (net_name "GND") (layer "F.Cu") (uuid "z") (connect_pads yes (clearance 0.2))
          (min_thickness 0.2) (polygon (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10)))))"#;

    fn track(net: u32, a: Point, b: Point) -> Item {
        Item { kind: Kind::Track, net, layers: 1, shape: Shape::Capsule { a, b, r: 100_000 }, hole: None, locked: false, span: None }
    }

    fn covered(fills: &[Fill], p: Point) -> bool {
        fills.iter().any(|f| f.rects.iter().any(|r| p.x >= r[0] && p.x < r[2] && p.y >= r[1] && p.y < r[3]))
    }

    #[test]
    fn a_pour_keeps_clear_of_other_nets_and_drops_islands_without_its_own_copper() {
        let (mut b, copper) = board(ZONE);
        assert!(fill_zones(&b, &copper).is_empty(), "a pour touching none of its net's copper is one island, removed");
        b.insert(track(1, Point::new(1_000_000, 1_000_000), Point::new(2_000_000, 1_000_000)));
        // A wall of another net from top to bottom cuts off the right side.
        b.insert(track(2, Point::new(5_000_000, -1_000_000), Point::new(5_000_000, 11_000_000)));
        let fills = fill_zones(&b, &copper);
        assert_eq!(fills.len(), 1);
        assert!(covered(&fills, Point::new(3_000_000, 5_000_000)));
        assert!(!covered(&fills, Point::new(8_000_000, 5_000_000)), "the island right of the wall has no GND copper");
        // 0.1 mm half-width plus 0.2 mm clearance: nothing within 0.3 mm of the wall's centre.
        assert!(!covered(&fills, Point::new(4_750_000, 5_000_000)));
        assert!(covered(&fills, Point::new(4_650_000, 5_000_000)));
    }
}
