//! A planner's widths (`--regions` routing.json `widths`), as KiCad's `track_width` constraint
//! has them: `min_mm` a rule the net is routed to, `opt_mm` taken after routing where there is
//! room, so it costs no connection.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use board::{Board, Kind, Shape};
use serde_json::Value;

/// The earring, stripped bare, routed with the default recipe on a coarse grid, with `widths`
/// as routing.json's: the stats, and each of 3V3's tracks' width and length in nanometres.
fn route(widths: Option<&str>) -> (Value, Vec<(i64, f64)>) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pcb");
    let mut args: Vec<String> = ["route", path.to_str().unwrap(), "-o", "unused.kicad_pcb", "--strip-all", "--seed", "0", "--recipe", "default", "--cell", "0.1", "--threads", "4"].map(String::from).to_vec();
    if let Some(w) = widths {
        let dir = std::env::temp_dir().join(format!("ultraroute-widths-{}-{}", std::process::id(), w.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let json = dir.join("routing.json");
        std::fs::write(&json, format!(r#"{{"version": 1, "widths": [{w}]}}"#)).unwrap();
        args.extend(["--regions".to_string(), json.to_str().unwrap().to_string()]);
    }
    let a = ultraroute::parse(&args).unwrap();
    let (board, text) = Board::load(&path).unwrap();
    let routed = ultraroute::route(&a, board, &text, None, Arc::new(AtomicBool::new(false)), None, None);
    let (pcb, copper) = kicad_pcb::parse_board(&routed.text).unwrap();
    let out = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
    let widths = out.items().filter(|(_, it)| it.kind == Kind::Track && out.net_name(it.net) == "3V3").filter_map(|(_, it)| match it.shape {
        Shape::Capsule { a, b, r } => Some((2 * r, a.distance(b))),
        _ => None,
    }).collect();
    (serde_json::to_value(&routed.stats).unwrap(), widths)
}

#[test]
fn an_optimum_width_is_taken_after_routing_and_a_minimum_is_routed_to() {
    let (plain, narrow) = route(None);
    assert!(narrow.iter().all(|&(w, _)| w < 500_000), "{narrow:?}");
    // An optimum moves nothing the routing laid: the same connections and vias, wider tracks.
    let (opt, widened) = route(Some(r#"{"nets": ["3V3"], "opt_mm": 0.5}"#));
    let (plain, opt) = (&plain["summary"], &opt["summary"]);
    assert_eq!((&opt["opens"], &opt["vias"]), (&plain["opens"], &plain["vias"]), "{opt} vs {plain}");
    assert!(widened.iter().any(|&(w, _)| w == 500_000), "{widened:?}");
    // A minimum is a rule, as a class's width is: every track of the net at least that wide but
    // the legs into pads no such track can leave, each reported as a neck.
    let (stats, ruled) = route(Some(r#"{"nets": ["3V3"], "min_mm": 0.3}"#));
    let under: Vec<_> = ruled.iter().filter(|&&(w, _)| w < 300_000).collect();
    assert!(ruled.len() > 2 * under.len(), "{ruled:?}");
    assert!(under.iter().all(|&&(_, len)| len < 1e6), "short legs only: {under:?}");
    let necked = stats["findings"].as_array().unwrap().iter().any(|f| f["kind"] == "neck" && f["nets"][0] == "3V3");
    assert!(under.is_empty() || necked, "{}", stats["findings"]);
}
