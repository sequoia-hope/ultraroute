//! `ultraroute route in.kicad_pcb -o out.kicad_pcb`: routes a KiCad board on the MIT core.
//!
//! The project's `.kicad_pro` beside the input supplies the rules. Pours are left as they are
//! for KiCad to refill. With `--live -` the board streams to standard output as the studio's
//! newline-delimited JSON frames, and a line `stop` on standard input stops the run. With
//! `--blame`, every connection still open at the end is listed with what stands in its way.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use board::Board;
use ultraroute::{fit_memory, parse, peak_memory_mb, route, Finding, Live, RunStats};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut a = match parse(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
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
        let stats = RunStats { memory: ultraroute::stats::Memory { peak_mb: peak, ..stats.memory }, ..stats };
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
