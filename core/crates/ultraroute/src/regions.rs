//! `--regions FILE`: a planner's routing regions and length limits for the board, as JSON
//! (circuit-composer's routing.json, version 1):
//!
//! ```json
//! {"version": 1,
//!  "regions": [{"name": "...", "polygon": [[x, y], ...], "layers": ["F.Cu"],
//!               "rules": [{"nets": ["/FB"], "classes": ["SWITCH"], "kind": "never|must|avoid|prefer",
//!                          "weight": 2.0, "vias": true}]}],
//!  "max_lengths": [{"nets": ["/XIN"], "max_length_mm": 10}]}
//! ```
//!
//! Polygons are in millimetres in KiCad's frame (y down); `layers` defaults to every copper
//! layer; a rule holds for the nets it names and every net of the classes it names, `"*"` for
//! all. Keys the router does not use (`generator`, `why`, ...) are ignored. Lengths are checked
//! and reported, not yet routed to.

use std::collections::{BTreeSet, HashSet};

use board::{Board, Kind, Point, Region, RegionKind, RegionRule, Shape};
use serde_json::Value;

/// How far copper may reach over a region's edge and still count as outside it, as KiCad's
/// geometry allows, in nanometres.
const TOLERANCE: f64 = 500.0;

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
                    if board.rules.class_of(board.net_name(n)).is_some_and(|c| classes.contains(&c.name)) {
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
    if !unknown.is_empty() {
        eprintln!("regions: nets the board does not have: {}", unknown.join(", "));
    }
    for r in &regions {
        eprintln!("regions: {}: {} rules", r.name, r.rules.len());
    }
    board.regions = regions;
    Ok(lengths)
}

/// What breaks the regions' hard rules or the length limits, one line each, naming the region
/// or net: routed copper (`laid`) of a `never` net inside its region, or of a `must` net outside.
/// Each line comes with the names of the nets it is about.
pub fn report(board: &Board, lengths: &[MaxLength], laid: &[board::ItemId]) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for region in &board.regions {
        for rule in &region.rules {
            let mut broken = 0;
            let mut nets: BTreeSet<u32> = BTreeSet::new();
            for it in laid.iter().filter_map(|id| board.item(*id)) {
                let vias_rule = it.kind == Kind::Via;
                if vias_rule != rule.vias || !rule.holds_for(it.net) || it.layers & region.layers == 0 {
                    continue;
                }
                // Copper reaches into the region when its centre line comes nearer than its
                // radius, by more than KiCad's half micron: touching the edge is outside.
                let touches = match &it.shape {
                    Shape::Capsule { a, b, r } => Shape::Capsule { a: *a, b: *b, r: 0 }.gap_within(&region.shape, *r as f64) + TOLERANCE < *r as f64,
                    Shape::Polygon(_) => it.shape.gap_within(&region.shape, 0.0) == 0.0,
                };
                let inside = match &it.shape {
                    Shape::Capsule { a, b, r } => region.shape.depth(*a) + TOLERANCE >= *r as f64 && region.shape.depth(*b) + TOLERANCE >= *r as f64,
                    Shape::Polygon(_) => false,
                };
                let breaks = match rule.kind {
                    RegionKind::Never => touches,
                    RegionKind::Must => !inside,
                    _ => false,
                };
                if breaks {
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
