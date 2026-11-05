//! `ultraroute route in.kicad_pcb -o out.kicad_pcb`: routes a KiCad board on the MIT core.
//!
//! The project's `.kicad_pro` beside the input supplies the rules. Pours are left as they are
//! for KiCad to refill. With `--live -` the board streams to standard output as the studio's
//! newline-delimited JSON frames, and a line `stop` on standard input stops the run. With
//! `--blame`, every connection still open at the end is listed with what stands in its way.
//! Copper and mask text is stroked by the `kicad-text` program when it is found, else boxed
//! (`kicad_pcb::text`); `ultraroute text-helper` prints the program a route would run.

use std::collections::HashSet;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use board::Board;
use serde_json::json;
use ultraroute::{
    fit_memory, parse, parse_compact, parse_scale, peak_memory_mb, route, Finding, Live, RunStats,
    COMPACT_USAGE, SCALE_USAGE, USAGE,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("scale") => return run_scale(&args),
        Some("compact") => return run_compact(&args),
        _ => {}
    }
    // Asked for: the flags on standard output, and success. A mistake prints them as an error.
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return;
    }
    // The text helper a route would run, for a bench to record; failing if text would be boxes.
    if args.first().is_some_and(|a| a == "text-helper") {
        match kicad_pcb::text::shaper() {
            kicad_pcb::text::Shaper::Helper(path, _) => println!("{}", path.display()),
            kicad_pcb::text::Shaper::Boxes(why) => {
                eprintln!("text as boxes: {why}");
                std::process::exit(1);
            }
        }
        return;
    }
    let mut a = match parse(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let load_t0 = if a.no_timing { 0 } else { board::work::process_cpu_ns() };
    let (board, text) = match Board::load(a.input.as_ref()) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let load_cpu_seconds = if a.no_timing {
        0.0
    } else {
        board::work::process_cpu_ns().saturating_sub(load_t0) as f64 * 1e-9
    };
    for w in board.rules.warnings.iter().chain(&board.warnings) {
        eprintln!("warning: {w}");
    }
    if let Some(note) = &board.text_note {
        eprintln!("text: {note}");
    }
    // Sized before anything is allocated: a board too large for the machine is fitted or refused,
    // and a refused one leaves the input as the output, as a crash does.
    match fit_memory(&mut a, &board) {
        Ok(lines) => {
            for l in lines {
                eprintln!("{l}");
            }
            if a.estimate {
                return;
            }
        }
        Err(e) => {
            eprintln!("{e}");
            if !a.estimate {
                if let Err(e) = std::fs::write(&a.output, &text) {
                    eprintln!("{}: {e}", a.output);
                }
            }
            std::process::exit(1);
        }
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let live = a.live.as_ref().map(|path| {
        let out: Box<dyn Write + Send> = if path == "-" {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                for line in std::io::stdin().lines() {
                    match line {
                        Ok(l) if l.trim() == "stop" => cancel.store(true, Ordering::Relaxed),
                        Ok(_) => {}
                        Err(_) => return,
                    }
                }
            });
            Box::new(std::io::stdout())
        } else {
            match std::fs::File::create(path) {
                Ok(f) => Box::new(std::io::BufWriter::new(f)),
                Err(e) => {
                    eprintln!("--live {path}: {e}");
                    std::process::exit(1);
                }
            }
        };
        Live::new(out, a.fps, a.pours_block).with_step(a.live_step)
    });
    #[cfg(feature = "profile")]
    let profiler = std::env::var_os("ULTRAROUTE_PROFILE").map(|path| {
        let guard = pprof::ProfilerGuardBuilder::default().frequency(250).blocklist(&["libc", "libgcc", "pthread", "vdso"]).build().expect("start the profiler");
        (path, guard)
    });
    // A crash still leaves the caller a board: the input as it came, and a failing exit.
    let routed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| route(&a, board, &text, live, cancel, None, None)));
    let (out, stats, code) = match routed {
        Ok(routed) => (routed.text, routed.stats, 0),
        Err(_) => {
            eprintln!("error: the router crashed; {} is the input board, unrouted", a.output);
            (text.clone(), crashed(&a, &text), 1)
        }
    };
    #[cfg(feature = "profile")]
    if let Some((path, guard)) = profiler {
        write_profile(&path, &guard);
    }
    let peak = peak_memory_mb();
    if let Some(mb) = peak {
        eprintln!("memory: peak {mb} MB");
    }
    if let Some(path) = &a.stats {
        let stats = RunStats {
            load_cpu_seconds,
            memory: ultraroute::stats::Memory { peak_mb: peak, ..stats.memory },
            ..stats
        };
        let json = serde_json::to_string_pretty(&stats).expect("the stats are plain data");
        if let Err(e) = std::fs::write(path, json + "\n") {
            eprintln!("{path}: {e}");
        }
    }
    if let Err(e) = std::fs::write(&a.output, out) {
        eprintln!("{}: {e}", a.output);
        std::process::exit(1);
    }
    std::process::exit(code);
}

fn warn_unknown_locks(board: &Board, locks: &[String]) -> HashSet<String> {
    let known: HashSet<&str> = board.footprints.iter().map(|f| f.reference.as_str()).collect();
    let mut set = HashSet::new();
    for r in locks {
        if !known.contains(r.as_str()) {
            eprintln!("warning: --lock {r}: no such part");
        } else {
            set.insert(r.clone());
        }
    }
    set
}

fn run_scale(args: &[String]) {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{SCALE_USAGE}");
        return;
    }
    let a = match parse_scale(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let started = Instant::now();
    let (board, text) = match Board::load(a.input.as_ref()) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    for w in board.rules.warnings.iter().chain(&board.warnings) {
        eprintln!("warning: {w}");
    }
    let lock = warn_unknown_locks(&board, &a.lock);
    let rep = match morph::scale(
        &text,
        board.rules,
        &morph::ScaleOptions { factor: a.factor, lock, uuid_seed: a.uuid_seed },
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = std::fs::write(&a.output, &rep.text) {
        eprintln!("{}: {e}", a.output);
        std::process::exit(1);
    }
    eprintln!(
        "scale {:.4}: {:.3} x {:.3} mm -> {:.3} x {:.3} mm, {} footprints moved, {} tracks, {} vias, {:.2} s",
        a.factor,
        rep.before_mm[0],
        rep.before_mm[1],
        rep.after_mm[0],
        rep.after_mm[1],
        rep.moved_footprints,
        rep.tracks,
        rep.vias,
        started.elapsed().as_secs_f64()
    );
}

fn run_compact(args: &[String]) {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{COMPACT_USAGE}");
        return;
    }
    let a = match parse_compact(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let started = Instant::now();
    grid::clock::use_cpu_time();
    let cpu_started = grid::clock::now();
    let (board, text) = match Board::load(a.input.as_ref()) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    for w in board.rules.warnings.iter().chain(&board.warnings) {
        eprintln!("warning: {w}");
    }
    let lock = warn_unknown_locks(&board, &a.lock);
    let rep = match morph::compact(
        &text,
        board.rules,
        &morph::CompactOptions {
            factor: a.factor,
            steps: a.steps,
            lock,
            uuid_seed: a.uuid_seed,
            verbose: a.verbose,
        },
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let elapsed = started.elapsed().as_secs_f64();
    let cpu_s = grid::clock::since(cpu_started).as_secs_f64();
    if !a.verbose {
        for s in &rep.steps {
            let status = if s.step == 0 {
                "base"
            } else if s.accepted {
                "kept"
            } else {
                "jam "
            };
            eprintln!(
                "step {:>2}/{} [{}] factor {:.4}: {:.3} x {:.3} mm | violations {} | length {:.2} mm ({} segs) | vias {} | stops {} | slides {} | acid traps {} ({} acute, {} slots){}",
                s.step,
                rep.steps_requested,
                status,
                s.factor,
                s.size_mm[0],
                s.size_mm[1],
                s.violations,
                s.track_length_mm,
                s.segments,
                s.vias,
                s.part_stops,
                s.via_slides,
                s.acid_traps.acute + s.acid_traps.slots,
                s.acid_traps.acute,
                s.acid_traps.slots,
                s.jam.as_ref().map_or_else(String::new, |j| format!(" | {j}"))
            );
        }
    }
    let last_kept = rep.steps.iter().rev().find(|s| s.accepted).unwrap();
    match &rep.jam {
        None => eprintln!(
            "compact done: reached factor {:.4} ({}/{} steps), {:.3} x {:.3} mm, {:.2} mm track ({} segs), {:.2} s ({:.2} cpu s)",
            rep.achieved_factor,
            rep.steps_accepted,
            rep.steps_requested,
            last_kept.size_mm[0],
            last_kept.size_mm[1],
            last_kept.track_length_mm,
            last_kept.segments,
            elapsed,
            cpu_s
        ),
        Some(reason) => eprintln!(
            "compact stopped at factor {:.4} ({}/{} steps toward {:.4}), {:.3} x {:.3} mm, {} segs, {:.2} s ({:.2} cpu s): {reason}",
            rep.achieved_factor,
            rep.steps_accepted,
            rep.steps_requested,
            rep.target_factor,
            last_kept.size_mm[0],
            last_kept.size_mm[1],
            last_kept.segments,
            elapsed,
            cpu_s
        ),
    }

    if let Some(path) = &a.stats {
        let steps_json: Vec<_> = rep
            .steps
            .iter()
            .map(|s| {
                json!({
                    "step": s.step,
                    "factor": s.factor,
                    "accepted": s.accepted,
                    "size_mm": s.size_mm,
                    "violations": s.violations,
                    "track_length_mm": s.track_length_mm,
                    "segments": s.segments,
                    "vias": s.vias,
                    "acid_traps": { "acute": s.acid_traps.acute, "slots": s.acid_traps.slots },
                    "courtyard_stops": s.courtyard_stops,
                    "part_stops": s.part_stops,
                    "via_slides": s.via_slides,
                    "jam": s.jam,
                })
            })
            .collect();
        let max_part_stops =
            rep.steps.iter().filter(|s| s.accepted).map(|s| s.part_stops).max().unwrap_or(0);
        let total_via_slides: usize =
            rep.steps.iter().filter(|s| s.accepted).map(|s| s.via_slides).sum();
        let summary = json!({
            "input": a.input,
            "output": a.output,
            "target_factor": rep.target_factor,
            "achieved_factor": rep.achieved_factor,
            "steps_requested": rep.steps_requested,
            "steps_accepted": rep.steps_accepted,
            "segments": last_kept.segments,
            "vias": last_kept.vias,
            "track_length_mm": last_kept.track_length_mm,
            "part_stops": max_part_stops,
            "via_slides": total_via_slides,
            "elapsed_s": elapsed,
            "cpu_s": cpu_s,
            "peak_memory_mb": peak_memory_mb(),
            "jam": rep.jam,
            "steps": steps_json,
        });
        if let Err(e) = std::fs::write(path, serde_json::to_string_pretty(&summary).unwrap() + "\n") {
            eprintln!("{path}: {e}");
        }
    }

    if let Err(e) = std::fs::write(&a.output, &rep.text) {
        eprintln!("{}: {e}", a.output);
        std::process::exit(1);
    }
}

/// The stats of a run that crashed: what it was asked, and the crash as its one finding.
fn crashed(a: &ultraroute::Args, text: &str) -> RunStats {
    let (version, commit) = ultraroute::stats::version();
    RunStats {
        router: ultraroute::stats::Router { version: version.to_string(), commit: commit.map(String::from) },
        input: ultraroute::stats::Input { path: a.input.clone(), sha256: ultraroute::stats::sha256(text.as_bytes()), bytes: text.len() },
        settings: ultraroute::stats::Settings { flags: a.line.clone(), seed: a.seed, uuid_seed: a.uuid_seed, timeout: a.timeout, ..Default::default() },
        findings: vec![Finding { severity: "error", kind: "crash", message: "the router crashed; the output is the input board, unrouted".into(), refs: Vec::new(), nets: Vec::new(), ends: None }],
        ..Default::default()
    }
}

/// The profile's stacks in the collapsed format, root first: one line per stack, its frames joined
/// by `;`, then its sample count.
#[cfg(feature = "profile")]
fn write_profile(path: &std::ffi::OsStr, guard: &pprof::ProfilerGuard) {
    let report = match guard.report().build() {
        Ok(r) => r,
        Err(e) => return eprintln!("profile: {e}"),
    };
    let mut out = String::new();
    for (frames, count) in &report.data {
        let names: Vec<String> = frames.frames.iter().rev().flat_map(|f| f.iter().rev().map(|s| s.name())).collect();
        out.push_str(&format!("{} {count}\n", names.join(";")));
    }
    match std::fs::write(path, out) {
        Ok(()) => eprintln!("profile: {} stacks written to {}", report.data.len(), path.to_string_lossy()),
        Err(e) => eprintln!("profile: {}: {e}", path.to_string_lossy()),
    }
}
