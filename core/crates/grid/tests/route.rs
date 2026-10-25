//! The router on a real board: strip a few nets' copper from the earring, route them back, and
//! judge the result with the board model's own checks. Kept to short nets on a coarse grid so
//! the suite stays quick in a debug build.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use board::*;
use ::grid::*;

/// Cells coarse enough that a debug build routes in seconds; the earring's tracks still fit.
const CELL: Nm = 100_000;

fn earring() -> Board {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pcb");
    Board::load(&path).unwrap().0
}

/// The earring with the tracks and vias of its `n` shortest-routed signal nets taken off, and
/// every net then open: the fixture is mostly bare pads, so that is most of them.
fn stripped(n: usize) -> (Board, BTreeSet<u32>) {
    let mut b = earring();
    let mut copper: std::collections::BTreeMap<u32, Vec<ItemId>> = Default::default();
    for (id, it) in b.items() {
        if matches!(it.kind, Kind::Track | Kind::Via) && it.net != 0 {
            copper.entry(it.net).or_default().push(id);
        }
    }
    let mut nets: Vec<(usize, u32)> = copper.iter().map(|(net, ids)| (ids.len(), *net)).collect();
    nets.sort();
    let chosen: BTreeSet<u32> = nets.iter().take(n).map(|(_, net)| *net).collect();
    for net in &chosen {
        for id in &copper[net] {
            b.remove(*id);
        }
    }
    let open = connectivity(&b).airlines.iter().map(|a| a.net).collect();
    (b, open)
}

/// Pours are kept as planes throughout, so only the stripped nets are open and a route through
/// another net's pour is a real fault.
fn open(b: &Board) -> usize {
    connectivity(b).airlines.len()
}

/// Violations as (rule, item, other item) so before and after can be compared as sets.
fn violations(b: &Board) -> BTreeSet<(String, ItemId, Option<ItemId>)> {
    check(b).into_iter().map(|v| (format!("{:?}", v.rule), v.a, v.b)).collect()
}

fn assert_clean_and_on_nets(before: &Board, after: &Board, laid: &[ItemId], nets: &BTreeSet<u32>) {
    let (was, now) = (violations(before), violations(after));
    let new: Vec<_> = now.difference(&was).collect();
    assert!(new.is_empty(), "routing added violations: {new:?}");
    for id in laid {
        let it = after.item(*id).expect("laid items stay on the board");
        assert!(matches!(it.kind, Kind::Track | Kind::Via));
        assert!(nets.contains(&it.net), "laid copper on net {} that was never opened", it.net);
    }
}

#[test]
fn stripped_nets_are_routed_back_without_new_violations() {
    let (mut b, nets) = stripped(4);
    let before = b.clone();
    let opened = open(&b);
    assert!(opened > 0);
    let stats = route_open(&mut b, &mut CpuSolver, &Options { cell: CELL, pours_block: true, ..Default::default() }, &mut |_| {});
    assert!(stats.routed > 0, "{stats:?}");
    assert!(open(&b) < opened, "{} open of {opened}", open(&b));
    assert_eq!(stats.failed, stats.failures.len());
    assert_clean_and_on_nets(&before, &b, &stats.laid, &nets);
}

#[test]
fn negotiation_commits_only_legal_routes_one_thread_or_many() {
    for threads in [1, 3] {
        let (mut b, nets) = stripped(4);
        let before = b.clone();
        let opened = open(&b);
        let mut shown = 0usize;
        let mut last: Vec<Trial> = Vec::new();
        let stats = route_negotiated(
            &mut b,
            &mut CpuSolver,
            &NegotiationOptions { cell: CELL, pours_block: true, threads, max_rounds: 8, ..Default::default() },
            &mut |_| {},
            &mut |_, trials| {
                shown += 1;
                last = trials.to_vec();
            },
        );
        assert_eq!(stats.connections, opened, "threads {threads}");
        assert!(stats.rounds >= 1 && stats.rounds <= 8);
        assert!(stats.committed + stats.unroutable <= stats.connections);
        assert!(stats.committed > 0, "threads {threads}: {stats:?}");
        assert!(shown >= stats.rounds, "every round shows its trials");
        // The trials shown after the last round carry that round's verdicts.
        assert_eq!(last.iter().filter(|t| t.2).count(), stats.conflicted, "threads {threads}");
        for (net, corners, _) in &last {
            assert!(nets.contains(net));
            assert!(corners.len() >= 2 || corners.len() == 1, "a trial has at least its ends");
        }
        assert_clean_and_on_nets(&before, &b, &stats.laid, &nets);
        assert!(open(&b) <= opened - stats.committed.min(opened));
    }
}

#[test]
fn negotiation_is_the_same_on_one_thread_per_round_however_many_run() {
    // Jacobi rounds route against one snapshot, so the thread count must not change the result.
    let run = |threads: usize| {
        let (mut b, _) = stripped(3);
        let stats = route_negotiated(
            &mut b,
            &mut CpuSolver,
            &NegotiationOptions { cell: CELL, pours_block: true, threads, max_rounds: 5, ..Default::default() },
            &mut |_| {},
            &mut |_, _| {},
        );
        (stats.rounds, stats.committed, stats.conflicted, stats.vias)
    };
    assert_eq!(run(2), run(4));
}

#[test]
fn a_cancelled_run_stops_before_routing() {
    let (mut b, _) = stripped(2);
    let before = violations(&b);
    let cancel = Arc::new(AtomicBool::new(true));
    let n = route_negotiated(
        &mut b,
        &mut CpuSolver,
        &NegotiationOptions { cell: CELL, pours_block: true, cancel: Some(cancel.clone()), ..Default::default() },
        &mut |_| {},
        &mut |_, _| {},
    );
    assert_eq!((n.rounds, n.committed), (0, 0));
    let r = route_open(&mut b, &mut CpuSolver, &Options { cell: CELL, pours_block: true, cancel: Some(cancel.clone()), ..Default::default() }, &mut |_| {});
    assert_eq!(r.routed, 0);
    assert!(r.laid.is_empty());
    assert_eq!(violations(&b), before);
    assert!(cancel.load(Ordering::Relaxed), "the router never clears the caller's flag");
}

#[test]
fn a_zero_time_budget_is_honoured() {
    let (mut b, _) = stripped(2);
    let t = std::time::Instant::now();
    let n = route_negotiated(
        &mut b,
        &mut CpuSolver,
        &NegotiationOptions { cell: CELL, pours_block: true, max_seconds: Some(0.0), ..Default::default() },
        &mut |_| {},
        &mut |_, _| {},
    );
    assert!(n.rounds <= 1);
    assert!(t.elapsed().as_secs_f64() < 30.0);
}

#[test]
fn a_board_with_nothing_open_routes_nothing() {
    let mut b = earring();
    // Join everything that is open first, so only a fully routed board is left to route again.
    route_open(&mut b, &mut CpuSolver, &Options { cell: CELL, pours_block: true, ..Default::default() }, &mut |_| {});
    let left = open(&b);
    let items = b.items().count();
    let again = route_open(&mut b, &mut CpuSolver, &Options { cell: CELL, pours_block: true, ..Default::default() }, &mut |_| {});
    assert_eq!(again.routed, 0);
    assert_eq!(open(&b), left);
    assert_eq!(b.items().count(), items);
}


/// The earring routed on the coarse grid, pours left to be refilled, and what the router laid.
fn routed() -> (Board, Vec<ItemId>) {
    let (mut b, _) = stripped(0);
    let stats = route_open(&mut b, &mut CpuSolver, &Options { cell: CELL, ..Default::default() }, &mut |_| {});
    assert!(stats.laid.len() > 100, "{} pieces laid", stats.laid.len());
    (b, stats.laid)
}

/// Violations not against pours, which are refilled around whatever moved.
fn violations_but_pours(b: &Board) -> BTreeSet<(String, ItemId, Option<ItemId>)> {
    let pour = |id: ItemId| b.item(id).is_some_and(|it| matches!(it.kind, Kind::Pour { .. }));
    check(b).into_iter().filter(|v| !pour(v.a) && !v.b.is_some_and(pour)).map(|v| (format!("{:?}", v.rule), v.a, v.b)).collect()
}

fn board_centre(b: &Board) -> Point {
    let bb = BBox::of(b.outline.iter().flatten().copied());
    Point::new((bb.min.x + bb.max.x) / 2, (bb.min.y + bb.max.y) / 2)
}

#[test]
fn a_push_flows_everything_free_away_from_its_centre_and_leaves_locked_things_alone() {
    let (mut b, mut laid) = routed();
    let before = b.clone();
    let centre = board_centre(&b);
    let field = Field { center: centre, outward: 300_000.0, vector: (0.0, 0.0), radius: 6_000_000.0, decay: 2.0 };
    let mut frames = 0;
    let p = push(&mut b, &field, &PushOptions { steps: 6, ..Default::default() }, &mut laid, &mut |_| frames += 1);
    assert_eq!(frames, 6, "every step is shown");
    assert!(!p.moved.is_empty(), "{p:?}");
    for (f, by) in &p.moved {
        let fp = &before.footprints[*f];
        assert!(!fp.locked, "{} is locked and moved", fp.reference);
        let out = Point::new(fp.at.x - centre.x, fp.at.y - centre.y);
        assert!((by.x as i128) * (out.x as i128) + (by.y as i128) * (out.y as i128) >= 0, "{} moved toward the centre", fp.reference);
    }
    for (i, fp) in before.footprints.iter().enumerate().filter(|(_, f)| f.locked) {
        assert_eq!(b.footprints[i].at, fp.at, "{} is locked", fp.reference);
    }
    for (id, it) in before.items().filter(|(_, it)| it.locked && matches!(it.kind, Kind::Track | Kind::Via)) {
        assert_eq!(b.item(id).map(|i| &i.shape), Some(&it.shape), "locked copper {id} moved");
    }
    // Copper that flowed is copper the router laid or file copper now to be written back.
    assert!(p.flowed > 0);
    assert!(laid.iter().all(|id| b.item(*id).is_some()));
    // Nothing the push left breaks a rule it did not already break.
    let (was, now) = (violations_but_pours(&before), violations_but_pours(&b));
    let new: Vec<_> = now.difference(&was).collect();
    assert!(new.is_empty(), "the push added violations: {new:?}");
}

#[test]
fn copper_that_only_flows_stays_joined() {
    // A gentle push rips nothing; then every connection the board had is still made.
    let (mut b, mut laid) = routed();
    let open_now = |b: &Board| connectivity_without_pours(b).airlines.len();
    let open_before = open_now(&b);
    let field = Field { center: board_centre(&b), outward: 20_000.0, vector: (0.0, 0.0), radius: 8_000_000.0, decay: 2.0 };
    let p = push(&mut b, &field, &PushOptions { steps: 2, ..Default::default() }, &mut laid, &mut |_| {});
    assert!(p.flowed > 0);
    assert!(p.ripped * 20 < p.flowed, "a gentle push rips little: {} ripped of {} moved", p.ripped, p.flowed);
    if p.ripped == 0 {
        assert_eq!(open_now(&b), open_before);
    }
}

#[test]
fn a_zero_field_moves_nothing() {
    let (mut b, mut laid) = routed();
    let before = b.clone();
    let field = Field { center: board_centre(&b), outward: 0.0, vector: (0.0, 0.0), radius: 5_000_000.0, decay: 2.0 };
    let p = push(&mut b, &field, &PushOptions::default(), &mut laid, &mut |_| {});
    assert_eq!((p.moved.len(), p.flowed, p.ripped), (0, 0, 0));
    assert_eq!(violations(&b), violations(&before));
    assert!(before.items().all(|(id, it)| b.item(id).map(|i| &i.shape) == Some(&it.shape)));
}


#[test]
fn combing_escapes_into_lanes_breaks_no_connection() {
    let (mut b, mut laid) = routed();
    let parts = morph::fine_pitch(&b, 500_000);
    assert!(!parts.is_empty(), "the earring has fine-pitch parts");
    let open_now = |b: &Board| connectivity_without_pours(b).airlines.len();
    let before = b.clone();
    let open_before = open_now(&b);
    let mut combed = 0;
    for &f in &parts {
        let e = morph::comb(&mut b, f, &morph::EscapeOptions::default(), &mut laid);
        assert!(e.lanes > 0, "{} has lanes", b.footprints[f].reference);
        combed += e.combed + e.pushed_out;
    }
    assert!(combed > 0);
    // Moved, never broken: everything joined before is joined still, before anything is ripped.
    assert_eq!(open_now(&b), open_before);
    for (id, it) in before.items().filter(|(_, it)| it.locked || matches!(it.kind, Kind::Pad { .. })) {
        assert_eq!(b.item(id).map(|i| &i.shape), Some(&it.shape), "{id} is fixed and moved");
    }
    assert!(laid.iter().all(|id| b.item(*id).is_some()));
    // What combing left breaking a rule is ripped, and nothing breaks one after that.
    rip_broken(&mut b, &mut laid, false);
    let (was, now) = (violations_but_pours(&before), violations_but_pours(&b));
    assert!(now.difference(&was).next().is_none());
}

