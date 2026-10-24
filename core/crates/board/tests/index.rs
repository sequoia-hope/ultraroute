//! The spatial index follows items in and out.

use std::path::PathBuf;

use board::*;

fn earring() -> Board {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pcb");
    Board::load(&path).unwrap().0
}

#[test]
fn every_item_is_found_where_it_is_on_each_of_its_layers() {
    let b = earring();
    for (id, item) in b.items() {
        for layer in (0..b.layers.len()).filter(|l| item.on(*l)) {
            assert!(b.near(layer, item.shape.bbox()).contains(&id), "{id} on {layer}");
        }
        if item.hole.is_some() {
            assert!(b.near(b.layers.len(), item.shape.bbox()).contains(&id));
        }
    }
}

#[test]
fn a_removed_item_is_gone_and_comes_back_when_inserted() {
    let mut b = earring();
    let (id, item) = b.items().find(|(_, it)| it.kind == Kind::Via).map(|(i, it)| (i, it.clone())).unwrap();
    let layer = (0..b.layers.len()).find(|l| item.on(*l)).unwrap();
    let removed = b.remove(id).unwrap();
    assert!(b.item(id).is_none());
    assert!(!b.near(layer, item.shape.bbox()).contains(&id));
    let again = b.insert(removed);
    assert!(b.near(layer, item.shape.bbox()).contains(&again));
}

#[test]
fn removing_a_track_opens_its_net() {
    let mut b = earring();
    let before = connectivity(&b).airlines.len();
    let track = b.items().find(|(_, it)| it.kind == Kind::Track && it.net != 0).map(|(i, _)| i).unwrap();
    b.remove(track);
    assert!(connectivity(&b).airlines.len() >= before);
}
