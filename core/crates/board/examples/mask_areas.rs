//! `mask_areas BOARD X Y R`: the openings drawn in the mask within R mm of (X, Y), and the net each admits.
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (b, _) = board::Board::load(std::path::Path::new(&a[0])).unwrap();
    let f = |i: usize| a[i].parse::<f64>().unwrap();
    let at = board::Point::new((f(1) * 1e6) as i64, (f(2) * 1e6) as i64);
    let near = board::Shape::Capsule { a: at, b: at, r: (f(3) * 1e6) as i64 };
    for (shape, layers, net) in &b.mask_areas {
        if shape.gap_within(&near, 1.0) <= 0.0 {
            let bb = shape.bbox();
            println!("layers {layers:b} net {net} ({}) box {:.3},{:.3} - {:.3},{:.3}", b.net_name(*net), bb.min.x as f64 * 1e-6, bb.min.y as f64 * 1e-6, bb.max.x as f64 * 1e-6, bb.max.y as f64 * 1e-6);
        }
    }
    let pads: Vec<_> = b.items().filter(|(_, it)| matches!(it.kind, board::Kind::Pad { .. }) && it.shape.gap_within(&near, 1.0) <= 0.0).map(|(_, it)| { let bb = it.shape.bbox(); (b.net_name(it.net).to_string(), (bb.min.x + bb.max.x) as f64 * 0.5e-6, (bb.min.y + bb.max.y) as f64 * 0.5e-6) }).collect();
    println!("{pads:?}");
}
