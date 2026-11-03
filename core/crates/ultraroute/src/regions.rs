//! `--regions FILE`: a planner's routing regions and length limits for the board, as JSON
//! (circuit-composer's routing.json, version 1):
//!
//! ```json
//! {"version": 1,
//!  "regions": [{"name": "...", "polygon": [[x, y], ...], "layers": ["F.Cu"],
//!               "rules": [{"nets": ["/FB"], "classes": ["SWITCH"], "kind": "never|must|avoid|prefer",
//!                          "weight": 2.0, "vias": true}]}],
//!  "max_lengths": [{"nets": ["/XIN"], "max_length_mm": 10}],
//!  "clear_under": [{"nets": ["/USB_DP", "/USB_DM"], "classes": ["USB"], "layers": ["F.Cu"],
//!                   "planes": ["In1.Cu"], "margin_mm": 0.0}]}
//! ```
//!
//! Polygons are in millimetres in KiCad's frame (y down); `layers` defaults to every copper
//! layer; a rule holds for the nets it names and every net of the classes it names, `"*"` for
//! all. Keys the router does not use (`generator`, `why`, ...) are ignored. Lengths are checked
//! and reported, not yet routed to.
//!
//! A `clear_under` rule keeps the antipads other nets' vias and plated holes cut in the zones on
//! `planes` at least `margin_mm` (default 0) from its nets' tracks on `layers`, the tracks' full
//! width: whichever is laid first keeps the other off, and the nets route first (see
//! [`board::Board::antipads`] for the antipad, and `grid::under` for how the searches see it).
//! A hole of the zone's own net cuts nothing; where no zone of another net is, nor does any.

use std::collections::{BTreeSet, HashSet};

use board::{Board, ClearUnder, Kind, Point, Region, RegionKind, RegionRule, Shape};
use serde_json::Value;

/// A net's length limit.
pub struct MaxLength {
    pub nets: Vec<u32>,
    pub mm: f64,
}

/// Reads the file into the board's regions; the length limits, for `report`.
pub fn load(path: &str, board: &mut Board) -> Result<Vec<MaxLength>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("--regions {path}: {e}"))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("--regions {path}: {e}"))?;
    if v["version"].as_u64() != Some(1) {
        return Err(format!("--regions {path}: version 1 expected"));
    }
    let net_of = |name: &str| (1..board.nets.len() as u32).find(|&n| board.net_name(n) == name);
    let strs = |v: &Value| v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect::<Vec<String>>();
    let mut unknown = Vec::new();
    let mut regions = Vec::new();
    for r in v["regions"].as_array().into_iter().flatten() {
        let name = r["name"].as_str().unwrap_or("").to_string();
        let polygon: Vec<Point> = r["polygon"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some(Point::new((p[0].as_f64()? * 1e6).round() as i64, (p[1].as_f64()? * 1e6).round() as i64)))
            .collect();
        if polygon.len() < 3 {
            return Err(format!("--regions {path}: region {name} has no polygon"));
        }
        let names = strs(&r["layers"]);
        let mut layers = 0u64;
        for l in &names {
            match board.layers.iter().position(|b| b == l) {
                Some(i) => layers |= 1 << i,
                None => return Err(format!("--regions {path}: region {name}: the board has no copper layer {l}")),
            }
        }
        if names.is_empty() {
            layers = (1u64 << board.layers.len()) - 1;
        }
        let mut rules = Vec::new();
        for rule in r["rules"].as_array().into_iter().flatten() {
            let classes = strs(&rule["classes"]);
            let nets: Option<HashSet<u32>> = if classes.iter().any(|c| c == "*") {
                None
            } else {
                let mut set = HashSet::new();
                for n in strs(&rule["nets"]) {
                    match net_of(&n) {
                        Some(id) => {
                            set.insert(id);
                        }
                        None => unknown.push(n),
                    }
                }
                for n in 1..board.nets.len() as u32 {
                    if classes.iter().any(|c| board.rules.in_class(board.net_name(n), c)) {
                        set.insert(n);
                    }
                }
                Some(set)
            };
            let weight = rule["weight"].as_f64().unwrap_or(1.0);
            let kind = match rule["kind"].as_str() {
                Some("never") => RegionKind::Never,
                Some("must") => RegionKind::Must,
                Some("avoid") => RegionKind::Avoid(weight),
                Some("prefer") => RegionKind::Prefer(weight),
                other => return Err(format!("--regions {path}: region {name}: kind {other:?}, not never, must, avoid or prefer")),
            };
            rules.push(RegionRule { nets, kind, vias: rule["vias"].as_bool().unwrap_or(false) });
        }
        regions.push(Region { name, shape: Shape::Polygon(polygon), layers, rules });
    }
    let mut lengths = Vec::new();
    for m in v["max_lengths"].as_array().into_iter().flatten() {
        let nets: Vec<u32> = strs(&m["nets"]).iter().filter_map(|n| net_of(n).or_else(|| { unknown.push(n.clone()); None })).collect();
        if let Some(mm) = m["max_length_mm"].as_f64() {
            lengths.push(MaxLength { nets, mm });
        }
    }
    let mut under = Vec::new();
    for c in v["clear_under"].as_array().into_iter().flatten() {
        let mask = |key: &str| -> Result<u64, String> {
            let names = strs(&c[key]);
            if names.is_empty() {
                return Err(format!("--regions {path}: clear_under: no {key}"));
            }
            names.iter().try_fold(0u64, |m, l| match board.layers.iter().position(|b| b == l) {
                Some(i) => Ok(m | 1 << i),
                None => Err(format!("--regions {path}: clear_under: the board has no copper layer {l}")),
            })
        };
        let (layers, planes) = (mask("layers")?, mask("planes")?);
        let classes = strs(&c["classes"]);
        let mut nets: HashSet<u32> = strs(&c["nets"]).iter().filter_map(|n| net_of(n).or_else(|| { unknown.push(n.clone()); None })).collect();
        nets.extend((1..board.nets.len() as u32).filter(|&n| classes.iter().any(|k| board.rules.in_class(board.net_name(n), k))));
        let margin = (c["margin_mm"].as_f64().unwrap_or(0.0).max(0.0) * 1e6).round() as board::Nm;
        under.push(ClearUnder { nets, layers, planes, margin });
    }
    if !unknown.is_empty() {
        eprintln!("regions: nets the board does not have: {}", unknown.join(", "));
    }
    for r in &regions {
        eprintln!("regions: {}: {} rules", r.name, r.rules.len());
    }
    for c in &under {
        let mut names: Vec<&str> = c.nets.iter().map(|n| board.net_name(*n)).collect();
        names.sort_unstable();
        eprintln!("regions: clear under {}: no other net's antipad within {} mm", names.join("+"), c.margin as f64 * 1e-6);
    }
    board.regions = regions;
    board.clear_under = under;
    Ok(lengths)
}

/// What breaks the regions' hard rules or the length limits, one line each, naming the region
/// or net: routed copper (`laid`) of a `never` net inside its region, or of a `must` net outside,
/// as `Region::breaks` says, the same test the router's checker refuses new copper by. Each line
/// comes with the names of the nets it is about.
pub fn report(board: &Board, lengths: &[MaxLength], laid: &[board::ItemId]) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for region in &board.regions {
        for rule in &region.rules {
            let mut broken = 0;
            let mut nets: BTreeSet<u32> = BTreeSet::new();
            for it in laid.iter().filter_map(|id| board.item(*id)) {
                if region.breaks(rule, it) {
                    broken += 1;
                    nets.insert(it.net);
                }
            }
            if broken > 0 {
                let line = format!("region {}: {broken} pieces break its {:?}{} rule", region.name, rule.kind, if rule.vias { " (vias)" } else { "" });
                out.push((line, nets.iter().map(|n| board.net_name(*n).to_string()).collect()));
            }
        }
    }
    // A clear-under rule's nets' tracks, laid or fixed, over another net's antipad: how many,
    // whose holes, on which planes, and where the first lies.
    for (k, rule) in board.clear_under.iter().enumerate() {
        let (mut tracks, mut nets, mut holders, mut planes, mut at) = (0, BTreeSet::new(), BTreeSet::new(), BTreeSet::new(), None);
        let mut ids: Vec<board::ItemId> = board.items().filter(|(_, it)| it.kind == Kind::Track && rule.nets.contains(&it.net)).map(|(id, _)| id).collect();
        ids.sort_unstable();
        for id in ids {
            let found: Vec<_> = board.clear_under_breaks(id, false).into_iter().filter(|b| b.0 == k).collect();
            if let Some(&(_, other, ..)) = found.first() {
                tracks += 1;
                nets.insert(board.item(id).map_or(0, |it| it.net));
                at = at.or(board.item(other).and_then(|o| o.hole).map(|h| h.0));
            }
            for (_, other, layer, _) in found {
                holders.insert(board.net_name(board.item(other).map_or(0, |o| o.net)).to_string());
                planes.insert(board.layers[layer].clone());
            }
        }
        if let Some(p) = at {
            let names: Vec<String> = nets.iter().map(|n| board.net_name(*n).to_string()).collect();
            let holders: Vec<String> = holders.into_iter().collect();
            let planes: Vec<String> = planes.into_iter().collect();
            let line = format!(
                "clear under {}: {tracks} tracks over antipads of {} on {}, the first at ({:.3}, {:.3})",
                names.join("+"),
                holders.join(", "),
                planes.join(", "),
                p.x as f64 * 1e-6,
                p.y as f64 * 1e-6
            );
            out.push((line, names));
        }
    }
    for m in lengths {
        let length: f64 = board
            .items()
            .filter(|(_, it)| m.nets.contains(&it.net) && it.kind == Kind::Track)
            .map(|(_, it)| match it.shape {
                Shape::Capsule { a, b, .. } => ((b.x - a.x) as f64).hypot((b.y - a.y) as f64) * 1e-6,
                _ => 0.0,
            })
            .sum();
        if length > m.mm {
            let names: Vec<&str> = m.nets.iter().map(|n| board.net_name(*n)).collect();
            let line = format!("length: {} is {length:.2} mm, over its {} mm", names.join("+"), m.mm);
            out.push((line, names.iter().map(|n| n.to_string()).collect()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use board::Item;

    #[test]
    fn clear_under_is_read_and_a_track_over_an_antipad_is_reported() {
        let (pcb, copper) = kicad_pcb::parse_board(
            r#"(kicad_pcb (version 20240101) (general (thickness 1.6))
            (layers (0 "F.Cu" signal) (1 "In1.Cu" signal) (31 "B.Cu" signal))
            (net 0 "") (net 1 "/GND") (net 2 "/USB_DP") (net 3 "/SIG")
            (gr_rect (start 0 0) (end 20 20) (layer "Edge.Cuts") (width 0.1))
            (zone (net 1) (net_name "/GND") (layer "In1.Cu") (uuid "z") (connect_pads (clearance 0.3))
              (min_thickness 0.2) (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20)))))"#,
        )
        .unwrap();
        let mut b = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
        let path = std::env::temp_dir().join(format!("clear-under-{}.json", std::process::id()));
        let json = r#"{"version": 1, "clear_under": [{"nets": ["/USB_DP", "/USB_DM"], "layers": ["F.Cu"], "planes": ["In1.Cu"], "margin_mm": 0.1}]}"#;
        std::fs::write(&path, json).unwrap();
        load(path.to_str().unwrap(), &mut b).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(b.clear_under, vec![ClearUnder { nets: [2].into(), layers: 1, planes: 2, margin: 100_000 }], "/USB_DM is not on the board");
        let p = |x: i64, y: i64| Point::new(x * 1_000, y * 1_000);
        b.insert(Item { kind: Kind::Track, net: 2, layers: 1, shape: Shape::Capsule { a: p(2_000, 10_000), b: p(18_000, 10_000), r: 100_000 }, hole: None, locked: false, span: None });
        assert!(report(&b, &[], &[]).is_empty());
        let c = p(10_000, 10_700);
        b.insert(Item { kind: Kind::Via, net: 3, layers: 0b111, shape: Shape::Capsule { a: c, b: c, r: 300_000 }, hole: Some((c, 150_000)), locked: false, span: None });
        let lines = report(&b, &[], &[]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].0, "clear under /USB_DP: 1 tracks over antipads of /SIG on In1.Cu, the first at (10.000, 10.700)");
        assert_eq!(lines[0].1, vec!["/USB_DP".to_string()]);
    }
}
