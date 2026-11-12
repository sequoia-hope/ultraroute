# ultraroute remediation plan

A staged plan to act on `REVIEW.md` (the independent review of commit `184f640`, 2026-10-28).
It is written for an agent that has not read the review's working context: every stage names the
files and functions, says what to change, how to prove it, and when to stop and report instead of
improvising. Finding numbers like `#37` refer to the tables in `REVIEW.md` §A.

Stages are ordered so that each one makes the next one measurable. Do not reorder the first six.
Do not start a stage before the previous one is merged (or the user has said to skip it).

---

## 0. How to work on this plan

### 0.1 Ground rules (from the project's standing instructions; do not relax them)

- **Never hardcode a port.** The lab page's URL comes from `proj url ultraroute-lab`. Never start an
  ad-hoc HTTP server; use `proj up ultraroute-lab` or the project's `serve.sh`.
- **Heavy commands go through `~/.claude/bin/reserve`**: cargo builds beyond one small crate, the
  test suite, and every benchmark run. Ask for what the job needs (4 for a build, 8 for the test
  suite, 16 for a benchmark run; 20 is the ceiling). If it exits 75 the command never ran: say so,
  do not rerun unbooked.
- **Memory-cap every router or test run**: prefix with
  `systemd-run --user --scope -q -p MemoryMax=8G -p MemorySwapMax=0`.
- **Run `tools/pcbench.py` from the main checkout** `~/Software/ultraroute`: `PCBench/`
  is untracked and absent in worktrees. Pin a copy of the binary per benchmark run (see §0.4).
- **Never write into `~/pcb/bluebuzz`**; work on copies. Leave the untracked `jobs/bluebuzz-holes/`,
  `jobs/earring-v2a/` and `jobs/motionhexa-unified/` alone; they belong to other sessions.
- **Do not `rustfmt` whole crates**; upstream files are not rustfmt-clean. Format only lines you
  changed, by hand.
- **Commit at the end of each stage on the stage's branch.** Do not push and do not merge to `main`
  unless the user says so. Commit messages: `Core: <what changed>` for the core, `Tools: …`,
  `Docs: …`, `CI: …`, and end with the attribution line the session's instructions give.
- **Record every benchmark and lab run** with `tools/lab.py` or `tools/pcbench.py` under the lab
  directory `~/Software/ultraroute-lab`, never inside the repository.

### 0.2 Branch and worktree per stage

```sh
cd ~/Software/ultraroute
git worktree add .claude/worktrees/remed-<n> -b remed/<n>-<short-name> main
cd .claude/worktrees/remed-<n>/core
~/.claude/bin/reserve -c 4 -l "remed-<n> build" -- cargo build --release -p ultraroute
```

Build in the worktree's own `core/target` so nothing a running benchmark uses is rebuilt under it.

### 0.3 Commands that prove a stage

| check | command (from the worktree's `core/`) |
|---|---|
| unit and integration tests | `~/.claude/bin/reserve -c 8 -l "remed tests" -- systemd-run --user --scope -q -p MemoryMax=8G -p MemorySwapMax=0 cargo test --workspace --release --no-fail-fast` |
| lint | `~/.claude/bin/reserve -c 4 -l "remed clippy" -- cargo clippy --workspace --all-targets` (25 cosmetic warnings exist at `184f640`; do not add any) |
| the two fixture boards with the benchmark recipe (see §0.5) | `tools/lab.py run remed-<n>-fixtures --title "…" --recipe "<the §0.5 recipe>" --threads 1 --timeout 240 earring=core/fixtures/earring.kicad_pcb::--strip-all bluebuzz=core/fixtures/bluebuzz-rm2.kicad_pcb::--strip-all`, run under `reserve -c 2` from the main checkout with `ULTRAROUTE_BIN` pointing at the pinned binary (`lab.py`'s own default recipe is a different, older one, so `--recipe` is required) |
| the benchmark subset | §0.4 |

At `184f640` the suite is 210 tests in 25 binaries, all passing, in about 20 s of test time.

### 0.4 The measurement protocol (used by every stage marked "measured")

The benchmark cannot see a change smaller than its noise: five runs of the same binary on the same
100 boards disagreed on the "clean" verdict for 1–7 boards per pair (`REVIEW.md` §1.7). Until
Stage 4 makes runs deterministic, a change is judged only against that floor.

1. Pin the binary: `cp core/target/release/ultraroute ~/Software/ultraroute-lab/bin/ultraroute-<commit>`
   (create `bin/` once). Record `sha256sum` of it in the run's notes.
2. Run the fixed subset, from the main checkout, under reserve:
   ```sh
   cd ~/Software/ultraroute
   ~/.claude/bin/reserve -c 16 -l "pcbench <run>" -- env ULTRAROUTE_BIN=~/Software/ultraroute-lab/bin/ultraroute-<commit> \
     python3 tools/pcbench.py run <run> --sample 200 --sample-seed 1 --timeout 120 --seed 0
   ```
   `--sample 200 --sample-seed 1` always picks the same 200 boards. A run takes about 40 minutes
   on 16 cores. Name runs `remed-<n>-<arm>` (for example `remed-3-base`, `remed-3-new`).
3. Compare with the baseline of the same subset (Stage 0 makes `remed-0-base-s0/s1/s2`): after
   Stage 2, `python3 tools/pcbench.py compare remed-0-base-s0 remed-<n>-new`; before Stage 2, use
   the one-off comparison in §0.6.
4. A stage **passes** when: clean count is not below the baseline by more than the noise floor
   (Stage 0 measures it; expect about 5 boards per 100 until Stage 4, then 0), no board gains a new
   error type, and the stage's own metric moved as intended. Any board that regresses from clean to
   not-clean must be looked at individually (open its `log.txt` and `out.drc.json`) and explained in
   the lab note.
5. Add a lab note with the numbers: `python3 tools/lab.py note <exp> "<text>"` or the pcbench run's
   `run.json` notes field once Stage 2 adds one.

### 0.5 The recipe

Until Stage 7 makes it the default, every measurement uses the benchmark recipe exactly:

```
--negotiate --rip-up --tighten --prefanout --diff-pairs --shove-plough region --shove-reach 0.5 --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8
```

with `--strip-all --seed 0 --threads 1 --timeout 120` for PCBench (the harness adds those) and
`--strip-all --seed 0 --threads 1 --timeout 240` for the two fixture boards. The fixture boards
must end at 0 unconnected and 0 KiCad errors after refill on seed 0; if a stage changes that,
stop and report.

### 0.6 One-off comparison before Stage 2 exists

```python
# python3 - <<'EOF'   (run from anywhere)
import json, glob
def load(run): return {json.load(open(f))['id']: json.load(open(f)) for f in glob.glob(f'~/Software/ultraroute-lab/pcbench/runs/{run}/*/result.json')}
a, b = load('remed-0-base-s0'), load('remed-<n>-new')
common = sorted(set(a) & set(b))
lost = [i for i in common if a[i].get('success') and not b[i].get('success')]
won  = [i for i in common if b[i].get('success') and not a[i].get('success')]
print(len(common), 'boards; clean', sum(bool(a[i].get('success')) for i in common), '->', sum(bool(b[i].get('success')) for i in common))
print('lost', lost); print('won', won)
print('vias', sum(a[i].get('vias') or 0 for i in common), '->', sum(b[i].get('vias') or 0 for i in common))
print('at limit', sum(a[i]['seconds'] >= 110 for i in common), '->', sum(b[i]['seconds'] >= 110 for i in common))
```

### 0.7 When to stop and report instead of continuing

- A test that passed at the stage's start fails and the cause is not your change.
- The benchmark loses more boards than the noise floor, or any fixture board leaves a connection
  open or gains a KiCad error.
- A step needs a design decision the plan does not settle (it says "ask" where this is expected).
- A command exits 75 from `reserve`, or a run is OOM-killed.
- You would have to touch `copperroute/`, `copperroute-main/`, `freerouting/`, or anything under
  `~/pcb/`.

Report with: the stage, the step, what was observed (paste the failing output), and what you
recommend. Do not "fix forward" past a stop condition.

---

## Stage 0: baseline capture (no code change)

**Why.** Every later stage is judged against this. Nothing exists today that records the noise
floor, a binary hash, or peak memory per board for the full benchmark recipe.

**Branch.** None; use `main` at `184f640`.

**Steps.**

1. Build release in the main checkout's `core/` (or confirm `core/target/release/ultraroute` is from
   `184f640`: `git -C ~/Software/ultraroute log -1 --format=%h` and the binary's mtime
   against the commit date; if unsure, rebuild under reserve). Pin it as
   `~/Software/ultraroute-lab/bin/ultraroute-184f640` and record its sha256.
2. Run the subset three times with seeds 0, 1 and 2 as `remed-0-base-s0`, `remed-0-base-s1`,
   `remed-0-base-s2` (§0.4). These are three *seeds*, not three repeats; they bound the seed
   effect. Then run seed 0 once more as `remed-0-base-s0b` to measure the run-to-run noise of one
   seed.
3. Compute and record in a lab note (`tools/lab.py note` on an experiment named `remed-0-baseline`,
   created with `tools/lab.py ingest remed-0-baseline --title "Remediation baseline" --dir <empty dir>`
   or simply a markdown file in `~/Software/ultraroute-lab/pcbench/runs/remed-0-notes.md`):
   - clean count per run; boards whose verdict differs between `s0` and `s0b` (the noise floor);
   - total vias and wirelength on the boards clean in all runs;
   - the list of boards at the time limit (`seconds >= 110`) and the three slowest;
   - the two fixture boards' results (0/0 expected).
4. Run `cargo test --workspace --release` and `cargo clippy --workspace --all-targets` once and keep
   the output (`~/Software/ultraroute-lab/pcbench/runs/remed-0-notes.md`).

**Acceptance.** The notes file exists with the numbers above. Nothing else.

**Effort.** Half a day of wall time, mostly waiting on the benchmark.

---

## Stage 1: a test gate in CI and honest tests

**Review findings.** `#26`, `#34`, `#33`, `#35`.

**Branch.** `remed/1-ci-gate`.

**Steps.**

1. Add `.github/workflows/test.yml`: on push and pull request, checkout, `dtolnay/rust-toolchain@stable`,
   `cargo test --workspace --no-fail-fast --release` and `cargo clippy --workspace --all-targets --
   -D warnings` run in `core/`, with cargo caching. Keep `pages.yml` as it is. Clippy has 25 existing
   warnings; either fix them in this stage (they are cosmetic: `is_multiple_of`, complex types,
   `sort_by_key`, `from_ref`) or run clippy without `-D warnings` and add a note. Fixing them is
   preferred; do not add `#[allow]`.
2. Make the silently passing tests fail loudly:
   - `core/crates/grid/src/pairs.rs` around lines 2547 and 2568: `let Ok((mut board, _)) =
     Board::load(&path) else { return };` reads a board from `jobs/earring-kicad-locked/input/`.
     Copy that board (`.kicad_pcb` and `.kicad_pro`) to `core/fixtures/earring-locked.kicad_pcb` /
     `.kicad_pro`, point the tests there, and replace the `else { return }` with `.expect(...)`.
     Check `core/fixtures/SOURCE.txt` and add a line for the new fixture.
   - `core/crates/grid/tests/blame.rs` around line 112: the test returns early with `eprintln!`
     when nothing is blamable; make it construct a case that is blamable, or `panic!` with the reason.
   - `core/crates/grid/tests/route.rs:105`: replace `corners.len() >= 2 || corners.len() == 1` with
     `!corners.is_empty()`.
3. Turn the pinned tuning numbers into property checks where the property is clear:
   - `core/crates/ultraroute/src/lib.rs` test `via_prices_follow_the_planes_and_scale_with_the_cell`:
     keep it (it documents the current prices) but add a comment that Stage 7 retires it.
   - `prefanout.rs` tests at ~1659, ~1692, ~1823: keep the `<= failed` style assertions, loosen
     exact counts to "at least N escaped and every laid piece checks clean". Do not loosen anything
     that asserts legality (`check_item(..).is_empty()`).
4. Run the suite (§0.3) and clippy.

**Acceptance.** CI config present and green locally by the same commands; no test returns early on
a missing file; the suite still passes; clippy clean or documented.

**Do not.** Do not touch router behaviour in this stage.

**Effort.** 2–4 hours.

---

## Stage 2: `pcbench.py compare`, provenance and referee honesty

**Review findings.** `#27`, `#28`, `#29`, `#30`, `#31`.

**Branch.** `remed/2-bench-compare`.

**Steps.** All in `tools/pcbench.py`.

1. **Provenance in `run.json`** (function `run`, around line 234): add `binary` (absolute path),
   `binary_sha256`, `kicad_cli_version` (output of `kicad-cli --version`), `python_version`,
   `hostname`, and `workers` (`RESERVED_CORES`). Write `run.json` once at the start; if it already
   exists with a different `commit` or `binary_sha256`, refuse to run unless `--resume-anyway` is
   given (print what differs).
2. **Referee failures distinct from router failures** (functions `drc`, `route_one`, `score`,
   `report`): when `refill_zones.py` or `kicad-cli` fails, record `res["referee_error"]` with the
   last 300 characters of stderr and leave `success` absent; in `report`, count these as
   `"referee failed"` not `"router failed"`. Use `check=False` but inspect `returncode` and stderr.
3. **`compare A B [C …]` subcommand**: per board, `success`, `unconnected`, `new_errors` keys,
   `vias`, `wirelength`, `seconds`, `code`. Print: clean A → B; boards lost (clean in A, not in B)
   and won, each with its open count and new error types; boards whose `new_errors` keys differ;
   via and wirelength totals over boards clean in both; count at the time limit; referee errors;
   and a one-line verdict. Exit code: 0 when B loses no more boards than `--noise N` (default 0)
   and no board gains an error type, else 1. Also accept `--json OUT` to write the table.
4. **Noise floor recorded**: `compare` accepts `--noise auto`, which reads
   `~/Software/ultraroute-lab/pcbench/runs/remed-0-base-s0` vs `remed-0-base-s0b` (or any two runs
   named with `--noise-from A B`) and uses the number of boards whose verdict differs.
5. **Peak memory**: run the router under `/usr/bin/time -f "%e %M" -o <d>/time.txt` as `lab.py`
   does (`tools/lab.py:201`), and store `peak_mb` in `result.json`; `compare` reports the maximum
   and the boards over 2 GB.
6. **Document** the new commands at the top of `pcbench.py` and in `docs/pcbench.md`, including the
   measured noise floor from Stage 0.
7. Run `compare` on `remed-0-base-s0` vs `remed-0-base-s0b` and on `s0` vs `s1`, paste the output
   into the Stage 0 notes file.

**Acceptance.** `python3 tools/pcbench.py compare remed-0-base-s0 remed-0-base-s0b` prints the
report and exits 0 with `--noise auto`; a run with a changed binary refuses to resume; a deliberately
broken `kicad-cli` path (set `PATH` to exclude it for one board) is reported as a referee failure.

**Effort.** Half a day.

---

## Stage 3: a real deadline, and a file always written

**Review findings.** `#37`, `#2`, `#5`, `#53`, `#21`; the probes in `REVIEW.md` §A.4.

**Branch.** `remed/3-deadline`. **Measured** (§0.4).

**Design.** One `Deadline` value created once in `run_with_solver`
(`core/crates/ultraroute/src/lib.rs`, the `started = Instant::now()` near line 1269) and handed to
every stage; every stage derives sub-deadlines from it, never from its own clock. Put the type in
`core/crates/grid/src/clock.rs` (it already hosts the wasm `Instant` shim):

```rust
#[derive(Clone)]
pub struct Deadline {
    /// When this stage must stop; `None` for no limit.
    pub at: Option<Instant>,
    /// When the whole run must stop: never later than `at`.
    pub hard: Option<Instant>,
    pub cancel: Arc<AtomicBool>,
}
impl Deadline {
    pub fn unlimited(cancel: Arc<AtomicBool>) -> Self;
    pub fn until(end: Instant, cancel: Arc<AtomicBool>) -> Self;            // at == hard == end
    pub fn expired(&self) -> bool;                                           // at or hard passed, or cancel set
    pub fn remaining(&self) -> Option<Duration>;                             // to `at`
    pub fn share(&self, fraction: f64) -> Deadline;                          // sub-stage: at = now + fraction * remaining, hard unchanged
    pub fn seconds(&self, s: f64) -> Deadline;                               // at = min(now + s, at), hard unchanged
}
```

Replace every `max_seconds: Option<f64>` + `cancel` pair in the stage option structs
(`grid::Options`, `NegotiationOptions`, `RipOptions`, `ChaseOptions`, `NudgeOptions`,
`ShoveOptions`, `ViaOptions`, `PairOptions`, `PrefanoutOptions` (which has none today),
`topo::Options`, `PushOptions`) with `deadline: Deadline`. Keep `max_seconds` as a convenience
constructor on `Deadline` for the library tests so they change minimally.

**Steps.**

1. Add `Deadline`; make `run_with_solver` build it from `--timeout` and `cancel`; convert the
   per-stage arithmetic at `lib.rs:1440, 1515-1541, 1550, 1576, 1605, 1638, 1672, 1699, 1740,
   2001-2007, 2089` to `deadline.share(f)`. Keep the same fractions for now so behaviour changes as
   little as possible; the point of this stage is that `hard` is everywhere.
2. **Check the deadline inside the expensive loops**, not only between items:
   - `search::route_astar` and `cost_field` (`core/crates/grid/src/search.rs`): take `&Deadline`
     and an `expansion_cap: usize`; check `expired()` every 4,096 pops and stop at the cap; return
     `None`. Cap default: `max(50_000, 64 × (manhattan cells between the target box and the
     nearest source) × layers)`; make it a field of `RouteGrid` or a parameter of `FieldSolver::route`.
   - `negotiate.rs` `run_pass`: check between connections in the sequential path (exists at line
     ~694) **and** in `route_round_in_parallel` (worker loop at ~1427: `if deadline.expired() { break }`)
     and in `route_round_in_batches` (per batch). A round cut short keeps the previous paths for the
     connections not rerouted (they are still in `connections[k].path`).
   - `negotiate.rs` set-up: compute `ends` (line ~634) and `reachable_airlines` (line ~513) under
     the deadline: stop and return what is done when it expires; `reachable_airlines` must also get
     the expansion cap.
   - `pairs.rs`: `couple_end_to_end` checks `late()` between ring pairs (~946, ~980): also pass the
     deadline into `route_astar` so a single search cannot run past it; `route_pairs_watch` checks
     the deadline before each pin pair and each candidate; `coupled_grid` is rebuilt per candidate
     (comment at ~551): build it once per pin pair instead (the end pads differ per candidate; stamp
     the four pads last with `without`/`stamp` rather than rebuilding the whole map).
   - `prefanout.rs`: give `PrefanoutOptions` a deadline; check it per pad and per part and in its
     local A* calls; `probe_trapped_nets` (~270-302) must use `options.threads` and the deadline, not
     hardcoded 4 threads and 4 s + 2 s.
   - `vias.rs`: `ViaOptions.max_seconds: Some(8.0)` becomes the deadline the caller passes
     (`lib.rs:2122-2128` passes `deadline.share(…)`, see below); default `unlimited`.
   - `shove.rs`, `ripup.rs`, `nudge.rs`, `chase.rs`, `planes.rs`, `cleanup.rs` (`pull_tight`,
     `prune_dangling`), `tune.rs`: check between tries; the reroutes they call get sub-deadlines.
   - `topo/src/lib.rs:320, 346` hardcoded 3 s / 2.5 s / 1 s become sub-deadlines.
3. **Budget the stages that have none today** (`lib.rs`): pre-fanout 15% of the total, plane
   joining 5%, via reduction 10%, cleanup and length matching whatever is left but never past
   `hard`. Keep negotiation at 50%, pairs at 25%. Write the fractions as named constants at the top
   of `lib.rs` with a comment that Stage 8 moves them into the stage table.
4. **Always write the output.** In `run_with_solver`, after the deadline's `hard` passes, every
   remaining stage is skipped except cleanup, `keep_orphans`, `edits` and `apply`, which must run
   within a small reserve (`hard` = `--timeout` minus `max(2 s, 2%)`; the write itself is not
   budgeted). In `main.rs`, if `run` panics, catch it (`std::panic::catch_unwind` around `run`),
   write the input text back unchanged to `--output` with a clear message, and exit 1: a crash must
   not leave the caller without a file.
5. **Report** overruns: at the end, `eprintln!` which stage was running when `hard` passed and by
   how much the run overran (Stage 8 moves this into the stats file).
6. Tests:
   - `core/crates/grid/tests/route.rs`: extend `a_zero_time_budget_is_honoured` so that with a
     1 s deadline on the bluebuzz fixture, `route_negotiated` returns within 3 s.
   - New `core/crates/ultraroute/tests/timeout.rs`: route `core/fixtures/bluebuzz-rm2.kicad_pcb`
     with the §0.5 recipe and `--timeout 5` through `ultraroute::run`; assert the call returns in
     under 8 s and the output parses. Same with `--timeout 1`.
   - A unit test for `Deadline::share` and `seconds` (sub-deadlines never extend the parent).
7. **Measure** (§0.4): run `remed-3-new`. Expect: no board over `timeout + 5 s` of wall time (check
   `seconds` in the results); the three slow boards (`KiCad-Library_Teensy_test_layout`,
   `RaspberryPi-PoE_PoELLi_PI`, `oskirby_logicbone`) finish and write a file; clean count within
   the noise floor of `remed-0-base-s0` (it may rise: boards that used to burn their time in set-up
   now negotiate). Also run the three boards by hand with `--timeout 60 --verbose` as in `REVIEW.md`
   §A.4 and record the phase timeline.

**Acceptance.** All of step 7; tests pass; every `max_seconds`/hardcoded-seconds site in `core/crates`
is gone (`grep -rn 'max_seconds\|Some([0-9.]*)' core/crates/*/src | grep -v test` finds none that
are budgets).

**Stop and ask** if making negotiation's set-up interruptible requires changing what
`reachable_airlines` returns for partially computed nets; the simple answer (treat the uncomputed
nets as not re-spanned) is acceptable, say so in the commit message.

**Effort.** 2–3 days.

---

## Stage 4: determinism

**Review findings.** `#4`, `#2` (via pass), `#21`, `#22`, `#48`.

**Branch.** `remed/4-determinism`. **Measured** (§0.4), plus byte-identity checks.

**Design.** Results must depend only on the input, the flags and the seed, unless the hard
deadline fires (then the stats say so). Two changes make that true: budgets in **work**, and one
negotiation mode.

**Steps.**

1. **Work budgets.** Each stage's stopping rule becomes a count the stage can observe without a
   clock: negotiation `max_rounds` (exists) and a per-round `max_connections_rerouted`; rip-up,
   nudge, shove, via reduction: `max_tries` per pass and per connection (make the numbers generous
   enough that the fixture boards reach the same result as today; choose them by running with
   `--verbose` and reading how many tries each stage used on the two fixtures and on the
   200-board subset at 120 s, then set the defaults to the 90th percentile); A* expansion cap from
   Stage 3. The `Deadline` from Stage 3 stays as the hard stop only.
2. **One negotiation mode.** In `negotiate.rs` `run_pass` (~679), route every round the Jacobi way
   (`route_round_in_parallel` with `threads` workers, 1 included) and delete the Gauss-Seidel branch
   (~693-721) or keep it behind an explicit `NegotiationOptions.gauss_seidel: bool` that nothing
   sets by default. The CLI default `--threads` stays `cores / 2` for speed; results no longer depend
   on it. Extend the test `negotiation_is_the_same_on_one_thread_per_round_however_many_run` to
   compare threads 1 vs 4 and to compare the laid copper (sorted `(net, layers, shape)`), not only
   the four counts.
3. **Deterministic UUIDs under a flag.** `kicad_pcb::write::uuid()` takes a `&mut UuidSource`;
   `apply` gets it from `Edit` (default: random as today). `ultraroute` adds `--uuid-seed N`
   (xorshift from N) for tests and benchmarks; `pcbench.py run` passes `--uuid-seed <seed>`.
4. **Hash order out of the output.** `lib.rs` ~702 (`Live::frame` `del` list): sort it.
   `model.rs` ~658 (`plane_layers`): break the tie by net number.
5. **Determinism test** (`core/crates/ultraroute/tests/determinism.rs`): route
   `core/fixtures/earring.kicad_pcb` twice with the §0.5 recipe, `--seed 0 --uuid-seed 1
   --threads 1`, no timeout; assert the two output texts are byte-identical. Then `--threads 4`
   against `--threads 1`: byte-identical. Then with `--timeout 240`: byte-identical to the untimed
   run (the fixture finishes well inside it). Mark the test `#[ignore]`-free but keep it under a
   minute; if the earring is too slow, use a stripped window of it as `grid/tests/route.rs` does.
6. **Measure.** `remed-4-new` seed 0 twice (`remed-4-new-a`, `remed-4-new-b`): `compare` must show
   0 boards differing on any field except `seconds`, apart from boards whose stats say the hard
   deadline fired. Record the new noise floor (expected 0 on boards under the limit).

**Acceptance.** Steps 5 and 6. From here on §0.4's noise floor is 0 for boards under the limit, and
any lost board is a real regression.

**Effort.** 2–3 days.

---

## Stage 5: bounded memory

**Review findings.** `#38`, `#14` (in part), `#15`.

**Branch.** `remed/5-memory`. **Measured**.

**Steps.**

1. **Estimate before allocating.** In `grid::ObstacleMap::without` (`grid.rs` ~280-324) the node
   count is `layers × nx × ny`. Add `grid::memory_estimate(board, cell, threads, classes) -> u64`
   that sums: per class `owner` (4 B/node) + `via_owner` (4 B/cell); per thread one view (9 B/node)
   + A* buffers (20 B/node); negotiation history (4 B/node) + counts (2 B/node per viewer class);
   plus the board itself. Expose it as `ultraroute route … --estimate` (prints and exits) and print
   it at the start of every run.
2. **A cap with a clear refusal.** `--max-memory MB` (default: 60% of `MemAvailable` from
   `/proc/meminfo`, or 4 GB where unknown, or `MemoryMax` from the cgroup if set): if the estimate
   exceeds it, first reduce `threads` (each thread costs 29 B/node), then coarsen `cell` by steps of
   10 µm up to 100 µm (print what was chosen), and if still over, exit 1 with the estimate and the
   cap. Never silently run past the cap.
3. **A* buffers per thread.** `search.rs:150-158`: `is_target`, `is_source` and `pocket_gen` are
   `u32` epoch stamps where one bit each would do; replace the three with one `u8` per node (bits
   for target, source, pocket) plus the epoch `gen` (`u32`) and `cost` (`u32`): 20 → 9 B/node.
   Keep the epoch trick for `gen`; clear the `u8` array lazily by keeping a list of touched nodes.
4. **Zones** (`negotiate.rs:234-237`): replace `per_net: HashMap<u32, HashMap<u32, u16>>` with
   `per_net: HashMap<u32, Vec<u32>>` of sorted node lists plus the existing `count: Vec<u16>`;
   `others()` becomes a binary search; `add`/`remove` merge sorted lists (zones are produced sorted
   by `zone_of_fast` after a `sort_unstable`). Measure memory on bluebuzz before and after with
   `/usr/bin/time -f %M`.
5. **Record peak RSS** in the stats (Stage 2 added it to the benchmark; the router itself can read
   `/proc/self/status` `VmHWM` at exit on Linux and print it).
6. **Measure.** `remed-5-new`: byte-identical results to `remed-4-new-a` on every board (the stage
   must not change routing), lower `peak_mb`; the 10 µm easy2350 case
   (`jobs/easy2350`, `--cell 0.010`) either runs under the cap or refuses with a message.

**Acceptance.** Identical outputs, lower peak memory, a refusal path with a test
(`--max-memory 64` on bluebuzz must exit 1 with a message, not allocate).

**Effort.** 2–3 days.

---

## Stage 6: the search and set-up costs

**Review findings.** `REVIEW.md` §3.2 (reachability, partial rerouting, terminals), `#17`, `#64`.

**Branch.** `remed/6-search-costs`. **Measured**.

**Steps.**

1. **Reachability labels.** For each `RouteGrid` view, compute connected components of the free
   nodes (8-neighbour on a layer plus via links at via-free cells) once per `view_into`, as a
   `Vec<u32>` label per node, built by a BFS over all nodes (O(nodes), cheap next to a search).
   `terminals` returns cells; `has_way`, `reachable_airlines` and the pocket pre-check in
   `route_astar` become "any source label equals any target label". Invalidate when the map is
   stamped (`ObstacleMap::stamp` bumps a generation; the view recomputes lazily). For the Jacobi
   round, the labels of the class's base view serve every connection of that class.
2. **Partial rerouting.** In `run_pass`, from round two on, reroute only connections that collided
   in the previous round or whose path crosses a cell whose price changed by more than a threshold;
   keep the others' paths. Guard with `NegotiationOptions.reroute_all: bool` (default false) so the
   old behaviour is one flag away for comparison. Measure rounds and committed count on the two
   fixtures and the subset.
3. **Cache terminals.** `route::terminals` is called per connection per stage with identical inputs
   when the board around the pad has not changed. Cache by `(item id, class, grid generation)` in
   a `HashMap` on the `ObstacleMap`; invalidate on `stamp` within the pad's reach box.
4. **Cache `Limits::of`** (`board/src/check.rs:91-116`): store `Limits` on `Board`, rebuilt in
   `insert`/`restore`/`remove` only when a pad's mask opening or clearance changes (they do not
   change during routing; a `dirty` flag set by the pad-inserting code in `Board::new` is enough).
5. **Measure.** `remed-6-new` against `remed-5-new`: median and p90 `seconds` down; boards at the
   limit down; clean count not down; vias within noise (partial rerouting can change paths, so this
   stage is allowed to change outputs; explain any lost board).

**Acceptance.** Fewer boards at the time limit than `remed-0-base-s0` by at least a third, no lost
board unexplained, `cargo test` green.

**Effort.** 3–5 days.

---

## Stage 7: the good router is the default; the experiments leave the main path

**Review findings.** `#1` (in part), `#3`, `#18`, `#6`, `#49`, `#50`, `#51`, `#52`, the recipe table in
`REVIEW.md` §1.5.

**Branch.** `remed/7-defaults`. **Measured**, and byte-identity for the default recipe.

**Steps.**

1. **Default recipe.** `ultraroute route IN -o OUT` with no stage flags runs the §0.5 recipe. Add
   `--recipe default|bench|minimal` (minimal = today's flagless behaviour, for library users and
   tests) and keep the individual flags as overrides. The studio (`studio/web/src/App.svelte:102-105,
   149-154`), `tools/lab.py:34`, `tools/pcbench.py:39`, `tools/search.py:25`, `core/README.md:19`,
   `docs/library.md` and `examples/feature_film.rs:730-734` all stop spelling the recipe out and use
   `--recipe default` (or nothing). `--nudge` is in the studio's recipe and not in the benchmark's;
   decide by measuring: run the subset with and without `--nudge` and keep the better as default;
   record the decision in `docs/pcbench.md`.
2. **Via prices in one place and one unit.** Replace `via_costs()` (`lib.rs:987-997`) and the
   library defaults (`route.rs:117`, `ripup.rs:40`, `chase.rs:36`, `blame.rs:50`, `nudge.rs:46`,
   `shove.rs:82`) with one `via_cost_mm` on the board (default chosen by measuring 1.0, 2.5 and
   5.0 mm on the subset; the review's branch `explore/escape-vias` found 5 mm best on four boards
   with `--prefanout-mode same`), converted per stage with `search::via_cost_mm(cell, mm)`. Keep
   the "plane boards price vias lower" rule only if the subset's plane boards (`--auto-planes`
   detects them) need it; otherwise delete it.
3. **Evaluate the two unmerged branches from the review session** under the new protocol:
   `explore/escape-vias` (`--prefanout-mode same`, `--via-seconds`, via-pass passes) and
   `fix/edge-clearance-rule` (`.kicad_dru` `edge_clearance` on Edge.Cuts circles/arcs). The second
   is a correctness fix: rebase, run its tests, merge. The first: rebase, keep `--prefanout-mode
   same` if it wins on the subset; its `--via-seconds` is superseded by Stage 3's deadline and should
   be dropped during the rebase.
4. **Delete** (each with its flag, options, tests and docs; `cargo build` and `cargo test` after
   each deletion):
   - ploughs `Linear`, `Warp`, `Gentle`, `Dipole` and `morph::shift_copper`, `plough_fields`,
     `gentle`, `put_back_broken` in `shove.rs`; `--shove-steps`, `--shove-cascade` (if `cascade`
     is unused by `Region`), `--shove-late`, `--shove-seconds`;
   - `--escape`, `--escape-zone`, `--clear-lanes` and `morph::escape::{comb, clear_lane, edges,
     Edge, Pole, poles_at, shift_poles}` (keep `morph::fine_pitch`, which pre-fanout uses);
   - `--push`, `--push-steps`, `--push-decay` and `grid::Field` (keep `grid::push::push` itself:
     nudge uses it);
   - `--chase`, `--chase-costs`, `grid::chase`;
   - `--first`, `--first-negotiate`, `--first-weight`, `--restart` and the restart loop in
     `run_with_solver` (~1498-1997, the `total_rounds` loop and `best_run`);
   - `--topo`, `--topo-first`, `--topo-trapped`, `--topo-trapped-rip`, `--topo-sample`: move the
     `topo` crate out of the `ultraroute` dependency list; keep the crate in the workspace behind a
     new workspace member list `experiments/` (or leave it where it is but unlinked) so its tests
     still build in CI;
   - `--negotiate-legacy`, `--no-negotiate-vias/-rip/-tree`, `--diff-pairs-legacy`,
     `--prefanout-soft`, `--prefanout-keep-unused`, `--no-prefanout-inward`, `--no-directional`,
     `--no-reduce-vias`, `--nudge-guided` and their `legacy()` branches in `negotiate.rs` and
     `pairs.rs` (`route_pairs_legacy`, `LEGACY_RINGS`);
   - `GpuSweepSolver` (`grid/src/sweep.rs`, `sweep.wgsl`), `search::pocket_bbox`,
     `GpuSolver::pocket_trapped` (use `route_astar`'s check);
   - the `*_watch` variants whose only caller is `feature_film.rs` once step 5 is done.
   Keep: `--negotiate-rounds/-patience/-growth/-repasses`, `--prefanout-mode/-order/-ring`,
   `--shove-reach/-repair/-passes/-patience/-ties`, `--via-cost`, `--cell`, `--seed`, `--threads`,
   `--timeout`, `--lock`, `--place`, `--planes`, `--auto-planes`, `--pours-block`, `--regions`,
   `--pour-cost`, `--via-avoid*`, `--rip-region`, `--adopt`, `--strip*`, `--live*`, `--verbose`,
   `--blame`, `--no-route`, `--gpu` (until Stage 10).
5. **`feature_film.rs` and `fanout_trace.rs`**: move `feature_film.rs` to its own binary crate
   `core/crates/lab-films` that calls `ultraroute::run_with_solver` with the real recipe and records
   films through the existing `watch`/`Log` hooks instead of its own copy of the pipeline (its
   `prepare_recipe` at ~738-795 goes). Delete `fanout_trace.rs` or add the region plough to it;
   update `lab/fanout.html`/`features.html` accordingly.
6. **Copperroute-era tooling**: move `tools/route_job.py`, `kicad_bench.py`, `apply_session.py`,
   `build_experiments.py`, `morph_outline.py`, `refit_frames.py`, `render_walls.py`,
   `slack_stats.py`, `softbody.py`, `publish_phone.sh`, `morph-frames/`, `experiments.json`,
   `history.html`, `jobs/leaderboard.html` under `legacy/` with one README saying what they were
   for; fix the links in `index.html`. Ask the user before this step if the Pages site is still
   meant to show the copperroute history.
7. **Byte-identity check.** Before deleting anything, route the two fixtures with the full recipe
   spelled out on the stage's start commit (`--seed 0 --uuid-seed 1 --threads 1`, no timeout) and
   keep the outputs; after the deletions, `ultraroute route … --recipe default` must produce
   byte-identical files. Then measure `remed-7-new` on the subset: identical to `remed-6-new`
   except where step 2's via price changed things, which is measured separately (`remed-7-vias`).

**Acceptance.** Flagless run equals the benchmark recipe; the deletions list is done; outputs
byte-identical where the plan says; `cargo test` green; `docs/library.md` stage list updated.

**Effort.** 3–4 days. Deletion is mechanical; the via-price measurement is the slow part.

---

## Stage 8: the driver becomes a list of stages, with structured stats

**Review findings.** `#1`, `REVIEW.md` §1.3, §4.2 items 1–3.

**Branch.** `remed/8-stages`. Byte-identity check (this stage changes no behaviour).

**Steps.**

1. Define in `ultraroute`:
   ```rust
   pub struct Ledger { laid: Vec<ItemId>, fanout: Vec<ItemId>, pairs: Vec<ItemId>, removed: Vec<Span> }
   pub struct StageCtx<'a> { board: &'a mut Board, ledger: &'a mut Ledger, solver: &'a mut dyn FieldSolver, deadline: Deadline, log: Option<Log>, watch: &'a mut dyn FnMut(&Board), stats: &'a mut RunStats }
   pub trait Stage { fn name(&self) -> &'static str; fn budget_share(&self) -> f64; fn run(&self, ctx: &mut StageCtx) -> StageStats; }
   ```
   One `impl Stage` per block of today's `run_with_solver` (setup, prefanout, planes, pairs,
   negotiate, finish, prune-escapes, rip-up, shove, nudge, tighten, vias, cleanup, tune, write).
   The ordering and the budget fractions become one table in `lib.rs`; `run_with_solver` becomes a
   loop over it.
2. `RunStats` is one `serde::Serialize` struct: per stage name, wall seconds, work counts (A* calls
   and expansions, `check_item` calls, connectivity recomputes, map builds: add counters in the
   grid crate behind `Cell`/atomic counters), open before and after, pieces laid and ripped, vias;
   plus run-level: input hash, binary version (`env!("CARGO_PKG_VERSION")` + git hash via
   `build.rs`), flags, seed, threads, cell, node count, peak RSS, whether the hard deadline fired and
   in which stage, and the per-connection failure list (`Stats.failures` with stage and reason).
   `--stats FILE` writes it as JSON; `--verbose` prints a table from it. The `eprintln!` summaries
   are generated from the same struct (one `Display`), so `tools/lab.py:91-98`'s regexes read the
   stats file instead (`lab.py` `parse_log` gets a `--stats` path; keep the regex path for old logs).
3. The `say(...)`/`tell` activities stay for the studio; the seven progress mechanisms collapse to
   `Log` + `watch` + `RunStats`.
4. **Byte-identity**: the two fixtures and the subset (`remed-8-new`) must equal `remed-7-new`.

**Acceptance.** `run_with_solver` under 200 lines; the stage table is the only place the order and
budgets live; `--stats` output validated by a test that routes the earring and checks the JSON has
every stage with non-negative numbers; byte-identical outputs.

**Effort.** 3–5 days.

---

## Stage 9: correctness fixes in the I/O layer and the board model

**Review findings.** `#40`–`#47`, `#60`–`#63`, `#10`, `#12`, `#13`, `#24`, `#25`, `#11`; the table in
`REVIEW.md` §1.8. Each item is independent; do them as separate commits on one branch, each with a
test, in this order (highest corpus impact first). **Measured** at the end.

**Branch.** `remed/9-io-correctness`.

1. **Warnings reach the user** (`#40`). `Board::load` (`board/src/model.rs:325-330`) returns the
   warnings (`pcb.warnings`, `copper.warnings`) alongside the board, or `Board` keeps a
   `warnings: Vec<String>`; `main.rs` prints them. `model.rs:390`: when `outline_loops()` fails,
   push a warning naming the error and continue. Test: a board with an open outline loads with a
   warning that mentions "outline".
2. **Rule defaults** (`#60`). `rules.rs:89`: missing `min_clearance`, `min_track_width`,
   `min_via_diameter`, `min_via_drill`, `min_copper_edge_clearance`, `min_hole_clearance`,
   `min_hole_to_hole` take KiCad 9's defaults (0.2 mm clearance, 0.2 mm track, 0.6/0.3 mm via,
   0.5 mm edge, 0.25 mm hole clearance, 0.25 mm hole-to-hole; verify against a fresh KiCad 9 project
   file on this machine with `kicad-cli` or a new project) and the load prints a warning per
   defaulted field. A project without a `Default` class gets one from those values. Test: `Rules::parse("{}")`
   yields the defaults with warnings.
3. **Plated slots** (`#10`, `#D3`). `Item.hole: Option<(Point, Nm)>` becomes a capsule
   `Option<Shape>` (or `(Point, Point, Nm)`); `model.rs:411` builds it from the drill's two
   dimensions and the pad rotation as the unplated keepout does at `:486-501`; `check.rs`
   `hole_shape` returns it; `grid.rs` `stamp` uses `cells_near` on it. Test in `board/tests/kicad5.rs`:
   a via placed along a plated slot's length at less than hole-to-hole is a `HoleToHole` violation,
   and a track beside the slot's end at less than hole clearance is a `HoleClearance` violation;
   compare both with `kicad-cli pcb drc` on a small written board (add the expected numbers to
   `kicad_agreement.rs`).
4. **Bezier edges** (`#42`). `kicad-pcb/src/read.rs`: parse `gr_curve`/`fp_curve` (start, two
   control points, end), flatten to lines at `MAX_ERROR_M`, and push them to `pcb.outline` as
   `OutlineShape::Line`s with the same width. Test: a curved outline closes and copper keeps the edge
   clearance from the curve.
5. **`--place` and footprint zones** (`#43`). `ultraroute/src/lib.rs` `moved_parts`: for a moved
   footprint, also emit `replace` edits for every `(xy …)` of its zones and rule areas (`Copper.zones`
   with `footprint == Some(fp)`; their spans are known), translated by the move. Test: write a board
   with a footprint zone, move it with `--place`, reload, assert the zone moved.
6. **Pad clearance zero** (`#44`). `copper.rs:356-361`: `pad.find("clearance")` → if present and
   zero, fall through to the footprint's value; same for `solder_mask_margin`. Unit test.
7. **Custom pads** (`#45`). `copper.rs:607-665`: read `(options (clearance convexhull))` and, when
   set, replace the pad polygon by the convex hull of all primitives; flatten `gr_arc` primitives.
   Test on a custom pad with an arc primitive.
8. **Arc tracks chorded outside** (`#46`). `model.rs:891-907`: use the same outside-polyline
   construction as `around(..., outside = true)` so chords never lie inside the arc; test that a
   track at exactly the clearance from the arc's true circle is accepted and one 1 µm closer is not.
9. **Net ties** (`#41`). Parse `net_tie_pad_groups` in `read.rs` into `Footprint.net_tie_groups:
   Vec<Vec<String>>`; `check.rs` `item_checks` skips the clearance check between two pads of the
   same footprint in the same group, and between a track and a pad of a group when the track's net
   is the net of another pad in the group and the track ends inside that pad. Test against
   `kicad-cli` on a small board with a 0 Ω net-tie footprint (KiCad's `NetTie-2_SMD_Pad0.5mm`).
   Ask the user if the KiCad behaviour cannot be pinned down with a test board.
10. **Writer** (`#12`, `#47`, `#62`): add `locked: bool` to `NewTrack`/`NewVia` and write
    `(locked yes)`; `grid::edits` carries `Item.locked`; for a non-through via, write the `blind`
    token or, better, return an error from `edits` since nothing creates them; duplicate footprint
    uuids become a warning plus a fresh uuid in memory (the file is left as is). `main.rs:50`: an
    unwritable `--live PATH` exits 1 with a message; `--lock REF` unknown warns.
11. **Zone outlines with cutouts** (`#61`): `copper.rs:542` collects every `(polygon …)`; keepouts
    and `soft-novia` areas become one item per polygon with the cutout polygons subtracted at
    rasterisation (simplest: keep the outer polygon and add the cutouts as `Shape::Polygon` items of
    a "not a keepout" marker; ask if unsure).
12. **Unfilled kept pours and planes** (`#11`): when a zone has no `filled_polygon` and is kept
    (block) or on a `--planes` layer, use its outline as the pour shape and warn.
13. **Measure** `remed-9-new` against `remed-8-new`: expect fewer boards with new errors of types
    `hole_clearance`, `hole_to_hole`, `copper_edge_clearance`, `clearance`; any lost board explained.

**Acceptance.** Every item has a test; `kicad_agreement.rs` gains the slot and arc cases with
numbers from `kicad-cli`; the subset does not lose boards.

**Effort.** 4–6 days.

---

## Stage 10: the GPU

**Review findings.** `#8`, `#9`, `#53`–`#58`, `REVIEW.md` §3.4.

**Branch.** `remed/10-gpu`.

**Decision to get from the user first** (ask with the numbers from `REVIEW.md` §1.4 and §3.4):
park or delete. The plan's recommendation is **park**:

1. Keep `GpuSolver` behind the `gpu` feature, off by default in `ultraroute` (`default = []`), and
   print a warning when `--gpu` is used: "the GPU field solver is about 10× slower than the CPU on
   boards of this size".
2. Fix the things that make it wrong when it is used (`#53`, `#54`, `#57`): check `cancel`/deadline
   between dispatch groups (`gpu.rs:452-469`), propagate read-back failures as `None` and fall back
   to the CPU solver, upload `penalty_at` (penalty + region cost) and pass the directional step
   costs as uniforms (`relax.wgsl:83`), use `saturating_add` in the three searches and the kernel
   (`#55`), and reorder `relax` to reload the cost after clearing the bit (`#56`).
3. Delete the dormant JavaScript kernel (`studio/web/src/lib/router-subworker.js:23-199` and the
   `--gpu` branch of `router-worker.js`) or fix its waiter logic; deleting is recommended since the
   Rust kernel is the one that would be developed.
4. Update the README, design doc and roadmap to say what runs where (CPU A* natively and in the
   browser's worker pool; GPU experimental).

If the user chooses **delete**: remove `gpu.rs`, `relax.wgsl`, the feature, `--gpu`, `pollster` and
`wgpu` from `grid/Cargo.toml`, the JS kernel, and the GPU paragraphs of the docs.

**Acceptance.** `cargo test --workspace` green with and without `--features gpu`; the docs no longer
claim GPU or WebGPU routing.

**Effort.** 1–2 days (park), half a day (delete).

---

## Stage 11: documentation and the single recipe source

**Review findings.** `#23`, `#50`, the stale-docs paragraph in `REVIEW.md` §1.5.

**Branch.** `remed/11-docs`.

1. Rewrite the root `README.md`: what the project is now (the MIT core routes; the studio shows it;
   copperroute is a legacy reference under `legacy/`), how to build and run (`cargo build --release
   -p ultraroute`, `ultraroute route IN -o OUT`), how to benchmark (`docs/pcbench.md`), licence
   paragraph updated for the WASM build of the MIT core.
2. `core/README.md`: all crates, the stage table from Stage 8, the full flag list generated from
   `USAGE` (or a `--help` that prints it; consider replacing the hand parser with `clap` or `lexopt`
   at this point, ask first).
3. `docs/library.md`: regenerate the stage order and option tables from the code; remove flags
   deleted in Stage 7.
4. `ROADMAP.md`: resolve the contradictions at lines 93, 130, 181; retitle Phase 2c; add a short
   "state at <date>" paragraph with the benchmark numbers from Stage 0 and the latest stage.
5. The design doc: add a dated addendum at the top saying which parts were built and which were
   not (the GPU and WebGPU milestones), pointing at `REVIEW.md`.
6. `kicad-pcb/src/lib.rs` header: drop the references to the waffle-iron spec files.
7. `docs/morph-port-plan.md`: add a header that it describes the copperroute fork's crate, not the
   core's.
8. `tools/lab.py`: remove the "studio's default recipe" comment; default to `--recipe default`.

**Acceptance.** `grep -rn "copperroute" README.md core/README.md docs/library.md` finds only the
legacy paragraph; every flag in `USAGE` appears in `core/README.md`; a new reader can build, route
the earring fixture and run the subset from the docs alone.

**Effort.** Half a day to a day.

---

## Stage 12: a global routing stage (experimental, measured)

**Review findings.** `REVIEW.md` §3.1, §3.3, executive summary item 5. Start only after Stages 3–8
are merged; nothing here is measurable before.

**Branch.** `remed/12-global`. **Measured** on the subset and the fixtures.

**Design (first version, deliberately small).**

1. GCell grid: 1 mm cells (`--global-cell`), one per signal layer. Capacity per cell edge per layer
   = free fine-grid lanes across that edge, computed from the class obstacle maps (count free
   cells along the edge divided by the class's lane pitch). Blocked cells (planes, keepouts) have
   capacity 0.
2. Nets: every open connection (airline) from `connectivity_without_pours`, as a two-pin
   source/target on the GCell grid; multi-pin nets as the existing airline tree.
3. Search: `route_astar` on a `RouteGrid` built over the GCell grid (reuse the type: `h` = 1 mm,
   `blocked` from capacity 0, `penalty` = congestion price); PathFinder over all connections with
   `route_negotiated`'s machinery but with capacity-based present cost (price rises with
   `demand / capacity`), 20 rounds, seconds of CPU. Via cost in mm, one number.
4. Output: for each net, its GCell path becomes a `board::Region` with `prefer` weight 1 inside
   the corridor (one GCell either side) and an `avoid` weight outside (start with 3), on the
   layers the global route assigned; feed them through the existing `board.regions` /
   `grid::regions` rasteriser (so the detailed router needs no change). Also emit a per-layer
   congestion map into the stats (`#4.2` item 4 of the review).
5. Flag: `--global` (off by default until it wins). Measure `remed-12-new` vs `remed-11-new`:
   total vias, wirelength, boards at the limit, clean count. Expected: vias down, time on large
   boards down, completion unchanged or up. If vias are not down by 10% or completion drops, stop,
   record the numbers in a lab note, and report; do not tune blindly.
6. If it wins: make the detailed searches window to the corridor (bounded memory and time follow),
   then make `--global` the default and retire the per-stage via prices.

**Acceptance.** Step 5's measurement recorded either way.

**Effort.** 2–4 weeks.

---

## Summary table

| stage | branch | kind | proves itself by | effort |
|---|---|---|---|---|
| 0 baseline | — | measurement | notes file with noise floor, timing, memory | ½ day |
| 1 CI gate | `remed/1-ci-gate` | hygiene | CI config; suite green; no silent passes | ½ day |
| 2 compare | `remed/2-bench-compare` | tooling | `compare` exits 0/1 correctly; provenance recorded | ½ day |
| 3 deadline | `remed/3-deadline` | behaviour | no board over timeout + 5 s; file always written; tests | 2–3 days |
| 4 determinism | `remed/4-determinism` | behaviour | byte-identical reruns; 1 vs N threads identical | 2–3 days |
| 5 memory | `remed/5-memory` | behaviour-preserving | identical outputs; lower peak; refusal path | 2–3 days |
| 6 search costs | `remed/6-search-costs` | performance | a third fewer boards at the limit; no lost board | 3–5 days |
| 7 defaults | `remed/7-defaults` | cleanup | flagless = benchmark recipe; deletions; byte-identical | 3–4 days |
| 8 stages | `remed/8-stages` | refactor | driver < 200 lines; `--stats`; byte-identical | 3–5 days |
| 9 I/O correctness | `remed/9-io-correctness` | correctness | tests per item; KiCad-agreement numbers; fewer error boards | 4–6 days |
| 10 GPU | `remed/10-gpu` | decision + fixes | user's choice applied; docs true | ½–2 days |
| 11 docs | `remed/11-docs` | docs | a new reader can build, route, benchmark | 1 day |
| 12 global stage | `remed/12-global` | algorithm | measured via/time/completion change | 2–4 weeks |

Total before the global stage: about four working weeks of agent time, in merge-sized pieces.
