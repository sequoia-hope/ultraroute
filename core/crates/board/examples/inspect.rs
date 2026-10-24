//! Loads a board and reports what the model sees: `cargo run --release --example inspect -- b.kicad_pcb`.

use std::collections::BTreeMap;
use std::time::Instant;

use board::*;

fn main() {
    let path = std::env::args().nth(1).expect("a .kicad_pcb");
    let json = std::env::args().any(|a| a == "--json");
    let t = Instant::now();
    let (b, _) = Board::load(path.as_ref()).expect("load");
    let loaded = t.elapsed();
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for (_, it) in b.items() {
        *kinds.entry(format!("{:?}", it.kind).split([' ', '{', '(']).next().unwrap().to_string()).or_default() += 1;
    }
    println!("{} layers, {} nets, {kinds:?}, outline loops {}, loaded in {loaded:.1?}", b.layers.len(), b.nets.len(), b.outline.len());
    let t = Instant::now();
    let c = connectivity(&b);
    println!("{} open connections in {:.1?}", c.airlines.len(), t.elapsed());
    if std::env::args().any(|a| a == "--nets") {
        let mut per: BTreeMap<&str, usize> = BTreeMap::new();
        for a in &c.airlines {
            *per.entry(b.net_name(a.net)).or_default() += 1;
        }
        println!("NETS {per:?}");
    }
    for a in c.airlines.iter().take(8) {
        println!("  open {} {:?} -> {:?}", b.net_name(a.net), a.from, a.to);
    }
    let t = Instant::now();
    let v = check(&b);
    let mut by: BTreeMap<Rule, usize> = BTreeMap::new();
    for x in &v {
        *by.entry(x.rule).or_default() += 1;
    }
    if json {
        let rules: Vec<String> = by.iter().map(|(r, n)| format!("\"{r:?}\": {n}")).collect();
        println!("JSON {{\"open\": {}, \"violations\": {{{}}}}}", c.airlines.len(), rules.join(", "));
    }
    println!("{} violations in {:.1?}: {by:?}", v.len(), t.elapsed());
    for x in v.iter().take(12) {
        let name = |id: ItemId| {
            let it = b.item(id).unwrap();
            let what = match &it.kind {
                Kind::Pad { footprint, number, .. } => format!("pad {}-{number}", b.footprints[*footprint].reference),
                k => format!("{k:?}").split([' ', '{']).next().unwrap().to_string(),
            };
            format!("{what} [{}] {:?}", b.net_name(it.net), it.shape.bbox().min)
        };
        println!("  {:?} {} / {} gap {:.0} nm, needs {} nm", x.rule, name(x.a), x.b.map(name).unwrap_or_default(), x.gap, x.required);
    }
}
