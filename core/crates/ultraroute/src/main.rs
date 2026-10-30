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
use ultraroute::{fit_memory, parse, peak_memory_mb, run, Live};

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
    for w in &board.rules.warnings {
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
            Box::new(std::io::BufWriter::new(std::fs::File::create(path).expect("create the live file")))
        };
        Live::new(out, a.fps, a.pours_block).with_step(a.live_step)
    });
    // A crash still leaves the caller a board: the input as it came, and a failing exit.
    let routed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&a, board, &text, live, cancel)));
    let (out, code) = match routed {
        Ok(routed) => (routed, 0),
        Err(_) => {
            eprintln!("error: the router crashed; {} is the input board, unrouted", a.output);
            (text, 1)
        }
    };
    if let Some(mb) = peak_memory_mb() {
        eprintln!("memory: peak {mb} MB");
    }
    if let Err(e) = std::fs::write(&a.output, out) {
        eprintln!("{}: {e}", a.output);
        std::process::exit(1);
    }
    std::process::exit(code);
}

