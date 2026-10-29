//! Blame on a real board: route the earring on a coarse grid, then ask, for every connection left
//! open, what is in the way. Whatever blame names must really be there: parts beside the
//! connection, nets with routed copper of their own, and the board untouched afterwards.

use std::path::PathBuf;

use board::*;
use ::grid::*;

/// Cells coarse enough that a debug build routes in seconds; the earring's tracks still fit.
const CELL: Nm = 100_000;

fn earring() -> Board {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pcb");
    Board::load(&path).unwrap().0
}

/// The earring with its own tracks and vias kept and the rest routed on the coarse grid. Pours
/// are left to be refilled, as the command line routes by default: kept as planes, the earring's
/// pours wall off nearly every connection, and blame has nothing to say but that.
fn routed() -> Board {
    let mut b = earring();
    route_open(&mut b, &mut CpuSolver, &Options { cell: CELL, ..Default::default() }, &mut |_| {});
    b
}

fn options() -> BlameOptions {
    BlameOptions { cell: CELL, ..Default::default() }
}

/// How near a part's pads come to the straight line between a connection's ends.
fn part_distance(b: &Board, footprint: usize, airline: &Airline) -> f64 {
    b.items()
        .filter(|(_, it)| matches!(it.kind, Kind::Pad { footprint: f, .. } if f == footprint))
        .map(|(_, it)| it.shape.gap(&Shape::Capsule { a: airline.from, b: airline.to, r: 0 }))
        .fold(f64::INFINITY, f64::min)
}

#[test]
fn blame_on_the_routed_earring_names_real_blockers() {
    let mut b = routed();
    let open = connectivity_without_pours(&b).airlines;
    let locked = locked(&b);
    assert!(!open.is_empty(), "the coarse grid leaves the earring with nothing open; nothing to blame");
    let before: Vec<(ItemId, Item)> = b.items().map(|(id, it)| (id, it.clone())).collect();
    let routed_nets: std::collections::BTreeSet<u32> =
        b.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via)).map(|(_, it)| it.net).collect();
    let mut named = 0;
    for airline in &open {
        let r = blame(&mut b, airline.from_item, airline.to_item, &options(), &|f| !locked[f]);
        let what = format!("{} {:?}", b.net_name(airline.net), r);
        if r.walled_off {
            continue;
        }
        // Ends whose free cells meet are one cell apart: the grid joins them, the copper does not.
        assert!(r.path_cells >= 1, "{what}");
        assert!(r.end_cells.0 > 0 && r.end_cells.1 > 0, "{what}");
        assert!(r.intruded_cells + r.pinned_cells + r.routed_cells <= 3 * r.path_cells, "{what}");
        named += usize::from(!r.blamed.is_empty() || !r.unmovable.is_empty() || !r.pinned_parts.is_empty() || !r.routed_nets.is_empty());
        let ends: Vec<Option<usize>> = [airline.from_item, airline.to_item]
            .iter()
            .map(|id| match b.item(*id).map(|it| &it.kind) {
                Some(Kind::Pad { footprint, .. }) => Some(*footprint),
                _ => None,
            })
            .collect();
        let span = airline.from.distance(airline.to);
        for blamed in &r.blamed {
            assert!(blamed.footprint < b.footprints.len(), "{what}");
            assert!(!ends.contains(&Some(blamed.footprint)), "a part at an end is blamed: {what}");
            assert!(!b.footprints[blamed.footprint].locked, "a locked part is blamed: {what}");
            let length = blamed.dx.hypot(blamed.dy);
            assert!(length > 0.0 && length <= options().max_move as f64, "{what}");
            // A part in the way of the cheapest path is near the connection, not across the board.
            assert!(part_distance(&b, blamed.footprint, airline) <= span + 2_000_000.0, "{what}");
        }
        for f in r.pinned_parts.iter().chain(&r.unmovable) {
            assert!(*f < b.footprints.len(), "{what}");
            assert!(part_distance(&b, *f, airline) <= span + 2_000_000.0, "{what}");
        }
        for net in &r.routed_nets {
            assert_ne!(*net, airline.net, "a connection is blocked by its own net: {what}");
            assert!(routed_nets.contains(net), "net {net} has no routed copper: {what}");
        }
        if r.routed_cells == 0 {
            assert!(r.routed_nets.is_empty(), "{what}");
        }
    }
    assert!(named > 0, "no open connection had anything named in its way");
    let after: Vec<(ItemId, Item)> = b.items().map(|(id, it)| (id, it.clone())).collect();
    assert_eq!(before, after, "blame left the board as it found it");
}

/// Which parts may not move, read before blame borrows the board.
fn locked(b: &Board) -> Vec<bool> {
    b.footprints.iter().map(|f| f.locked).collect()
}

#[test]
fn a_part_moved_as_blame_asks_no_longer_stands_in_that_path() {
    // Take a connection with a part to blame, move the part as asked, and blame again: the part
    // is no longer in the way, or at least the path cuts less deeply into parts. Routed copper is
    // a wall here, so only moving parts can open the path.
    let mut b = routed();
    let open = connectivity_without_pours(&b).airlines;
    let locked = locked(&b);
    let walled = BlameOptions { routed_copper: None, ..options() };
    let found = open.iter().find_map(|a| {
        let r = blame(&mut b, a.from_item, a.to_item, &walled, &|f| !locked[f]);
        r.blamed.first().cloned().map(|bl| (a.clone(), bl, r))
    });
    // The coarse routed earring leaves connections with a movable part in the way; if that stops
    // being so, this test has lost its case and must say so rather than pass.
    let Some((airline, first, report)) = found else {
        panic!("no open connection on the routed earring has a movable part to blame, so there is nothing to move");
    };
    b.move_footprint(first.footprint, Point::new(first.dx.round() as Nm, first.dy.round() as Nm));
    let again = blame(&mut b, airline.from_item, airline.to_item, &walled, &|f| !locked[f]);
    eprintln!("moved footprint {} by ({:.0}, {:.0}) nm\nbefore {report:?}\nafter {again:?}", first.footprint, first.dx, first.dy);
    let still = again.blamed.iter().any(|x| x.footprint == first.footprint) || again.unmovable.contains(&first.footprint);
    assert!(!still || again.intruded_cells < report.intruded_cells, "before {report:?}\nafter {again:?}");
}

#[test]
fn routed_copper_as_a_wall_is_never_crossed() {
    let mut b = routed();
    let open = connectivity_without_pours(&b).airlines;
    let locked = locked(&b);
    for a in open.iter().take(4) {
        let r = blame(&mut b, a.from_item, a.to_item, &BlameOptions { routed_copper: None, ..options() }, &|f| !locked[f]);
        assert_eq!(r.routed_cells, 0, "{r:?}");
        assert!(r.routed_nets.is_empty());
    }
}
