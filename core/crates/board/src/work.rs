//! Counts and CPU timers for the work the router does, kept for a run's stats (`--report`):
//! how often copper is checked or connectivity is worked out, and how many nanoseconds of thread
//! CPU time went to each kind of operation (`Bucket`). Nothing in the router reads these figures
//! to decide anything, so turning them on or off cannot change a route.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// `check_item` and `check_item_with_pours` calls.
pub static CHECKS: AtomicU64 = AtomicU64::new(0);
/// Connectivity worked out, for the whole board or one net.
pub static CONNECTIVITY: AtomicU64 = AtomicU64::new(0);

/// One more of `counter`.
pub fn count(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// The sub-operations a stage's CPU time is split across. Nested scopes credit only the
/// innermost active bucket on each thread, so the eight buckets never overlap and their sum on
/// every thread is at most the thread's own CPU time across the stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Bucket {
    /// A* searches, cost fields and path backtracing (`grid::search`).
    Astar = 0,
    /// Building whole-board or windowed obstacle maps from the spatial index (`grid::ObstacleMap`).
    MapBuild = 1,
    /// Stamping items and patching congestion or soft-copper prices onto grids.
    MapPatch = 2,
    /// Negotiation's clearance-zone claims, collision checks and conflict-graph commit order.
    Zones = 3,
    /// Design-rule checks (`board::check`).
    Checks = 4,
    /// Union-find clustering and Prim spanning trees (`board::connect`).
    Connectivity = 5,
    /// Finding legal launch/landing cells on pads and clusters, and reachable-tree respanning.
    Terminals = 6,
    /// Laying copper, pulling tight, mending joints, pruning, harmonic flow and length bumps.
    Geometry = 7,
}

pub const BUCKET_COUNT: usize = 8;

const NO_SINK: u8 = 255;
const IDLE: u8 = 254;

/// Process CPU time in nanoseconds across all threads (`0` in WebAssembly).
#[cfg(not(target_arch = "wasm32"))]
#[inline]
pub fn process_cpu_ns() -> u64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes the timespec it is given and reads nothing else.
    unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut t) };
    (t.tv_sec as u64).saturating_mul(1_000_000_000).saturating_add(t.tv_nsec as u64)
}

#[cfg(target_arch = "wasm32")]
#[inline]
pub fn process_cpu_ns() -> u64 {
    0
}

/// Calling thread's CPU time in nanoseconds (`0` in WebAssembly).
#[cfg(not(target_arch = "wasm32"))]
#[inline]
pub fn thread_cpu_ns() -> u64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes the timespec it is given and reads nothing else.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) };
    (t.tv_sec as u64).saturating_mul(1_000_000_000).saturating_add(t.tv_nsec as u64)
}

#[cfg(target_arch = "wasm32")]
#[inline]
pub fn thread_cpu_ns() -> u64 {
    0
}

/// One top-level move or round recorded along the run's CPU timeline so the report can slice
/// any window of the run (such as the second half of a failing run) or find the exact moment
/// the open connection count last fell.
#[derive(Debug, Clone)]
pub struct RawEvent {
    pub start_ns: u64,
    pub end_ns: u64,
    pub stage: &'static str,
    pub kind: &'static str,
    pub outcome: &'static str,
    pub open_after: Option<usize>,
    pub buckets_ns: [u64; BUCKET_COUNT],
}

/// Per-run accumulator owned by `ultraroute::route` and shared with any scoped worker threads
/// it spawns (`negotiate`'s Jacobi batch). Keeping one sink per run keeps concurrent runs in
/// tests from mixing their sub-stage times.
pub struct CpuSink {
    /// Main-thread CPU clock reading when the sink was created.
    pub origin_ns: u64,
    /// Main-thread CPU nanoseconds spent in stats-only bookkeeping while paused (`Mark::take`).
    pub paused_ns: AtomicU64,
    /// Total thread CPU nanoseconds spent on spawned worker threads (excluding the main thread).
    pub worker_ns: AtomicU64,
    /// Exclusive CPU nanoseconds per [`Bucket`], summed over the run's main and worker threads.
    pub buckets_ns: [AtomicU64; BUCKET_COUNT],
    /// Name of the stage currently running on the main thread.
    pub stage: Mutex<&'static str>,
    /// Non-overlapping top-level events recorded in chronological order.
    pub events: Mutex<Vec<RawEvent>>,
}

impl Default for CpuSink {
    fn default() -> Self {
        Self {
            origin_ns: thread_cpu_ns(),
            paused_ns: AtomicU64::new(0),
            worker_ns: AtomicU64::new(0),
            buckets_ns: std::array::from_fn(|_| AtomicU64::new(0)),
            stage: Mutex::new("set-up"),
            events: Mutex::new(Vec::new()),
        }
    }
}

/// A snapshot of a [`CpuSink`] at one moment; subtracting an earlier snapshot gives the work
/// between the two.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuSnap {
    pub run_cpu_ns: u64,
    pub worker_ns: u64,
    pub buckets_ns: [u64; BUCKET_COUNT],
}

impl CpuSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Cumulative routing CPU nanoseconds on the main thread plus any joined worker threads,
    /// excluding paused stats-counting sections.
    pub fn run_cpu_ns(&self) -> u64 {
        let main = thread_cpu_ns()
            .saturating_sub(self.origin_ns)
            .saturating_sub(self.paused_ns.load(Ordering::Relaxed));
        main.saturating_add(self.worker_ns.load(Ordering::Relaxed))
    }

    pub fn snap(&self) -> CpuSnap {
        let mut buckets_ns = [0u64; BUCKET_COUNT];
        for (dst, src) in buckets_ns.iter_mut().zip(&self.buckets_ns) {
            *dst = src.load(Ordering::Relaxed);
        }
        CpuSnap {
            run_cpu_ns: self.run_cpu_ns(),
            worker_ns: self.worker_ns.load(Ordering::Relaxed),
            buckets_ns,
        }
    }

    pub fn set_stage(&self, stage: &'static str) {
        *self.stage.lock().unwrap_or_else(|e| e.into_inner()) = stage;
    }

    pub fn events_len(&self) -> usize {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn push_event(&self, ev: RawEvent) {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).push(ev);
    }

    pub fn take_events(&self) -> Vec<RawEvent> {
        std::mem::take(&mut *self.events.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl CpuSnap {
    pub fn since(&self, earlier: &CpuSnap) -> CpuSnap {
        let mut buckets_ns = [0u64; BUCKET_COUNT];
        for ((dst, cur), old) in buckets_ns.iter_mut().zip(&self.buckets_ns).zip(&earlier.buckets_ns) {
            *dst = cur.saturating_sub(*old);
        }
        CpuSnap {
            run_cpu_ns: self.run_cpu_ns.saturating_sub(earlier.run_cpu_ns),
            worker_ns: self.worker_ns.saturating_sub(earlier.worker_ns),
            buckets_ns,
        }
    }
}

thread_local! {
    static ACTIVE: Cell<u8> = const { Cell::new(NO_SINK) };
    static SINCE_NS: Cell<u64> = const { Cell::new(0) };
    static EVENT_DEPTH: Cell<u32> = const { Cell::new(0) };
    static SINK: RefCell<Option<Arc<CpuSink>>> = const { RefCell::new(None) };
}

/// Whether timing is enabled on the calling thread.
#[inline]
pub fn timing_active() -> bool {
    ACTIVE.with(|a| a.get() != NO_SINK)
}

/// The calling thread's CPU nanoseconds when timing is active, or `0` without reading the clock
/// when timing is off (`--no-timing`).
#[inline]
pub fn timed_thread_ns() -> u64 {
    if timing_active() { thread_cpu_ns() } else { 0 }
}

/// Cumulative CPU nanoseconds on the active run's sink (main thread plus joined workers), or `0`
/// without reading the clock when timing is off (`--no-timing`).
#[inline]
pub fn timed_run_ns() -> u64 {
    if !timing_active() {
        return 0;
    }
    SINK.with(|s| s.borrow().as_ref().map_or_else(thread_cpu_ns, |k| k.run_cpu_ns()))
}

/// The sink installed on the calling thread, if any, to hand across `std::thread::scope`.
pub fn current_sink() -> Option<Arc<CpuSink>> {
    SINK.with(|s| s.borrow().clone())
}

/// Installs `sink` on the calling thread for the life of the returned guard. When `sink` is
/// `None` (`--no-timing`), timing stays disabled and bucket guards cost a single byte check.
pub fn install_sink(sink: Option<Arc<CpuSink>>) -> SinkGuard {
    let prev_sink = SINK.with(|s| s.replace(sink.clone()));
    let prev_active = ACTIVE.with(|a| a.replace(if sink.is_some() { IDLE } else { NO_SINK }));
    let prev_since = SINCE_NS.with(|t| t.replace(if sink.is_some() { thread_cpu_ns() } else { 0 }));
    SinkGuard { prev_sink, prev_active, prev_since, worker_start: None }
}

/// Attaches a scoped worker thread to `sink` and adds the worker's total thread CPU time to
/// `sink.worker_ns` when the guard drops at the end of the worker's run.
pub fn attach_worker(sink: Option<Arc<CpuSink>>) -> SinkGuard {
    let start = sink.as_ref().map(|_| thread_cpu_ns());
    let prev_sink = SINK.with(|s| s.replace(sink.clone()));
    let prev_active = ACTIVE.with(|a| a.replace(if sink.is_some() { IDLE } else { NO_SINK }));
    let prev_since = SINCE_NS.with(|t| t.replace(start.unwrap_or(0)));
    SinkGuard { prev_sink, prev_active, prev_since, worker_start: start }
}

pub struct SinkGuard {
    prev_sink: Option<Arc<CpuSink>>,
    prev_active: u8,
    prev_since: u64,
    worker_start: Option<u64>,
}

impl Drop for SinkGuard {
    fn drop(&mut self) {
        if let Some(start) = self.worker_start {
            let end = thread_cpu_ns();
            SINK.with(|s| {
                if let Some(sink) = s.borrow().as_ref() {
                    sink.worker_ns.fetch_add(end.saturating_sub(start), Ordering::Relaxed);
                }
            });
        }
        SINK.with(|s| {
            *s.borrow_mut() = self.prev_sink.take();
        });
        ACTIVE.with(|a| a.set(self.prev_active));
        SINCE_NS.with(|t| t.set(self.prev_since));
    }
}

/// Pauses bucket attribution and `run_cpu_ns` while stats-only bookkeeping (`Mark::take`) runs,
/// returning the CPU nanoseconds spent inside the paused block when closed.
pub fn pause_sink() -> PauseGuard {
    let prev = ACTIVE.with(|a| a.replace(NO_SINK));
    if prev == NO_SINK {
        return PauseGuard { prev, start_ns: 0 };
    }
    let now = thread_cpu_ns();
    if (prev as usize) < BUCKET_COUNT {
        let elapsed = now.saturating_sub(SINCE_NS.with(|t| t.get()));
        if elapsed > 0 {
            SINK.with(|s| {
                if let Some(sink) = s.borrow().as_ref() {
                    sink.buckets_ns[prev as usize].fetch_add(elapsed, Ordering::Relaxed);
                }
            });
        }
    }
    PauseGuard { prev, start_ns: now }
}

pub struct PauseGuard {
    prev: u8,
    start_ns: u64,
}

impl PauseGuard {
    pub fn close(mut self) -> u64 {
        let dt = self.finish_inner();
        self.prev = NO_SINK;
        dt
    }

    fn finish_inner(&mut self) -> u64 {
        if self.prev == NO_SINK {
            return 0;
        }
        let end = thread_cpu_ns();
        let dt = end.saturating_sub(self.start_ns);
        SINK.with(|s| {
            if let Some(sink) = s.borrow().as_ref() {
                sink.paused_ns.fetch_add(dt, Ordering::Relaxed);
            }
        });
        ACTIVE.with(|a| a.set(self.prev));
        SINCE_NS.with(|t| t.set(end));
        dt
    }
}

impl Drop for PauseGuard {
    fn drop(&mut self) {
        let _ = self.finish_inner();
    }
}

/// Starts a top-level timeline event when `EVENT_DEPTH` is zero. Nested calls (such as `shove`
/// calling `route_open` or `rip_up` internally) increment depth without emitting overlapping
/// events.
#[inline]
pub fn begin_event() -> EventGuard {
    if !timing_active() {
        return EventGuard { active: false, top: false, snap: CpuSnap::default() };
    }
    begin_event_slow()
}

#[cold]
#[inline(never)]
fn begin_event_slow() -> EventGuard {
    let d = EVENT_DEPTH.with(|c| {
        let cur = c.get();
        c.set(cur + 1);
        cur
    });
    if d > 0 {
        return EventGuard { active: true, top: false, snap: CpuSnap::default() };
    }
    let snap = SINK.with(|s| s.borrow().as_ref().map(|k| k.snap()).unwrap_or_default());
    EventGuard { active: true, top: true, snap }
}

pub struct EventGuard {
    active: bool,
    top: bool,
    snap: CpuSnap,
}

impl EventGuard {
    #[inline]
    pub fn finish(mut self, kind: &'static str, outcome: &'static str, open_after: Option<usize>) {
        if self.top {
            self.record(kind, outcome, open_after);
            self.top = false;
        }
    }

    #[cold]
    #[inline(never)]
    fn record(&self, kind: &'static str, outcome: &'static str, open_after: Option<usize>) {
        SINK.with(|s| {
            if let Some(sink) = s.borrow().as_ref() {
                let end = sink.snap();
                let delta = end.since(&self.snap);
                let stage = *sink.stage.lock().unwrap_or_else(|e| e.into_inner());
                sink.events.lock().unwrap_or_else(|e| e.into_inner()).push(RawEvent {
                    start_ns: self.snap.run_cpu_ns,
                    end_ns: end.run_cpu_ns,
                    stage,
                    kind,
                    outcome,
                    open_after,
                    buckets_ns: delta.buckets_ns,
                });
            }
        });
    }
}

impl Drop for EventGuard {
    #[inline]
    fn drop(&mut self) {
        if self.active {
            if self.top {
                self.record("aborted", "cut", None);
            }
            EVENT_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
        }
    }
}

/// Credits the calling thread's CPU time to `bucket` until the returned [`Scope`] drops,
/// pausing any outer bucket so nested calls never double-count.
#[inline]
pub fn enter(bucket: Bucket) -> Scope {
    let b = bucket as u8;
    let cur = ACTIVE.with(|a| a.get());
    if cur == NO_SINK || cur == b {
        return Scope { prev: cur, entered: b, switched: false };
    }
    enter_slow(cur, b)
}

#[cold]
#[inline(never)]
fn enter_slow(cur: u8, b: u8) -> Scope {
    let now = thread_cpu_ns();
    if (cur as usize) < BUCKET_COUNT {
        let elapsed = now.saturating_sub(SINCE_NS.with(|t| t.get()));
        if elapsed > 0 {
            SINK.with(|s| {
                if let Some(sink) = s.borrow().as_ref() {
                    sink.buckets_ns[cur as usize].fetch_add(elapsed, Ordering::Relaxed);
                }
            });
        }
    }
    ACTIVE.with(|a| a.set(b));
    SINCE_NS.with(|t| t.set(now));
    Scope { prev: cur, entered: b, switched: true }
}

#[cold]
#[inline(never)]
fn exit_slow(prev: u8, entered: u8) {
    let now = thread_cpu_ns();
    if (entered as usize) < BUCKET_COUNT {
        let elapsed = now.saturating_sub(SINCE_NS.with(|t| t.get()));
        if elapsed > 0 {
            SINK.with(|s| {
                if let Some(sink) = s.borrow().as_ref() {
                    sink.buckets_ns[entered as usize].fetch_add(elapsed, Ordering::Relaxed);
                }
            });
        }
    }
    ACTIVE.with(|a| a.set(prev));
    SINCE_NS.with(|t| t.set(now));
}

pub struct Scope {
    prev: u8,
    entered: u8,
    switched: bool,
}

impl Drop for Scope {
    #[inline]
    fn drop(&mut self) {
        if self.switched {
            exit_slow(self.prev, self.entered);
        }
    }
}
