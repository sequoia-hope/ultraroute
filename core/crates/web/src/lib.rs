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
use grid::{FieldSolver, PricedJob, RoundPrices, RouteGrid, ViaSpec};
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
    /// Whether the pool takes a round's prices once and prices each connection itself; off for
    /// the WebGPU path, which takes every connection's prices.
    priced: bool,
    /// The round whose prices the pool holds.
    sent: Option<u64>,
}

/// A message to the pool: a round's shared prices, then batches of connections priced from them.
const ROUND: u32 = 1;
const BATCH: u32 = 2;

impl WebBatchSolver {
    /// Hands `self.packed` to the pool; its answer, if it gave one.
    fn send(&mut self) -> Option<Vec<u32>> {
        let view = unsafe { js_sys::Uint32Array::view(&self.packed) };
        let answer = self.dispatch.call1(&JsValue::NULL, &view).ok()?;
        answer.is_instance_of::<js_sys::Uint32Array>().then(|| js_sys::Uint32Array::from(answer).to_vec())
    }
}

/// Packs a grid's blocked nodes and via sites as bits.
fn pack_bits(out: &mut Vec<u32>, bits: &[bool]) {
    let start = out.len();
    out.resize(start + bits.len().div_ceil(32), 0);
    for (i, &b) in bits.iter().enumerate() {
        if b {
            out[start + (i >> 5)] |= 1u32 << (i & 31);
        }
    }
}

/// Paths as the pool answers them: per job its length and nodes, or [`TOO_LONG`] for a path
/// that did not fit its slot, routed here instead. `None` when the answer does not add up.
fn unpack_paths(words: &[u32], jobs: usize, cells: usize, nx: usize, route_here: &mut dyn FnMut(usize) -> Option<GridPath>) -> Option<Vec<Option<GridPath>>> {
    let mut pos = 0usize;
    let mut out = Vec::with_capacity(jobs);
    for k in 0..jobs {
        let len = *words.get(pos)?;
        pos += 1;
        if len == TOO_LONG {
            out.push(route_here(k));
        } else if len == 0 {
            out.push(None);
        } else {
            let nodes = words.get(pos..pos + len as usize)?;
            pos += len as usize;
            out.push(Some(nodes.iter().map(|&idx| {
                let index = idx as usize;
                let cell = index % cells;
                (index / cells, cell % nx, cell / nx)
            }).collect()));
        }
    }
    Some(out)
}

/// A slot's length word for a path too long for the slot.
const TOO_LONG: u32 = u32::MAX;

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

    fn route_priced(&mut self, prices: &RoundPrices, jobs: &[PricedJob], via_cost: u32) -> Option<Vec<Option<GridPath>>> {
        if !self.priced || jobs.len() < 2 {
            return None;
        }
        if self.sent != Some(prices.generation) {
            // The round: its generation, node count, classes, present price, history, then each
            // class's counts two to a word.
            let nodes = prices.history.len();
            self.packed.clear();
            self.packed.extend([ROUND, prices.generation as u32, (prices.generation >> 32) as u32, nodes as u32, prices.counts.len() as u32]);
            let present = prices.present.to_bits();
            self.packed.extend([present as u32, (present >> 32) as u32]);
            self.packed.extend_from_slice(prices.history);
            for counts in &prices.counts {
                self.packed.extend(counts.chunks(2).map(|c| u32::from(c[0]) | u32::from(c.get(1).copied().unwrap_or(0)) << 16));
            }
            self.send()?;
            self.sent = Some(prices.generation);
        }
        let first = jobs[0].grid;
        let (nx, ny, layers) = (first.nx, first.ny, first.layers.len());
        let header = 7;
        self.packed.clear();
        self.packed.extend([BATCH, nx as u32, ny as u32, layers as u32, via_cost, u32::from(first.via.is_some()), jobs.len() as u32]);
        self.packed.resize(header + jobs.len(), 0);
        for (k, job) in jobs.iter().enumerate() {
            self.packed[header + k] = self.packed.len() as u32;
            let weight = job.weight.to_bits();
            self.packed.extend([job.class as u32, weight as u32, (weight >> 32) as u32, job.own.len() as u32]);
            self.packed.extend_from_slice(&job.own);
            pack_bits(&mut self.packed, &job.grid.blocked);
            pack_bits(&mut self.packed, &job.grid.via_blocked);
            self.packed.push(job.sources.len() as u32);
            self.packed.extend(job.sources.iter().map(|&s| s as u32));
            self.packed.push(job.targets.len() as u32);
            self.packed.extend(job.targets.iter().map(|&t| t as u32));
        }
        let answer = self.send()?;
        let mut grid = None;
        unpack_paths(&answer, jobs.len(), nx * ny, nx, &mut |k| {
            // Priced here, as a worker would have.
            let job = &jobs[k];
            let grid = grid.get_or_insert_with(|| job.grid.clone());
            grid.clone_from(job.grid);
            grid::price_into(prices.history, prices.counts[job.class], job.own.iter().copied(), prices.present, job.weight, &mut grid.penalty);
            route_astar(grid, job.sources, job.targets, via_cost)
        })
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

/// A round's prices as a pool worker holds them, from the [`ROUND`] message.
struct Round {
    history: Vec<u32>,
    counts: Vec<Vec<u16>>,
    present: f64,
}

thread_local! {
    static ROUND_HELD: RefCell<Option<Round>> = const { RefCell::new(None) };
    static PRICED_GRID: RefCell<Option<RouteGrid>> = const { RefCell::new(None) };
}

/// A pool worker takes a round's shared prices (a [`ROUND`] message), once a round.
#[wasm_bindgen]
pub fn set_round(words: &[u32]) {
    let (Some(&nodes), Some(&classes)) = (words.get(3), words.get(4)) else { return };
    let (nodes, classes) = (nodes as usize, classes as usize);
    let half = nodes.div_ceil(2);
    if words.len() < 7 + nodes + classes * half {
        return;
    }
    let present = f64::from_bits(u64::from(words[5]) | u64::from(words[6]) << 32);
    let history = words[7..7 + nodes].to_vec();
    let counts = (0..classes)
        .map(|c| {
            let packed = &words[7 + nodes + c * half..7 + nodes + (c + 1) * half];
            (0..nodes).map(|i| (packed[i / 2] >> (16 * (i % 2))) as u16).collect()
        })
        .collect();
    ROUND_HELD.with(|r| *r.borrow_mut() = Some(Round { history, counts, present }));
}

/// A pool worker routes one connection of a [`BATCH`] message (`header`, its first seven words,
/// and `job`, the connection's part), priced from the round it holds as the router would price
/// it. Returns `[len, node, …]`, `[0]` when there is no way, `[TOO_LONG]` when it holds no round.
#[wasm_bindgen]
pub fn route_priced_job(header: &[u32], job: &[u32]) -> Vec<u32> {
    let [_, nx, ny, layers, via_cost, via_allowed, _] = header[..] else { return vec![0] };
    let (nx, ny, layers) = (nx as usize, ny as usize, layers as usize);
    let cells = nx * ny;
    let nodes = cells * layers;
    let mut pos = 0usize;
    let mut take = |n: usize| -> Option<&[u32]> {
        let s = job.get(pos..pos + n)?;
        pos += n;
        Some(s)
    };
    let routed = (|| -> Option<Option<GridPath>> {
        let [class, w0, w1, own_len] = take(4)?[..] else { return None };
        let weight = f64::from_bits(u64::from(w0) | u64::from(w1) << 32);
        let own = take(own_len as usize)?.to_vec();
        let blocked = take(nodes.div_ceil(32))?.to_vec();
        let via_bits = take(cells.div_ceil(32))?.to_vec();
        let s_len = take(1)?[0] as usize;
        let sources: Vec<usize> = take(s_len)?.iter().map(|&x| x as usize).collect();
        let t_len = take(1)?[0] as usize;
        let targets: Vec<usize> = take(t_len)?.iter().map(|&x| x as usize).collect();
        ROUND_HELD.with(|round| {
            let round = round.borrow();
            let round = round.as_ref()?;
            let counts = round.counts.get(class as usize)?;
            if round.history.len() != nodes {
                return None;
            }
            PRICED_GRID.with(|g| {
                let mut g = g.borrow_mut();
                let grid = g.get_or_insert_with(|| RouteGrid {
                    net: 0,
                    x0: 0,
                    y0: 0,
                    h: 1,
                    nx: 0,
                    ny: 0,
                    layers: Vec::new(),
                    half_width: 0,
                    via: None,
                    blocked: Vec::new(),
                    via_blocked: Vec::new(),
                    penalty: Vec::new(),
                    pours_block: false,
                });
                (grid.nx, grid.ny) = (nx, ny);
                grid.layers = (0..layers).collect();
                grid.via = (via_allowed != 0).then_some(ViaSpec { radius: 1, drill_radius: 1 });
                grid.blocked.clear();
                grid.blocked.extend((0..nodes).map(|i| blocked[i >> 5] >> (i & 31) & 1 != 0));
                grid.via_blocked.clear();
                grid.via_blocked.extend((0..cells).map(|i| via_bits[i >> 5] >> (i & 31) & 1 != 0));
                grid::price_into(&round.history, counts, own.iter().copied(), round.present, weight, &mut grid.penalty);
                Some(route_astar(grid, &sources, &targets, via_cost))
            })
        })
    })();
    match routed {
        Some(Some(path)) => {
            let mut out = Vec::with_capacity(1 + path.len());
            out.push(path.len() as u32);
            out.extend(path.into_iter().map(|(l, i, j)| (l * cells + j * nx + i) as u32));
            out
        }
        Some(None) => vec![0],
        None => vec![TOO_LONG],
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
/// `dispatch_batch`, which routes them on the page's worker pool and answers with their paths;
/// `priced`, the pool is sent each round's prices once and prices each connection itself.
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
    priced: bool,
) -> Result<String, JsValue> {
    let (mut a, model) = setup(pcb, pro, dru, options)?;
    // The module has no threads of its own: the pool is the only parallelism, through batches.
    a.threads = Some(1);
    let live = ultraroute::Live::new(Box::new(Lines { pending: Vec::new(), send: on_line }), fps, a.pours_block);
    let solver: Option<Box<dyn FieldSolver>> =
        (batch_size > 1).then(|| Box::new(WebBatchSolver { batch_size, dispatch: dispatch_batch, packed: Vec::new(), priced, sent: None }) as Box<dyn FieldSolver>);
    Ok(ultraroute::run_with_solver(&a, model, pcb, Some(live), Arc::new(AtomicBool::new(false)), solver))
}
