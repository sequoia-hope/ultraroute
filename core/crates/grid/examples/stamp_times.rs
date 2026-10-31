//! `stamp_times BOARD`: the items that take longest to stamp into a map, slowest first, and the
//! time by kind and vertex count (PowerGloveUHID's drawn arcs were 60,715 polygons, 55 s a map).
use std::path::PathBuf;
use board::{Board, Kind};
use grid::{ObstacleMap, Profile};

fn main() {
    let path = PathBuf::from(std::env::args().nth(1).expect("usage: stamp_times BOARD"));
    let mut board = Board::load(&path).unwrap().0;
    let strip: Vec<_> = board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via) && !it.locked).map(|(id, _)| id).collect();
    for id in strip { board.remove(id); }
    let net = (1..board.nets.len() as u32).next().unwrap();
    let mut map = ObstacleMap::without(&board, Profile::for_net(&board, net), 50_000, false, &|_| true);
    let mut times: Vec<(f64, String)> = Vec::new();
    let ids: Vec<_> = board.items().map(|(id, _)| id).collect();
    for id in ids {
        let t = std::time::Instant::now();
        map.stamp(&board, id);
        let it = board.item(id).unwrap();
        let bb = it.shape.bbox();
        let verts = match &it.shape { board::Shape::Polygon(p) => p.len(), _ => 2 };
        times.push((t.elapsed().as_secs_f64(), format!("{:?} net {} layers {:b} {} verts bbox {:.1}x{:.1} mm", std::mem::discriminant(&it.kind), board.net_name(it.net), it.layers, verts, (bb.max.x-bb.min.x) as f64*1e-6, (bb.max.y-bb.min.y) as f64*1e-6)));
    }
    let total: f64 = times.iter().map(|t| t.0).sum();
    times.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!("{} items, {total:.1} s", times.len());
    for (t, s) in times.iter().take(5) { println!("{t:8.3} s  {s}"); }
    let mut kinds: std::collections::BTreeMap<String, (usize, f64)> = Default::default();
    for (t, s) in &times {
        let key = s.split(" bbox").next().unwrap().split(" net ").next().unwrap().to_string() + " " + s.split(" verts").next().unwrap().rsplit(' ').next().unwrap();
        let e = kinds.entry(key).or_default(); e.0 += 1; e.1 += t;
    }
    let mut v: Vec<_> = kinds.into_iter().collect(); v.sort_by(|a, b| b.1 .1.total_cmp(&a.1 .1));
    for (k, (n, t)) in v.iter().take(10) { println!("{n:6} items {t:6.1} s  {k} verts"); }
}
