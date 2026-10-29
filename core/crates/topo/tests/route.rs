//! The topological router on a real board: a piece of the earring with its pours taken off, so
//! every open connection is a real one and any copper the router lays too near something is a
//! fault the board's own check reports. Kept to one window of the board and one round of rip-up
//! so the suite stays quick in a debug build.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use board::*;
use topo::*;

fn earring() -> Board {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pcb");
    Board::load(&path).unwrap().0
}

/// Half the side of the window kept, in nanometres.
const WINDOW: Nm = 3_500_000;

/// The earring with its tracks and vias off, and only the parts within `WINDOW` of `centre` (the
/// board's centre plus an offset) left: a small board of bare pads with its open connections all
/// inside it. The outline stays, so its edge clearance is still checked; the pours stay only
/// when asked for.
fn window(offset: Point, pours: bool) -> Board {
    let mut b = earring();
    let bb = BBox::of(b.outline.iter().flatten().copied());
    let centre = Point::new((bb.min.x + bb.max.x) / 2 + offset.x, (bb.min.y + bb.max.y) / 2 + offset.y);
    let keep = BBox::of([centre]).grow(WINDOW);
    let footprint_in = |f: usize, b: &Board| {
        let at = b.footprints[f].at;
        keep.overlaps(BBox::of([at]))
    };
    let gone: Vec<ItemId> = b
        .items()
        .filter(|(_, it)| match &it.kind {
            Kind::Pad { footprint, .. } => !footprint_in(*footprint, &b),
            Kind::Keepout { footprint: Some(f), .. } => !footprint_in(*f, &b),
            Kind::Keepout { .. } => false,
            Kind::Pour { .. } => !pours,
            Kind::Track | Kind::Via => true,
        })
        .map(|(id, _)| id)
        .collect();
    for id in gone {
        b.remove(id);
    }
    b
}

fn open(b: &Board) -> usize {
    connectivity(b).airlines.len()
}

/// Violations as (rule, item, other item) so before and after can be compared as sets.
fn violations(b: &Board) -> BTreeSet<(String, ItemId, Option<ItemId>)> {
    check(b).into_iter().map(|v| (format!("{:?}", v.rule), v.a, v.b)).collect()
}

/// Coarse enough for a debug build: wide samples, a short search, one round of rip-up.
fn quick() -> Options {
    Options { sample: 120_000, max_expansions: 50_000, rip_passes: 1, ..Default::default() }
}

fn assert_clean(before: &Board, after: &Board, laid: &[ItemId]) {
    let (was, now) = (violations(before), violations(after));
    let new: Vec<_> = now.difference(&was).collect();
    assert!(new.is_empty(), "routing added violations: {new:?}");
    let nets: BTreeSet<u32> = connectivity(before).airlines.iter().map(|a| a.net).collect();
    for id in laid {
        let it = after.item(*id).expect("laid items stay on the board");
        assert!(matches!(it.kind, Kind::Track | Kind::Via));
        assert!(!it.locked);
        assert!(nets.contains(&it.net), "laid copper on net {} that was never open", it.net);
    }
}

#[test]
fn open_connections_are_routed_as_rubber_bands_without_new_violations() {
    let mut b = window(Point::new(0, 0), false);
    let before = b.clone();
    let opened = open(&b);
    assert!(opened >= 5, "the window holds {opened} connections");
    let mut frames = 0;
    let stats = route_topological(&mut b, &quick(), &mut |_| frames += 1);
    eprintln!("{} of {} routed, {} vias, {} open of {opened}", stats.routed, stats.connections, stats.vias, open(&b));
    assert_eq!(stats.connections, opened);
    assert!(stats.routed > opened / 2, "{stats:?}");
    assert!(open(&b) < opened, "{} open of {opened}", open(&b));
    assert!(frames > 0, "every connection laid is shown");
    assert_eq!(stats.vias, stats.laid.iter().filter(|id| b.item(**id).is_some_and(|it| it.kind == Kind::Via)).count());
    assert_clean(&before, &b, &stats.laid);
}

#[test]
fn another_part_of_the_board_routes_cleanly_too() {
    let mut b = window(Point::new(-4_000_000, 4_000_000), false);
    let before = b.clone();
    let opened = open(&b);
    let stats = route_topological(&mut b, &quick(), &mut |_| {});
    eprintln!("{} of {} routed, {} vias, {} open of {opened}", stats.routed, stats.connections, stats.vias, open(&b));
    assert!(open(&b) <= opened);
    assert!(opened == 0 || stats.routed > 0, "{stats:?}");
    assert_clean(&before, &b, &stats.laid);
}

#[test]
fn kept_pours_are_routed_round_not_through() {
    // Pours kept as planes: the bare pads sit in the pours' clearance gaps, so little can be
    // routed, but nothing laid may touch another net's pour.
    let mut b = window(Point::new(0, 0), true);
    let before = b.clone();
    let opened = open(&b);
    let stats = route_topological(&mut b, &Options { pours_block: true, rip_passes: 0, ..quick() }, &mut |_| {});
    eprintln!("{} of {} routed, {} vias, {} open of {opened}", stats.routed, stats.connections, stats.vias, open(&b));
    assert!(open(&b) <= opened);
    assert_clean(&before, &b, &stats.laid);
}

#[test]
fn a_cancelled_run_lays_nothing() {
    let mut b = window(Point::new(0, 0), false);
    let before = violations(&b);
    let items = b.items().count();
    let stats = route_topological(&mut b, &Options { deadline: grid::Deadline::unlimited(Arc::new(AtomicBool::new(true))), ..quick() }, &mut |_| {});
    assert_eq!((stats.routed, stats.laid.len()), (0, 0), "{stats:?}");
    assert_eq!(b.items().count(), items);
    assert_eq!(violations(&b), before);
}

#[test]
fn a_routed_board_routes_nothing_more() {
    let mut b = window(Point::new(0, 0), false);
    route_topological(&mut b, &quick(), &mut |_| {});
    let left = open(&b);
    let items = b.items().count();
    let again = route_topological(&mut b, &Options { rip_passes: 0, ..quick() }, &mut |_| {});
    // Only what is still open is tried again, and whatever it lays joins something.
    assert_eq!(again.connections, left);
    assert!(open(&b) <= left - again.routed.min(left));
    if left == 0 {
        assert_eq!(b.items().count(), items);
    }
}

#[test]
fn only_selected_connections_are_routed_and_soft_blockers_can_be_ripped() {
    let mut b = window(Point::new(0, 0), false);
    let before = b.clone();
    let air = connectivity(&b).airlines;
    assert!(air.len() >= 3);
    let pair = (air[0].from_item.min(air[0].to_item), air[0].from_item.max(air[0].to_item));
    let only = std::collections::HashSet::from([pair]);
    let stats = route_topological(&mut b, &Options { only: Some(only), rip_passes: 0, ..quick() }, &mut |_| {});
    assert_eq!(stats.connections, 1);
    assert!(stats.routed <= 1);
    assert_clean(&before, &b, &stats.laid);

    // Fence pad `b1` inside a soft closed ring of net 1 on a 1-layer board (taken from `earring`
    // with all items stripped and layers truncated to F.Cu), so `b1` has no channel out unless
    // `route_topological_with_soft` rips net 1's soft cluster and reroutes net 1 round the back.
    let mut syn = earring();
    let all_ids: Vec<ItemId> = syn.items().map(|(id, _)| id).collect();
    for id in all_ids {
        syn.remove(id);
    }
    syn.layers.truncate(1);
    let bb = BBox::of(syn.outline.iter().flatten().copied());
    let c = Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2);
    let pad = |net: u32, dx: Nm, dy: Nm, num: &str| Item {
        kind: Kind::Pad { footprint: 0, number: num.into(), plated: false },
        net,
        layers: 1,
        shape: Shape::Capsule { a: Point::new(c.x + dx, c.y + dy), b: Point::new(c.x + dx, c.y + dy), r: 120_000 },
        hole: None,
        locked: true,
        span: None,
    };
    let _a1 = syn.insert(pad(1, -900_000, -600_000, "1"));
    let _a2 = syn.insert(pad(1, -900_000, 600_000, "2"));
    let b1 = syn.insert(pad(2, 0, 0, "3"));
    let b2 = syn.insert(pad(2, 1_800_000, 0, "4"));
    let corners = [
        Point::new(c.x - 900_000, c.y - 600_000),
        Point::new(c.x + 700_000, c.y - 600_000),
        Point::new(c.x + 700_000, c.y + 600_000),
        Point::new(c.x - 900_000, c.y + 600_000),
        Point::new(c.x - 900_000, c.y - 600_000),
    ];
    let mut soft: Vec<ItemId> = corners
        .windows(2)
        .map(|w| {
            syn.insert(Item {
                kind: Kind::Track,
                net: 1,
                layers: 1,
                shape: Shape::Capsule { a: w[0], b: w[1], r: 75_000 },
                hole: None,
                locked: false,
                span: None,
            })
        })
        .collect();
    assert_eq!(open(&syn), 1, "only net B is open while net A's ring encloses b1");
    assert!(violations(&syn).is_empty(), "starting setup is legal: {:?}", violations(&syn));

    let only_b = std::collections::HashSet::from([(b1.min(b2), b1.max(b2))]);
    let no_rip = route_topological(
        &mut syn.clone(),
        &Options { only: Some(only_b.clone()), rip_passes: 0, sample: 80_000, ..Default::default() },
        &mut |_| {},
    );
    assert_eq!(no_rip.routed, 0, "cannot escape a closed ring without ripping it");

    let ripped = route_topological_with_soft(
        &mut syn,
        &Options { only: Some(only_b), rip_passes: 2, sample: 80_000, ..Default::default() },
        &mut soft,
        &mut |_| {},
    );
    assert_eq!(ripped.routed, 1, "{ripped:?}");
    assert_eq!(open(&syn), 0, "both nets end connected");
    assert!(violations(&syn).is_empty(), "violations: {:?}", violations(&syn));
}
