//! A KiCad group is a block: its pours are kept as planes other nets keep clear of, and its parts
//! are held still.

use std::path::PathBuf;

use board::*;

const POURS: [&str; 4] = ["vreg_vdd_core_3", "vreg_vdd_core_6", "vreg_3v3_1", "vreg_3v3_7"];

/// The earring with its RP2350 power pours, U1 and L1 grouped, as `tools/add_block.py` groups them.
fn grouped() -> (Board, Board) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let text = std::fs::read_to_string(dir.join("earring.kicad_pcb")).unwrap();
    let rules = || kicad_pcb::Rules::read(&dir.join("earring.kicad_pro")).unwrap();
    let (pcb, copper) = kicad_pcb::parse_board(&text).unwrap();
    let plain = Board::new(&pcb, &copper, rules());
    let mut members: Vec<String> = copper.zones.iter().filter(|z| POURS.contains(&z.name.as_str())).map(|z| z.uuid.clone()).collect();
    assert_eq!(members.len(), 4);
    members.extend(pcb.footprints.iter().filter(|f| f.reference == "U1" || f.reference == "L1").map(|f| f.uuid.clone()));
    let listed: Vec<String> = members.iter().map(|m| format!("\"{m}\"")).collect();
    let close = text.trim_end().rfind(')').unwrap();
    let text = format!("{}\t(group \"rp2350_power\" (uuid \"0b10c000-0000-4000-8000-000000000001\") (members {}))\n{}", &text[..close], listed.join(" "), &text[close..]);
    let (pcb, copper) = kicad_pcb::parse_board(&text).unwrap();
    (plain, Board::new(&pcb, &copper, rules()))
}

fn part(b: &Board, reference: &str) -> usize {
    b.footprints.iter().position(|f| f.reference == reference).unwrap()
}

#[test]
fn a_group_is_a_block_of_its_parts_and_zones_and_holds_the_parts_still() {
    let (plain, b) = grouped();
    assert!(plain.blocks.is_empty());
    let [block] = b.blocks.as_slice() else { panic!("{:?}", b.blocks) };
    assert_eq!(block.name, "rp2350_power");
    assert_eq!(block.zones.len(), 4);
    let mut parts = block.footprints.clone();
    parts.sort_unstable();
    let mut expected = vec![part(&b, "U1"), part(&b, "L1")];
    expected.sort_unstable();
    assert_eq!(parts, expected);
    assert!(b.footprints[part(&b, "U1")].locked && b.footprints[part(&b, "L1")].locked);
    assert!(!plain.footprints[part(&plain, "U1")].locked);
}

#[test]
fn only_a_blocks_pours_are_kept() {
    let (plain, b) = grouped();
    let kept = |b: &Board| b.items().filter(|(_, it)| matches!(it.kind, Kind::Pour { kept: true, .. })).count();
    assert_eq!(kept(&plain), 0);
    assert!(kept(&b) >= 4, "each of the four zones pours at least one polygon");
    assert!(b.items().any(|(_, it)| it.refilled()), "the ground pours are still refilled");
}

/// A via of `net` at the point of a kept pour furthest from its edges.
fn via_in_a_kept_pour(b: &Board, net: impl Fn(&Item) -> u32) -> Item {
    let (_, pour) = b.items().filter(|(_, it)| matches!(it.kind, Kind::Pour { kept: true, .. }) && it.on(0)).max_by_key(|(_, it)| {
        let bb = it.shape.bbox();
        (bb.max.x - bb.min.x).min(bb.max.y - bb.min.y)
    }).unwrap();
    let Shape::Polygon(points) = &pour.shape else { panic!("a pour is a polygon") };
    let depth = |p: Point| (0..points.len()).map(|i| point_segment(p, points[i], points[(i + 1) % points.len()])).fold(f64::INFINITY, f64::min);
    let bb = pour.shape.bbox();
    let step = 50_000;
    let at = (bb.min.x..bb.max.x)
        .step_by(step)
        .flat_map(|x| (bb.min.y..bb.max.y).step_by(step).map(move |y| Point::new(x, y)))
        .filter(|p| pour.shape.contains(*p))
        .max_by(|p, q| depth(*p).total_cmp(&depth(*q)))
        .unwrap();
    assert!(depth(at) > 200_000.0, "the pour has room for a via well inside it: {}", depth(at));
    Item { kind: Kind::Via, net: net(pour), layers: 0b11, shape: Shape::Capsule { a: at, b: at, r: 100_000 }, hole: Some((at, 50_000)), locked: false, span: None }
}

#[test]
fn another_nets_via_in_a_kept_pour_breaks_a_rule_and_its_own_nets_does_not() {
    let (_, mut b) = grouped();
    let foreign = via_in_a_kept_pour(&b, |pour| (1..b.nets.len() as u32).find(|n| *n != pour.net && !b.nets[*n as usize].is_empty()).unwrap());
    let own = via_in_a_kept_pour(&b, |pour| pour.net);
    let id = b.insert(foreign);
    assert!(
        check_item(&b, id).iter().any(|v| b.item(v.b.unwrap_or(usize::MAX)).is_some_and(|o| matches!(o.kind, Kind::Pour { kept: true, .. }))),
        "a foreign via in a kept pour clashes with it"
    );
    b.remove(id);
    let id = b.insert(own);
    let clashes: Vec<String> = check_item(&b, id).iter().filter_map(|v| b.item(v.b?).map(|o| format!("{:?} net {} {:?} gap {} of {}", o.kind, b.nets[o.net as usize], v.rule, v.gap, v.required))).collect();
    assert!(clashes.is_empty(), "a via of the pour's own net joins it: {clashes:?} (via net {})", b.nets[b.item(id).unwrap().net as usize]);
}

#[test]
fn a_kept_pour_joins_its_net_where_refilled_pours_join_nothing() {
    let (plain, b) = grouped();
    let net = b.items().find(|(_, it)| matches!(it.kind, Kind::Pour { kept: true, .. })).map(|(_, it)| it.net).unwrap();
    let open = |b: &Board| connectivity_without_pours(b).airlines.iter().filter(|a| b.item(a.from_item).is_some_and(|it| it.net == net)).count();
    assert!(open(&b) <= open(&plain));
}
