//! A run's stats (`--stats FILE`, as JSON): every stage the recipe runs, with its time, work and
//! open connections, a summary, and a finding for every connection the run leaves open.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use board::Board;
use serde_json::Value;

/// The earring, stripped bare, routed with the default recipe and `extra`: the stats as the
/// `--stats` file has them.
fn stats(extra: &[&str]) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pcb");
    let mut args: Vec<String> = ["route", path.to_str().unwrap(), "-o", "unused.kicad_pcb", "--strip-all", "--seed", "0", "--recipe", "default"].map(String::from).to_vec();
    args.extend(extra.iter().map(|s| s.to_string()));
    let a = ultraroute::parse(&args).unwrap();
    let (board, text) = Board::load(&path).unwrap();
    let routed = ultraroute::route(&a, board, &text, None, Arc::new(AtomicBool::new(false)), None, None);
    kicad_pcb::parse_board(&routed.text).expect("the routed board reads back");
    serde_json::to_value(&routed.stats).unwrap()
}

/// Every number in `v`, wherever it is.
fn numbers(v: &Value, out: &mut Vec<f64>) {
    match v {
        Value::Number(n) => out.push(n.as_f64().unwrap()),
        Value::Array(a) => a.iter().for_each(|x| numbers(x, out)),
        Value::Object(o) => o.values().for_each(|x| numbers(x, out)),
        _ => {}
    }
}

#[test]
fn every_stage_the_recipe_runs_is_in_the_stats() {
    // A coarse grid and four threads keep it to a few seconds; every stage still runs.
    let s = stats(&["--cell", "0.1", "--threads", "4"]);
    let names: Vec<&str> = s["stages"].as_array().unwrap().iter().map(|st| st["name"].as_str().unwrap()).collect();
    // The earring has no planes, no planner's regions, and the recipe does not nudge.
    let expected: Vec<&str> = ultraroute::STAGES.iter().map(|e| e.stage.name()).filter(|n| !["planes", "first", "nudge"].contains(n)).collect();
    assert_eq!(names, expected);
    for stage in s["stages"].as_array().unwrap() {
        let mut ns = Vec::new();
        numbers(stage, &mut ns);
        assert!(ns.iter().all(|n| n.is_finite() && *n >= 0.0), "{}: {ns:?}", stage["name"]);
        assert!(stage["seconds"].as_f64().is_some() && stage["work"]["astar"].as_u64().is_some(), "{stage}");
    }
    let negotiation = s["stages"].as_array().unwrap().iter().find(|st| st["name"] == "negotiation").unwrap();
    assert!(negotiation["detail"]["rounds"].as_u64().unwrap() > 0 && negotiation["work"]["astar"].as_u64().unwrap() > 0, "{negotiation}");
    let m = &s["summary"];
    assert_eq!(m["connections"], 376, "{m}");
    assert_eq!(m["routed"].as_u64().unwrap() + m["opens"].as_u64().unwrap(), 376, "{m}");
    assert!(m["vias"].as_u64().unwrap() > 0 && m["seconds"].as_f64().unwrap() > 0.0, "{m}");
    assert_eq!(s["input"]["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(s["router"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(s["settings"]["threads"], 4);
    let opens = s["findings"].as_array().unwrap().iter().filter(|f| f["kind"] == "open").count();
    assert_eq!(opens as u64, m["opens"].as_u64().unwrap());
}

#[test]
fn every_open_connection_is_a_finding() {
    // Two seconds are all the reserve: the hard deadline passes as routing would start, so the
    // board is written as stripped, every connection open.
    let s = stats(&["--threads", "1", "--timeout", "2"]);
    let m = &s["summary"];
    assert_eq!(m["opens"], 376, "{m}");
    let findings = s["findings"].as_array().unwrap();
    let opens: Vec<&Value> = findings.iter().filter(|f| f["kind"] == "open").collect();
    assert_eq!(opens.len(), 376);
    for f in &opens {
        assert_eq!(f["severity"], "error");
        let net = f["nets"][0].as_str().unwrap();
        let ends = f["ends"].as_array().unwrap();
        assert_eq!(f["message"].as_str().unwrap(), format!("open {net}: {} to {}", ends[0].as_str().unwrap(), ends[1].as_str().unwrap()));
    }
    // Pads name their parts.
    assert!(opens.iter().any(|f| f["refs"].as_array().is_some_and(|r| !r.is_empty())));
    assert_eq!(s["deadline"]["hard_passed_in"], "set-up", "{}", s["deadline"]);
    let deadline: Vec<&str> = findings.iter().filter(|f| f["kind"] == "deadline").map(|f| f["message"].as_str().unwrap()).collect();
    assert_eq!(deadline.len(), 1, "{deadline:?}");
    assert!(deadline[0].starts_with("deadline: the run's hard limit passed during set-up; "));
    // Only the stages that run whatever the time ran.
    let names: Vec<&str> = s["stages"].as_array().unwrap().iter().map(|st| st["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["set-up", "cleaning", "result", "write"]);
}
