//! A route depends on the board, not on how its file orders it: the earring with its items, nets,
//! footprints, pads and fills shuffled, its nets renumbered and its uuids made new routes to the
//! same copper, down to the uuids the seed gives what it lays.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use board::Board;
use kicad_pcb::parse_board;
use kicad_pcb::sexpr::{self, Node};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn shuffle<T>(&mut self, v: &mut [T]) {
        for k in (1..v.len()).rev() {
            v.swap(k, (self.next() % (k as u64 + 1)) as usize);
        }
    }
}

/// `node`'s text with the children `moves` picks changing places, and those of its footprints and
/// zones shuffled within them.
fn shuffled(text: &str, node: &Node, rng: &mut Rng) -> String {
    let head = node.head().unwrap_or("");
    let moves = |c: &Node| {
        let h = c.head().unwrap_or("");
        match head {
            "kicad_pcb" => matches!(h, "net" | "footprint" | "segment" | "arc" | "via" | "zone" | "group") || h.starts_with("gr_"),
            "footprint" => h == "pad" || h == "zone" || h.starts_with("fp_"),
            "zone" => h == "filled_polygon",
            _ => false,
        }
    };
    let kids = node.args();
    let mut pieces: Vec<String> = kids
        .iter()
        .map(|c| match (c.head(), matches!(head, "kicad_pcb" | "footprint")) {
            (Some("footprint" | "zone"), true) => shuffled(text, c, rng),
            _ => text[c.pos().start..c.pos().end].to_string(),
        })
        .collect();
    let slots: Vec<usize> = (0..kids.len()).filter(|&k| moves(&kids[k])).collect();
    let mut order = slots.clone();
    rng.shuffle(&mut order);
    let moved: Vec<String> = order.iter().map(|&k| std::mem::take(&mut pieces[k])).collect();
    for (&slot, piece) in slots.iter().zip(moved) {
        pieces[slot] = piece;
    }
    let (mut out, mut at) = (String::with_capacity(node.pos().end - node.pos().start), node.pos().start);
    for (kid, piece) in kids.iter().zip(&pieces) {
        out.push_str(&text[at..kid.pos().start]);
        out.push_str(piece);
        at = kid.pos().end;
    }
    out.push_str(&text[at..node.pos().end]);
    out
}

/// Every `(net N` under a new number, net 0 staying.
fn renumbered(text: &str, rng: &mut Rng) -> String {
    let mut numbers: Vec<u32> = Vec::new();
    let mut scan = text;
    while let Some(i) = scan.find("(net ") {
        scan = &scan[i + 5..];
        numbers.extend(scan[..scan.find([' ', ')']).unwrap_or(0)].parse::<u32>().ok().filter(|&n| n != 0));
    }
    numbers.sort_unstable();
    numbers.dedup();
    let mut to = numbers.clone();
    rng.shuffle(&mut to);
    let new: HashMap<u32, u32> = numbers.into_iter().zip(to).collect();
    let (mut out, mut rest) = (String::with_capacity(text.len()), text);
    while let Some(i) = rest.find("(net ") {
        out.push_str(&rest[..i + 5]);
        rest = &rest[i + 5..];
        let end = rest.find([' ', ')']).unwrap_or(0);
        if let Some(n) = rest[..end].parse::<u32>().ok().and_then(|n| new.get(&n)) {
            out.push_str(&n.to_string());
            rest = &rest[end..];
        }
    }
    out.push_str(rest);
    out
}

/// Every uuid made another, the same one wherever it stands.
fn new_uuids(text: &str, rng: &mut Rng) -> String {
    let is_uuid = |s: &[u8]| s.iter().enumerate().all(|(k, c)| if matches!(k, 8 | 13 | 18 | 23) { *c == b'-' } else { c.is_ascii_hexdigit() });
    let bytes = text.as_bytes();
    let mut seen: HashMap<&str, String> = HashMap::new();
    let (mut out, mut k, mut kept) = (String::with_capacity(text.len()), 0, 0);
    while k + 36 <= bytes.len() {
        if bytes[k + 8] == b'-' && is_uuid(&bytes[k..k + 36]) {
            out.push_str(&text[kept..k]);
            let new = seen.entry(&text[k..k + 36]).or_insert_with(|| {
                let (a, b) = (rng.next(), rng.next());
                format!("{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}", a as u32, (a >> 32) as u16, (a >> 48) & 0xfff, b & 0xfff, b >> 16)
            });
            out.push_str(new);
            k += 36;
            kept = k;
        } else {
            k += 1;
        }
    }
    out.push_str(&text[kept..]);
    out
}

fn reordered(text: &str, seed: u64) -> String {
    let mut rng = Rng(seed);
    let root = sexpr::parse(text).unwrap();
    let out = shuffled(text, &root, &mut rng);
    let out = renumbered(&out, &mut rng);
    new_uuids(&out, &mut rng)
}

/// A board's file in `dir`, with the project file beside it.
fn route(text: &str, dir: &Path, project: &Path) -> String {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("board.kicad_pcb");
    std::fs::write(&path, text).unwrap();
    std::fs::copy(project, path.with_extension("kicad_pro")).unwrap();
    let args: Vec<String> = ["route", path.to_str().unwrap(), "-o", "unused.kicad_pcb", "--strip-all", "--seed", "0", "--uuid-seed", "1", "--recipe", "default", "--threads", "1", "--timeout", "1000", "--cpu-clock"]
        .map(String::from)
        .to_vec();
    let a = ultraroute::parse(&args).unwrap();
    let (board, text) = Board::load(&path).unwrap();
    ultraroute::run(&a, board, &text, None, Arc::new(AtomicBool::new(false)))
}

/// What a routed file's copper is, each piece by its net's name, as text so that lists compare.
fn copper(out: &str) -> Vec<String> {
    let (pcb, copper) = parse_board(out).unwrap();
    let name = |n: u32| pcb.nets.iter().find(|x| x.number == n).map_or("?", |x| x.name.as_str());
    let tracks = copper.tracks.iter().map(|t| format!("track {} {} {:?} {:?} {} {}", name(t.net), t.layer, t.start, t.end, t.width, t.uuid));
    let vias = copper.vias.iter().map(|v| format!("via {} {:?} {:?} {} {} {}", name(v.net), v.layers, v.at, v.size, v.drill, v.uuid));
    let zones = copper.zones.iter().map(|z| format!("zone {} {:?} {:?}", z.net_name, z.layers, z.filled));
    let parts = pcb.footprints.iter().map(|f| format!("part {} {:?} {}", f.reference, f.at, f.rotation_deg));
    tracks.chain(vias).chain(zones).chain(parts).collect()
}

/// The order new items are written in.
fn written(out: &str) -> Vec<String> {
    let root = sexpr::parse(out).unwrap();
    root.args().iter().filter(|c| matches!(c.head(), Some("segment" | "via"))).filter_map(|c| c.find("uuid")?.args().first()?.text().map(str::to_string)).collect()
}

/// That the numbers written are the file's: read back through the file's own table, tracks that
/// start on a pad's centre start on a pad of their net.
fn on_pads_of_their_nets(out: &str) {
    let (pcb, copper) = parse_board(out).unwrap();
    let pads: Vec<(u32, [f64; 2])> = pcb.footprints.iter().flat_map(|f| &f.pads).filter_map(|p| p.net.as_ref().map(|n| (n.number, p.position))).collect();
    let on = |t: &kicad_pcb::Track, own: bool| pads.iter().any(|(n, at)| (*n == t.net) == own && *n != 0 && (at[0] - t.start[0]).hypot(at[1] - t.start[1]) < 1e-6);
    assert!(copper.tracks.iter().filter(|t| on(t, true)).count() > 20, "tracks start on pads of their nets");
    assert_eq!(copper.tracks.iter().filter(|t| on(t, false)).count(), 0, "no track starts on a pad of another net");
}

#[test]
fn a_shuffled_file_routes_to_the_same_copper() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let (text, project) = (std::fs::read_to_string(root.join("earring.kicad_pcb")).unwrap(), root.join("earring.kicad_pro"));
    let base = std::env::temp_dir().join(format!("ultraroute-order-free-{}", std::process::id()));
    let seeds = [0x1234_5678_9abc_def1_u64, 0x0fed_cba9_8765_4321, 0x5555_aaaa_3333_cccc];
    let mut inputs = vec![text.clone()];
    inputs.extend(seeds.iter().map(|&s| reordered(&text, s)));
    // That the shuffle does what it says: the footprints, nets and uuids stand otherwise.
    let firsts = |t: &str| {
        let root = sexpr::parse(t).unwrap();
        let of = |h: &str| root.find_all(h).map(|c| t[c.pos().start..c.pos().end].to_string()).take(5).collect::<Vec<_>>();
        (of("footprint"), of("net"))
    };
    for t in &inputs[1..] {
        let (a, b) = (firsts(&inputs[0]), firsts(t));
        assert_ne!(a.0, b.0, "footprints moved");
        assert_ne!(a.1, b.1, "nets moved");
    }
    // At once: each run is half a minute.
    let outs: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = inputs.iter().enumerate().map(|(k, t)| { let (base, project) = (&base, &project); scope.spawn(move || route(t, &base.join(k.to_string()), project)) }).collect();
        handles.into_iter().map(|h| h.join().expect("no run panics")).collect()
    });
    let _ = std::fs::remove_dir_all(&base);
    assert!(outs[0].contains("(segment"), "the earring is routed");
    let reference = copper(&outs[0]);
    assert!(reference.iter().any(|l| l.starts_with("track ")), "copper is read back");
    for (k, out) in outs.iter().enumerate().skip(1) {
        let got = copper(out);
        if let Some((a, b)) = reference.iter().zip(&got).find(|(a, b)| a != b) {
            panic!("shuffle {k} differs: {} vs {}", &a[..a.len().min(200)], &b[..b.len().min(200)]);
        }
        assert_eq!(reference.len(), got.len(), "shuffle {k}");
        assert_eq!(written(&outs[0]), written(out), "shuffle {k} writes its new items in the same order");
        on_pads_of_their_nets(out);
    }
}

/// KiCad 10 names nets in place: the number each is read under means nothing to the file, and
/// what is laid goes back under the right name.
#[test]
fn a_board_that_names_its_nets_gets_new_copper_by_name() {
    let text = "(kicad_pcb (version 20260206) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal))
        (footprint \"R\" (layer \"F.Cu\") (uuid \"f\") (at 10 10) (property \"Reference\" \"R1\")
          (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net \"ZED\"))
          (pad \"2\" smd rect (at 2 0) (size 1 1) (layers \"F.Cu\") (net \"ALPHA\"))))";
    let (pcb, copper) = parse_board(text).unwrap();
    let mut b = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
    let zed = b.nets.iter().position(|n| n == "ZED").unwrap() as u32;
    assert_eq!(zed, 2, "ALPHA sorts first though the file met ZED first");
    let (a, c) = (board::Point::new(10_000_000, 10_000_000), board::Point::new(10_000_000, 12_000_000));
    let id = b.insert(board::Item { kind: board::Kind::Track, net: zed, layers: 1, shape: board::Shape::Capsule { a, b: c, r: 100_000 }, hole: None, locked: false, span: None });
    let (tracks, vias) = grid::edits(&b, &[id]);
    let out = kicad_pcb::apply(text, &kicad_pcb::Edit { tracks, vias, ..Default::default() });
    let root = sexpr::parse(&out).unwrap();
    let seg = root.find("segment").expect("the track is written");
    assert_eq!(seg.find("net").unwrap().args()[0].text(), Some("ZED"));
}
