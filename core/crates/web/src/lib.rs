//! The router in a browser tab. The page's Web Worker hands over a board's text, its project's
//! text and the command's options; the run streams back as the studio's live frames, one JSON
//! line at a time, and the routed board's text is returned at the end. A run cannot be stopped
//! from inside: the page ends it by terminating the worker. Where the page is cross-origin
//! isolated, negotiation's rounds go out in batches to a pool of workers, each running
//! [`route_packed_job`] on its own copy of this module.

use std::cell::RefCell;
use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use board::Board;
use grid::search::{cost_field, route_astar, GridPath};
use grid::{FieldSolver, RouteGrid, ViaSpec};
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

struct WebBatchSolver {
    batch_size: usize,
    dispatch: js_sys::Function,
    packed: Vec<u32>,
}

impl FieldSolver for WebBatchSolver {
    fn solve(&mut self, grid: &RouteGrid, sources: &[usize], via_cost: u32) -> Vec<u32> {
        cost_field(grid, sources, via_cost)
    }

    fn route(&mut self, grid: &RouteGrid, sources: &[usize], targets: &[usize], via_cost: u32) -> Option<GridPath> {
        route_astar(grid, sources, targets, via_cost)
    }

    fn batch_size(&self) -> usize {
        self.batch_size.max(1)
    }

    fn route_batch(&mut self, jobs: &[(&RouteGrid, &[usize], &[usize])], via_cost: u32) -> Vec<Option<GridPath>> {
        if jobs.is_empty() {
            return Vec::new();
        }
        if jobs.len() == 1 || self.batch_size <= 1 {
            return jobs
                .iter()
                .map(|&(g, s, t)| route_astar(g, s, t, via_cost))
                .collect();
        }
        let (first, _, _) = jobs[0];
        let nx = first.nx;
        let ny = first.ny;
        let layers = first.layers.len();
        let cells = nx * ny;
        let per_field = cells * layers;
        let blocked_words = per_field.div_ceil(32);
        let via_words = cells.div_ceil(32);
        let header_len = 6 + jobs.len();

        self.packed.clear();
        self.packed.resize(header_len, 0);
        self.packed[0] = jobs.len() as u32;
        self.packed[1] = nx as u32;
        self.packed[2] = ny as u32;
        self.packed[3] = layers as u32;
        self.packed[4] = via_cost;
        self.packed[5] = u32::from(first.via.is_some());

        for (k, &(grid, sources, targets)) in jobs.iter().enumerate() {
            self.packed[6 + k] = self.packed.len() as u32;
            let b_start = self.packed.len();
            self.packed.resize(b_start + blocked_words, 0);
            for (i, &b) in grid.blocked.iter().enumerate() {
                if b {
                    self.packed[b_start + (i >> 5)] |= 1u32 << (i & 31);
                }
            }
            let v_start = self.packed.len();
            self.packed.resize(v_start + via_words, 0);
            for (i, &b) in grid.via_blocked.iter().enumerate() {
                if b {
                    self.packed[v_start + (i >> 5)] |= 1u32 << (i & 31);
                }
            }
            if grid.penalty.is_empty() {
                self.packed.push(0);
            } else {
                let non_zero = grid.penalty.iter().filter(|&&p| p != 0).count();
                if non_zero == 0 {
                    self.packed.push(0);
                } else if non_zero * 2 < per_field {
                    self.packed.push(2);
                    self.packed.push(non_zero as u32);
                    for (idx, &p) in grid.penalty.iter().enumerate() {
                        if p != 0 {
                            self.packed.push(idx as u32);
                            self.packed.push(p);
                        }
                    }
                } else {
                    self.packed.push(1);
                    self.packed.extend_from_slice(&grid.penalty);
                }
            }
            self.packed.push(sources.len() as u32);
            self.packed.extend(sources.iter().map(|&s| s as u32));
            self.packed.push(targets.len() as u32);
            self.packed.extend(targets.iter().map(|&t| t as u32));
        }

        let dispatched = {
            let view = unsafe { js_sys::Uint32Array::view(&self.packed) };
            self.dispatch.call1(&JsValue::NULL, &view).ok()
        };
        if let Some(val) = dispatched {
            if val.is_instance_of::<js_sys::Uint32Array>() {
                let arr = js_sys::Uint32Array::from(val);
                let out_words = arr.to_vec();
                let mut pos = 0usize;
                let mut out = Vec::with_capacity(jobs.len());
                for _ in 0..jobs.len() {
                    if pos >= out_words.len() {
                        out.push(None);
                        continue;
                    }
                    let len = out_words[pos] as usize;
                    pos += 1;
                    if out_words[pos - 1] == u32::MAX {
                        // Too long for the pool's slot: routed here instead.
                        let (g, s, t) = jobs[out.len()];
                        out.push(route_astar(g, s, t, via_cost));
                    } else if len == 0 || pos + len > out_words.len() {
                        pos = (pos + len).min(out_words.len());
                        out.push(None);
                    } else {
                        let path: GridPath = out_words[pos..pos + len]
                            .iter()
                            .map(|&idx| {
                                let index = idx as usize;
                                let cell = index % cells;
                                (index / cells, cell % nx, cell / nx)
                            })
                            .collect();
                        pos += len;
                        out.push(Some(path));
                    }
                }
                if out.len() == jobs.len() {
                    return out;
                }
            }
        }
        jobs.iter()
            .map(|&(g, s, t)| route_astar(g, s, t, via_cost))
            .collect()
    }
}

/// Unpacks and routes a single job from a packed batch buffer in a worker thread.
/// Returns `[len, node_0, node_1, ...]` (`[0]` when unroutable).
#[wasm_bindgen]
pub fn route_packed_job(words: &[u32], job_index: usize) -> Vec<u32> {
    thread_local! {
        static GRID_BUF: RefCell<RouteGrid> = const {
            RefCell::new(RouteGrid {
                net: 0,
                x0: 0,
                y0: 0,
                h: 50_000,
                nx: 0,
                ny: 0,
                layers: Vec::new(),
                half_width: 0,
                via: None,
                blocked: Vec::new(),
                via_blocked: Vec::new(),
                penalty: Vec::new(),
                pours_block: false,
            })
        };
    }
    if words.len() < 6 {
        return vec![0];
    }
    let count = words[0] as usize;
    if job_index >= count || words.len() < 6 + count {
        return vec![0];
    }
    let nx = words[1] as usize;
    let ny = words[2] as usize;
    let layers = words[3] as usize;
    let via_cost = words[4];
    let via_allowed = words[5] != 0;
    let cells = nx * ny;
    let per_field = cells * layers;
    let blocked_words = per_field.div_ceil(32);
    let via_words = cells.div_ceil(32);
    let mut pos = words[6 + job_index] as usize;
    if pos + blocked_words + via_words + 1 > words.len() {
        return vec![0];
    }

    GRID_BUF.with(|buf| {
        let mut grid = buf.borrow_mut();
        grid.nx = nx;
        grid.ny = ny;
        if grid.layers.len() != layers {
            grid.layers = (0..layers).collect();
        }
        grid.via = via_allowed.then_some(ViaSpec { radius: 1, drill_radius: 1 });
        grid.blocked.resize(per_field, false);
        for i in 0..per_field {
            grid.blocked[i] = ((words[pos + (i >> 5)] >> (i & 31)) & 1) != 0;
        }
        pos += blocked_words;

        grid.via_blocked.resize(cells, false);
        for i in 0..cells {
            grid.via_blocked[i] = ((words[pos + (i >> 5)] >> (i & 31)) & 1) != 0;
        }
        pos += via_words;

        let pen_mode = words[pos];
        pos += 1;
        match pen_mode {
            0 => grid.penalty.clear(),
            1 => {
                if pos + per_field > words.len() {
                    return vec![0];
                }
                grid.penalty.clear();
                grid.penalty.extend_from_slice(&words[pos..pos + per_field]);
                pos += per_field;
            }
            2 => {
                if pos >= words.len() {
                    return vec![0];
                }
                let nz = words[pos] as usize;
                pos += 1;
                if pos + nz * 2 > words.len() {
                    return vec![0];
                }
                grid.penalty.clear();
                grid.penalty.resize(per_field, 0);
                for k in 0..nz {
                    let idx = words[pos + k * 2] as usize;
                    let val = words[pos + k * 2 + 1];
                    if idx < per_field {
                        grid.penalty[idx] = val;
                    }
                }
                pos += nz * 2;
            }
            _ => return vec![0],
        }

        if pos >= words.len() {
            return vec![0];
        }
        let s_len = words[pos] as usize;
        pos += 1;
        if pos + s_len >= words.len() {
            return vec![0];
        }
        let sources: Vec<usize> = words[pos..pos + s_len].iter().map(|&x| x as usize).collect();
        pos += s_len;

        let t_len = words[pos] as usize;
        pos += 1;
        if pos + t_len > words.len() {
            return vec![0];
        }
        let targets: Vec<usize> = words[pos..pos + t_len].iter().map(|&x| x as usize).collect();

        match route_astar(&grid, &sources, &targets, via_cost) {
            Some(path) => {
                let mut out = Vec::with_capacity(1 + path.len());
                out.push(path.len() as u32);
                for (l, i, j) in path {
                    out.push((l * cells + j * nx + i) as u32);
                }
                out
            }
            None => vec![0],
        }
    })
}

/// The command's arguments and the board, from what the page hands over.
fn setup(pcb: &str, pro: &str, dru: &str, options: &str) -> Result<(ultraroute::Args, Board), JsValue> {
    std::panic::set_hook(Box::new(|info| PANIC.with(|p| *p.borrow_mut() = info.to_string())));
    let mut args: Vec<String> = ["route", "board.kicad_pcb", "-o", "routed.kicad_pcb"].map(String::from).to_vec();
    args.extend(options.split_whitespace().map(String::from));
    let a = ultraroute::parse(&args).map_err(|e| JsValue::from_str(&e))?;
    let (pcb_model, copper) = kicad_pcb::parse_board(pcb).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let mut rules = kicad_pcb::Rules::parse(pro).map_err(|e| JsValue::from_str(&e.to_string()))?;
    if !dru.is_empty() {
        rules.apply_custom(dru, "board.kicad_dru");
    }
    Ok((a, Board::new(&pcb_model, &copper, rules)))
}

/// Routes `pcb` with the rules in `pro` (a `.kicad_pro`'s text) and `dru` (the custom rules of
/// the `.kicad_dru` beside it, or nothing), as the command would with `options` (its flags,
/// space separated), calling `on_line` with every live frame. Returns the routed board's text.
#[wasm_bindgen]
pub fn route(pcb: &str, pro: &str, dru: &str, options: &str, fps: f64, on_line: js_sys::Function) -> Result<String, JsValue> {
    let (a, model) = setup(pcb, pro, dru, options)?;
    let live = ultraroute::Live::new(Box::new(Lines { pending: Vec::new(), send: on_line }), fps, a.pours_block);
    Ok(ultraroute::run(&a, model, pcb, Some(live), Arc::new(AtomicBool::new(false))))
}

/// As [`route`], with negotiation's rounds handed `batch_size` connections at a time to
/// `dispatch_batch`, which routes them on the page's worker pool and answers with their paths.
#[wasm_bindgen]
#[allow(clippy::too_many_arguments)]
pub fn route_accelerated(
    pcb: &str,
    pro: &str,
    dru: &str,
    options: &str,
    fps: f64,
    on_line: js_sys::Function,
    batch_size: usize,
    dispatch_batch: js_sys::Function,
) -> Result<String, JsValue> {
    let (mut a, model) = setup(pcb, pro, dru, options)?;
    // The module has no threads of its own: the pool is the only parallelism, through batches.
    a.threads = Some(1);
    let live = ultraroute::Live::new(Box::new(Lines { pending: Vec::new(), send: on_line }), fps, a.pours_block);
    let solver: Option<Box<dyn FieldSolver>> =
        (batch_size > 1).then(|| Box::new(WebBatchSolver { batch_size, dispatch: dispatch_batch, packed: Vec::new() }) as Box<dyn FieldSolver>);
    Ok(ultraroute::run_with_solver(&a, model, pcb, Some(live), Arc::new(AtomicBool::new(false)), solver))
}
