//! `what_is_here BOARD X Y [R]`: every item within R mm (default 0.5) of (X, Y), by layer, with its
//! net, kind and gap: for finding why the router judges a piece of copper as it does.
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (b, _) = board::Board::load(std::path::Path::new(&a[0])).unwrap();
    let f = |i: usize, d: f64| a.get(i).map_or(d, |v| v.parse::<f64>().unwrap());
    let at = board::Point::new((f(1, 0.0) * 1e6) as i64, (f(2, 0.0) * 1e6) as i64);
    let probe = board::Shape::Capsule { a: at, b: at, r: (f(3, 0.5) * 1e6) as i64 };
    for (l, name) in b.layers.iter().enumerate() {
        for id in b.near(l, probe.bbox()) {
            let Some(it) = b.item(id) else { continue };
            if !it.on(l) || it.shape.gap_within(&probe, 1.0) > 0.0 {
                continue;
            }
            let gap = it.shape.gap(&board::Shape::Capsule { a: at, b: at, r: 0 }) * 1e-6;
            println!("{name:8} #{id:<6} {:<10} {:?} gap to point {gap:.3} mm", b.net_name(it.net), it.kind);
        }
    }
}
