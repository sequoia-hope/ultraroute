//! Fills a board's zones and compares them with the fills KiCad wrote into the same board after
//! its own refill: per zone and layer, how much of KiCad's copper ours covers and how much of
//! ours KiCad has not. `cargo run --release -p board --example fill_check -- filled.kicad_pcb`

use std::path::Path;
use std::time::Instant;

use board::{fill_zones, Board, Nm, Point};

/// Sampling step for the comparison.
const STEP: Nm = 20_000;

fn inside(p: Point, poly: &[Point]) -> bool {
    board::inside(p, poly)
}

fn main() {
    let path = std::env::args().nth(1).expect("a .kicad_pcb, refilled by KiCad");
    let (board, text) = Board::load(Path::new(&path)).expect("board");
    let (_, copper) = kicad_pcb::parse_board(&text).expect("parse");
    let started = Instant::now();
    let fills = fill_zones(&board, &copper);
    let ms = started.elapsed().as_secs_f64() * 1e3;
    let rects: usize = fills.iter().map(|f| f.rects.len()).sum();
    println!("{} fills, {rects} rectangles, {ms:.0} ms", fills.len());
    let (mut ours_all, mut kicad_all, mut both_all) = (0u64, 0u64, 0u64);
    for f in &fills {
        let zone = &copper.zones[f.zone];
        let name = &board.layers[f.layer];
        let theirs: Vec<Vec<Point>> =
            zone.filled.iter().filter(|(l, _)| l == name).map(|(_, p)| p.iter().copied().map(Point::from_m).collect()).collect();
        let b = board::BBox::of(zone.outline.iter().copied().map(Point::from_m));
        let (mut ours, mut kicad, mut both) = (0u64, 0u64, 0u64);
        let mut y = b.min.y + STEP / 2;
        while y < b.max.y {
            let mut x = b.min.x + STEP / 2;
            while x < b.max.x {
                let p = Point::new(x, y);
                let o = f.rects.iter().any(|r| x >= r[0] && x < r[2] && y >= r[1] && y < r[3]);
                let k = theirs.iter().filter(|q| inside(p, q)).count() % 2 == 1;
                ours += o as u64;
                kicad += k as u64;
                both += (o && k) as u64;
                x += STEP;
            }
            y += STEP;
        }
        let pct = |a: u64, b: u64| if b == 0 { 100.0 } else { 100.0 * a as f64 / b as f64 };
        println!(
            "{:<24} {:<6} {:>8.2} mm² ours {:>8.2} mm² KiCad: covers {:5.1}% of KiCad's, {:5.1}% of ours is KiCad's",
            if zone.name.is_empty() { &zone.net_name } else { &zone.name },
            name,
            ours as f64 * (STEP as f64 * 1e-6).powi(2),
            kicad as f64 * (STEP as f64 * 1e-6).powi(2),
            pct(both, kicad),
            pct(both, ours),
        );
        (ours_all, kicad_all, both_all) = (ours_all + ours, kicad_all + kicad, both_all + both);
    }
    println!(
        "all: ours covers {:.1}% of KiCad's copper; {:.1}% of ours is KiCad's",
        100.0 * both_all as f64 / kicad_all.max(1) as f64,
        100.0 * both_all as f64 / ours_all.max(1) as f64
    );
}
