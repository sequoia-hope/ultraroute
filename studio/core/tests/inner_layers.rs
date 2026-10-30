//! An inner copper layer shown alone draws its own copper: the view bar's Inner menu masks the
//! mesh to one layer, so a layer that drew nothing alone would show an empty board.

use studio_core::Scene;

const STREAM: &str = include_str!("fixtures/easy2350-stream.jsonl");

#[test]
fn each_copper_layer_alone_draws_something_of_its_own() {
    let mut s = Scene::default();
    for line in STREAM.lines() {
        s.apply(line);
    }
    let names: Vec<String> = s.board.layers().iter().map(|l| l.name.clone()).collect();
    assert_eq!(names.iter().filter(|n| n.ends_with(".Cu")).count(), 4, "easy2350 has four copper layers: {names:?}");
    let base = s.mesh_with(0.02, 0, 0.0).len();
    for (i, name) in names.iter().enumerate().filter(|(_, n)| n.ends_with(".Cu")) {
        let alone = s.mesh_with(0.02, 1 << i, 0.0).len();
        assert!(alone > base, "{name} alone draws nothing beyond the board itself ({alone} vs {base} floats)");
        println!("{name}: {} floats beyond the board's own", alone - base);
    }
}
