//! `kicad-text`: one JSON text item a line on standard input, its KiCad stroke polygons a line
//! on standard output (see the library's documentation for the format).

use std::io::{BufRead, Write};

fn main() {
    if std::env::args().nth(1).is_some_and(|a| a == "-h" || a == "--help") {
        println!("usage: kicad-text < items.jsonl > polygons.jsonl\none JSON text item a line in, one {{\"polygons\": [...]}} or {{\"error\": ...}} a line out, mm");
        return;
    }
    let stdin = std::io::stdin();
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("kicad-text: {e}");
                std::process::exit(1);
            }
        };
        let reply = serde_json::to_string(&kicad_text::answer(&line)).expect("polygons are plain data");
        if writeln!(out, "{reply}").is_err() {
            std::process::exit(1);
        }
    }
    if out.flush().is_err() {
        std::process::exit(1);
    }
}
