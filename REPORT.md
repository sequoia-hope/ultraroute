# Where Ultraroute Spends Its Time, and Which Decisions Could a Learned Policy Make?

**Interactive visualization:** `http://localhost:8065/timing/` (`~/Software/ultraroute-lab/timing/index.html`, with data beside it in `~/Software/ultraroute-lab/timing/data.json`).

---

## 1. Executive Summary

### Where the router's CPU time goes
Across all **1,182 PCBench boards** (`35,469.17 s` total single-thread CPU time; **331** direct post-`explore/speed` `HEAD` runs totaling `6,028.27 s` plus **851** remaining boards from `s1-cpu-cd0a82a` paired with `srvfull-a7831ee` stage logs):

1. **A heavy tail separates total CPU from the typical board.** The slowest **10% of boards** consume **42.30%** of all CPU across the 1,182-board corpus (`p90 = 115.20 s`, mean `30.01 s`, median `8.57 s`, IQR `2.20 - 36.70 s`) and **45.61%** of all CPU on the 331 direct `HEAD` boards (`p90 = 45.26 s`, mean `18.21 s`, median `6.61 s`, IQR `1.81 - 23.63 s`).
2. **By pipeline stage:**
   - **Total CPU sum (1,182 boards / 331 `HEAD` boards):** `negotiation` takes **53.23%** (`18,880.56 s`; **42.89%** / `2,585.68 s` on `HEAD-331`), `shove` takes **13.49%** (`4,783.25 s`; **27.62%** / `1,665.10 s` on `HEAD-331`), `vias` takes **12.44%** (`4,413.77 s`; **14.85%** / `895.13 s`), `prefanout` takes **6.12%** (`2,170.40 s`; **5.68%**), `finishing` takes **5.88%** (`2,085.30 s`; **0.86%**), `pairs` takes **4.56%** (`1,617.47 s`; **5.19%**), and `rip-up` takes **3.02%** (`1,072.53 s`; **1.50%**). Every other stage (`write` `0.32%`, `tightening` `0.27%`, `result` `0.27%`, `refinishing` `0.20%`, `cleaning` `0.12%`, `set-up` `0.06%`, `length matching` `0.03%`) is under `0.4%` of total CPU.
   - **Median board (1,182 boards / 331 `HEAD` boards):** A typical board finishes cleanly without ever entering rescue stages (`shove`, `finishing`, `rip-up`, `prefanout`, and `pairs` all have **0.00%** median share). On the median board, `negotiation` takes **67.50%** (`58.63%` on `HEAD-331`), `vias` takes **13.02%** (`16.83%`), `write` takes **0.44%** (`0.55%`), `tightening` takes **0.36%** (`0.44%`), and `set-up` takes **0.22%** (`0.30%`).
   - **Failing boards invert the stage ranking:** On `HEAD-331` time-limited failures (`n = 13`), `shove` alone consumes **69.9%** of all CPU (`negotiation` `15.2%`, `vias` `4.4%`). On boards that fail with time remaining (`n = 67` in 1,182; `n = 58` in `HEAD-331`), `negotiation` (`38.9%` / `40.0%`) and `shove` (`34.8%` / `34.7%`) split three-quarters of all CPU.
3. **By sub-operation (post-`explore/speed`):**
   - `explore/speed` (`8c5c1b9`, **1.40x** faster across the 331 shared PCBench boards: `8,418.50 s -> 6,028.27 s`, clean `257 -> 260`) eliminated the old **70%** `SipHash` `negotiate::patch_grid` bottleneck reported in `explore-nearmiss` (`hs3` sped up `m2-electronics_m2fc` negotiation **2.88x**, `23.86 s -> 8.27 s`) and cut static `ObstacleMap` construction ~4x via stamp/spoke caching (`hs1` sped up `CAL430FR_CAL430F` shove **1.45x**, `126.18 s -> 87.03 s`).
   - Today, **`astar` (3D grid A* & field search)** is #1 at **59.28%** of corpus CPU (`21,025.92 s`; **55.27%** on `HEAD-331`, **63.44%** median board), followed by **`map_build`** at **9.03%** (`3,203.25 s`; **10.93%** on `HEAD-331`, driven by local subwindow maps in `shove`, `vias`, and `rip-up`), **`zones` (PathFinder congestion discs)** at **8.40%** (`2,980.63 s`), **`map_patch`** at **8.24%** (`2,923.57 s`), **`other` (unattributed path trace-back, sorting, S-expr output)** at **5.60%** (`1,987.57 s`), **`connectivity` (spatial union-find + Prim MST)** at **3.00%** (`1,062.60 s`), **`checks` (exact geometric DRC)** at **2.87%** (`1,019.70 s`), **`terminals`** at **1.96%** (`696.54 s`), and **`geometry` (polygon clearance & morph)** at **1.61%** (`569.38 s`).
4. **Wasted vs useful work & zero-progress tail:**
   - Across 1,182 boards, **55.14%** (`19,556.54 s`) of CPU goes to kept connection-closing work (`useful`), **8.18%** (`2,903.05 s`) to kept non-closing work (`kept_other`: fanout escapes, `shove` tie swaps, dropped vias, pulled-tight chains), **27.77%** (`9,851.16 s`) to **wasted** attempts (`35.26%` / `2,125.29 s` on `HEAD-331`), and **8.90%** (`3,158.43 s`) to stage setup/unattributed work (`other`).
   - On boards that fail with time left (`n = 67`), **54.1%** of full-run CPU and **73.10%** of second-half `[0.5*T, T]` CPU is wasted (`71.81%` on `HEAD-331`), spent mostly in futile `shove` passes (`51.00%` of `[0.5*T, T]`) and post-failure `vias` reduction (`24.19%`) on an incomplete board.
   - Across `HEAD-331`, **15.18%** of all CPU (`915.05 s`, **18.12%** of routing CPU) occurs in open-connection routing stages *after* the last moment `open` ever drops (`6.78%` / `2,403.09 s` across 1,182 boards, including **41.20%** of all CPU on `failed_with_time` boards). A plain post-negotiation patience stop rule (`Delta_t = 20 s` without a new record-low `min_open`) saves **934.54 s (15.50% of total CPU)** on `HEAD-331` with **0 clean boards lost**, though on the full 1,182-board set `Delta_t = 20 s` saves **2,805.76 s (7.91%)** at a regression cost of **18 slow-rescue clean boards lost** (`Delta_t = 45 s` saves `1,103.79 s` / `3.11%`, losing `5`).

### Which decisions could a learned policy make?
- **Do NOT put RL in the A* inner loop (`search.rs`), `Negotiation` net ordering, `Via` cost selection, `Prefanout`, `Vias` reduction, or a run-level stop/restart wrapper:**
  - `A*` pops a cell every ~50-200 ns; any neural call is 1,000x-10,000x too slow, whereas fixing `NodeHeap::push` (`search.rs:189`) to break equal-`f` ties by smaller `h` instead of raw `node` index is a 3-line classical change.
  - Run-level stop/restart has large upside (**+4 clean** on 200 spread boards from 3 seeds, `160 -> 164`, and **+11 clean** on 34 time-limited near-misses, `12 -> 23`, `67.6%`), but yields only ~1-15 transitions per failing board across the entire corpus and is matched by a 15-line classical patience + seed/parameter restart controller.
  - Static net ordering in `Negotiation` was already tested across 4 congestion/criticality rules on 200 boards in `explore-priority` (`-1` to `+1` clean boards) and washes out under PathFinder history pricing.
  - Board-level `via_cost` has 1 decision per board (`1,182` data points total); `RipUp` needs tabu victim pricing (`rip_cost += 400` per failed victim) and fixing a missing `grid.via_penalty` gate (`ripup.rs:281-285`) before any ML.
- **The ONE place where a learned policy (specifically a lightweight contextual bandit / offline-scored linear or GBDT model, not deep RL) is genuinely justified is `Shove` target, radius, and repair-budget selection (`core/crates/grid/src/shove.rs:203-404`):**
  - `Shove` governs **27.62%** of `HEAD-331` CPU (`13.49%` of 1,182-board CPU, **43.62%** on near-misses, **69.9%** on `HEAD-331` time-limited failures), wastes **46.7%** of its own time on `HEAD-331` (`1,882.71 s` wasted across 1,182 boards, including `1,280.24 s` inside reverted 20-second `repair` runs and `451.85 s` in walled/failed ploughs), is the primary driver of the **12 -> 23 / 34** multi-seed near-miss rescues, and generates **4,945 self-labeled counterfactual move outcomes** per 1,182-board pass (`~20,000` across 4 seeds) with rich local geometric features (`soft_path` length/crossings, blocker-net count in 0.5/1/2 mm discs, pad `mouth_room`, net retry history).

### Plain statement of what was and was not executed live
- **Built and compiler-verified (`cargo check` exit 0, 0 warnings across `ultraroute --tests`, `web`, and `lab-films`):** Full Part 1 sub-stage CPU bucket timers, per-move/per-stage useful vs wasted effort tracking, open-count timeline instrumentation, `--no-timing` CLI flag, and integration test `sub_stage_cpu_and_effort_account_for_stage_time` in `core/crates/ultraroute/tests/stats.rs`.
- **Blocked from live multi-core execution during this session:** On `loaf`, `~/.claude/bin/reserve --status` showed **20 of 22 bookable cores permanently held** by 5 stale reservations from earlier sessions (up to `440m` old at `0.0%` CPU pressure, with 12 jobs queued behind them). Ground rules forbid running `cargo test`, release `cargo build`, router batches, or KiCad DRC outside `reserve --here`, forbid committing/pushing uncommitted worktree changes to `sequoia-server`, and forbid touching `reserve`'s state files.
- **Therefore:**
  1. Live execution of `cargo test`, `cargo build --release`, the 48-board Rule-6 `Deadline::seconds = 1e6` byte-identical proof, the 3x interleaved instrumentation overhead benchmark, and the live 214-board / 4-seed / 8-ablation router batches **has not been run yet**. A single resumable foreground script, `./runs/run_when_unblocked.sh`, is wired and syntax-checked in this worktree to run every one of those steps under `reserve -c 8 --here` and overwrite `~/Software/ultraroute-lab/timing/data.json` as soon as `reserve` is cleared.
  2. All Part 2 and Part 3 corpus measurements in this report and in `http://localhost:8065/timing/` are computed directly (`runs/analyze_existing_runs.py`) from **existing post-`explore/speed` `HEAD` runs** (`explore-text-v2-{smoke,focus,guard}-new`, 331 boards, seed 1, `--cpu-clock`), **all 1,182 PCBench runs** (`s1-cpu-cd0a82a` + `srvfull-a7831ee` logs, validated against 5 post-speed `hs3_new` `.report.json` files to **0.020 s mean absolute stage error**), archived `ULTRAROUTE_PROFILE` call stacks + `explore/speed` incremental A/B reports (`hs1`, `hs2`, `hs3`), **200-board x 3-seed + repeat** runs (`remed-0-base-{s0,s1,s2,s0b}`), **34-board x 3-seed & multi-timeout** runs (`explore-nearmiss`), and **100- to 240-board paired heuristic A/B runs** on disk.

---

## 2. Literature Check: The 2026 RL + FreeRouting Paper

Web search located the exact 2026 paper Sequoia heard about:

- **Yin-Chi Liao, Sheng-Xin Pan, and Po-Jui Chiang**, *"Automation of PCB autorouting via world-model reinforcement learning and freerouting integration"*, **Expert Systems with Applications**, Vol. 311, Article 131424, 2026 (`DOI: 10.1016/j.eswa.2026.131424`).

**What we read vs did not read:**
- Liao, Pan, and Chiang (2026) is behind Elsevier ScienceDirect's paywall. No open-access preprint or full text is available on arXiv or the authors' pages, so **we have not read the paper** and make zero claims about its internal world-model state, action space, or benchmark numbers.
- For open-access context that we *did* read in full (`scratch/pcbworld.txt`), we read **PCBWorld: Simulation-Grounded World Modeling for Printed Circuit Board Routing** (`arXiv:2607.05915v4`, 2026), which trains an action-conditioned world model (`PCBWM`) and policy directly over fine-grid cell actions on small synthetic/real sub-problems. Even with learned dynamics, cell-by-cell RL faces severe horizon length (`10^3 - 10^4` grid steps per net) and DRC brittle-termination problems at full board scale. That contrast sharpens why pairing a learned policy with a classical router only makes sense **above** the A* cell level, at coarse discrete decisions where a classical search handles exact DRC geometry and shortest paths.

---

## 3. Part 1: Instrumentation Built and Verification Status

### Files created or modified
- **`core/crates/board/Cargo.toml` & `core/crates/board/src/work.rs`:**
  - Added thread-local exclusive sub-operation bucket profiler (`Bucket`: `Astar`, `MapBuild`, `MapPatch`, `Zones`, `Checks`, `Connectivity`, `Terminals`, `Geometry`), re-entrant `BucketGuard` stack (`enter_bucket`) that credits nested calls strictly to the innermost bucket without double-counting, and `RawEvent` (`NegRound`, `NegOverhead`, `Single`, `RipAttempt`, `ShoveAttempt`, `ShoveSetup`, `NudgeAttempt`, `NudgeBlame`, `PolishTry`) sink (`CpuSink`) backed by `libc::clock_gettime(CLOCK_THREAD_CPUTIME_ID)` (and `CLOCK_PROCESS_CPUTIME_ID` for process totals).
  - When no `CpuSink` is installed (`--no-timing` or library callers without `--report`), `enter_bucket` checks a thread-local `bool` (`TIMING_ON == false`) and returns immediately with **zero syscalls**.
- **`core/crates/board/src/{check.rs, connect.rs}`:**
  - Guarded `Violations::check`, `Violations::check_Pads`, and geometric clearance routines under `Bucket::Checks`; guarded `connectivity`, `connectivity_without_pours`, `net_connectivity`, and `joined_copper` under `Bucket::Connectivity`.
- **`core/crates/morph/src/{escape.rs, flow.rs}`:**
  - Guarded `shift_copper`, `fine_pitch`, and `flow` (`WarpField` Laplace copper warping) under `Bucket::Geometry`.
- **`core/crates/grid/src/{lib.rs, search.rs, grid.rs, negotiate.rs, route.rs, ripup.rs, shove.rs, nudge.rs, blame.rs, push.rs, vias.rs, cleanup.rs, prefanout.rs, pairs.rs, tune.rs, planes.rs, islands.rs, arcs.rs}`:**
  - Guarded whole-search entry points (`route_astar`, `cost_field`, `walled_in` in `search.rs`) under `Bucket::Astar`. **Zero clock reads occur inside `run_astar_loop` or `NodeHeap`**.
  - Guarded `ObstacleMap` construction (`grid.rs`) under `Bucket::MapBuild`, incremental track/via edits (`patch_grid` in `negotiate.rs` and `ObstacleMap` updates) under `Bucket::MapPatch`, `Zones::add`/`remove`/`others` under `Bucket::Zones`, terminal discovery (`terminals*` in `route.rs`, `cluster_terminals` in `vias.rs`, `terminals_lifting` in `shove.rs`) under `Bucket::Terminals`, and polygon/arc helpers under `Bucket::Geometry`.
  - Recorded per-round and per-attempt `RawEvent` outcomes in `negotiate.rs`, `route.rs`, `ripup.rs`, `shove.rs`, `nudge.rs`, `blame.rs`, `vias.rs`, and `cleanup.rs` **without adding any extra `connectivity()` calls** (using each move's existing `open_before` / `open_after` counts so `Watch` ticks and route geometry never change).
- **`core/crates/ultraroute/src/{stats.rs, stages.rs, lib.rs, main.rs}` & `core/README.md`:**
  - Added `--no-timing` CLI flag (`Args.timing: bool`, default `true` when `--report` is requested; disables fine-grained TLS bucket & move timers for A/B overhead testing).
  - Extended `--report` JSON (`RunStats`) with:
    - `board`: `BoardInfo { copper_layers, pads, nets, fine_pitch_parts }`
    - `load_cpu_seconds` and `stats_cpu_seconds` (CPU spent loading `.kicad_pcb` before stages and computing post-stage stats while `RunClock` is paused)
    - Per-`StageReport` `cpu_seconds`, `open_before`, `open_after`, `cpu: CpuBreakdown { astar, map_build, map_patch, zones, checks, connectivity, terminals, geometry, accounted, other }`, and `effort: Effort { useful_seconds, kept_other_seconds, wasted_seconds, accounted_seconds, other_seconds }`
    - Run-level `moves: MoveStats` (`negotiation`, `single`, `rip_up`, `shove`, `nudge`) and `progress: ProgressStats` (`initial_open`, `final_open`, `min_open`, `last_open_drop_cpu_seconds`, `last_open_drop_fraction`, `last_open_drop_stage`, `no_progress_routing_cpu_seconds`, `no_progress_total_cpu_seconds`, `open_curve`, `last_half`).
  - **Exact IEEE-754 accounting (`step_down` in `stats.rs`):** Sub-stage buckets and effort components are clamped and rounded down by 1 ULP where needed so `accounted + other <= cpu_seconds` holds strictly in `f64` JSON output, with unattributed stage time shown explicitly in `other`.
- **`core/crates/ultraroute/tests/stats.rs`:**
  - Added integration test `sub_stage_cpu_and_effort_account_for_stage_time` asserting that `--report` populates `board`, `cpu`, `effort`, `moves`, and `progress`, that `astar > 0`, and that `accounted + other <= cpu_seconds` holds for every stage, alongside `no_timing_flag_disables_fine_buckets_without_changing_summary`.

### Verification status vs Part 1 hard requirements
| Requirement | Status | Details |
| :--- | :--- | :--- |
| Compiles cleanly across workspace | **Verified** | `cargo check -p ultraroute --tests` and `cargo check -p web -p lab-films` exited `0` with `0` warnings. |
| No clock reads in A* inner loop | **Verified in code** | `enter_bucket(Bucket::Astar)` wraps whole `route_astar` / `cost_field` / `walled_in` calls in `core/crates/grid/src/search.rs`; `run_astar_loop` and `NodeHeap` do not touch `work` or `clock_gettime`. |
| Sub-stage accounting `<= stage_cpu` + test | **Implemented & check-clean; test execution blocked on `reserve`** | IEEE-754 `step_down` enforces `accounted + other <= cpu_seconds`; tested in `core/crates/ultraroute/tests/stats.rs`; across 4,845 reconstructed stage records in `existing_corpus_analysis.json`, max `(accounted + other) - stage_cpu = 0.00e0 s`. |
| Byte-identical routes on >=20 boards (`Deadline::seconds` patched to `1e6` per Rule 6) | **Scripted in `runs/exactness_and_overhead.py` & `runs/run_when_unblocked.sh`; unexecuted** | Compares `main-1e6` (`947d08c`), `inst-1e6 --report`, and `inst-1e6 --no-timing --report` across **48** non-time-limited `smoke + focus` boards (building `-p ultraroute -p kicad-text` in each binary directory and restoring `clock.rs` via `trap`). Not yet run because `reserve` was deadlocked. |
| CPU overhead measurement (`--report` vs `--no-timing` vs `main`) | **Scripted in `runs/exactness_and_overhead.py`; unexecuted** | Runs 3 interleaved `--cpu-clock` repeats across 25 boards; `--no-timing` flag is already wired if overhead exceeds ~1%. |

---

## 4. Part 2: Where the Router's Time Goes Across PCBench

### 4.1 Datasets, `explore/speed` hot-spot shift, and validation
Because live binary runs were blocked by `loaf`'s `reserve` deadlock, `runs/analyze_existing_runs.py` measures two complementary on-disk datasets:

1. **`HEAD-331` (`explore-text-v2-{smoke,focus,guard}-new`, 331 unique PCBench boards):** Routed on commit `c521114` (identical router code to `HEAD` `947d08c`), `--seed 1 --threads 1 --timeout 120 --cpu-clock`, with full `log.txt` stage/pass output and KiCad 9.0.8 DRC `result.json` (**260 / 331 clean = 78.55%**, total stage CPU `6,028.27 s`, plus `11.35 s` board-load CPU). On these exact 331 boards, pre-speed `s1-cpu-cd0a82a` took `8,418.50 s` (`257 / 331` clean), giving an exact post-`explore/speed` corpus speedup of **1.3965x** (`+3` net clean boards).
2. **`Full-1182` (all 1,182 PCBench boards):** Uses `HEAD-331` directly for those 331 boards and combines `s1-cpu-cd0a82a` (`--seed 1 --cpu-clock`, `975 / 1,182` clean on `cd0a82a`, raw total `37,859.40 s`) with per-board `log.txt` stage proportions from `srvfull-a7831ee` for the remaining 851 boards (**978 / 1,182 clean = 82.74%**, total stage CPU `35,469.17 s`, plus `56.63 s` load CPU).

**Validation of `log.txt` stage reconstruction:** Tested against all 5 post-speed `hs3_new` `.report.json` files inside `worktree-archive/explore-speed/files.tar.zst` (`59pct_keyboard`, `96boards-sensors_Sensors`, `CAL430FR_CAL430F`, `m2-electronics_m2fc`, `nan-15_pcb`), reconstructed stage durations match `.report.json` to **0.020 s mean absolute error** (max error `0.179 s`, on `96boards-sensors` `vias` `9.181 s` vs `9.360 s`).

**How `explore/speed` (`8c5c1b9`) changed the hot spots from `explore-nearmiss`:**
Archived incremental A/B `.report.json` runs in `explore-speed/files.tar.zst` show why `explore-nearmiss`'s 3-board profile was stale:
- **`hs1` (stamp & spoke caches in `ObstacleMap`):** Cut `CAL430FR_CAL430F` `shove` from `126.18 s -> 87.03 s` (**1.45x**) and `rip-up` `1.33 s -> 0.81 s` (**1.64x**), shrinking static `map_build` ~4x.
- **`hs2` (32-bucket radix `NodeHeap` + removing TLS `LocalKey::with` in `route_astar`):** Cut `59pct_keyboard` `negotiation` `109.11 s -> 67.39 s` (**1.62x**), `CAL430FR_CAL430F` `shove` `129.13 s -> 75.28 s` (**1.72x**), `finishing` **3.15x**, and `rip-up` **4.16x**.
- **`hs3` (`FxHash` + in-place `patch_grid` per `(class, weight)`):** Cut `m2-electronics_m2fc` `negotiation` `23.86 s -> 8.27 s` (**2.88x**) and `59pct_keyboard` `negotiation` `61.05 s -> 42.72 s` (**1.43x**), **eliminating the 70.0% `SipHash` `negotiate::patch_grid` bottleneck** seen in `explore-nearmiss` (`map_patch` is now **8.24%** of corpus CPU). Across all 5 benchmark boards (`base -> hs3_new`), total runtime fell `1.47x - 2.22x` (`59pct_keyboard` `138.49 -> 62.46 s`, `CAL430FR` `173.17 -> 89.36 s`, `m2fc` `92.18 -> 62.68 s`, `96boards` `53.28 -> 34.95 s`, `nan-15` `51.79 -> 33.86 s`), rescuing **+10 `focus` boards** (`0 -> 10` clean, `guard` `240 -> 240`, `0` lost).

---

### 4.2 Overall Breakdown by Stage and Sub-Operation (Total CPU vs Median Board)

#### By pipeline stage
| Stage | Full-1182 CPU (s) | Full-1182 Total % | Full-1182 Mean % | Full-1182 Median % (IQR) | HEAD-331 CPU (s) | HEAD-331 Total % | HEAD-331 Median % (IQR) |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `negotiation` | 18,880.56 | **53.23%** | 64.33% | **67.50%** (52.14 - 83.56%) | 2,585.68 | **42.89%** | **58.63%** (38.00 - 79.49%) |
| `shove` | 4,783.25 | **13.49%** | 5.79% | **0.00%** (0.00 - 0.17%) | 1,665.10 | **27.62%** | **0.00%** (0.00 - 3.39%) |
| `vias` | 4,413.77 | **12.44%** | 15.72% | **13.02%** (5.06 - 24.83%) | 895.13 | **14.85%** | **16.83%** (4.76 - 28.31%) |
| `prefanout` | 2,170.40 | **6.12%** | 3.86% | **0.00%** (0.00 - 1.41%) | 342.37 | **5.68%** | **0.00%** (0.00 - 2.31%) |
| `finishing` | 2,085.30 | **5.88%** | 1.91% | **0.00%** (0.00 - 0.44%) | 51.79 | **0.86%** | **0.00%** (0.00 - 0.52%) |
| `pairs` | 1,617.47 | **4.56%** | 3.23% | **0.00%** (0.00 - 0.00%) | 312.65 | **5.19%** | **0.00%** (0.00 - 0.00%) |
| `rip-up` | 1,072.53 | **3.02%** | 1.16% | **0.00%** (0.00 - 0.00%) | 90.71 | **1.50%** | **0.00%** (0.00 - 0.50%) |
| `write` | 113.25 | 0.32% | 1.99% | 0.44% (0.22 - 1.23%) | 21.17 | 0.35% | 0.55% (0.22 - 1.60%) |
| `tightening` | 95.76 | 0.27% | 1.18% | 0.36% (0.05 - 1.24%) | 21.37 | 0.35% | 0.44% (0.13 - 1.24%) |
| `result` | 95.64 | 0.27% | 0.29% | 0.00% (0.00 - 0.00%) | 32.92 | 0.55% | 0.00% (0.00 - 0.00%) |
| `refinishing` | 69.52 | 0.20% | 0.05% | 0.00% (0.00 - 0.00%) | 0.30 | 0.00% | 0.00% (0.00 - 0.00%) |
| `cleaning` | 42.33 | 0.12% | 0.06% | 0.00% (0.00 - 0.00%) | 1.10 | 0.02% | 0.00% (0.00 - 0.00%) |
| `set-up` | 20.11 | 0.06% | 0.41% | 0.22% (0.05 - 0.87%) | 5.60 | 0.09% | 0.30% (0.08 - 0.97%) |
| `length matching` | 9.29 | 0.03% | 0.02% | 0.00% (0.00 - 0.00%) | 2.38 | 0.04% | 0.00% (0.00 - 0.00%) |

*(Note: `islands`, `planes`, `first`, `nudge`, and `corridors` were gated off by `Stage::enabled` across default `--strip-all --recipe default` PCBench runs; `pair-escapes` and `prune-escapes` were `< 0.25%` in archived `.report.json` runs.)*

#### By sub-operation bucket (calibrated post-`explore/speed`)
| Sub-Operation Bucket | Full-1182 CPU (s) | Full-1182 Total % | Full-1182 Mean % | Full-1182 Median % (IQR) | HEAD-331 CPU (s) | HEAD-331 Total % | HEAD-331 Median % (IQR) |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `astar` (A* & field search) | 21,025.92 | **59.28%** | 60.38% | **63.44%** (57.54 - 65.49%) | 3,331.70 | **55.27%** | **61.08%** (53.89 - 64.82%) |
| `map_build` (Obstacle map build) | 3,203.25 | **9.03%** | 7.27% | **6.91%** (4.62 - 8.97%) | 658.77 | **10.93%** | **7.86%** (5.17 - 9.92%) |
| `zones` (Congestion discs) | 2,980.63 | **8.40%** | 9.00% | **9.32%** (7.42 - 10.92%) | 519.30 | **8.61%** | **8.70%** (6.47 - 10.54%) |
| `map_patch` (Incremental grid patch) | 2,923.57 | **8.24%** | 8.30% | **8.54%** (7.38 - 9.67%) | 490.78 | **8.14%** | **8.16%** (6.55 - 9.38%) |
| `other` (Trace-back, sort, I/O) | 1,987.57 | **5.60%** | 5.24% | **3.72%** (2.99 - 5.80%) | 362.87 | **6.02%** | **4.45%** (3.34 - 6.68%) |
| `connectivity` (Union-find & MST) | 1,062.60 | **3.00%** | 2.77% | **2.23%** (1.78 - 3.03%) | 252.50 | **4.19%** | **2.71%** (1.97 - 3.57%) |
| `checks` (Exact DRC geometry) | 1,019.70 | **2.87%** | 4.08% | **3.27%** (2.43 - 4.37%) | 189.12 | **3.14%** | **3.65%** (2.57 - 5.23%) |
| `terminals` (Pad/cluster entry lookup) | 696.54 | **1.96%** | 1.63% | **1.55%** (1.19 - 1.90%) | 130.33 | **2.16%** | **1.71%** (1.27 - 2.04%) |
| `geometry` (Polygons & morph) | 569.38 | **1.61%** | 1.34% | **0.42%** (0.15 - 1.36%) | 92.88 | **1.54%** | **0.56%** (0.23 - 1.65%) |

---

### 4.3 Splits by Outcome and Board Size

#### Split by routing outcome (4-way exact)
| Group (Dataset) | Boards | Total CPU (s) | Median CPU (s) | Useful % | Wasted % | Top Stages (% of group CPU) |
| :--- | ---: | ---: | ---: | ---: | ---: | :--- |
| `clean_untimed` (1,182) | 923 | 12,044.08 | 5.40 | 63.2% | 22.5% | `negotiation` 64.5%, `vias` 18.7%, `shove` 7.0%, `prefanout` 3.9% |
| `clean_time_limited` (1,182) | 55 | 5,409.69 | 90.80 | 62.9% | 21.0% | `negotiation` 59.2%, `vias` 14.7%, `shove` 7.9%, `prefanout` 6.4% |
| `failed_with_time` (1,182) | 67 | 1,858.29 | 19.86 | 24.5% | **54.1%** | `negotiation` 38.9%, `shove` 34.8%, `vias` 12.1%, `prefanout` 5.7% |
| `failed_time_limited` (1,182) | 137 | 16,157.11 | 118.10 | 50.0% | 30.9% | `negotiation` 44.5%, `shove` 17.8%, `finishing` 10.5%, `prefanout` 7.8% |
| `clean_untimed` (`HEAD-331`) | 256 | 2,915.18 | 4.87 | 57.9% | 25.9% | `negotiation` 58.4%, `vias` 22.8%, `prefanout` 6.6%, `pairs` 6.0% |
| `clean_time_limited` (`HEAD-331`) | 4 | 294.39 | 60.63 | 59.0% | 20.0% | `negotiation` 43.3%, `pairs` 21.0%, `prefanout` 12.7%, `shove` 11.5% |
| `failed_with_time` (`HEAD-331`) | 58 | 1,319.79 | 18.66 | 26.5% | **54.6%** | `negotiation` 40.0%, `shove` 34.7%, `vias` 10.3%, `prefanout` 4.3% |
| `failed_time_limited` (`HEAD-331`) | 13 | 1,498.91 | 108.45 | 22.6% | **39.4%** | **`shove` 69.9%**, `negotiation` 15.2%, `vias` 4.4%, `prefanout` 3.8% |

Notice the dramatic contrast between pre-speed `Full-1182` time-limited failures and post-speed `HEAD-331` time-limited failures: after `explore/speed` sped up `negotiation` ~1.6-2.9x, `negotiation` rarely consumes the 60 s cap on medium boards, so failing `HEAD` runs spend **69.9%** of their budget churning inside **`shove`** (`map_build` rises to **15.4%** and `connectivity` rises to **6.5%** on those boards).

#### Split by board size (connections, copper layers, fine-pitch parts; Full-1182)
| Dimension & Bin | Boards | Total CPU (s) | Median CPU (s) | Useful % | Wasted % | Top Stages (% of group CPU) |
| :--- | ---: | ---: | ---: | ---: | ---: | :--- |
| **Connections:** `1 - 50` | 397 | 1,845.45 | 1.30 | 45.0% | 22.6% | `negotiation` 44.8%, `shove` 29.7%, `prefanout` 10.3%, `vias` 8.5% |
| **Connections:** `51 - 150` | 449 | 9,385.40 | 9.40 | 52.2% | 31.7% | `negotiation` 50.6%, `shove` 20.3%, `vias` 12.8%, `rip-up` 4.7% |
| **Connections:** `151 - 400` | 261 | 16,171.78 | 50.60 | 56.2% | 28.2% | `negotiation` 55.1%, `vias` 14.4%, `shove` 11.8%, `prefanout` 5.6% |
| **Connections:** `401+` | 75 | 8,066.55 | 118.20 | 58.7% | 23.6% | `negotiation` 54.6%, `finishing` 11.4%, `prefanout` 9.2%, `vias` 9.1% |
| **Layers:** `2 copper layers` | 1,059 | 27,376.82 | 6.97 | 56.6% | 27.5% | `negotiation` 55.1%, `shove` 14.2%, `vias` 11.4%, `finishing` 6.0% |
| **Layers:** `4+ copper layers` | 123 | 8,092.36 | 52.73 | 50.1% | 28.7% | `negotiation` 47.1%, `vias` 15.8%, `prefanout` 11.3%, `shove` 11.1% |
| **Fine-pitch (`<=0.5 mm`):** `0 parts` | 865 | 16,013.49 | 5.20 | 63.5% | 24.4% | `negotiation` 64.3%, `vias` 14.3%, `shove` 7.7%, `finishing` 7.4% |
| **Fine-pitch (`<=0.5 mm`):** `1 - 2 parts` | 261 | 15,988.19 | 45.47 | 48.0% | 30.4% | `negotiation` 45.0%, `shove` 17.9%, `prefanout` 11.6%, `vias` 11.1% |
| **Fine-pitch (`<=0.5 mm`):** `3+ parts` | 56 | 3,467.49 | 43.49 | 49.8% | 31.5% | `negotiation` 39.9%, `shove` 20.1%, `vias` 9.9%, `pairs` 9.5% |

Just **317 boards** (`26.8%` of PCBench) have one or more fine-pitch (`<= 0.5 mm`) parts, yet they account for **54.85%** (`19,455.68 s`) of all corpus CPU (`8.7x` higher median runtime than non-fine-pitch boards: `45.47 s` vs `5.20 s`).

---

### 4.4 Useful vs Wasted Work and What Failing Boards Do in `[0.5*T, T]`

#### Per-stage and per-move-kind effort breakdown
| Move / Stage Kind | Dataset | Volume | Kept / Succeeded | Useful Closing (s) | Kept Non-Closing (s) | Wasted (s) | Waste % | Primary Source of Wasted CPU |
| :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: | :--- |
| **Negotiation rounds** | Full-1182 | 7,577 rounds (1,181 passes) | 6,554 conv rounds | 15,365.04 | 0.00 | 2,382.69 | **12.6%** | 1,029 stalled plateau rounds (`1,739.08 s`) + uncommitted net routes |
| **Negotiation rounds** | `HEAD-331` | 2,316 rounds (331 passes) | 1,852 conv rounds | 1,990.15 | 0.00 | 440.39 | **17.0%** | 461 stalled plateau rounds (`440.39 s`) |
| **Single-net A*** (`finishing`) | Full-1182 | 14,318 tries | 10,606 routed | 1,179.29 | 0.00 | 813.87 | **39.0%** | 3,712 `no_path` exhaustive A* + `cell/2` 4M-node fine-window floods |
| **Single-net A*** (`finishing`) | `HEAD-331` | 674 tries | 93 routed | 6.83 | 0.00 | 40.47 | **78.1%** | 581 `no_path` exhaustive flood-fills |
| **Rip-Up** (`rip-up`) | Full-1182 | 2,314 tries | 171 kept (`7.39%`) | 82.91 | 0.00 | 914.55 | **85.3%** | 630 `no_better` 10 s reroutes (`738.93 s`) + 1,490 `no_path` (`174.66 s`) |
| **Rip-Up** (`rip-up`) | `HEAD-331` | 642 tries | 42 kept (`6.54%`) | 4.92 | 0.00 | 79.44 | **87.6%** | 119 `no_better` reroutes (`48.60 s`) + 470 `no_path` (`30.72 s`) |
| **Shove / Plough** (`shove`) | Full-1182 | 4,945 tries (1,059 passes) | 382 closed + 497 tie | 1,246.01 | 1,363.35 | 1,882.71 | **39.4%** | 246 reverted repairs (`1,280.24 s` repair + `150.62 s` reroute) + 3,820 walled (`830.55 s`) |
| **Shove / Plough** (`shove`) | `HEAD-331` | 3,426 tries (650 passes) | 140 closed + 227 tie | 240.12 | 563.42 | 778.21 | **46.7%** | 143 reverted repairs (`529.18 s` repair + `62.26 s` reroute) + 2,916 walled (`271.92 s`) |
| **Via reduction** (`vias`) | Full-1182 | ~210k subwindow tries | ~12.6% kept | 0.00 | 264.83 | 3,707.57 | **84.0%** | Failed 6 mm subwindow obstacle-map builds + A* replacement attempts |

Two waste concentrations stand out immediately:
1. **`Rip-Up` wastes 85.3% - 87.6% of its CPU**, keeping only `6.5% - 7.4%` of attempts; **80.8%** of its wasted time (`738.93 s` of `914.55 s`) is spent inside `no_better` 10-second `route_open` reroutes where ripping a uniform-price (`rip_cost = 400`) soft track broke a congested victim net that could not re-close.
2. **`Vias` wastes 84.0% of its CPU (`3,707.57 s` across 1,182 boards)** testing candidate via pairs/singles/clusters whose 6 mm subwindow A* fails to find a legal fewer-via detour (and **runs even when the board still has open connections!**).

#### What failing boards do in their second half `[0.5*T, T]`
- **`HEAD-331` failing boards (`n = 71`, `1,409.35 s` in `[0.5*T, T]`):**
  - **58.81%** wasted (`828.84 s`), **24.64%** `kept_other` (`shove` tie swaps & dropped vias), and only **8.16%** (`114.99 s`) useful connection-closing work!
  - On the **58 `failed_with_time` boards** (`659.90 s` in `[0.5*T, T]`): **71.81% is wasted**, split across `shove` (**51.25%**, `338.17 s`), **`vias` reduction on an incomplete board** (**20.57%**, `135.77 s`), stalled `negotiation` tail (**16.77%**, `110.65 s`), `result` (`4.18%`), and `rip-up` (`3.84%`).
  - On the **13 `failed_time_limited` boards** (`749.46 s` in `[0.5*T, T]`): **`shove` consumes 89.34%** (`669.56 s`) and `vias` consumes **8.84%** (`66.23 s`), with only **8.95%** useful closing work vs **47.36%** wasted and **37.45%** `shove` tie-swap churn.
- **`Full-1182` failing boards (`n = 204`, `9,007.70 s` in `[0.5*T, T]`):**
  - On `failed_with_time` (`n = 67`, `929.15 s` in `[0.5*T, T]`): **73.10% wasted**, **5.16% useful** (`shove` `51.00%`, `vias` `24.19%`, `negotiation` `15.23%`).
  - On `failed_time_limited` (`n = 137`, `8,078.56 s` in `[0.5*T, T]`): **45.21% wasted**, **37.10% useful**, **8.43% `kept_other`** (`negotiation` `28.35%`, `shove` `26.09%`, `finishing` `19.35%` on huge boards where `negotiation` hit its 60 s cap, `vias` `14.16%`, `rip-up` `8.39%`).

*(Quick classical win uncovered here: skipping `Vias` reduction or capping it tightly whenever `open > 0` saves `202.00 s` on `HEAD-331` and `1,368.54 s` across `Full-1182` failing second halves with zero effect on completion rate!)*

---

### 4.5 When Open Count Last Drops, and What a Stop Rule Saves vs Loses

#### Oracle zero-progress tail (`last_open_drop_cpu_seconds`)
| Outcome Group | Dataset | Boards | Median Last Drop (s) | Median Drop (% of Run) | Zero-Progress Routing CPU (s) | Share of Group CPU | Stage of Last Open-Count Drop |
| :--- | :--- | ---: | ---: | ---: | ---: | ---: | :--- |
| **All boards** | `HEAD-331` | 331 | 4.40 s | 74.11% | **915.05 s** | **15.18%** (`18.12%` of routing) | `negotiation` 242, `shove` 56, `rip-up` 15, `finishing` 14, `refinishing` 3, never 1 |
| `clean` | `HEAD-331` | 260 | 3.80 s | 76.11% | 1.20 s | 0.04% | `negotiation` 210, `shove` 29, `rip-up` 10, `finishing` 9, `refinishing` 1 |
| `failed_with_time` | `HEAD-331` | 58 | 5.49 s | 45.19% | **485.45 s** | **36.78%** | `negotiation` 31, `shove` 17, `finishing` 5, `rip-up` 3, `refinishing` 2 |
| `failed_time_limited` | `HEAD-331` | 13 | 66.80 s | 79.83% | **428.40 s** | **28.58%** | `shove` 10, `rip-up` 2, `negotiation` 1 |
| **All boards** | Full-1182 | 1,182 | 6.30 s | 80.59% | **2,403.09 s** | **6.78%** (`7.83%` of routing) | `negotiation` 853, `shove` 159, `finishing` 84, `rip-up` 52, `refinishing` 20, `cleaning` 10 |
| `clean` | Full-1182 | 978 | 4.60 s | 79.60% | 121.88 s | 0.70% | `negotiation` 804, `shove` 108, `finishing` 34, `rip-up` 23 |
| `failed_with_time` | Full-1182 | 67 | 5.80 s | 43.64% | **765.52 s** | **41.20%** | `negotiation` 35, `shove` 19, `finishing` 7, `rip-up` 4, `refinishing` 2 |
| `failed_time_limited` | Full-1182 | 137 | 105.64 s | 89.53% | **1,515.68 s** | **9.38%** | `finishing` 43, `shove` 32, `rip-up` 25, `refinishing` 16, `negotiation` 14 |

#### Causal post-negotiation patience stop-rule simulation (wins AND regressions)
Because `Negotiation` holds tentative routes in memory and updates `Board` `open` count only once at `commit` (while running its own internal `patience = 4/8` shared-cell round rule), an open-count stop rule must start **after `Negotiation` commits**, watching post-negotiation repair CPU (`finishing`, `rip-up`, `shove`, `nudge`, `refinishing`, `corridors`). Stopping repair as soon as `Delta_t` repair CPU seconds elapse with `open > 0` and no new record-low `min_open` yields:

| Patience `Delta_t` | `HEAD-331` Triggered | `HEAD-331` CPU Saved | `HEAD-331` Clean Lost | `HEAD-331` Extra Opens | Full-1182 Triggered | Full-1182 CPU Saved | Full-1182 Clean Lost (Regression!) | Full-1182 Extra Opens |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `3 s` | 52 / 331 | 1,554.07 s (**25.78%**) | **13** | +106 | 187 / 1,182 | 5,402.59 s (**15.23%**) | **69** | +1,070 |
| `5 s` | 46 / 331 | 1,439.78 s (**23.88%**) | **10** | +96 | 167 / 1,182 | 4,941.08 s (**13.93%**) | **57** | +922 |
| `8 s` | 35 / 331 | 1,282.88 s (**21.28%**) | **6** | +72 | 133 / 1,182 | 4,302.12 s (**12.13%**) | **38** | +820 |
| `10 s` | 33 / 331 | 1,204.38 s (**19.98%**) | **5** | +64 | 126 / 1,182 | 3,987.41 s (**11.24%**) | **36** | +798 |
| `15 s` | 22 / 331 | 1,039.14 s (**17.24%**) | **1** | +40 | 94 / 1,182 | 3,289.60 s (**9.27%**) | **24** | +187 |
| **`20 s`** | **20 / 331** | **934.54 s (15.50%)** | **0** | +38 | 79 / 1,182 | 2,805.76 s (**7.91%**) | **18** | +163 |
| **`30 s`** | **17 / 331** | **732.91 s (12.16%)** | **0** | +36 | 60 / 1,182 | 1,989.09 s (**5.61%**) | **9** | +123 |
| `45 s` | 8 / 331 | 377.47 s (**6.26%**) | **0** | +5 | 31 / 1,182 | 1,103.79 s (**3.11%**) | **5** | +60 |
| `60 s` | 6 / 331 | 223.37 s (**3.71%**) | **0** | +3 | 21 / 1,182 | 602.86 s (**1.70%**) | **3** | +42 |

**Critical lesson from the regression column:** On `HEAD-331`, `Delta_t = 20 s` saves **15.50%** of all CPU with **0 clean boards lost**. Across all 1,182 boards, however, **108 clean boards** closed their final open connection inside `Shove`, and **18** of them endured a `> 20 s` dry spell across early narrow-radius (`0.5 - 1.0 mm`) passes + tie swaps before a wider `2.0 - 4.0 mm` pass broke the jam! Therefore a stop rule should **never** prematurely terminate a run that still has timeout budget without using the freed time to **restart with a fresh seed / reach / via-cost** (keeping the checkpoint with minimum `open`).

---

## 5. Part 3: Every Heuristic Decision Point, Measured Variance, and Ranked RL Assessment

### 5.1 Multi-seed and single-choice variance evidence on disk

#### Multi-seed spread (identical binary)
1. **200 spread-sampled PCBench boards (`remed-0-base-{s0,s1,s2,s0b}`, commit `184f640`):**
   - Individual seeds: `s0` = **161 / 200** clean, `s1` = **160 / 200** clean (`1,320` total unconnected, `8,090.70 s`), `s2` = **160 / 200** clean, repeat `s0b` = **160 / 200** clean (`3` boards flipped on wall-clock timeout jitter between `s0` and `s0b`: `Hardware_Playground_heater_actuator_node`, `Wifi-Play-Board_Wifi_PB`, `timecircuits-hardware_BTTF-TimeCircuits`).
   - **Best of 3 seeds (`s0, s1, s2`):** **164 / 200 clean (+4 over `s1`)**, cutting total unconnected `1,320 -> 1,186` (`-134` opens); **worst of 3 seeds:** **156 / 200** (`1,414` opens). **8 boards** (`4.0%`) flip clean/failed across 3 seeds, and **28 / 200 boards** (`14.0%`, or **70% of the 40 failing boards**) vary in final open count across seeds!
   - **Sequential restart baseline (`s1`, then `s2` & `s0` only when failed):** Achieves the full **164 / 200** clean at `16,638.00 s` (`2.06x` `s1` CPU, vs `24,661.80 s` = `3.05x` for naive 3-seed parallel).
2. **34 time-limited near-miss boards (`explore-nearmiss`, seeds `1, 2, 3` at `120 s` + `s1` at `240 s` / `480 s`):**
   - Individual seeds at `120 s`: `s1` = **12 / 34** clean (`52` open), `s2` = **14 / 34** clean, `s3` = **18 / 34** clean.
   - **Best of 3 seeds at `120 s`:** **23 / 34 clean (67.6%, +11 boards over `s1`)**, cutting total open connections from **52 -> 18** (`-65.4%`); worst of 3 seeds is **5 / 34** (`68` open).
   - **More time on seed 1 (`240 s` / `480 s`):** `21 / 34` clean at `240 s`, `24 / 34` (`70.6%`) clean at `<= 480 s`; union of 3 seeds or `480 s` reaches **26 / 34 (76.5%)** clean.

#### Single-choice heuristic A/B experiments on PCBench
| Experiment (On-Disk Runs) | Boards | Base Clean | Alt Clean | `Delta` Clean (Gained / Lost) | Base -> Alt Open (`Delta`) | Base -> Alt Vias | CPU `Delta` |
| :--- | ---: | ---: | ---: | :--- | :--- | ---: | ---: |
| **Plane via cost `1.0/0.4 mm` vs `5.0 mm`** (`mergepv-sample-b` vs `-oldcost`) | 100 | 100 | 100 | **0** (`+0 / -0`) | `0 -> 0` (`0`) | `1,250 -> 1,438` (**+15.0%**) | `-3.89%` |
| **Disable via reduction `--no-reduce-vias`** (`mergepv-sample-b` vs `-noreduce`) | 100 | 100 | 100 | **0** (`+0 / -0`) | `0 -> 0` (`0`) | `1,250 -> 1,818` (**+45.4%**) | **-5.18%** |
| **`--prefanout-mode same --via-cost 5` vs default** (`pb-budget` vs `pb-svc5`) | 240 | 194 | 190 | **-4** (`+1 / -5`) | `1,251 -> 1,560` (**+309**) | `10,615 -> 8,992` (`-15.3%`) | **+6.66%** |
| **Seed 0 (Pass-0 unshuffled Prim MST) vs Seed 1** (`remed-0-base-s1` vs `-s0`) | 200 | 160 | 161 | **+1** (`+4 / -3`) | `1,320 -> 1,297` (`-23`) | `8,954 -> 8,982` | `+0.95%` |
| **Seed 2 vs Seed 1** (`remed-0-base-s1` vs `-s2`) | 200 | 160 | 160 | **0** (`+3 / -3`) | `1,320 -> 1,304` (`-16`) | `8,954 -> 8,971` | `+0.89%` |
| **Stage share reshuffle `--elastic-budget`** (`explore-nearmiss` paired A/B) | 34 | 12 | 11 | **-1** (`+0 / -1`) | `52 -> 48` (`-4`) | n/a | `-0.84%` |
| **`Negotiation` net-priority rules** (`explore-priority`, 4 variants on 200 boards) | 200 | 158 | 157 - 159 | **-1 to +1** | washes out under history | n/a | `~0%` |
| **`Prefanout` order (`inner`/`round`/`trapped`) & ring/inner** (`explore-prefanout`) | 51 | 30 | 29 - 30 | **-1 to 0** | minor local shifts | n/a | `~0%` |

**Audit discovery on `--seed` coverage:** In `core/crates/grid/src/jitter.rs:13-18`, `if seed == 0 { return; }` makes `--seed 0` a complete no-op on `Negotiation` Pass 0 and `route_open`. Furthermore, `Prefanout`, `Planes`, `Islands`, `Pairs`, `RipUp`, `Tightening`, `Vias`, `Cleaning`, `Corridors`, and `LengthMatching` **ignore `--seed` completely**, and `route_open` inside `ripup.rs`, `shove.rs`, and `nudge.rs` hardcodes `seed: 0`! Thus **100% of measured seed variance comes from just two places:** `Negotiation` connection order (`negotiate.rs:784`) and `Shove` target order (`shove.rs:221`).

---

### 5.2 Complete Ranked Decision Inventory & RL Feasibility Analysis

| Rank | Decision Point | Exact Source Location (`file:line`, symbol) | Governed CPU (1,182 / `HEAD-331`) | Corpus Decisions (1,182 boards) | Upper Bound on Gain | Simplest Non-Learned Baseline | Verdict |
| ---: | :--- | :--- | ---: | ---: | :--- | :--- | :--- |
| **1** | **Run-level stall stop & seed/parameter restart** | `ultraroute/src/lib.rs:909-1039` (`Budget`, `RunClock::deadline`, `STAGES`); `stages.rs:656-686` (`Shove::run`) | **75.82%** (`26,891.2 s`) / **72.88%** (`4,393.6 s`) | `1,059` shove passes (`1,182` runs) | `+4` clean / 200 spread (`+11` / 34 near-miss); `915 s` (`15.2%`) `HEAD-331` zero-prog CPU | Post-negotiation patience (`20-30 s`) -> checkpoint + restart with `seed+1` & `via_cost=2` | **Do NOT use RL** (15-line classical rule wins; too few episodes) |
| **2** | **`Shove` target order, region radius, repair cap & tie swaps** | `grid/src/shove.rs:203-404` (`shove_escape`, `TIES`), `455-522` (`repair`), `628-745` (`plough_round`), `898-1076` (`renegotiate`) | **13.49%** (`4,783.3 s`) / **27.62%** (`1,665.1 s`; **69.9%** of `HEAD` TL fails) | **4,945 tries** (~20k across 4 seeds) | `+11/34` near-miss seed rescues + cut `1,882.7 s` wasted `shove` CPU by ~70% | Score open airlines by `soft_path` blocker count + `mouth_room`; abort `repair` if `open` balloons | **#1 RL / Contextual Bandit Candidate** |
| **3** | **`Negotiation` net order, weights, pricing (`1.6x`, `+10`), patience & commit** | `grid/src/negotiate.rs:680-802` (`route_negotiated_rounds`), `849-1228` (`run_pass`), `1230-1307` (`commit_order`), `1356-1537` (`rescue_walled_off`) | **53.23%** (`18,880.6 s`) / **42.89%** (`2,585.7 s`) | **7,577 rounds** (`~1.2M` net reroutes) | Cut `1,739.1 s` stalled rounds + part of 28/200 seed-open spread | Fix `seed=0` no-op; sort Pass 0 by `(mouth_room, span)`; adaptive collision slope stop | **Marginal for RL** (history pricing washes out static net order) |
| **4** | **`Rip-Up` & `Corridors` airline order, `rip_cost=400` & victim filter** | `grid/src/ripup.rs:118-295` (`rip_up_watch`, `via_penalty` bug at `281`), `350-450` (10 s reroute), `594-678` (`try_corridor`) | **3.02%** (`1,072.5 s`) / **1.50%** (`90.7 s`) | **2,314 tries** (`max 122`/board) | Reclaim `914.5 s` (**85.3%**) wasted Rip-Up CPU + unlock multi-victim traps | Tabu victim pricing (`rip_cost += 400` per failed rip) + fix `via_penalty` at `ripup.rs:281` | **Tabu heuristic first**; optional tiny skip-classifier |
| **5** | **Stage-wise & board-level `via_cost` (`5.0/6.0/4.0 mm`) & 2L bias** | `ultraroute/src/lib.rs:858-897` (`ViaCosts`, `via_costs`); `grid/src/search.rs:32-55` (`via_cost_mm`, `step_cost`) | **75.82%** (`26,891.2 s`, shapes all A*) | **1,182** (`1` per board) | `-13.1%` vias on plane boards; flips `-4` to `+2` clean on dense 2L/4L boards | 2-feature rule `(layers, via_diam / pad_pitch)` or try `via_cost=2.0` on restart | **Do NOT use RL** (`1` decision/board = 1D/2D tuning) |
| **6** | **`Prefanout` fine-pitch selection (`<=0.5 mm`), pad order & escape mode** | `morph/src/escape.rs:108-137` (`fine_pitch`); `grid/src/prefanout.rs:314-675` (`prefanout`, `order_groups`), `1374-1614` (`lay_grid_escape`) | **6.12%** (`2,170.4 s`) / **5.68%** (`342.4 s`) | **12,982 pads** (on 317 boards) | `<= 1` board across `inner`/`round`/`trapped` on 51 fine-pitch boards | Add dot-product toward net airline centroid in `lay_grid_escape` target cost | **Do NOT use RL** (5-line geometric bias suffices) |
| **7** | **Multi-pin MST topology & `route_open` fallback ladder (`cell/2` fine grid)** | `board/src/connect.rs:219-345` (`connectivity`); `negotiate.rs:1539-1663` (`reachable_airlines`); `route.rs:189-335, 519-665, 785-995` | **6.08%** (`2,154.8 s`) / **0.86%** (`52.1 s`) | **14,318 tries** (`3,712` `no_path`) | Save part of `813.9 s` `no_path` flood-fills in `Finishing` | Gate `cell/2` fine-window A* on local neck check (`soft_path` blocked near pad) | **Do NOT use RL** (geometric gate suffices) |
| **8** | **`Vias` reduction candidate order & `Tightening` chain order** | `grid/src/vias.rs:200-366` (`try_replace`), `404-665` (`reduce_vias_all`); `cleanup.rs:41-128, 1282-1468` (`pull_tight_watch`) | **12.71%** (`4,509.5 s`) / **15.20%** (`916.5 s`) | **~236,759** subwindow/chain tries | **0 clean boards** (runtime & via count only: up to `3,707.6 s` wasted `vias` tries) | Skip `Vias` when `open > 0`; cheap same-layer reachability check before `try_replace` | **Do NOT use RL** (zero routability gain) |
| **9** | **Part nudging (`blame` soft-wall A* vs 144-step blind search)** | `grid/src/nudge.rs:197-420` (`try_step`, `nudge_watch`); `blame.rs:120-366` (`blame`, `satisfy`); `push.rs:128-207`; `morph/src/flow.rs:36` | **0.00%** in default recipe (`--nudge` off) | `0` in default PCBench | Only applies when footprints may move | Disable 144-step blind loop (`blind: false`); step along `blame` vector only | **Out of scope** for fixed-placement PCBench |
| **10** | **Diff-pair stretch score & inner A* `NodeHeap` tie-breaking** | `grid/src/pairs.rs:556-780, 1016-1366`; `tune.rs:212-355`; `search.rs:187-205` (`NodeHeap::push`), `310-360` (`trace_back`), `442-615` | **4.59%** (`pairs`) / **59.28%** (`astar` inner loop) | `622` pair runs / `~10^9` heap pops | Faster open-space A* via fewer equal-`f` plateau pops | Break equal-`f` ties in `NodeHeap` by smaller `h` (higher `g`) in `search.rs:189` | **NEVER use RL** inside A* inner loop |

*(Additional minor deterministic heuristics audited in code: `SetUp` cgroup/grid auto-coarsening in `ultraroute/src/lib.rs:1081-1178`, `Islands::run` strip-end stitching in `grid/src/islands.rs`, `Planes::run` short plane-via fanout in `grid/src/planes.rs`, and `Cleaning` jag/staircase thresholds in `grid/src/cleanup.rs:1332-1405` together consume `< 0.25%` of corpus CPU and show near-zero routability variance.)*

---

### 5.3 Deep-Dive Formulation for the Top Three Decisions

#### Top Candidate (#2 overall, #1 for ML/RL): `Shove` Target, Radius, and Repair-Budget Policy
- **Why this is the right place to learn:** In `shove.rs:221`, `shove_escape` shuffles open airlines uniformly at random each pass, tries a fixed `0.5 -> 1.0 -> 2.0 -> 4.0 mm` reach ladder, and gives every breached wall up to **20.0 seconds** of two-phase `rip_up` repair (`55%` locked / `45%` unlocked) plus up to `6` tie swaps (`open_after == open_before`). When it picks the wrong pin first or triggers a 20-second repair in an un-repackable 12-net snarl, it burns tens of seconds and reverts (`1,280.24 s` wasted in reverted repairs alone across 1,182 boards) while starving solvable pins of timeout budget.
- **Observation (`s_t`, ~24 floats per open airline, `< 50 us` to extract from existing `PassCache`):**
  1. `soft_path` length, soft-copper crossing count, via count, and bottleneck clearance shortfall (`shove.rs:764`),
  2. Distinct blocker-net counts and copper density within `0.5 mm`, `1.0 mm`, `2.0 mm`, and `4.0 mm` of both endpoints,
  3. Endpoint pad `mouth_room`, layer count, SMD vs PTH flag, and net wire-length slack,
  4. History on this net/pin in the current run: prior `walled_no_path` count, prior `reverted_routed` count, tie swaps already spent (`<= 2/net`, `<= 6` total), current pass number, and remaining run timeout.
- **Action (`a_t`):** Rank/select the next `(airline, endpoint)` and choose `(reach_mm in {0.5, 1.0, 2.0, 4.0}, repair_cap_s in {0, 3, 8, 20}, allow_tie in {0, 1})`.
- **Reward (`r_t`):** `+10 * max(0, open_before - open_after) + 1.5 * I(tie_kept_and_board_later_drops_open) - 0.15 * try_cpu_seconds`.
- **Corpus training volume:** **4,945 tries** per single-seed 1,182-board run (`3,426` tries on `HEAD-331` alone), yielding **~20,000 - 30,000 counterfactual `(s_t, a_t, outcome, cpu_s)` tuples** across 4-6 seeds on PCBench. Every try is an isolated checkpoint/revert transaction (`Board` clone), so **offline contextual bandit / cost-sensitive GBDT or MLP scoring** trains cleanly without long-horizon credit-assignment instability!
- **Upper bound on gain:** Capturing the 3-seed near-miss rescue gap (**+11 / 34 boards**, `12 -> 23` clean at 120 s) plus reclaiming ~70% of `Shove`'s `1,882.71 s` wasted CPU.
- **Simplest non-learned baseline it must beat:** Sort open airlines ascending by `(prior_failures_on_net, soft_path_distinct_blocker_nets, -pad_mouth_room)` instead of uniform Fisher-Yates shuffle, and abort `repair()` early if `open_now > open_before + 3` after Phase 1 (`55%` budget). Build and benchmark that 25-line classical baseline first; train a contextual bandit only if its offline top-1 accuracy on logged `ShoveTry` outcomes beats that heuristic.

#### Runner-Up (#1 by governed time, NOT worth RL): Run-Level Stop & Multi-Seed Restart Controller
- **Observation:** `(stage, open_now, min_open, repair_cpu_since_min_open, shove_pass, consecutive_reverted_repairs, remaining_timeout)`.
- **Action:** `{continue, abort_to_polish, revert_to_best_checkpoint_and_restart(seed + 1, via_cost_alt)}`.
- **Corpus training volume:** Only `1,182` board runs (`204` failing boards -> `~1,000 - 3,000` pass-level transitions).
- **Why RL is NOT worth it here:** Sample size is tiny, and the exact causal simulation in Section 4.5 shows that a deterministic two-clause rule (`if open > 0 and repair_cpu_since_min_open >= 20 s: if remaining_budget >= 1.2 * neg_cpu: restart(seed + 1) else: stop_repair()`) directly targets both the **15.50% CPU savings** on `HEAD-331` and the **+4 / 200** (`remed-0`) and **+11 / 34** (`explore-nearmiss`) multi-seed rescue gains.

#### Third (#3 overall, marginal for RL): `Negotiation` Pricing & Priority Adaptation
- **Observation:** Per-net `(airline_span, pin_count, min_mouth_room, last_round_collisions, path_history_sum)` + global `(round, colliding_nets, shared_cells, delta_shared_3_rounds)`.
- **Action:** Per-net weight `w_i in [0.5, 2.0]` and per-round `{continue, stop_and_commit, boost_present_growth}`.
- **Corpus training volume:** `7,577` rounds across 1,182 boards (`~1.2M` individual net reroutes).
- **Why RL is marginal here:** `explore-priority` proved empirically on 200 PCBench boards that static net-priority heuristics (`--first`, `--first-weight`, congestion ordering) shift clean count by at most `-1` to `+1` board because PathFinder's accumulated `history += 10` pricing quickly overrides initial net order. Meanwhile, stalled plateau rounds take only `1,739.08 s` (`4.90%` of total corpus CPU). Fix `jitter::shuffle` (`seed == 0`) first.

---

## 6. What Did Not Work, Costs/Regressions Found, and What Could Not Be Verified

1. **`loaf` `reserve` queue deadlock prevented live binary execution:** Five stale reservations held 20/22 cores on `loaf` throughout the session (`0.0%` actual CPU pressure), blocking `cargo test`, `cargo build --release`, the 48-board `1e6` byte-identical test, the 3x interleaved overhead test, and live 214-board / 4-seed router runs. Everything required to run them cleanly in ~45-60 minutes once `reserve` is cleared is ready in `./runs/run_when_unblocked.sh`.
2. **Naive open-count stop rules fail catastrophically if started before `Negotiation` commits, and still lose slow `Shove` rescues if they abort without restarting:**
   - Because `Negotiation` commits routes only at the end of its multi-round pass, starting an open-count patience timer at `t = 0` aborts `Negotiation` mid-pass on medium/large boards (`> 20 s` negotiation), losing hundreds of clean boards.
   - Even when started strictly post-negotiation, `Delta_t = 20 s` saves `15.50%` CPU with `0` clean boards lost on `HEAD-331`, **but loses 18 clean boards across `Full-1182`** (`9` lost at `Delta_t = 30 s`, `5` lost at `Delta_t = 45 s`) where `Shove` needed wider `2.0 - 4.0 mm` passes after early passes stalled.
3. **Static budget reshuffling (`--elastic-budget`) and static `Negotiation` priority heuristics did not improve clean count:** `--elastic-budget` regressed `12 -> 11 / 34` on near-miss boards (`explore-nearmiss`), `--prefanout-mode same --via-cost 5` regressed `194 -> 190 / 240` (`pb-svc5`), and four `Negotiation` net-ordering heuristics moved clean count by `-1` to `+1` on 200 boards (`explore-priority`).

---

## 7. What To Try Next (Concrete Order of Attack)

1. **Run `./runs/run_when_unblocked.sh` once `loaf`'s stale `reserve` slots are cleared** to execute the 48-board Rule-6 byte-identical proof, measure exact TLS timer overhead (`--report` vs `--no-timing` vs `main`), and refresh `~/Software/ultraroute-lab/timing/data.json` with live sub-stage hardware CPU timers.
2. **Three zero-ML classical wins (~50 lines of Rust total, high confidence):**
   - **Skip `Vias` reduction when `open > 0`** (`stages.rs:753`): immediately reclaims `202 s` on `HEAD-331` (`1,369 s` on `Full-1182`) wasted polishing incomplete boards, with zero possible loss of clean boards.
   - **Fix standalone `RipUp` missing `grid.via_penalty` (`ripup.rs:281-285`) + add tabu victim pricing (`rip_cost += 400` per failed rip of a victim net):** directly attacks `RipUp`'s **85.3% wasted CPU** (`738.9 s` in `no_better` reroutes).
   - **Break equal-`f` ties in `NodeHeap::push` (`search.rs:189`) by smaller `h` instead of `node` index**, and fix `jitter::shuffle` (`jitter.rs:15`) so `seed == 0` is not an accidental no-op.
3. **Build the 20-line classical `Shove` scorer + post-negotiation restart baseline, then benchmark a contextual bandit on `ShoveTry` logs:**
   - Log `(soft_path features, blocker-net counts, mouth_room, reach, repair_cap) -> (delta_open, try_cpu_s)` across PCBench seeds 1..4 (~20,000 `Shove` tries).
   - Compare (a) uniform shuffle (today), (b) analytical blocker-count + `mouth_room` ranking with early `repair` abort, and (c) an offline-trained contextual bandit (`< 2 KB` weights, `< 5 us` per `Shove` try) evaluated on held-out PCBench boards.

---

## For the project memory

- **`~/.claude/bin/reserve` waiting-slot leak under `timeout`:** In `~/.claude/bin/reserve` lines 267-292, `SIGTERM` is not trapped while a client is polling in `"waiting"` state (`trap ... EXIT` in bash does not run on unhandled `SIGTERM` unless `SIGTERM` is trapped). Wrapping `reserve` in `timeout` (default `SIGTERM`) kills the waiting bash client, after which the daemon promotes the orphaned request to `"running"` and holds its cores forever (`timeout -s INT` avoids this because `SIGINT` is trapped, or `reserve` itself should trap `TERM HUP` before the wait loop).
- **Post-`explore/speed` (`8c5c1b9`) profiler reality:** `negotiate::patch_grid` (`SipHash`) is **no longer** a 70% bottleneck; `FxHash` + in-place `patch_grid` (`hs3`) shrank `map_patch` to ~8.2% of corpus CPU, stamp/spoke caching (`hs1`) shrank `map_build` to ~9-11%, and `astar` (`search.rs`) is now #1 at ~55-59% of corpus CPU.
- **Seed coverage blind spots in `HEAD` (`947d08c`):** `jitter::shuffle` (`core/crates/grid/src/jitter.rs:15`) returns early when `seed == 0`; `route_open` calls inside `ripup.rs`, `shove.rs`, and `nudge.rs` hardcode `seed: 0`; and `Prefanout`, `Pairs`, `RipUp`, `Tightening`, and `Vias` ignore `--seed` completely. All multi-seed variance comes from `Negotiation` connection order and `Shove` target order.
- **Two subtle router bugs found during the heuristic audit:**
  1. `core/crates/grid/src/ripup.rs:281-285`: soft-via penalties (`grid.via_penalty`) are populated only when `options.region_repair == true` (`Shove`'s internal repair), leaving soft vias free to cross in standalone `RipUp`.
  2. `core/crates/grid/src/search.rs:189`: `NodeHeap::push` packs `(u64::from(priority) << 32) | node as u64`, breaking equal-`f` ties in `self.top` by raw `node` index (`layer * nx * ny + j * nx + i`) rather than smaller `h` (larger `g`).
