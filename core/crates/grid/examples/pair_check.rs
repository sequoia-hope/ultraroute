//! How well a routed board's differential pairs are coupled: per pair, how much of each net's
//! track runs beside the other's at the pair's pitch (within 10%), on the same layer, the two
//! nets' track lengths and vias, and the skew end to end as the length matching measures it.
//! `cargo run --release -p grid --example pair_check -- B.kicad_pcb`

use std::path::Path;

use board::{point_segment, Board, Kind, Nm, Point, Shape};

/// Sampling step along each track.
const STEP: f64 = 25_000.0;

fn main() {
    for path in std::env::args().skip(1) {
        let (board, _) = Board::load(Path::new(&path)).expect("board");
        println!("{path}");
        for pair in grid::diff_pairs(&board) {
            let tracks = |net: u32| -> Vec<(usize, Point, Point)> {
                board
                    .items()
                    .filter_map(|(_, it)| match (&it.kind, &it.shape) {
                        (Kind::Track, Shape::Capsule { a, b, .. }) if it.net == net => Some((it.layers.trailing_zeros() as usize, *a, *b)),
                        _ => None,
                    })
                    .collect()
            };
            let vias = |net: u32| board.items().filter(|(_, it)| it.net == net && it.kind == Kind::Via).count();
            let (tp, tn) = (tracks(pair.p), tracks(pair.n));
            let pitch = pair.pitch() as f64;
            let coupled = |mine: &[(usize, Point, Point)], other: &[(usize, Point, Point)]| -> (f64, f64) {
                let (mut total, mut beside) = (0.0, 0.0);
                for &(layer, a, b) in mine {
                    let len = a.distance(b);
                    let n = (len / STEP).ceil().max(1.0) as i64;
                    for k in 0..n {
                        let t = (k as f64 + 0.5) / n as f64;
                        let p = Point::new(a.x + ((b.x - a.x) as f64 * t) as Nm, a.y + ((b.y - a.y) as f64 * t) as Nm);
                        let d = other.iter().filter(|o| o.0 == layer).map(|o| point_segment(p, o.1, o.2)).fold(f64::INFINITY, f64::min);
                        total += len / n as f64;
                        if (d - pitch).abs() <= 0.1 * pitch {
                            beside += len / n as f64;
                        }
                    }
                }
                (total * 1e-6, beside * 1e-6)
            };
            let ((lp, cp), (ln, cn)) = (coupled(&tp, &tn), coupled(&tn, &tp));
            // End to end along the copper, as the length matching measures it.
            let matched = grid::pair_lengths(&board, &pair).map(|((a, _), (b, _))| (a - b).abs() * 1e-6);
            println!(
                "  {}/{}: pitch {:.3} mm; {}: {:.2} mm, {:.0}% coupled, {} vias; {}: {:.2} mm, {:.0}% coupled, {} vias; skew {:.2} mm in all, {} end to end",
                board.net_name(pair.p),
                board.net_name(pair.n),
                pitch * 1e-6,
                board.net_name(pair.p),
                lp,
                100.0 * cp / lp.max(1e-9),
                vias(pair.p),
                board.net_name(pair.n),
                ln,
                100.0 * cn / ln.max(1e-9),
                vias(pair.n),
                (lp - ln).abs(),
                matched.map_or("unmeasured".to_string(), |m| format!("{m:.2} mm"))
            );
        }
    }
}
