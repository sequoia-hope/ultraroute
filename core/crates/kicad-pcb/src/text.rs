//! Copper and mask text shaped by a helper program, or as boxes without one.
//!
//! KiCad's DRC tests text by its stroked glyphs, and a box round a rotated or multi-line label
//! covers pads beside its shorter lines. The glyphs and their layout are KiCad's (GPL), so they
//! live in a separate program, `kicad-text` (`core/crates/kicad-text`, GPL-3.0-or-later), which
//! this MIT crate only runs: all of a board's texts go to it in one batch, one JSON item a line
//! on its standard input, and come back as one line of polygons each on its standard output,
//! millimetres, in the frame the anchor is given in. Without the program, or if it fails, each
//! text is the box its strokes stay within, a little generously. A helper that has not answered
//! within [`HELPER_TIMEOUT`] has failed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The environment variable naming the helper: a path, or `none` for boxes.
pub const HELPER_VAR: &str = "ULTRAROUTE_TEXT_HELPER";
/// The helper's program name, looked for next to the running program and on `PATH`.
pub const HELPER_NAME: &str = "kicad-text";

/// How long the helper may take over a board's texts: it needs milliseconds, even for thousands.
pub const HELPER_TIMEOUT: Duration = Duration::from_secs(30);

const MM: f64 = 1e-3;

/// One text item as the file gives it, as the helper reads it (mm, degrees, Y down).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub text: String,
    /// Glyph width and height (the file's `(size H W)` is height first).
    pub size: [f64; 2],
    pub thickness: Option<f64>,
    pub at: [f64; 2],
    pub angle: f64,
    /// `left`, `center` or `right`.
    pub h_align: String,
    /// `top`, `center` or `bottom`.
    pub v_align: String,
    pub mirror: bool,
    pub bold: bool,
    pub italic: bool,
    pub knockout: bool,
    pub line_spacing: f64,
}

/// Where a board's text shapes come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shaper<'a> {
    /// The helper program at this path, given this long to answer.
    Helper(&'a Path, Duration),
    /// Boxes, for this reason.
    Boxes(&'a str),
}

/// The shaper the environment gives: [`HELPER_VAR`], else [`HELPER_NAME`] next to the running
/// program, else on `PATH`; boxes when none is found, and always in WASM, which runs no programs.
/// Looked for once.
pub fn shaper() -> Shaper<'static> {
    static FOUND: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    match FOUND.get_or_init(find) {
        Ok(path) => Shaper::Helper(path, HELPER_TIMEOUT),
        Err(why) => Shaper::Boxes(why),
    }
}

#[cfg(target_arch = "wasm32")]
fn find() -> Result<PathBuf, String> {
    Err("no text helper in WASM".to_string())
}

#[cfg(not(target_arch = "wasm32"))]
fn find() -> Result<PathBuf, String> {
    if let Some(given) = std::env::var_os(HELPER_VAR).filter(|v| !v.is_empty()) {
        if given == "none" {
            return Err(format!("{HELPER_VAR} is none"));
        }
        let path = PathBuf::from(&given);
        return if path.is_file() { Ok(path) } else { Err(format!("{HELPER_VAR} {} is no file", path.display())) };
    }
    let name = format!("{HELPER_NAME}{}", std::env::consts::EXE_SUFFIX);
    let beside = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|d| d.join(&name)));
    let on_path = std::env::var_os("PATH").into_iter().flat_map(|p| std::env::split_paths(&p).map(|d| d.join(&name)).collect::<Vec<_>>());
    beside
        .into_iter()
        .chain(on_path)
        .find(|p| p.is_file())
        .ok_or_else(|| format!("no {name} next to this program or on PATH, nor {HELPER_VAR}"))
}

/// The polygons of each of `items`, metres, from the helper at `helper`, all in one run; an error
/// if it fails or has not finished within `timeout`.
#[cfg(not(target_arch = "wasm32"))]
pub fn run(helper: &Path, items: &[Item], timeout: Duration) -> Result<Vec<Vec<Vec<[f64; 2]>>>, String> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use std::sync::mpsc::channel;
    use std::time::Instant;
    let mut request = String::new();
    for item in items {
        request.push_str(&serde_json::to_string(item).map_err(|e| e.to_string())?);
        request.push('\n');
    }
    let mut child = Command::new(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("{}: {e}", helper.display()))?;
    let (mut stdin, mut stdout) = (child.stdin.take().expect("piped"), child.stdout.take().expect("piped"));
    let deadline = Instant::now() + timeout;
    // Threads, so a helper answering before it has read everything cannot stall both ends, and
    // joined by nothing: a process the helper left behind can hold a pipe open after it is killed.
    let (wrote_tx, wrote) = channel();
    std::thread::spawn(move || wrote_tx.send(stdin.write_all(request.as_bytes()).map(|_| drop(stdin))));
    let (read_tx, read) = channel();
    std::thread::spawn(move || {
        let mut reply = String::new();
        let got = stdout.read_to_string(&mut reply);
        read_tx.send(got.map(|_| reply))
    });
    let hung = |child: &mut std::process::Child| {
        let _ = child.kill();
        let _ = child.wait();
        format!("{} did not finish within {} s", helper.display(), timeout.as_secs_f64())
    };
    let Ok(reply) = read.recv_timeout(deadline.saturating_duration_since(Instant::now())) else { return Err(hung(&mut child)) };
    let status = loop {
        match child.try_wait().map_err(|e| format!("{}: {e}", helper.display()))? {
            Some(status) => break status,
            None if Instant::now() >= deadline => return Err(hung(&mut child)),
            None => std::thread::sleep(Duration::from_millis(1)),
        }
    };
    if !status.success() {
        return Err(format!("{} exited with {status}", helper.display()));
    }
    // It has exited, so a write still going can only be to a process it left behind.
    let Ok(wrote) = wrote.recv_timeout(deadline.saturating_duration_since(Instant::now())) else { return Err(hung(&mut child)) };
    wrote.map_err(|e| format!("{}: {e}", helper.display()))?;
    let reply = reply.map_err(|e| format!("{}: {e}", helper.display()))?;
    parse_reply(&reply, items.len())
}

#[cfg(target_arch = "wasm32")]
pub fn run(_helper: &Path, _items: &[Item], _timeout: Duration) -> Result<Vec<Vec<Vec<[f64; 2]>>>, String> {
    Err("no text helper in WASM".to_string())
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Reply {
    Polygons { polygons: Vec<Vec<[f64; 2]>> },
    Error { error: String },
}

/// The helper's answer to `n` items, one line each, as polygons in metres; an error for a line
/// that does not read, an item it could not shape, or too few or many lines.
pub fn parse_reply(text: &str, n: usize) -> Result<Vec<Vec<Vec<[f64; 2]>>>, String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() != n {
        return Err(format!("{} replies to {n} texts", lines.len()));
    }
    lines
        .iter()
        .enumerate()
        .map(|(k, line)| match serde_json::from_str::<Reply>(line) {
            Ok(Reply::Polygons { polygons }) => Ok(polygons.into_iter().map(|p| p.into_iter().map(|[x, y]| [x * MM, y * MM]).collect()).collect()),
            Ok(Reply::Error { error }) => Err(format!("text {}: {error}", k + 1)),
            Err(e) => Err(format!("reply {}: {e}", k + 1)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_read_as_metres_in_order() {
        let got = parse_reply("{\"polygons\":[[[1,2],[3,2],[3,4]]]}\n{\"polygons\":[]}\n", 2).unwrap();
        assert_eq!(got, vec![vec![vec![[1e-3, 2e-3], [3e-3, 2e-3], [3e-3, 4e-3]]], vec![]]);
    }

    #[test]
    fn a_short_or_failed_reply_is_an_error() {
        assert!(parse_reply("{\"polygons\":[]}\n", 2).unwrap_err().contains("1 replies to 2"));
        assert!(parse_reply("{\"polygons\":[]}\n{\"error\":\"h_align \\\"up\\\"\"}\n", 2).unwrap_err().contains("text 2: h_align"));
        assert!(parse_reply("not json\n", 1).unwrap_err().starts_with("reply 1:"));
    }
}
