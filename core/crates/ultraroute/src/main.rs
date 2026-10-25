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
use ultraroute::{parse, run, Live};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a = match parse(&args) {
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
        Live::new(out, a.fps, a.pours_block)
    });
    let routed = run(&a, board, &text, live, cancel);
    if let Err(e) = std::fs::write(&a.output, routed) {
        eprintln!("{}: {e}", a.output);
        std::process::exit(1);
    }
}

