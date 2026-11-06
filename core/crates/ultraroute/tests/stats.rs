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
    // The earring has no planes, no planner's regions or widths, no laid strips, and the recipe does not nudge.
    let expected: Vec<&str> = ultraroute::STAGES.iter().map(|e| e.stage.name()).filter(|n| !["islands", "planes", "first", "nudge", "widening"].contains(n)).collect();
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
    // A failure for every connection left open, and none for one closed.
    assert_eq!(s["failures"].as_array().unwrap().len() as u64, m["opens"].as_u64().unwrap(), "{}", s["failures"]);
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
    // No stage tried them: each is filed as never tried, when the run ended.
    let failures = s["failures"].as_array().unwrap();
    assert_eq!(failures.len(), 376);
    assert!(failures.iter().all(|f| f["stage"] == "result" && f["reason"] == "never tried"), "{:?}", failures[0]);
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

#[test]
fn sub_stage_cpu_and_effort_account_for_stage_time() {
    let s = stats(&["--cell", "0.1", "--threads", "2", "--timing"]);
    for section in ["board", "summary", "moves", "progress"] {
        let mut ns = Vec::new();
        numbers(&s[section], &mut ns);
        assert!(ns.iter().all(|n| n.is_finite() && *n >= 0.0), "{section}: {ns:?}");
    }
    let mut stage_cpu_sum = 0.0;
    for st in s["stages"].as_array().unwrap() {
        let name = st["name"].as_str().unwrap();
        let cpu_s = st["cpu_seconds"].as_f64().unwrap();
        stage_cpu_sum += cpu_s;
        let c = &st["cpu"];
        let buckets = ["astar", "map_build", "map_patch", "zones", "checks", "connectivity", "terminals", "geometry"];
        let bucket_sum: f64 = buckets.iter().map(|k| c[*k].as_f64().unwrap()).sum();
        let accounted = c["accounted"].as_f64().unwrap();
        let other = c["other"].as_f64().unwrap();
        assert!(bucket_sum <= cpu_s, "{name}: buckets {bucket_sum} > stage {cpu_s}");
        assert!((accounted - bucket_sum).abs() <= 1e-12, "{name}: accounted {accounted} != {bucket_sum}");
        assert!(accounted + other <= cpu_s, "{name}: accounted + other {} > stage {cpu_s}", accounted + other);
        assert!((cpu_s - (accounted + other)).abs() <= 1e-9, "{name}: remainder gap on {cpu_s}");

        let e = &st["effort"];
        let eff_sum = e["useful_seconds"].as_f64().unwrap()
            + e["kept_other_seconds"].as_f64().unwrap()
            + e["wasted_seconds"].as_f64().unwrap();
        let eff_acc = e["accounted_seconds"].as_f64().unwrap();
        let eff_oth = e["other_seconds"].as_f64().unwrap();
        assert!(eff_sum <= cpu_s, "{name}: effort {eff_sum} > stage {cpu_s}");
        assert!((eff_acc - eff_sum).abs() <= 1e-12, "{name}: effort accounted {eff_acc} != {eff_sum}");
        assert!(eff_acc + eff_oth <= cpu_s, "{name}: effort accounted + other {} > stage {cpu_s}", eff_acc + eff_oth);
        assert!((cpu_s - (eff_acc + eff_oth)).abs() <= 1e-9, "{name}: effort remainder gap on {cpu_s}");
    }
    assert!(stage_cpu_sum > 0.0);
    assert!((s["summary"]["cpu_seconds"].as_f64().unwrap() - stage_cpu_sum).abs() <= 1e-9);
    let neg = s["stages"].as_array().unwrap().iter().find(|st| st["name"] == "negotiation").unwrap();
    assert!(neg["cpu"]["astar"].as_f64().unwrap() > 0.0, "{neg}");
    assert!(neg["cpu"]["zones"].as_f64().unwrap() > 0.0, "{neg}");
    assert!(neg["effort"]["accounted_seconds"].as_f64().unwrap() > 0.0, "{neg}");
    assert!(s["moves"]["negotiation"]["rounds"].as_u64().unwrap() > 0, "{}", s["moves"]);
    assert!(!s["progress"]["open_curve"].as_array().unwrap().is_empty(), "{}", s["progress"]);

    // Default (without --timing): coarse stage CPU seconds are recorded, while fine-grained
    // buckets, effort and progress timelines stay zero/empty.
    let coarse = stats(&["--cell", "0.1", "--threads", "1"]);
    assert!(coarse["summary"]["cpu_seconds"].as_f64().unwrap() > 0.0, "{coarse}");
    for st in coarse["stages"].as_array().unwrap() {
        assert_eq!(st["cpu"]["accounted"].as_f64().unwrap(), 0.0, "{st}");
        assert_eq!(st["effort"]["accounted_seconds"].as_f64().unwrap(), 0.0, "{st}");
    }
    assert!(coarse["progress"]["open_curve"].as_array().unwrap().is_empty(), "{}", coarse["progress"]);

    // With --no-timing, all CPU and effort counters stay zero while stages and work counts still report.
    let off = stats(&["--cell", "0.1", "--threads", "1", "--no-timing"]);
    assert_eq!(off["summary"]["cpu_seconds"].as_f64().unwrap(), 0.0);
    for st in off["stages"].as_array().unwrap() {
        assert_eq!(st["cpu_seconds"].as_f64().unwrap(), 0.0, "{st}");
        assert_eq!(st["cpu"]["accounted"].as_f64().unwrap(), 0.0, "{st}");
        assert_eq!(st["effort"]["accounted_seconds"].as_f64().unwrap(), 0.0, "{st}");
    }
}

fn run_text(text: &str, flags: &[&str]) -> Value {
    let mut args = vec!["route".to_string(), "inline.kicad_pcb".to_string(), "-o".to_string(), "unused.kicad_pcb".to_string()];
    args.extend(flags.iter().map(|s| (*s).to_string()));
    let a = ultraroute::parse(&args).unwrap();
    let (pcb, copper) = kicad_pcb::parse_board(text).unwrap();
    let board = Board::new(&pcb, &copper, kicad_pcb::Rules::parse("{}").unwrap());
    let routed = ultraroute::route(&a, board, text, None, Arc::new(AtomicBool::new(false)), None, None);
    serde_json::to_value(&routed.stats).unwrap()
}

#[test]
fn shove_updates_report_failures_when_closing_or_opening_connections() {
    // One-lane corridor: B starts adopted with a narrow escape through two fixed jaws, so finishing
    // gives up on A; with --no-guard-ties, shove's tie swap routes A and knocks B open inside the
    // jaws. A must leave `failures` and B must enter it.
    let swap_board = "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal))
        (net 0 \"\") (net 1 \"A\") (net 2 \"B\") (net 3 \"W1\") (net 4 \"W2\") (net 5 \"W3\")
        (gr_rect (start 0 0) (end 10 4.2) (layer \"Edge.Cuts\") (width 0.1))
        (footprint \"Part\" (layer \"F.Cu\") (at 0 0)
            (property \"Reference\" \"U1\" (at 0 0) (layer \"F.SilkS\"))
            (pad \"1\" smd circle (at 1.5 2.0) (size 0.4 0.4) (layers \"F.Cu\") (net 1 \"A\"))
            (pad \"2\" smd circle (at 8.5 2.0) (size 0.4 0.4) (layers \"F.Cu\") (net 1 \"A\"))
            (pad \"3\" smd circle (at 5.0 1.0) (size 0.24 0.24) (layers \"F.Cu\") (net 2 \"B\"))
            (pad \"4\" smd circle (at 8.5 3.1) (size 0.4 0.4) (layers \"F.Cu\") (net 2 \"B\"))
            (pad \"w1\" smd rect (at 4.5 1.1) (size 0.4 1.0) (layers \"F.Cu\") (net 3 \"W1\"))
            (pad \"w2\" smd rect (at 5.5 1.1) (size 0.4 1.0) (layers \"F.Cu\") (net 4 \"W2\"))
            (pad \"w3\" smd rect (at 5.0 3.0) (size 0.4 1.2) (layers \"F.Cu\") (net 5 \"W3\")))
        (segment (start 5.0 1.0) (end 5.0 2.0) (width 0.12) (layer \"F.Cu\") (net 2) (uuid \"b1\"))
        (segment (start 5.0 2.0) (end 8.5 2.0) (width 0.2) (layer \"F.Cu\") (net 2) (uuid \"b2\"))
        (segment (start 8.5 2.0) (end 8.5 3.1) (width 0.2) (layer \"F.Cu\") (net 2) (uuid \"b3\")))";
    let swapped = run_text(
        swap_board,
        &["--adopt", "--shove-plough", "region", "--shove-reach", "0.8", "--shove-repair", "0", "--shove-ties", "--no-guard-ties", "--shove-passes", "1"],
    );
    assert_eq!(swapped["summary"]["opens"], 1, "{swapped}");
    let failures = swapped["failures"].as_array().unwrap();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0]["net"], "B", "{failures:?}");
    assert_eq!(failures[0]["stage"], "shove", "{failures:?}");

    // B's adopted ring walls A1 in on its one layer, so finishing gives up on A; shove rips a
    // side of the ring (B stays joined round the other) and closes A. `failures` must end empty.
    let rescue_board = "(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal))
        (net 0 \"\") (net 1 \"A\") (net 2 \"B\")
        (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\") (width 0.1))
        (footprint \"Part\" (layer \"F.Cu\") (at 0 0)
            (property \"Reference\" \"U1\" (at 0 0) (layer \"F.SilkS\"))
            (pad \"1\" smd circle (at 5 10) (size 0.6 0.6) (layers \"F.Cu\") (net 1 \"A\"))
            (pad \"2\" smd circle (at 15 10) (size 0.6 0.6) (layers \"F.Cu\") (net 1 \"A\"))
            (pad \"3\" smd circle (at 2 2) (size 0.6 0.6) (layers \"F.Cu\") (net 2 \"B\"))
            (pad \"4\" smd circle (at 2 18) (size 0.6 0.6) (layers \"F.Cu\") (net 2 \"B\")))
        (segment (start 2 2) (end 8 2) (width 0.2) (layer \"F.Cu\") (net 2) (uuid \"s1\"))
        (segment (start 8 2) (end 8 18) (width 0.2) (layer \"F.Cu\") (net 2) (uuid \"s2\"))
        (segment (start 8 18) (end 2 18) (width 0.2) (layer \"F.Cu\") (net 2) (uuid \"s3\"))
        (segment (start 2 18) (end 2 2) (width 0.2) (layer \"F.Cu\") (net 2) (uuid \"s4\")))";
    let rescued = run_text(rescue_board, &["--adopt", "--shove-plough", "region", "--shove-reach", "1.0", "--shove-passes", "2"]);
    let shove = rescued["stages"].as_array().unwrap().iter().find(|st| st["name"] == "shove").expect("shove ran");
    assert_eq!((shove["open_before"].as_u64(), shove["open_after"].as_u64()), (Some(1), Some(0)), "{shove}");
    assert_eq!(rescued["summary"]["opens"], 0, "{rescued}");
    assert!(rescued["failures"].as_array().unwrap().is_empty(), "{:?}", rescued["failures"]);
}
