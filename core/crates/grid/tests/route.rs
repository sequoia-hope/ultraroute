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
    for (threads, jacobi) in [(1, false), (3, true)] {
        let (mut b, nets) = stripped(4);
        let before = b.clone();
        let opened = open(&b);
        let mut shown = 0usize;
        let mut last: Vec<Trial> = Vec::new();
        let stats = route_negotiated(
            &mut b,
            &mut CpuSolver,
            &NegotiationOptions { cell: CELL, pours_block: true, threads, jacobi, max_rounds: 8, ..Default::default() },
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
            assert!(!corners.is_empty(), "a trial is never empty");
        }
        assert_clean_and_on_nets(&before, &b, &stats.laid, &nets);
        assert!(open(&b) <= opened - stats.committed.min(opened));
    }
}

#[test]
fn negotiation_is_the_same_however_many_threads_run() {
    // The default rounds run on one thread whatever `threads` says; Jacobi rounds route against
    // one snapshot. Either way the copper laid must not depend on the thread count.
    let run = |threads: usize, jacobi: bool| {
        let (mut b, _) = stripped(3);
        let stats = route_negotiated(
            &mut b,
            &mut CpuSolver,
            &NegotiationOptions { cell: CELL, pours_block: true, threads, jacobi, max_rounds: 5, ..Default::default() },
            &mut |_| {},
            &mut |_, _| {},
        );
        let mut copper: Vec<String> = stats.laid.iter().filter_map(|id| b.item(*id)).map(|it| format!("{} {} {:?}", it.net, it.layers, it.shape)).collect();
        copper.sort();
        ((stats.rounds, stats.committed, stats.conflicted, stats.vias), copper)
    };
    for jacobi in [false, true] {
        let one = run(1, jacobi);
        assert!(!one.1.is_empty(), "jacobi {jacobi}: something is laid");
        assert_eq!(one, run(4, jacobi), "jacobi {jacobi}");
    }
}

#[test]
fn a_cancelled_run_stops_before_routing() {
    let (mut b, _) = stripped(2);
    let before = violations(&b);
    let cancel = Arc::new(AtomicBool::new(true));
    let n = route_negotiated(
        &mut b,
        &mut CpuSolver,
        &NegotiationOptions { cell: CELL, pours_block: true, deadline: Deadline::unlimited(cancel.clone()), ..Default::default() },
        &mut |_| {},
        &mut |_, _| {},
    );
    assert_eq!((n.rounds, n.committed), (0, 0));
    let r = route_open(&mut b, &mut CpuSolver, &Options { cell: CELL, pours_block: true, deadline: Deadline::unlimited(cancel.clone()), ..Default::default() }, &mut |_| {});
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
        &NegotiationOptions { cell: CELL, pours_block: true, deadline: Deadline::max_seconds(Some(0.0)), ..Default::default() },
        &mut |_| {},
        &mut |_, _| {},
    );
    assert_eq!(n.rounds, 0);
    assert!(t.elapsed().as_secs_f64() < 30.0);
}

/// A second's budget on a whole board stripped bare, at the benchmark's cell: negotiation's
/// set-up (every connection's ends, the trees re-spanned) is under the deadline too, not only
/// its rounds.
#[test]
fn a_one_second_budget_holds_on_a_whole_board() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/bluebuzz-rm2.kicad_pcb");
    let mut b = Board::load(&path).unwrap().0;
    let copper: Vec<ItemId> = b.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via)).map(|(id, _)| id).collect();
    for id in copper {
        b.remove(id);
    }
    assert!(connectivity_without_pours(&b).airlines.len() > 100, "the fixture is a whole board to route");
    let t = std::time::Instant::now();
    route_negotiated(&mut b, &mut CpuSolver, &NegotiationOptions { deadline: Deadline::max_seconds(Some(1.0)), ..Default::default() }, &mut |_| {}, &mut |_, _| {});
    let took = t.elapsed().as_secs_f64();
    assert!(took < 3.0, "negotiation given 1 s took {took:.1} s");
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


#[test]
fn copper_leading_nowhere_is_pruned_and_connected_copper_is_kept() {
    let mut b = earring();
    // Every track and via the file has counts as the router's, and none of it dangles.
    let mut soft: Vec<ItemId> = b.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via)).map(|(id, _)| id).collect();
    let before = soft.len();
    let dangling_before: Vec<ItemId> = {
        let mut probe = b.clone();
        let mut s = soft.clone();
        prune_dangling(&mut probe, &mut s);
        soft.iter().copied().filter(|id| probe.item(*id).is_none()).collect()
    };
    // A stub off the end of a routed track of a net with no pour, to a point clear of its net,
    // then a via there meeting only the stub.
    let poured: BTreeSet<u32> = b.items().filter(|(_, it)| matches!(it.kind, Kind::Pour { .. })).map(|(_, it)| it.net).collect();
    let clear = |b: &Board, net: u32, p: Point| {
        let dot = Shape::Capsule { a: p, b: p, r: 300_000 };
        b.items().all(|(_, o)| o.net != net || !o.is_copper() || o.shape.gap_within(&dot, 0.0) > 0.0)
    };
    let (net, layers, from, far, r) = b
        .items()
        .find_map(|(id, it)| match (&it.kind, &it.shape) {
            (Kind::Track, Shape::Capsule { a, r, .. }) if !dangling_before.contains(&id) && !poured.contains(&it.net) => {
                let far = [(1, 0), (-1, 0), (0, 1), (0, -1)].iter().map(|(dx, dy)| Point::new(a.x + dx * 800_000, a.y + dy * 800_000)).find(|p| clear(&b, it.net, *p))?;
                Some((it.net, it.layers, *a, far, *r))
            }
            _ => None,
        })
        .unwrap();
    let stub = b.insert(Item { kind: Kind::Track, net, layers, shape: Shape::Capsule { a: from, b: far, r }, hole: None, locked: false, span: None });
    let via = b.insert(Item { kind: Kind::Via, net, layers: (1 << b.layers.len()) - 1, shape: Shape::Capsule { a: far, b: far, r: 200_000 }, hole: Some((far, 100_000)), locked: false, span: None });
    soft.extend([stub, via]);
    let (pruned, _) = prune_dangling(&mut b, &mut soft);
    assert!(b.item(stub).is_none() && b.item(via).is_none(), "the via meets only the stub, so both go");
    assert_eq!(pruned, 2 + dangling_before.len());
    assert_eq!(soft.len(), before - dangling_before.len());
}

#[test]
fn a_track_laid_on_an_identical_one_is_dropped_and_the_files_is_kept() {
    let mut b = earring();
    let (file_track, twin) = b.items().find(|(_, it)| it.kind == Kind::Track && it.net != 0).map(|(id, it)| (id, it.clone())).unwrap();
    let laid = b.insert(Item { span: None, ..twin });
    let mut soft = vec![laid];
    assert_eq!(drop_duplicates(&mut b, &mut soft).0, 1);
    assert!(b.item(laid).is_none() && b.item(file_track).is_some());
    assert!(soft.is_empty());
}

#[test]
fn pruning_never_opens_a_connection() {
    let mut b = earring();
    let open = |b: &Board| connectivity_without_pours(b).airlines.len();
    let before = open(&b);
    let mut soft: Vec<ItemId> = b.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via)).map(|(id, _)| id).collect();
    drop_duplicates(&mut b, &mut soft);
    prune_dangling(&mut b, &mut soft);
    assert!(open(&b) <= before, "{} open after pruning, {before} before", open(&b));
}

#[test]
fn route_open_and_negotiate_respect_net_filter_and_weights() {
    let (mut b, nets) = stripped(4);
    let chosen: Vec<u32> = nets.iter().copied().take(2).collect();
    let only: std::collections::HashSet<u32> = chosen.iter().copied().collect();
    let only_tree: BTreeSet<u32> = chosen.iter().copied().collect();
    let before = b.clone();

    let stats = route_open(
        &mut b,
        &mut CpuSolver,
        &Options { cell: CELL, nets: Some(only.clone()), pours_block: true, ..Default::default() },
        &mut |_| {},
    );
    assert!(stats.routed > 0, "{stats:?}");
    assert_clean_and_on_nets(&before, &b, &stats.laid, &only_tree);

    let (mut b2, nets2) = stripped(4);
    let before2 = b2.clone();
    let mut weights = std::collections::HashMap::new();
    weights.insert(*nets2.iter().next().unwrap(), 0.2);
    let n = route_negotiated(
        &mut b2,
        &mut CpuSolver,
        &NegotiationOptions { cell: CELL, pours_block: true, threads: 2, max_rounds: 6, nets: Some(only.clone()), net_weight: weights, ..Default::default() },
        &mut |_| {},
        &mut |_, _| {},
    );
    assert!(n.committed > 0, "{n:?}");
    assert_clean_and_on_nets(&before2, &b2, &n.laid, &only_tree);
}

#[test]
fn shared_cell_without_copper_refuses_instead_of_looping() {
    // A locked track crossing a pad's corner without ending in it is unjoined in the board model
    // yet shares free cells inside the overlap; if every other cell around them is walled off so
    // only the shared one-cell path exists, `route_open` must refuse the zero-piece path and stop.
    let src = r#"(kicad_pcb
      (version 20241229)
      (generator "pcbnew")
      (general (thickness 1.6))
      (layers (0 "F.Cu" signal) (31 "B.Cu" signal) (44 "Edge.Cuts" user))
      (net 0 "")
      (net 1 "SIG")
      (net 2 "WALL")
      (net 3 "WALL_B")
      (gr_rect (start 0 0) (end 40 40) (stroke (width 0.1) (type default)) (fill none) (layer "Edge.Cuts"))
      (footprint "Test:FP" (layer "F.Cu") (uuid "00000000-0000-0000-0000-000000000010") (at 20 20)
        (pad "1" smd rect (at 0 0) (size 2.0 2.0) (layers "F.Cu") (net 1 "SIG"))
        (pad "2" smd rect (at 0 0) (size 10.0 10.0) (layers "B.Cu") (net 3 "WALL_B"))
      )
      (segment (start 18.0 20.85) (end 22.0 20.85) (width 0.25) (locked yes) (layer "F.Cu") (net 1))
      (segment (start 17.5 18.5) (end 22.5 18.5) (width 1.2) (locked yes) (layer "F.Cu") (net 2))
      (segment (start 17.5 22.0) (end 22.5 22.0) (width 1.2) (locked yes) (layer "F.Cu") (net 2))
      (segment (start 17.5 18.5) (end 17.5 22.0) (width 1.2) (locked yes) (layer "F.Cu") (net 2))
      (segment (start 22.5 18.5) (end 22.5 22.0) (width 1.2) (locked yes) (layer "F.Cu") (net 2))
    )"#;
    let (pcb, copper) = kicad_pcb::parse_board(src).unwrap();
    let pro = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pro");
    let mut b = Board::new(&pcb, &copper, kicad_pcb::Rules::read(&pro).unwrap());
    assert_eq!(connectivity_without_pours(&b).airlines.len(), 1);
    let stats = route_open(
        &mut b,
        &mut CpuSolver,
        &Options { cell: 50_000, deadline: Deadline::max_seconds(Some(2.0)), ..Default::default() },
        &mut |_| {},
    );
    assert_eq!(stats.routed, 0, "{stats:?}");
    assert_eq!(stats.failed, 1, "{stats:?}");
    assert_eq!(stats.refused, 1, "{stats:?}");
}

#[test]
fn an_arcs_other_chords_are_written_when_one_goes() {
    let text = "(kicad_pcb (version 20240108) (generator \"pcbnew\") (general (thickness 1.6))
        (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal) (44 \"Edge.Cuts\" user))
        (net 0 \"\") (net 1 \"A\")
        (gr_rect (start 0 0) (end 30 30) (layer \"Edge.Cuts\") (width 0.1))
        (arc (start 5 15) (mid 15 5) (end 25 15) (width 0.2) (layer \"F.Cu\") (net 1) (uuid \"0b10c000-0000-4000-8000-0000000000c1\")))";
    let (pcb, copper) = kicad_pcb::parse_board(text).unwrap();
    let mut b = board::Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
    let chords: Vec<board::ItemId> = b.items().filter(|(_, it)| it.kind == board::Kind::Track).map(|(id, _)| id).collect();
    assert!(chords.len() > 2, "an arc is several chords");
    // A stage rips one chord: its span, the arc's, goes from the file.
    let gone = b.remove(chords[0]).unwrap();
    let removed = vec![gone.span.unwrap()];
    let mut laid = Vec::new();
    keep_orphans(&b, &removed, &mut laid);
    assert_eq!(laid.len(), chords.len() - 1, "every other chord is written anew");
    let (tracks, _) = edits(&b, &laid);
    assert_eq!(tracks.len(), chords.len() - 1);
}
