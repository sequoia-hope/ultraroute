# What Plain Seed Restarts Win and What They Cost on PCBench (`main` at `aae1126`)

## Summary

Before investing in a learned net-order or seed-selection policy, we measured the plain seed-restart baseline and its natural equal-CPU alternative—doubling the per-board CPU time limit—across every board not clean in `full-1e935fc` (`173` of `1,182` boards; `1,009` clean at seed 1, `85.36%`, `30,332.6` CPU-s = `8.43` core-h).

- **Unconditional restarts (`K = 1..5` seeds) gain up to `+20` boards (`1,009 -> 1,029`, `87.06%`), at steep CPU cost:**
  - `K = 2` (add seed 2): **`1,020` clean (`+11`)**, `+15,079.3` CPU-s (`+4.19` core-h, **`1,370.8` CPU-s/win**). Across all 4 single-restart choices `{2, 3, 4, 5}`, wins range from **`+6` (seed 3) to `+17` (seed 5)**, mean **`+10.75`** (`+15,020.6` CPU-s, `1,397.3` CPU-s/win).
  - `K = 3` (seeds 1..3): **`1,021` clean (`+12`)**, sequential stop-on-clean `+29,120.0` CPU-s (`+8.09` core-h, `2,426.7` CPU-s/win); 24-permutation mean **`+15.83` wins** (`+28,979.8` CPU-s, `1,830.3` CPU-s/win).
  - `K = 4` (seeds 1..4): **`1,024` clean (`+15`)**, sequential `+43,043.7` CPU-s (`+11.96` core-h, `2,869.6` CPU-s/win); permutation mean **`+18.50` wins** (`+42,465.5` CPU-s, `2,295.4` CPU-s/win).
  - `K = 5` (seeds 1..5): **`1,029` clean (`+20`)**, sequential `+56,308.4` CPU-s (`+15.64` core-h, **`2,815.4` CPU-s/win**; parallel all-173 `+60,082.4` CPU-s = `+16.69` core-h); permutation mean `+55,701.8` CPU-s (`2,785.1` CPU-s/win).
- **Doubling the time limit (`fail173-s1-2x`) beats unconditional `K = 2` on both wins and CPU:**
  - Running seed 1 at `2×` each board's `full-1e935fc` budget (`240 s`, or `480 s` for the 2 boards already at `240 s`) wins **`+16` boards (`1,025` clean, `86.72%`)** in `26,762.9` total CPU-s on the `173` boards.
  - As an up-front budget choice (`1 × 2T` instead of `2 × 1T`), incremental CPU over `1×` is **`+10,980.0` CPU-s (`+3.05` core-h, `686.3` CPU-s/win)**—**`27.2%` less added CPU and `+5` more wins** (`+5.25` over permutation mean) than `K = 2`, because the `57` non-time-limited failures stop early in the same short time instead of rerunning from scratch. Even as a post-failure retry from scratch (`+26,762.9` CPU-s added, `1,672.7` CPU-s/win), `2×` timeout matches `K = 3` permutation-mean wins (`+16` vs `+15.83`) at `7.7%` lower CPU.
  - Time and seed diversity solve partly disjoint problems: `12` boards are won by both `s1-2x` and `s2..s5`, **`4` only by `s1-2x`** (pure time starvation), and **`8` only by `s2..s5`** (local-minimum traps where seed 1 stays stuck even at `2×` time, e.g. `roz_roz-power-board`, clean on 4/4 other seeds in `78.8–112.0` s yet failing at `239.0` s on `s1-2x`). Union: **`24` boards (`1,033` clean, `87.39%`)**.
- **`153` of `173` failed boards (`88.4%`) never come clean on any of seeds `1..5`:**
  - `100` (`65.4%`) are time-limited at seed 1 (`76` hard-deadline, `24` stage-only; `48` have `>25` open) and `53` (`34.6%`) are not time-limited (median `31.2` CPU-s; **`49 / 53 = 92.5%` leave the exact same open count on every seed `1..5`**).
  - Failing with `1–3` open (`n = 52`) predicts `75.0%` (`15 / 20`) of all `K = 5` wins (`28.8%` win rate vs `9.1%` on `4–10` open, `4.2%` on `11–25` open, and **`0.0%` on `>25` open**), **but `1` open + time-limited (`n = 8`) is where the signal concentrates (`50.0%` on `s2`, `62.5%` 1-restart mean, `100.0%` = `8/8` at `K = 5`)**, whereas `3` open won `0 / 10` on `s2..s5`.
- **Gated restart policies cut CPU per win by `4×`–`8×`; "restart only if not time-limited" is a trap:**
  - Restarting **only non-time-limited failures** (`57` boards) wins just **`+2` at `K = 2`** (`1,041.5` s/win) and **`+4` at `K = 5`** (`1,824.0` s/win), missing `80%` of restart wins because early-stopping failures are almost always deterministic geometric/rule walls.
  - **`open <= 1`** (`21` boards; `20` with `open == 1` and no new DRC errors): **`+6` wins (`+6.75` mean) at `K = 2`** for `+1,225.6` s (**`204.3` s/win**, `186.0` s/win mean) and **`+11` wins at `K = 5`** for `+3,066.5` s (`278.8` s/win).
  - **`open <= 3`** (`53` boards): **`+8` wins (`+8.25` mean) at `K = 2`** (`+2,987.8` s, **`373.5` s/win**) and **`+15` wins at `K = 5`** (`+9,600.5` s = `+2.67` core-h, **`640.0` s/win**). Narrowing to **`1 <= open <= 2` and time-limited** (`17` boards) wins **`+6` (`+6.25` mean) at `K = 2`** (`268.2` s/win) and **`+11` at `K = 5`** (`427.4` s/win static; `+10` at `306.3` s/win dynamic).
- **Seed variability on clean boards is confined to the `3.4%` deadline-touched tail:**
  - On an unbiased uniform sample of `40` seed-1-clean boards (`clean40-s2`), **`0 of 40` failed (`0.0%`)** at seed 2 (`40 / 40` clean; `0 / 40` identical copper).
  - On an exhaustive census of **all `34` deadline-touched clean boards** in `full-1e935fc` (`clean34-touched-s2`), **`3 of 34` failed (`8.82%`)** at seed 2 (`minima-hardware_FrontPanel` `1` open, `Protoboard-DIP-for-CHIP_chip-dip-protoboard` `1` open, `Own-Mailbox-Hardware_mailbox-before` `7` open). Only ~`2%–3%` of the `1,182`-board corpus sits on the seed-flip boundary.
- **Implication for RL / learned net order:** Plain restarts (`+20` at `K = 5`, `+15` gated on `open <= 3` for `+2.67` core-h) and `2×` time (`+16` for `+3.05` core-h) bound what ordering luck can buy. A learned **seed** policy can essentially **only save the `0.5–6.7` core-h of gated retry CPU** (and risks losing `3+` borderline clean boards if run without fallback). Even a fine-grained **net-order** policy cannot overcome the `~50` seed-invariant non-time-limited geometric/rule walls or the `~71` heavily congested (`>10` open) boards that fail `5` seeds and `2×` time, leaving at most a couple dozen borderline time-limited (`<=10` open) boards beyond gated restarts.

---

## 1. Provenance, Methodology, and Parity Check

### Build and execution setup

All routing and KiCad judging ran on `sequoia-server` (Ryzen 7 5800X, 9 booked cores via `reserve -m sequoia-server -c 9`) from detached worktree `/home/sequoia/ab/restarts-aae1126` at `main` (`aae1126899f89535c57b7f1b4040d4d11896e557`), built with isolated `CARGO_BUILD_BUILD_DIR=$PWD/target/build cargo build --release -p ultraroute -p kicad-text`:

- **Router binary:** `/home/sequoia/ab/restarts-aae1126/core/target/release/ultraroute` (inode `6567647`, sha256 `221e2c8c1efb5f545420c92e8b99f54f5e53c8fd9f4beea4e9ac8bdf2f09f3a5`)
- **Text shaper:** `/home/sequoia/ab/restarts-aae1126/core/target/release/kicad-text` (inode `6567599`, sha256 `a7a1bb1797ccccbbccab8ae5bfc3abb4ec2f9197c8526bd9802500cd395956ef`, matching `full-1e935fc`)
- **Command & referee:** Driven directly through [`pcbench.route_one`](file:///home/sequoia/Software/ultraroute/.claude/worktrees/explore-restarts/tools/pcbench.py#L261-L309) and [`pcbench.start`](file:///home/sequoia/Software/ultraroute/.claude/worktrees/explore-restarts/tools/pcbench.py#L558-L578) (`--strip-all --recipe default --seed N --uuid-seed N --threads 1 --timeout T --cpu-clock`, `systemd-run --user --scope -q -p MemoryMax=8G -p MemorySwapMax=0`, KiCad 9.0.8 zone refill + DRC + `dfm.py` acid-trap and neck checks, `judge = 3`).
- **Raw outputs on `sequoia-server`:** `/home/sequoia/ab/restarts-runs/pcbench/runs/{sanity3-s1,clean40-s2,clean34-touched-s2,fail173-s2,fail173-s3,fail173-s4,fail173-s5,fail173-s1-2x}`, manifest `/home/sequoia/ab/restarts-runs/manifest.json`, and recount JSON `/home/sequoia/ab/restarts-runs/analysis_summary.json` (`942` board-runs total, `0` router crashes/non-zero exits, `0` `referee_error`s, `50.2 GiB` free disk remaining).
- **Seed-1 baseline:** `/home/sequoia/Software/ultraroute-lab/pcbench/runs/full-1e935fc` (`1,182` boards, `1,009` with `success: true`, `173` with `success: false`). Parity run `sanity3-s1` (`Practicas-Curso-Kicad_Fusari_Diego`, `retroreflectors_RUMBAFLOCK`, `AVR-ISP_pogo-plug_1.27mm_AVR-ISP_pogo-plug_1.27mm`) matched `full-1e935fc` to `0.000000000` mm wirelength, identical vias, open counts, and DRC verdicts (`3/3`), confirming stroked text (`kicad-text`) and exact seed-1 equivalence.

### Per-board timeouts on `1×` and `2×`

[`tools/pcbench_timeouts.json`](file:///home/sequoia/Software/ultraroute/.claude/worktrees/explore-restarts/tools/pcbench_timeouts.json#L1-L36) gives `31` boards a `240 s` timeout instead of `120 s`. In `full-1e935fc`, `29` of those `31` were clean and **`2` failed** (`medusa_medusa_cape` and `roz_roz-power-board`).
- In `fail173-s2`..`fail173-s5`, those `2` boards received `240 s` and the other `171` boards received `120 s`, identical to `full-1e935fc`.
- Because `pcbench.py`'s `board_timeouts()` takes `max(240, --timeout)`, passing `--timeout 240` alone would leave those `2` boards at `1×` (`240 s`) instead of `2×` (`480 s`). In `fail173-s1-2x/run.json`, we set `480.0 s` for `medusa_medusa_cape` and `roz_roz-power-board` and `240.0 s` for the remaining `171` boards, and we report **both** true `2×` (`+16` wins) and flat `240 s` (`+15` wins, taking `full-1e935fc`'s `240 s` result on those `2` boards).

### What `--seed` changes in `ultraroute`

Inspecting the router source confirms [`Args::seed`](file:///home/sequoia/Software/ultraroute/.claude/worktrees/explore-restarts/core/crates/ultraroute/src/lib.rs#L120-L124) only permutes tie-broken connection and try orders via [`jitter::shuffle`](file:///home/sequoia/Software/ultraroute/.claude/worktrees/explore-restarts/core/crates/grid/src/jitter.rs#L13-L22):
1. **Negotiation** ([`negotiate.rs`](file:///home/sequoia/Software/ultraroute/.claude/worktrees/explore-restarts/core/crates/grid/src/negotiate.rs#L909-L912)): shuffles all open `Connection`s before a stable sort by `net_weight` (under `--recipe default` with no custom length weights, all nets have weight `1.0`, so the initial round-1 route order is a uniform Fisher-Yates permutation); repasses and `rescue_walled_off` offset `options.seed`.
2. **Single-net routing (`finishing`, `refinishing`, non-rip `corridors`)** ([`route.rs`](file:///home/sequoia/Software/ultraroute/.claude/worktrees/explore-restarts/core/crates/grid/src/route.rs#L235-L241)): sorts open airlines shortest-first, then shuffles within contiguous chunks of `8`.
3. **Shove plough** ([`shove.rs`](file:///home/sequoia/Software/ultraroute/.claude/worktrees/explore-restarts/core/crates/grid/src/shove.rs#L236-L240)): shuffles open non-plane connections before picking the next untried pad (`seed = a.seed * 1000 + pass + 1` plus `stats.tries.len()`).

`--seed` does **not** alter `prefanout` escape synthesis, differential-pair routing, grid resolution, DRC clearances, via pricing, or A* tie-breaking.

---

## 2. Whole-Corpus Clean Count for Best-of-`K` Seeds vs `2×` Time Limit

### Baseline (`K = 1`, `full-1e935fc`)

- **Clean:** `1,009 / 1,182` (`85.36%`); `1,010` routed (`free-of-charge_BMS` has `0` unconnected and `3` `solder_mask_bridge` errors).
- **CPU:** `30,332.6` CPU-s (`8.43` core-h) across all `1,182` boards—`14,549.7` CPU-s (`48.0%`) on the `1,009` clean boards and **`15,782.9` CPU-s (`52.0%`, `4.38` core-h)** on the `173` failed boards.

### Individual seeds on the `173` failed boards

| Run | Seed | Timeout | Won (`success`) | Routed (`open=0`, mask-only) | CPU on 173 (s) | Core-h | Time-limited (of 173) | Hard-deadline (of 173) |
|---|---:|---|---:|---:|---:|---:|---:|---:|
| `full-1e935fc` (failed subset) | 1 | `1×` (120/240 s) | 0 | 1 | 15,782.9 | 4.38 | 116 | 87 |
| `fail173-s2` | 2 | `1×` (120/240 s) | **11** | 12 | 15,079.3 | 4.19 | 111 | 80 |
| `fail173-s3` | 3 | `1×` (120/240 s) | **6** | 7 | 15,109.1 | 4.20 | 107 | 80 |
| `fail173-s4` | 4 | `1×` (120/240 s) | **9** | 10 | 15,134.7 | 4.20 | 111 | 85 |
| `fail173-s5` | 5 | `1×` (120/240 s) | **17** | 18 | 14,759.3 | 4.10 | 109 | 76 |
| `fail173-s1-2x` (true `2×`) | 1 | `2×` (240/480 s) | **16** | 17 | 26,762.9 | 7.43 | 98 | 74 |
| `fail173-s1-2x` (flat `240 s`) | 1 | `240 s` cap | **15** | 16 | 26,934.1 | 7.48 | 99 | 74 |

Single-restart wins vary nearly **3×** by seed (`+6` on seed 3 vs `+17` on seed 5; mean `+10.75`). In every run, `routed = clean + 1` because `free-of-charge_BMS` reaches `0` open with `solder_mask_bridge` errors (`3, 3, 3, 7, 11` on seeds `1..5`, `3` on `s1-2x`).

### Best-of-`K` seeds (`K = 1..5`) vs `2×` time limit

Because sequential restarts stop as soon as a board comes clean, CPU added is slightly lower than running all `K - 1` extra seeds in parallel on every failed board. Because individual seed yields vary (`+6`..`+17`), we report both the canonical seed sequence `1, 2, 3, 4, 5` and the exact mean/min/max across all `24` permutations of `{2, 3, 4, 5}`:

| Strategy | Corpus clean (`/ 1,182`) | Clean % | Gained over `s1` | Perm min..max (mean) gained | Added CPU seq (s / core-h) | Added CPU par (s / core-h) | Seq CPU / win (s) | Perm-mean seq CPU (s) & CPU / win (s) |
|---|---:|---:|---:|---|---:|---:|---:|---|
| **`K = 1`** (`s1` baseline) | **1,009** | 85.36% | +0 | — | +0.0 / +0.00 h | +0.0 / +0.00 h | — | — |
| **`K = 2`** (`s1..s2`) | **1,020** | 86.29% | **+11** | +6..+17 (**+10.75**) | +15,079.3 / +4.19 h | +15,079.3 / +4.19 h | **1,370.8** | +15,020.6 s (**1,397.3** s/win) |
| **`K = 3`** (`s1..s3`) | **1,021** | 86.38% | **+12** | +11..+19 (**+15.83**) | +29,120.0 / +8.09 h | +30,188.4 / +8.39 h | **2,426.7** | +28,979.8 s (**1,830.3** s/win) |
| **`K = 4`** (`s1..s4`) | **1,024** | 86.63% | **+15** | +15..+20 (**+18.50**) | +43,043.7 / +11.96 h | +45,323.1 / +12.59 h | **2,869.6** | +42,465.5 s (**2,295.4** s/win) |
| **`K = 5`** (`s1..s5`) | **1,029** | 87.06% | **+20** | +20..+20 (**+20.00**) | +56,308.4 / +15.64 h | +60,082.4 / +16.69 h | **2,815.4** | +55,701.8 s (**2,785.1** s/win) |
| **`s1` at `2×` limit (up-front budget vs `1×`)** | **1,025** | 86.72% | **+16** | — | **+10,980.0 / +3.05 h** | — | **686.3** | *(flat 240 s: +15 wins, +11,151.2 s, 743.4 s/win)* |
| **`s1` at `2×` limit (rerun from scratch after `1×` fails)** | **1,025** | 86.72% | **+16** | — | **+26,762.9 / +7.43 h** | — | **1,672.7** | *(flat 240 s: +15 wins, +26,934.1 s, 1,795.6 s/win)* |

*(Note: if an internal router loop stopped on `unconnected == 0` without running KiCad DRC, `free-of-charge_BMS` would stop after seed 1/2 instead of running seeds 3..5, trimming `seq_cpu_added` slightly to `29,038.8` s at `K=3`, `42,888.9` s at `K=4`, and `56,080.8` s at `K=5`, with identical clean counts.)*

### Head-to-head: `2×` time limit vs restarts at the same CPU

1. **Same nominal per-board budget (`1 × 2T` vs `2 × 1T`):** Budgeting `2×` time up-front on a single seed (`+10,980.0` incremental CPU-s, `+16` wins, `686.3` s/win) strictly dominates `K = 2` (`+15,079.3` CPU-s, `+11` canonical / `+10.75` mean wins, `1,370.8` / `1,397.3` s/win), winning **`+5` (`+5.25` mean) more boards with `27.2%` less extra CPU**. Two effects explain why:
   - The `57` non-time-limited failures finish before the `1×` ceiling (`0` wins at `2×`, near-zero extra CPU), whereas unconditional `K = 2` pays `+2,083.0` CPU-s rerunning all `57` from scratch.
   - Time-limited boards under `1 × 2T` do not pay twice for `prefanout` and initial `negotiation` setup; notably `medusa_medusa_cape` (`229.0` s at `240 s` limit, `prefanout` time-limited) finished clean in just **`101.6` CPU-s** once its `480 s` ceiling gave `prefanout` enough budget to finish cleanly before `negotiation` started.
2. **Post-failure retry from scratch (`+26,762.9` CPU-s):** Even if `1×` is run first and failed boards are re-routed from scratch at `2×`, `+26,762.9` CPU-s (`+16` wins, `1,672.7` s/win) beats `K = 3` canonical (`+12` wins for `+29,120.0` s) and matches `K = 3` permutation-mean (`+15.83` wins for `+28,979.8` s) for `2,216.9` fewer CPU-s.
3. **Why restarts still win boards that `2×` time cannot:**
   - **Won by both `s1-2x` and `s2..s5` (`12` boards):** `59pct_keyboard`, `Fox-1-MPPT_TL1451_MPPT_Flight_Rev1`, `Hardware_Playground_sd_wav_pcm5100`, `Keyboard_PCB_Keyboard`, `iso50_pcb`, `kitspace_USB-C-Screen-Adapter`, `kitspace_stack-light`, `mechkeys_58r-mxalps`, `mechkeys_MF68+10`, `medusa_medusa_cape`, `mightyduino_mightyduino`, `rjw57_cpu-board`.
   - **Won ONLY by `s1-2x` (`4` boards, `0/4` across `s2..s5`):** `Hardware_Playground_E3_Pcb_rev0.3` (`3` open -> `0` in `166.6` s), `headstage-adapter_headstage adapter` (`10` -> `0` in `173.0` s), `zx-sizif-128_sizif128` (`9` -> `0` in `217.5` s), `slushengine-modelx_slushengine_modelx` (`13` -> `0` in `236.3` s). Every seed simply runs out of `120 s`.
   - **Won ONLY by `s2..s5` (`8` boards, all fail on `s1-2x`):**
     - `4` time-limited local minima where seed 1's early copper locks in a jam that twice the time cannot untangle: `roz_roz-power-board` (`1` open at `s1` and `s1-2x` after `239.0` s, yet clean on **all 4** seeds `s2..s5` in `78.8–112.0` s), `Teensy-3.5-Breakout-Boaard_TeensyMegaIoShield` (clean on `s2, s3, s4` in `51.7–57.9` s; stuck at `1` open after `236.2` s on `s1-2x`), `rvi_v2x_hardware_V2X` (`4` open at `s1`; clean on `s2, s4, s5` in `83.8–98.5` s), `pcb_HDMI2RGB_HDMI2RGB` (`12` open at `s1`; clean on `s2, s5` in `119.5–121.8` s).
     - `4` non-time-limited local minima (`1–2` open, where `s1` stops with time to spare so `2×` timeout changes nothing): `Hardware_Playground_serial_gw_ATMEGA328P` (`1` open in `35.8` s at `s1`; clean in `8.6` s on `s2` and `5.8` s on `s5`), `USB-TypeC-breakout-board_USB-TypeC-breakout-board` (`2` open in `59.6` s at `s1`; clean in `9.6` s on `s5`), `kitspace_f-91w` (`1` open in `91.5` s at `s1`; clean in `61.1` s on `s4`), `phone_amp_phone_amp` (`1` open in `106.7` s at `s1`; clean in `76.8` s on `s5`).

---

## 3. Never-Clean Boards (`153 / 173`) and Open-Count Bucket Win Rates

### Does failing with `1–3` open predict a restart win? Win rates by seed-1 open bucket

| Seed-1 open bucket | Failed (`n`) | Won `s2` (`%`) | 1-restart mean (`%`) | Won `K = 5` (`%`) | Never clean (`s1..s5`) | Won `s1-2x` (`%`) |
|---|---:|---:|---:|---:|---:|---:|
| `0` open (DRC errors only) | 1 | 0 (`0.0%`) | 0.00 (`0.0%`) | 0 (`0.0%`) | 1 | 0 (`0.0%`) |
| `1` open | 20 | 6 (**`30.0%`**) | 6.75 (**`33.8%`**) | 11 (**`55.0%`**) | 9 | 6 (`30.0%`) |
| `2` open | 22 | 2 (`9.1%`) | 1.50 (`6.8%`) | 4 (`18.2%`) | 18 | 3 (`13.6%`) |
| `3` open | 10 | 0 (`0.0%`) | 0.00 (`0.0%`) | **0 (`0.0%`)** | 10 | 1 (`10.0%`) |
| **`1–3` open (combined)** | **52** | **8 (`15.4%`)** | **8.25 (`15.9%`)** | **15 (`28.8%`)** | **37** | **10 (`19.2%`)** |
| `4–5` open | 22 | 1 (`4.5%`) | 1.50 (`6.8%`) | 3 (`13.6%`) | 19 | 2 (`9.1%`) |
| `6–10` open | 22 | 1 (`4.5%`) | 0.50 (`2.3%`) | 1 (`4.5%`) | 21 | 3 (`13.6%`) |
| **`4–10` open (combined)** | **44** | **2 (`4.5%`)** | **2.00 (`4.5%`)** | **4 (`9.1%`)** | **40** | **5 (`11.4%`)** |
| `11–25` open | 24 | 1 (`4.2%`) | 0.50 (`2.1%`) | 1 (`4.2%`) | 23 | 1 (`4.2%`) |
| `>25` open | 52 | 0 (`0.0%`) | 0.00 (`0.0%`) | **0 (`0.0%`)** | 52 | 0 (`0.0%`) |
| **All failed boards** | **173** | **11 (`6.4%`)** | **10.75 (`6.2%`)** | **20 (`11.6%`)** | **153** | **16 (`9.2%`)** |

Splitting each open-count bucket by whether seed 1 was **time-limited (`n = 116`)** vs **not time-limited (`n = 57`)** exposes a sharp interaction that any restart rule should use:

| Seed-1 open bucket | Time-limited `n` | TL 1-restart mean (`%`) | TL `K = 5` won (`%`) | Not time-limited `n` | Not-TL 1-restart mean (`%`) | Not-TL `K = 5` won (`%`) |
|---|---:|---:|---:|---:|---:|---:|
| `0` open (DRC errors) | 0 | — | — | 1 | 0.00 (`0.0%`) | 0 (`0.0%`) |
| `1` open | **8** | **5.00 (`62.5%`)** | **8 (`100.0%`)** | 12 | 1.75 (`14.6%`) | 3 (`25.0%`) |
| `2` open | 9 | 1.25 (`13.9%`) | 3 (`33.3%`) | 13 | 0.25 (`1.9%`) | 1 (`7.7%`) |
| `3` open | 2 | 0.00 (`0.0%`) | 0 (`0.0%`) | 8 | 0.00 (`0.0%`) | 0 (`0.0%`) |
| **`1–3` open (combined)** | **19** | **6.25 (`32.9%`)** | **11 (`57.9%`)** | **33** | **2.00 (`6.1%`)** | **4 (`12.1%`)** |
| `4–10` open | 29 | 2.00 (`6.9%`) | 4 (`13.8%`) | 15 | 0.00 (`0.0%`) | **0 (`0.0%`)** |
| `11–25` open | 20 | 0.50 (`2.5%`) | 1 (`5.0%`) | 4 | 0.00 (`0.0%`) | **0 (`0.0%`)** |
| `>25` open | 48 | 0.00 (`0.0%`) | 0 (`0.0%`) | 4 | 0.00 (`0.0%`) | **0 (`0.0%`)** |
| **Total** | **116** | **8.75 (`7.5%`)** | **16 (`13.8%`)** | **57** | **2.00 (`3.5%`)** | **4 (`7.0%`)** |

**Answer on `1–3` open:**
- `1–3` open (`52` boards, `30.1%` of failures) captures **`15` of all `20` restart wins (`75.0%`)** (`19` of `20` = `95.0%` lie at `<= 10` open; the sole outlier above `10` open is `pcb_HDMI2RGB_HDMI2RGB` at `12` open, time-limited).
- Inside `1–3` open, however, **`1` open (`55.0%` at `K=5`) and `time_limited` (`57.9%` on `1–3` open, `100.0%` = `8/8` on `1` open) do nearly all the work**. At `3` open, `0 / 10` boards came clean on `s2..s5`. And when a run is **not** time-limited with `>= 3` open (`31` boards), the restart win rate across all 4 extra seeds is **`0 / 31` (`0.0%`)**.

### Anatomy of the `153` boards never clean on any seed (`s1..s5`)

| Dimension | Never clean (`153` boards) | Won on `s2..s5` (`20` boards) |
|---|---|---|
| **Seed-1 time-limited** | **`100`** (`65.4%`): `76` hard-deadline, `24` stage-only (`92` TL on all 5 seeds, `8` on 1–4 seeds) | **`16`** (`80.0%`): `11` hard-deadline, `5` stage-only |
| **Seed-1 not time-limited** | **`53`** (`34.6%`; `51` never TL on any seed, median `31.2` CPU-s) | **`4`** (`20.0%`, all `1–2` open) |
| **Exact same `open` on all 5 seeds** | **`60 / 153` (`39.2%`)**: **`49 / 53` (`92.5%`) of not-TL** vs `11 / 100` (`11.0%`) of TL | `0 / 20` (`0.0%`) |
| **Seed-1 open count** | `0` (DRC): `1`; `1`: `9` (all `9` not-TL!); `2`: `18` (`6` TL, `12` not-TL); `3`: `10` (`2` TL, `8` not-TL); `4–10`: `40` (`25` TL, `15` not-TL); `11–25`: `23` (`19` TL, `4` not-TL); `>25`: `52` (`48` TL, `4` not-TL) | `1`: `11` (`8` TL, `3` not-TL); `2`: `4` (`3` TL, `1` not-TL); `3`: `0`; `4–5`: `3` (TL); `7`: `1` (TL); `12`: `1` (TL) |
| **Board size (pads)** | `<100` pads: `13` (`8.5%`); `100–399` pads: `63` (`41.2%`); `>=400` pads: `77` (`50.3%`); median `412` pads (IQR `223–679`) | `<100` pads: `1` (`5.0%`); `100–399` pads: `8` (`40.0%`); `>=400` pads: `11` (`55.0%`); median `471.5` pads |
| **Copper layers** | 2-layer: `116` (`75.8%`); 4+-layer: `37` (`24.2%`) | 2-layer: `16` (`80.0%`); 4+-layer: `4` (`20.0%`) |
| **Solved elsewhere** | `s1-2x` cleans **`4`** (`2.6%`); FreeRouting cleans **`38`** (`24.8%`) | `s1-2x` cleans **`12`** (`60.0%`); FreeRouting cleans **`5`** (`25.0%`) |

Two facts stand out among the `153` never-clean boards:
1. **`49` of the `53` non-time-limited never-clean boards (`92.5%`) produce the exact same `unconnected` count on all 5 seeds** (`min_open == max_open`), and only `1` (`free-of-charge_BMS`) ever reaches `0` unconnected on any seed (failing on `solder_mask_bridge` every time). Those `53` boards stop early because of hard geometric/DRC/escape constraints (local pad/mask clearances, boxed terminals, net-ties, inner cutouts), not net-ordering luck.
2. **Board size does not separate restartable failures from never-clean failures** (median `471.5` pads on won boards vs `412` pads on never-clean boards); **open count and deadline status** do (`68 / 100` time-limited never-clean boards have `>10` open, including `48` with `>25` open up to `499` on `Aleste-520EX_aleste`). All `153` never-clean board IDs are listed by split in Appendix B.

---

## 4. Simulated Restart Policies (`K = 2..5`)

In all policy simulations below, seed 1 runs on the whole corpus as in `full-1e935fc`; when a failed board satisfies the trigger on seed 1, additional seeds are run sequentially, stopping as soon as the board routes clean ("static trigger"). We also show the 24-permutation mean over `{2, 3, 4, 5}` and, in brackets for `K = 5`, a "dynamic re-check" variant that aborts further restarts on a board as soon as an intermediate restart violates the trigger (e.g., `open` climbs above `N`). Shorter per-attempt timeouts (`< 1×`) were **not** simulated because `ultraroute` scales stage deadlines (`prefanout`, `negotiation`, `rip-up`, `shove`) proportionally to `--timeout`, so `120 s` logs cannot support counterfactual `60 s` stage cutoffs.

| Policy (trigger on seed-1 failure) | Triggered (`/ 173`) | `K = 2`: Clean gained (`perm mean`) | `K = 2`: CPU added (`perm mean`) | `K = 2`: CPU / win (`perm mean`) | `K = 3`: Clean gained (`perm mean`) | `K = 3`: CPU / win (`perm mean`) | `K = 5` (up to 4 retries): Clean gained | `K = 5`: CPU added (`core-h`) | `K = 5`: CPU / win (`perm mean` / `dynamic`) |
|---|---:|---|---|---|---:|---|---:|---:|---|
| **All failed boards** (unconditional) | 173 | **+11** (+10.75) | +15,079.3 s (+15,020.6) | 1,370.8 s (1,397.3) | +12 (+15.83) | 2,426.7 s (1,830.3) | **+20** | +56,308.4 s (15.64 h) | **2,815.4 s** (2,785.1 / 2,815.4) |
| **`open <= 1`** *(required, `N=1`)* | 21 | **+6** (+6.75) | +1,225.6 s (+1,255.7) | **204.3 s** (186.0) | +7 (+9.17) | 283.1 s (205.2) | **+11** | +3,066.5 s (0.85 h) | **278.8 s** (241.3 / +8 @ 279.4) |
| **`open <= 3`** *(required, `N=3`)* | 53 | **+8** (+8.25) | +2,987.8 s (+3,058.7) | **373.5 s** (370.8) | +9 (+11.83) | 600.2 s (451.3) | **+15** | +9,600.5 s (2.67 h) | **640.0 s** (609.1 / +14 @ 611.9) |
| **`open <= 10`** *(required, `N=10`)* | 97 | **+10** (+10.25) | +6,882.0 s (+6,888.8) | **688.2 s** (672.1) | +11 (+15.00) | 1,172.1 s (851.8) | **+19** | +24,137.6 s (6.70 h) | **1,270.4 s** (1,234.5 / +18 @ 1,246.1) |
| **Not time-limited (`time_limited == []`)** *(required)* | 57 | **+2** (+2.00) | +2,083.0 s (+1,960.3) | **1,041.5 s** (980.2) | +2 (+2.83) | 1,954.8 s (1,328.6) | **+4** | +7,296.1 s (2.03 h) | **1,824.0 s** (1,814.0 / +3 @ 2,272.2) |
| *Not hard-deadline (`hard_deadline == false`)* | 86 | +4 (+5.25) | +5,099.3 s (+4,898.1) | 1,274.8 s (933.0) | +5 (+7.17) | 1,937.0 s (1,300.4) | +9 | +18,183.1 s (5.05 h) | 2,020.3 s (1,979.5 / +9 @ 1,863.5) |
| *Time-limited (`time_limited != []`)* | 116 | +9 (+8.75) | +12,996.3 s (+13,060.3) | 1,444.0 s (1,492.6) | +10 (+13.00) | 2,521.0 s (1,939.6) | +16 | +49,012.3 s (13.61 h) | 3,063.3 s (3,027.9 / +16 @ 2,976.3) |
| *`open == 1` and no new DRC errors* | 20 | **+6** (+6.75) | +1,176.2 s (+1,186.4) | **196.0 s** (175.8) | +7 (+9.17) | 264.5 s (190.1) | **+11** | +2,789.5 s (0.77 h) | **253.6 s** (216.1 / +8 @ 244.8) |
| *`1 <= open <= 3` and no new DRC errors* | 51 | **+8** (+8.25) | +2,822.2 s (+2,896.4) | **352.8 s** (351.1) | +9 (+11.83) | 563.1 s (423.8) | **+15** | +8,951.0 s (2.49 h) | **596.7 s** (565.8 / +14 @ 565.5) |
| *`open == 1` and time-limited* | 8 | **+4** (+5.00) | +746.3 s (+762.8) | **186.6 s** (152.6) | +5 (+6.83) | 233.3 s (152.4) | **+8** | +1,640.3 s (0.46 h) | **205.0 s** (150.6 / +6 @ 166.9) |
| *`1 <= open <= 2` and time-limited* | 17 | **+6** (+6.25) | +1,609.0 s (+1,697.7) | **268.2 s** (271.6) | +7 (+9.00) | 392.6 s (308.2) | **+11** | +4,701.6 s (1.31 h) | **427.4 s** (388.9 / +10 @ 306.3) |
| *`open <= 3` and time-limited* | 19 | **+6** (+6.25) | +1,838.7 s (+1,928.0) | **306.5 s** (308.5) | +7 (+9.00) | 458.8 s (359.4) | **+11** | +5,622.7 s (1.56 h) | **511.2 s** (472.6 / +10 @ 414.6) |
| *`open <= 3` and not time-limited* | 34 | +2 (+2.00) | +1,149.1 s (+1,130.8) | 574.5 s (565.4) | +2 (+2.83) | 1,095.4 s (743.1) | +4 | +3,977.8 s (1.10 h) | 994.4 s (984.4 / +3 @ 1,261.9) |

*(Full `K = 4` entries for every policy are in `analysis_summary.json`: `open <= 1` `+9` / `+10.25` mean for `+2,629.4` s = `292.2` s/win; `open <= 3` `+11` / `+13.75` mean for `+7,641.3` s = `694.7` s/win; `open <= 10` `+14` / `+17.50` mean for `+18,787.7` s = `1,342.0` s/win; `not time-limited` `+3` / `+3.50` mean for `+5,797.6` s = `1,932.5` s/win.)*

Key policy takeaways:
- **`open <= 1` is the highest-efficiency single filter** (`196–204` CPU-s/win at `K = 2`, adding just `+4.0%` to baseline corpus CPU for `+6` to `+6.75` wins; `+11` wins for `+0.85` core-h at `K = 5`).
- **`open <= 3` captures `75%` (`15 / 20`) of all `K = 5` wins for `17.1%` (`9,600.5 / 56,308.4` s) of the unconditional restart CPU** (`640.0` s/win, or `596.7` s/win excluding `0`-open / DRC-error boards).
- **`open <= 10` captures `95%` (`19 / 20`) of all `K = 5` wins** (`+24,137.6` s = `+6.70` core-h, `1,270.4` s/win), at more than double the marginal cost per win of `open <= 3` (`+4` extra wins for `+14,537.1` extra CPU-s = `3,634.3` s per marginal win).
- **"Restart only if not time-limited" fails badly:** despite cheap individual retries (~`36.5` s each), `53 / 57` never come clean across 5 seeds, driving cost to `1,041.5` s/win at `K = 2` and `1,824.0` s/win at `K = 5` for just `+2` to `+4` wins.

---

## 5. Seed Variability on Clean Boards (`clean40-s2` and `clean34-touched-s2`)

To measure the downside of changing seeds without keeping seed 1—and how much of the clean corpus sits near the failure edge—we ran seed 2 on two clean subsets from `full-1e935fc`:

1. **`clean40-s2` (primary unbiased sample, `40` boards drawn uniformly via `random.Random(1)` from the `1,009` clean boards):**
   - **Failed on seed 2: `0 of 40` (`0.0%`; `40 / 40` clean).**
   - Total CPU: `709.7` s on seed 1 vs `432.7` s on seed 2 (`2` boards in the sample had touched a stage deadline on seed 1—`OpenHardwareExG_Shield_OpenHardwareExG_Shield` and `kitspace_T32_ref`—and `0` touched a deadline on seed 2).
   - Route perturbation across the `40` clean boards: **`0 / 40` had identical vias and wirelength** (every board routed differently under seed 2), with aggregate vias `1,520 -> 1,484` (`-2.4%`), wirelength `45,002.7 -> 44,753.9` mm (`-0.6%`), and necks `4 -> 3`.

2. **`clean34-touched-s2` (exhaustive census of all `34` seed-1-clean boards in `full-1e935fc` where `time_limited` was non-empty or `hard_deadline` fired—the `3.37%` borderline clean tail):**
   - **Failed on seed 2: `3 of 34` (`8.82%`; `31 / 34` clean)**; total CPU `2,654.5` s (seed 1) -> `2,899.4` s (seed 2):
     - `minima-hardware_FrontPanel` (`240 s` board timeout, 2-layer, `185` pads): clean in `122.4` s (`shove` time-limited) on seed 1 -> **FAIL (`1` open, `{}` new errors, `218.6` s, `shove` time-limited)** on seed 2.
     - `Protoboard-DIP-for-CHIP_chip-dip-protoboard` (`120 s` limit, 2-layer, `274` pads): clean in `107.1` s (`shove` time-limited) on seed 1 -> **FAIL (`1` open, `{}` new errors, `107.2` s, `shove` time-limited)** on seed 2.
     - `Own-Mailbox-Hardware_mailbox-before` (`240 s` board timeout, 4-layer, `717` pads): clean in `102.6` s (`prefanout, pairs` time-limited) on seed 1 -> **FAIL (`7` open, `{}` new errors, `226.5` s, `prefanout, pairs, shove` time-limited)** on seed 2.
   - On the `31 / 34` boards clean on both seeds: vias `2,675 -> 2,664`, wirelength `86,103.3 -> 85,705.5` mm, necks `0 -> 2`, `0 / 31` identical copper.

**How much of the corpus lives near the seed edge:** `975 / 1,009` (`96.6%`) of clean boards finish without touching any deadline (`38 / 38` clean in `clean40-s2`), while the `34` deadline-touched clean boards flip to failure at `8.82%` (`3 / 34`) on seed 2. Combined with the `20` failed boards that come clean on `s2..s5` (`10.75` on a single seed on average; note `29` formerly borderline boards in `pcbench_timeouts.json` already have `240 s` headroom), **only ~`23–31` boards out of `1,182` (~`2.0%–2.6%`, estimate combining `20` measured `fail->clean` boards + `3` measured deadline-touched `clean->fail` flips + `0/38` untouched `clean->fail` sample) sit on the seed-luck boundary**. Replacing seed 1 with another single seed without a fallback loses `3+` clean boards directly from the deadline-touched tail.

---

## 6. Route Quality on Boards Won

Every board won by restarts (`20 / 20`) or by `2×` timeout (`16 / 16`) had a valid `code = 0` output in `full-1e935fc`. Note an inherent asymmetry when comparing against seed 1: **seed 1's board is incomplete** (missing `1` to `13` unrouted connections whose track length and vias were never laid), whereas the winning run is `100%` routed and DRC-clean.

| Comparison set | Boards (`n`) | Vias (`s1` partial -> win) | Median via delta / ratio | Wirelength mm (`s1` partial -> win) | Median WL ratio | Necks (`s1` -> win) | Acid traps: acute / slots (`s1` -> win) |
|---|---:|---|---:|---|---:|---|---|
| **`s2` wins** | 11 | `1,234 -> 1,216` (**`-18`**, `6` fewer / `5` more) | `-3.0` / `0.9651×` | `42,772.8 -> 44,843.6` (`+2,070.7`) | `1.0253×` | **`0 -> 0`** | `54 -> 42` / `406 -> 397` |
| **`K = 5` first-clean seed** | 20 | `2,485 -> 2,472` (**`-13`**, `8` fewer / `1` eq / `11` more) | `+2.0` / `1.0326×` | `71,570.1 -> 74,216.7` (`+2,646.6`) | `1.0261×` | **`1 -> 1`** (`1` lost, `1` gained) | `101 -> 90` / `790 -> 770` |
| **`K = 5` min-via clean seed** | 20 | `2,485 -> 2,383` (**`-102`**, `-4.1%`) | `+1.0` / `1.0127×` | `71,570.1 -> 74,319.9` (`+2,749.8`) | `1.0299×` | **`1 -> 1`** | `101 -> 88` / `790 -> 776` |
| **`s1-2x` wins** | 16 | `2,375 -> 2,525` (`+150`, `6` fewer / `1` eq / `9` more) | `+1.5` / `1.0129×` | `78,406.0 -> 82,529.3` (`+4,123.2`) | `1.0293×` | **`3 -> 2`** (`3` lost, `2` gained) | `100 -> 113` / `803 -> 801` |
| **12 boards clean in BOTH `K=5` first-clean & `s1-2x`** | 12 | `s1` partial `1,680` -> `K=5` **`1,652`** vs `2×` **`1,646`** | — | `s1` `58,621.1` -> `K=5` `60,729.7` vs `2×` `60,009.6` | — | `s1` `1` -> `K=5` **`1`** vs `2×` **`2`** | `K=5` `59 / 520` vs `2×` `77 / 553` |

Even while completing all missing nets, **restart wins slightly *reduce* aggregate vias (`2,485 -> 2,472`, `-13`) and acid traps (`101 -> 90` acute, `790 -> 770` slots)** because a cleaner negotiation order avoids the frantic late-stage rip-up/shove detours that seed 1 accumulated before failing (e.g. `rjw57_cpu-board` `388 -> 347` vias, `mechkeys_58r-mxalps` `131 -> 105` vias and `-234.4` mm wirelength, `Fox-1-MPPT` `342 -> 320` vias). Wirelength rises by a median of only `+2.61%` (`+54.1` mm) to close the unrouted airwires, and necks stay flat (`1 -> 1`: `kitspace_USB-C-Screen-Adapter` `1 -> 0`, `rjw57_cpu-board` `0 -> 1`). Per-board numbers for all `24` won boards appear in Appendix A.

---

## 7. Upper Bound for a Learned Net-Order or Seed Policy, and Restart Recommendation

### What this implies as an upper bound for a learned net-order or seed policy

1. **A learned *seed* policy (choosing one `--seed` up front) has almost no clean-count upside beyond restarts—it is strictly a CPU-saving tool:**
   - Across `5` independent seeds on all `173` failed boards, total clean boards rise by **`+20`** (`1,009 -> 1,029`), with permutation-averaged marginal gains already decaying sharply across `K = 2..5`: **`+10.75`, `+5.08`, `+2.67`, `+1.50`**.
   - Simple gated restarts already capture **`+11` wins for `+0.85` core-h** (`open <= 1`), **`+15` wins for `+2.67` core-h** (`open <= 3`), or **`+19` wins for `+6.70` core-h** (`open <= 10`). An oracle seed picker that guessed the winning seed among `{1..5}` on the first try would gain **`0` boards beyond `K = 5`** (`+5` beyond `open <= 3` `K = 5`, `+1` beyond `open <= 10` `K = 5`) and **only save those `0.85–6.70` core-h of gated retry CPU** (or `15.64` core-h vs unconditional `K = 5`).
   - Worse, if a learned seed picker replaces seed 1 in a single-shot run (no retry fallback) and is anything less than an oracle, it risks losing boards from the `34`-board deadline-touched clean tail (`3 / 34 = 8.82%` failed on seed 2).

2. **What a finer-grained learned *connection-order* policy could—and could not—gain beyond restarts:**
   - Unlike `--seed` (which draws uniform random permutations), a per-connection policy could order specific nets intentionally in negotiation round 1, finishing chunks, or shove. Even so, our data wall off most of the `153` never-clean boards from net-order fixes:
     - **Blocked by geometry, rules, or seed-independent `prefanout` (`~50+` boards):** Among the `53` non-time-limited never-clean boards, **`49` (`92.5%`) finish with the exact same open count across all 5 seeds** (`min_open == max_open`, median `31.2` s), after `rip-up`, `shove`, `refinishing`, and `corridors` have all tried to clear the remaining `1..37` connections with time to spare (`1` more, `free-of-charge_BMS`, routes `0` open with DRC mask bridges on all 5 seeds). Reordering round 1 cannot route past a pad/mask clearance wall, an unescaped pin from `prefanout`, or a missing net-tie/cutout rule.
     - **Blocked by heavy congestion / throughput (`~70` boards):** Among the `100` time-limited never-clean boards, `67` fail with `>10` open (`48` with `>25` open), and `66` of those `67` still fail even at `2×` CPU (`s1-2x`, which rescued only `slushengine-modelx` at `13` open). In PathFinder negotiation, round-1 ordering fades quickly as shared history costs accumulate across tens of rounds; boards leaving `30–499` open connections at `120–240 s` are bound by grid cell/A* throughput, escape congestion, or via/history pricing, not initial net sequence.
   - **Realistic ceiling beyond `K = 5` restarts:** That leaves primarily the **`33` time-limited never-clean boards with `1–10` open** (`8` with `1–3` open, `25` with `4–10` open; `3` of which already come clean by simply doubling `--timeout`) plus at most the `4` seed-varying non-time-limited never-clean boards—**at most ~`30–37` candidate boards where faster ordering convergence could plausibly help at all**, and likely a small fraction of those in practice since `5` independent random orders already missed all `37`. Fixing deterministic non-time-limited failure classes (`53` boards) and router speed/stage budgets (`+16` clean from `2×` time alone at `+3.05` core-h) targets a much larger and more reliable prize than learning net order.

### Recommendation: should `ultraroute` add a restart feature, and with what rule?

1. **Do NOT turn on unconditional restarts, and keep single-seed (`0` restarts) as the default for `pcbench` / development `ab` runs:**
   - Unconditional restarts (`1,370.8–2,815.4` CPU-s/win) lose decisively to simply giving the router a `2×` timeout (`686.3` CPU-s/win incremental, `+16` wins vs `+11` at `K = 2`).
   - In benchmark and `ab` runs, automatic multi-seed retries would mask deterministic/performance regressions on borderline boards, inflate run noise, and hide fixable `1`-open bugs behind `2–4×` extra CPU.

2. **DO offer an opt-in, tightly gated in-process near-miss restart (`--restarts K`, off by default in CLI/bench, optional in Studio / end-user batch routing) on the already-loaded `Board`:**
   - Why it has genuine end-user value: `8` boards (`roz_roz-power-board`, `TeensyMegaIoShield`, `rvi_v2x_hardware_V2X`, `pcb_HDMI2RGB`, `serial_gw_ATMEGA328P`, `USB-TypeC-breakout-board`, `kitspace_f-91w`, `phone_amp`) are trapped in seed-1 local minima that **even `2×` time (`240–480 s`) fails to solve**, whereas a fresh seed cleans them—often faster than seed 1's failed attempt (`8.6` s vs `35.8` s on `serial_gw_ATMEGA328P`; `53.2` s vs `118.0` s on `TeensyMegaIoShield`; `78.8` s vs `229.0` s on `medusa_medusa_cape`). Running in-process in `ultraroute` also skips reloading the board and re-stroking text via `kicad-text`, and can keep the minimum-open (or minimum-via clean) result if no attempt finishes at `0` open.
   - **Recommended rule (if added later):**
     - **High-efficiency rule (recommended default for `--restarts`):** Retry only when **`1 <= open <= 2` and `time_limited` is non-empty**, up to **`2` extra seeds (`K = 3`)** (or `4` extra seeds `K = 5` when user time allows), **dynamically aborting further retries if an attempt finishes with `open == 0`, `open > 2`, or `!time_limited`**.
       - *Measured performance on PCBench:* Triggers on just **`17` of `1,182` boards (`1.4%`)**; `1` retry (`K = 2`) gains **`+6` (`+6.25` perm-mean) clean boards** for **`+1,609.0` CPU-s (`+0.45` core-h = `+5.3%` corpus CPU, `268.2` CPU-s/win)**; `2` retries (`K = 3`) gain **`+7` (`+9.00` perm-mean)** for `+2,206.4` dynamic / `+2,774.0` perm-mean CPU-s (`308–315` s/win); `4` retries (`K = 5`) gain **`+10` dynamic / `+11` static** for `+3,062.7` / `+4,701.6` CPU-s (`306.3` / `427.4` s/win). Note `open == 3` won `0 / 10` across all seeds, so capping at `open <= 2` drops `0` wins while saving `16%–26%` of retry CPU vs `open <= 3`.
     - **Broader near-miss variant (if rescuing non-time-limited `1–2`-open jams is also desired):** Gate on **`1 <= open <= 2`** regardless of `time_limited` (`42` boards; captures all `15` wins in `1–3` open—`11` time-limited + `4` non-time-limited—at `~500` CPU-s/win), never restarting `open == 0` or `open >= 3`.

---

## Appendix A: All `24` Failed Boards Won by Restarts (`s2..s5`) or `2×` Time Limit (`s1-2x`)

| Board ID | `s1` open | `s1` TL? | `s1` CPU (s) | Winning seeds (`s2..s5`) | `s1-2x` clean? (`CPU s`) | `s1` vias -> 1st clean seed (`seed`) / `2×` vias | `s1` WL -> 1st clean WL / `2×` WL (mm) | Necks (`s1` / seed / `2×`) |
|---|---:|---|---:|---|---|---|---|---|
| `59pct_keyboard` | 2 | yes | 118.2 | `s2, s5` | yes (175.9) | 86 -> 83 (`s2`) / 86 | 6689.1 -> 6986.4 / 7044.0 | 0 / 0 / 0 |
| `Fox-1-MPPT_TL1451_MPPT_Flight_Rev1` | 5 | yes | 118.9 | `s4, s5` | yes (215.3) | 342 -> 320 (`s4`) / 300 | 4214.2 -> 4286.3 / 4336.8 | 0 / 0 / 1 |
| `Hardware_Playground_sd_wav_pcm5100` | 1 | yes | 110.1 | `s2, s3, s5` | yes (110.5) | 53 -> 45 (`s2`) / 52 | 1099.0 -> 1126.8 / 1102.9 | 0 / 0 / 0 |
| `Hardware_Playground_serial_gw_ATMEGA328P` | 1 | no | 35.8 | `s2, s5` | no (35.8) | 38 -> 43 (`s2`) / — | 600.3 -> 616.4 / — | 0 / 0 / — |
| `Keyboard_PCB_Keyboard` | 7 | yes | 118.4 | `s2, s5` | yes (236.2) | 88 -> 114 (`s2`) / 122 | 9380.2 -> 10774.7 / 10751.4 | 0 / 0 / 0 |
| `Teensy-3.5-Breakout-Boaard_TeensyMegaIoShield` | 1 | yes | 118.0 | `s2, s3, s4` | no (236.2) | 97 -> 105 (`s2`) / — | 3203.7 -> 3263.5 / — | 0 / 0 / — |
| `USB-TypeC-breakout-board_USB-TypeC-breakout-board` | 2 | no | 59.6 | `s5` | no (59.5) | 10 -> 12 (`s5`) / — | 498.0 -> 546.4 / — | 0 / 0 / — |
| `iso50_pcb` | 2 | yes | 118.1 | `s2, s3` | yes (144.5) | 68 -> 71 (`s2`) / 76 | 4933.2 -> 5055.7 / 4962.2 | 0 / 0 / 1 |
| `kitspace_USB-C-Screen-Adapter` | 1 | yes | 113.5 | `s4, s5` | yes (114.0) | 95 -> 97 (`s4`) / 96 | 919.9 -> 924.1 / 947.1 | 1 / 0 / 0 |
| `kitspace_f-91w` | 1 | no | 91.5 | `s4` | no (91.5) | 84 -> 88 (`s4`) / — | 622.2 -> 688.8 / — | 0 / 0 / — |
| `kitspace_stack-light` | 1 | yes | 115.2 | `s3, s4, s5` | yes (75.9) | 69 -> 84 (`s3`) / 81 | 1541.5 -> 1512.6 / 1542.0 | 0 / 0 / 0 |
| `mechkeys_58r-mxalps` | 1 | yes | 118.4 | `s2, s4, s5` | yes (135.1) | 131 -> 105 (`s2`) / 111 | 6661.1 -> 6426.7 / 6429.6 | 0 / 0 / 0 |
| `mechkeys_MF68+10` | 2 | yes | 118.5 | `s5` | yes (123.8) | 172 -> 206 (`s5`) / 155 | 11889.0 -> 12237.3 / 11734.9 | 0 / 0 / 0 |
| `medusa_medusa_cape` *(240->480 s)* | 1 | yes | 229.0 | `s2, s3, s4, s5` | yes (101.6) | 131 -> 123 (`s2`) / 133 | 2624.9 -> 2686.1 / 2763.1 | 0 / 0 / 0 |
| `mightyduino_mightyduino` | 1 | yes | 117.8 | `s5` | yes (214.9) | 57 -> 57 (`s5`) / 54 | 848.1 -> 876.5 / 850.0 | 0 / 0 / 0 |
| `pcb_HDMI2RGB_HDMI2RGB` | 12 | yes | 124.6 | `s2, s5` | no (222.8) | 148 -> 135 (`s2`) / — | 2525.5 -> 2653.7 / — | 0 / 0 / — |
| `phone_amp_phone_amp` | 1 | no | 106.7 | `s5` | no (106.8) | 34 -> 45 (`s5`) / — | 443.7 -> 464.7 / — | 0 / 0 / — |
| `rjw57_cpu-board` | 4 | yes | 118.4 | `s5` | yes (235.8) | 388 -> 347 (`s5`) / 380 | 7820.9 -> 7836.6 / 7545.6 | 0 / 1 / 0 |
| `roz_roz-power-board` *(240->480 s)* | 1 | yes | 133.8 | `s2, s3, s4, s5` | no (239.0) | 67 -> 83 (`s2`) / — | 1414.3 -> 1430.8 / — | 0 / 0 / — |
| `rvi_v2x_hardware_V2X` | 4 | yes | 118.5 | `s2, s4, s5` | no (236.9) | 327 -> 309 (`s2`) / — | 3641.6 -> 3822.8 / — | 0 / 0 / — |
| `Hardware_Playground_E3_Pcb_rev0.3` | 3 | yes | 118.2 | *(none)* | **yes** (166.6) | 162 -> — / 187 | 2980.2 -> — / 3310.8 | 0 / — / 0 |
| `headstage-adapter_headstage adapter` | 10 | yes | 116.2 | *(none)* | **yes** (173.0) | 95 -> — / 123 | 1635.1 -> — / 1905.4 | 1 / — / 0 |
| `slushengine-modelx_slushengine_modelx` | 13 | yes | 118.6 | *(none)* | **yes** (236.3) | 305 -> — / 368 | 8488.3 -> — / 9899.6 | 1 / — / 0 |
| `zx-sizif-128_sizif128` | 9 | yes | 118.2 | *(none)* | **yes** (217.5) | 133 -> — / 201 | 6681.4 -> — / 7403.8 | 0 / — / 0 |

---

## Appendix B: Complete List of the `153` Boards Never Clean on Seeds `1..5`

Each entry lists `` `board_id` (s1_open, pads) ``.

### Not time-limited at seed 1 (`53` boards; `49` have identical open count on all 5 seeds)

- **`0` open, DRC errors (`1`):** `free-of-charge_BMS` (0, 412p)
- **`1` open (`9`):** `SpindleController_spindle_controller` (1, 210p), `avr-divecomputer_dc` (1, 281p), `esp_hat_esp_hat` (1, 287p), `hotstick_Wand` (1, 108p), `kitspace_PWR` (1, 71p), `m2-electronics_m2r` (1, 303p), `motor-high-current-sensors_CurrentSensorPCB_line` (1, 641p), `retroreflectors_RUMBAFLOCK` (1, 16p), `wifi_pants_wifi_pants` (1, 227p)
- **`2` open (`12`):** `ErgodoneWithHole_ErgoDone` (2, 734p), `FaradayRF-Hardware_Faraday` (2, 487p), `Hardware_Playground_livolo_1_channel_1way_eu_switch` (2, 126p), `Hardware_Playground_samd51_micropython` (2, 132p), `Helium_helium_1-6` (2, 373p), `LED-strip_PWM_LED-strip_PWM` (2, 46p), `OpenHardwareExG_ActiveElectrode_OpenHardwareExG_ActiveElectrode` (2, 37p), `antdroid-board_antdroid-board` (2, 234p), `badge2015_badge2015` (2, 190p), `psx2vectrex_vectrexmando` (2, 161p), `sbc_sbc` (2, 652p), `uSKY_uSKY` (2, 99p)
- **`3` open (`8`):** `AVR-ISP_pogo-plug_1.27mm_AVR-ISP_pogo-plug_1.27mm` (3, 36p), `HACK_hack` (3, 174p), `SPEX_HAB_Mainboard_Hardware_Power` (3, 369p), `SPEX_HAB_Mainboard_Hardware__autosave-Power` (3, 369p), `kitspace_hack` (3, 174p), `miniboard-stm32f0_miniboard-stm32f0` (3, 85p), `nrf52-kicad_nrf51x22_qfax_dcdc` (3, 137p), `typec-charger_TypeC-DC-Charger` (3, 343p)
- **`4–10` open (`15`):** `Class_D_Amp_class_D_ampl` (4, 260p), `Hardware_Playground_nrf5x_coin` (4, 107p), `MixSID_mixsid` (4, 313p), `SPEX_HAB_Mainboard_Hardware_Spex-Mainboard-Hardware` (4, 517p), `SPEX_HAB_Mainboard_Hardware_Spex-Mainboard-Hardware_rev1` (4, 517p), `SPEX_HAB_Mainboard_Hardware__autosave-Spex-Mainboard-Hardware` (4, 517p), `card_card` (4, 53p), `edid-injector_edid-injector` (4, 186p), `kitspace_parasite` (4, 101p), `kitspace_USB-LED-Otter` (5, 82p), `rfcx-sentinel-pcb_MPPT` (5, 202p), `DronPi_dronPi` (6, 539p), `kitspace_Oak` (6, 237p), `ATMEGA328-Motor-Board_ATMEGA328_Motor_Board` (10, 635p), `MySensors_SecureKeyfob_MySensors_RNDKeyfob` (10, 113p)
- **`11–25` open (`4`):** `FMCW_RADAR_Radar RF` (11, 541p), `Ttl_wlan_radar` (11, 431p), `bullion_bullion` (12, 108p), `Hornbill-ESP32-Minima_Beacon32` (25, 166p)
- **`>25` open (`4`):** `kitspace_test-jig` (26, 440p), `dc25_spqr_badge_badge-kicad` (34, 452p), `wifi-iot-core-hw_wifi-iot-core-hw` (36, 208p), `pmod-rgmii_pmod-rgmii-RTL8211E-module` (37, 333p)

### Time-limited at seed 1 (`100` boards; `4` marked `*` came clean at `2×` timeout)

- **`1` open (`0`):** *(none — all `8` time-limited `1`-open boards came clean on `s2..s5`)*
- **`2` open (`6`):** `DSKY-alarm-panel-replica_DSKY_alarm` (2, 245p), `HamShield09_HamShield09` (2, 528p), `Own-Mailbox-Hardware_pierre` (2, 681p), `TB6600StepperDriver_DEW_TB6600-V1` (2, 238p), `chip_lcd_dip_CHIP_LCD_DIP` (2, 223p), `smart-meter_SmartMeter` (2, 414p)
- **`3` open (`2`):** `CAL430FR_CAL430F` (3, 139p), `Hardware_Playground_E3_Pcb_rev0.3`* (3, 512p)
- **`4–10` open (`25`):** `BMS-bq76940_EvaluationsBoard` (4, 356p), `ESPglobe_ESPglobe` (4, 67p), `MySensorsNode_LE-BOX-0028` (4, 246p), `PiBook_pibook` (4, 601p), `elec-power-bms_OSBMS Balancer Rev1` (4, 238p), `kitspace_TS100C` (5, 274p), `kitspace_tomu` (5, 67p), `pcbproject_opensda_project_opensda` (5, 165p), `KiCad-Library_Teensy_test_layout` (6, 2563p), `FreeSmartEEG__autosave-AD7779 for Intel Edison umdk` (7, 393p), `Vento_Vento` (7, 386p), `kitspace_teensy-fx` (7, 341p), `reach-nrf51822-hardware_reach-target-hw` (7, 221p), `LibreSolar_MPPT_charger_20A` (8, 494p), `elec-power-bms_rLoopPowerBms` (8, 270p), `esp32-iot-uno_esp32-iot-uno-hw` (8, 319p), `gsm-iot-core-hw_gsm-iot-core` (8, 379p), `kitspace_OtterPill` (8, 204p), `DSP-ADAU1452_DSP-ADAU1452` (9, 951p), `zx-sizif-128_sizif128`* (9, 486p), `VC4000MultiROM_MultiRomCard` (10, 371p), `cjmcu2_CJMCU2` (10, 250p), `headstage-adapter_headstage adapter`* (10, 190p), `kitspace_lpa96` (10, 502p), `mod12003_mod12003` (10, 758p)
- **`11–25` open (`19`):** `EtherCAT_shield_v1_EtherCAT_shield_v1` (11, 436p), `dorkyboard_keyboard` (11, 1326p), `tinyFISH_tinyFISH` (11, 300p), `FMCW_RADAR_Radar v2` (12, 922p), `freeDSP-CLASSIC-SMD-BALANCED_FreeDSP_BAL` (12, 652p), `slushengine-modelx_slushengine_modelx`* (13, 796p), `srambo_1_srambo_1` (13, 235p), `keytee_pcb` (14, 89p), `LPC2148_Stick_LPC2148_stick` (15, 331p), `LPC2148_Stick__autosave-LPC2148_stick` (16, 331p), `ReST32_ReST` (16, 643p), `OvenController_Oven` (17, 238p), `zx-sizif-xxs_sizif-xxs` (17, 436p), `bms-8s50-ic_bms-8s50-ic` (19, 634p), `kitspace_dropbot-front-panel` (19, 356p), `mod1base_mod1base` (19, 515p), `stm32_mech_keyboard_TCKB` (19, 809p), `freeDSPx-AMP-x4_freeDSPx-AMPx4` (22, 582p), `CoreOne-xCORE200-Original_CoreOne` (25, 1114p)
- **`>25` open (`48`):** `Blink-Eras_Blink Eras` (27, 57p), `PCIE-to-MXM-Adapter_PCIEx1toMXM3.0` (28, 419p), `zx-sizif-512-ext_sizif512ext` (29, 774p), `TCKB_Kicad_TCKB` (30, 1023p), `avr-fuser-32_adapter` (31, 244p), `kitspace_Unifying` (31, 857p), `zx-tsid_zx-tsid` (32, 343p), `kitspace_120-channel-pogo-pin-board` (35, 291p), `Board-RZA1L_BoardRZA1` (36, 700p), `kitspace_tri_panel_4up_r0.1` (36, 657p), `poncho_fpga_fpga-dongle` (36, 660p), `open-source-multimeter_board` (37, 172p), `RainbowFox_RainbowFox` (39, 1220p), `fifogfx_c64cart` (41, 148p), `ArduinoDueClone_ATSAM3X8EA` (42, 380p), `P8000_WDC_Emulator_P8000_WDC_Emulator` (45, 500p), `mechkeys_lfk_revc` (49, 1202p), `MonApollo_analog-board` (50, 1596p), `kitspace_d20_tri_r0.2` (50, 669p), `kitspace_d20_tri_r0.3` (51, 669p), `domotics_base-board-arranged` (52, 666p), `kitspace_glasgow` (53, 1167p), `kitspace_XassetteAsterisk` (59, 617p), `VB-IoT1_VB-IoT1_v1` (60, 847p), `wiggleport_wiggle-spine` (64, 834p), `mini_mass_prog_bench_prog_rig` (69, 1651p), `kitspace_OtterCastAudioV2` (74, 863p), `karabas-nano_karabas-nano-revG` (75, 856p), `LargeWordClock_LargeKickstarter` (77, 2496p), `karabas-nano_karabas-nano-revB` (77, 832p), `karabas-nano_karabas-nano-revA` (80, 787p), `karabas-nano_karabas-nano-revC` (82, 879p), `Own-Mailbox-Hardware_eth` (84, 465p), `Own-Mailbox-Hardware_mailbox` (84, 465p), `MonApollo_digital-board` (85, 1021p), `kitspace_EEZ%20DIB%20MCU%20r1B2` (100, 906p), `Librecalc-Hardware__autosave-calculator` (102, 679p), `kitspace_OtterCastAmp` (106, 1228p), `kitspace_ulx3s` (110, 1177p), `HID_PID_controller` (122, 192p), `mechkeys_lfk_revd` (125, 1252p), `kitspace_ulx4m` (140, 1183p), `mechkeys_LFK78` (197, 1147p), `oskirby_logicbone` (287, 1656p), `decelerator4030_decelerator4030` (386, 1308p), `kitspace_d20_tri_r1.0` (412, 669p), `LeeChee_1800` (491, 2066p), `Aleste-520EX_aleste` (499, 2210p)

---

## What Was Not Done

- **Router behaviour was not changed**, and no restart feature was added to `ultraroute`.
- **Seeds `2..5` were not run on all `1,009` seed-1-clean boards:** instead, per the task brief, seed 2 was run on a `40`-board uniform random sample (`clean40-s2`: `0 / 40` failed) plus an exhaustive census of all `34` deadline-touched clean boards (`clean34-touched-s2`: `3 / 34` failed).
- **Shorter per-attempt restart budgets (`< 1×`, e.g. `60 s`) were not run or simulated:** `ultraroute` stage deadlines scale proportionally with `--timeout`, so `120 s` logs cannot simulate shorter-timeout runs without executing new benchmark passes.
- **Seeds beyond `5` (`K > 5`) and combined `2×`-timeout + multi-seed runs (`s2..s5` at `240 s`) were not run.**

---

## For the project memory

- **PCBench seed-restart baseline (`main` `aae1126` vs `full-1e935fc`, `1,009 / 1,182` clean):** Rerunning all `173` failed boards on seeds `2..5` wins `+11, +6, +9, +17` individually (mean `+10.75`); cumulative best-of-`K` (`K = 1..5`) reaches `1,009, 1,020, 1,021, 1,024, 1,029` clean (`+20` total; 24-perm means `+10.75, +15.83, +18.50, +20.00`) for `+4.19, +8.09, +11.96, +15.64` sequential core-h.
- **`2×` timeout vs restarts:** Doubling per-board timeout on seed 1 (`fail173-s1-2x`, `240 s` + `480 s` on the two `240 s` boards in `pcbench_timeouts.json`) wins `+16` clean (`1,025 / 1,182`) for `+3.05` incremental core-h (`686.3` CPU-s/win), beating unconditional `K = 2` (`+11` wins for `+4.19` core-h). Note `pcbench.py`'s `board_timeouts()` uses `max(240, --timeout)`, so true `2×` on `pcbench_timeouts.json` boards requires explicit `480 s` entries in `run.json` (`medusa_medusa_cape` came clean in `101.6` s at `480 s` once `prefanout` stopped timing out).
- **Where restarts help vs never help:** `49 / 53` (`92.5%`) of non-time-limited never-clean boards leave the exact same open count on all `5` seeds; restarting non-time-limited runs (`57` boards) wins only `+2` at `K = 2` and `+4` at `K = 5`. Conversely, `open == 1` + time-limited (`8` boards) won `8 / 8` (`100%`) by `K = 5`, while `open == 3` won `0 / 10`. Gating restarts on `1 <= open <= 2` and `time_limited` (`17` boards) wins `+6` (`K = 2`) to `+11` (`K = 5`) at `268–427` CPU-s/win (`306` s/win dynamic).
- **Clean-board seed sensitivity:** `0 / 40` uniform random clean boards failed on seed 2 (`clean40-s2`), while `3 / 34` (`8.82%`) of all deadline-touched clean boards failed on seed 2 (`clean34-touched-s2`: `minima-hardware_FrontPanel`, `Protoboard-DIP-for-CHIP_chip-dip-protoboard`, `Own-Mailbox-Hardware_mailbox-before`).
