//! The router in a browser tab. The page's Web Worker hands over a board's text, its project's
//! text and the command's options; the run streams back as the studio's live frames, one JSON
//! line at a time, and the routed board's text is returned at the end. A run cannot be stopped
//! from inside: the page ends it by terminating the worker.

use std::cell::RefCell;
use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use board::Board;
use wasm_bindgen::prelude::*;

/// Live frames handed to a JavaScript function, a line at a time.
struct Lines {
    pending: Vec<u8>,
    send: js_sys::Function,
}

// A page's worker runs on one thread, so the function is never touched from another; `Live`
// asks for `Send` only because the command streams from several.
unsafe impl Send for Lines {}

impl Write for Lines {
    // Serde writes a frame in many small pieces, so only the new bytes are searched for the end
    // of a line; searching all that is pending made a board's first frame take most of a minute.
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        for (k, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                self.pending.extend_from_slice(&bytes[..k]);
                let text = String::from_utf8_lossy(&self.pending).into_owned();
                self.pending.clear();
                let _ = self.send.call1(&JsValue::NULL, &JsValue::from_str(&text));
                return self.write(&bytes[k + 1..]).map(|n| n + k + 1);
            }
        }
        self.pending.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

thread_local! {
    /// The message of the last panic, kept for the page: WebAssembly reports one only as
    /// `unreachable`.
    static PANIC: RefCell<String> = const { RefCell::new(String::new()) };
}

/// What the last run that failed with `unreachable` panicked with, and where.
#[wasm_bindgen]
pub fn last_panic() -> String {
    PANIC.with(|p| p.borrow().clone())
}

/// Routes `pcb` with the rules in `pro` (a `.kicad_pro`'s text) and `dru` (the custom rules of
/// the `.kicad_dru` beside it, or nothing), as the command would with `options` (its flags,
/// space separated), calling `on_line` with every live frame. Returns the routed board's text.
#[wasm_bindgen]
pub fn route(pcb: &str, pro: &str, dru: &str, options: &str, fps: f64, on_line: js_sys::Function) -> Result<String, JsValue> {
    std::panic::set_hook(Box::new(|info| PANIC.with(|p| *p.borrow_mut() = info.to_string())));
    let mut args: Vec<String> = ["route", "board.kicad_pcb", "-o", "routed.kicad_pcb"].map(String::from).to_vec();
    args.extend(options.split_whitespace().map(String::from));
    let a = ultraroute::parse(&args).map_err(|e| JsValue::from_str(&e))?;
    let (pcb_model, copper) = kicad_pcb::parse_board(pcb).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let mut rules = kicad_pcb::Rules::parse(pro).map_err(|e| JsValue::from_str(&e.to_string()))?;
    if !dru.is_empty() {
        rules.apply_custom(dru, "board.kicad_dru");
    }
    let model = Board::new(&pcb_model, &copper, rules);
    let live = ultraroute::Live::new(Box::new(Lines { pending: Vec::new(), send: on_line }), fps, a.pours_block);
    Ok(ultraroute::run(&a, model, pcb, Some(live), Arc::new(AtomicBool::new(false))))
}

