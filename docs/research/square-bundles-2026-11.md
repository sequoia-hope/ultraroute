# Report: Square Parallel Slanted Tracks Together as a Bus (`grid::bundle`)

## Summary

Single-chain squaring (`grid::octilinear_round`) bends one net at a time. When 2 or more nets run side by side at an odd angle near minimum clearance, no single track can take a 45-degree knee without cutting into its neighbour's clearance envelope, so all stay slanted even across 3 retry rounds.

1. **Corpus measurement (Part 1):** Across the 315 routed gate boards of `sqg-2f44840`, `2,759.61 mm` (`3.40%`) of the `81,107.23 mm` off-45 track length sits in parallel multi-net groups (`<= 1 deg`, overlapping, edge-to-edge gap `<= 3x` clearance) across `108` boards (`358` groups). Splitting out coupled differential pairs (`242` groups, `2,050.49 mm`, `2.53%`, which are excluded from single-ended squaring) leaves **`116` single-ended groups on `70/315` boards totaling `709.12 mm` whole-piece (`0.87%` of off-45) and `488.30 mm` overlapping span (`0.60%`)**. Before squaring (`srv-tidy-gm`), single-ended groups hold `4,198.27 mm` (`1.94%` of off-45) across `524` groups on `157/315` boards; single-track squaring already peels many 2-net groups from the outside in when one side has room, leaving the jammed ones.
2. **Coordinated group move (Part 2):** Implemented `grid::bundle::square_bundles` in `core/crates/grid/src/bundle.rs` (`1,235` lines, 3 unit tests) with a small hook in `core/crates/grid/src/cleanup.rs` (`+14/-8` lines, `octilinear_round` untouched). It detects parallel off-45 multi-net groups in `soft`, expands curved fanout spans up to `OCTILINEAR_SPAN` (`8` segments), generates coordinated knees and staggered Z-bends across all 8 octants whose diagonal knee stagger preserves each adjacent pair's run gap (`gap_diag == gap_run`), and applies the move all-or-none only when every member passes all `try_way` checks (clearance rules with `TAUT_MARGIN = 2 um`, endpoint joins, pad slides/centres, `grazes`, `leaves_slot`, `slants_into`, non-increasing `doglegs`, and non-increasing `net_connectivity` airline count).
3. **Evaluation (Part 3):**
   - **Synthetic 8-bit odd-angle buses:** On `tqfp08_bus8` (two 0.80 mm pitch 32-pin TQFPs at `15.52 deg`), `off45` drops from **`100.0%` (`174.324 mm`) to `0.0%` (`0.0 mm`)**, `bus_bundled = 0.993`, `0` DRC errors, `0` slots, `0` acute, `0` doglegs. On `hdr_bus8` (two 1x08 0.85 mm headers at `20.56 deg`), `off45` drops from **`86.3%` (`116.348 mm`) to `0.0%` (`0.0 mm`)**, and `bus_bundled` rises from **`85.8%` to `90.7%`**, with diagonal spacing matching run spacing (`0.545880 mm`) to the nanometre.
   - **PCBench 76 boards with groups (`sqbun-grp-base` vs `sqbun-grp-new`):** Clean `65 -> 64` of `76` (`lost 1`: `f4fc_f4fc` hit CPU-clock deadline inside `prefanout` before routing or squaring ran; well inside PCBench's `noise = 3` floor; `time_limited` boards overall went `8 -> 6`, `0 -> 0` in `tightening` and `squaring`). On the **64 boards clean in both**, `off45` drops **`24,408.792 -> 24,193.948 mm` (`-214.844 mm`, `25.6% -> 25.3%`, tidier on 11 boards)**, `bus_bundled` improves **`8.0% -> 8.1%`** (`791.696 -> 798.952 mm`; `CAL430FR_CAL430F` jumps **`1.9% -> 12.3%`**), `slots` drop **`1,274 -> 1,272`**, `acute` stays `172 -> 172`, `vias` go `3,435 -> 3,434`, and router CPU goes **`1,235.0 -> 1,231.2 s`**. Costs: wirelength `+30.188 mm` (`+0.032%`) and doglegs `363 -> 367` (`3.800 -> 3.841 /m`).
   - **PCBench `--tier smoke` (`sqbun-smoke-base` vs `sqbun-smoke-new`, 48 boards):** **`PASS`** (`clean 48 -> 48`, `lost 0`, `won 0`, `time_limited 0 -> 0`, `slots 251 -> 250`, `acute 22 -> 22`, `doglegs/m 1.802 -> 1.747`, `off45 29.0% -> 28.9%`, `CPU 71.5 -> 71.7 s`; on the 45 smoke boards without groups, `CPU 65.5 -> 65.8 s`, or `+0.57` to `+4.18 ms` in the `squaring` stage).

Visual gallery and all SVG crops are served at:
- Index page: http://localhost:8065/squaring/ (`~/Software/ultraroute-lab/squaring/index.html`)

---

## What Was Built

| File | Change | Purpose |
|---|---|---|
| `core/crates/grid/src/bundle.rs` | New (`1,235` lines) | Group detection (`find_groups`), span expansion (`expand_span`), isometric `(M, N)` staggered knee/Z generation (`group_ways`, `build_ways_for_ends`), all-or-none validation & rollback (`try_bundle`), `ULTRAROUTE_SQUARE_TRACE` diagnostics, and 3 unit tests. |
| `core/crates/grid/src/cleanup.rs` | Modified (`+14/-8` lines) | Expose `TAUT_MARGIN`, `OCTILINEAR_SPAN`, `same_heading`, `doglegs`, `leaves_slot`, `square_trace`, and `pad_centre` as `pub(crate)`; call `crate::bundle::square_bundles` before and after the `OCTILINEAR_ROUNDS` loop in `octilinear_watch`. `octilinear_round` is completely untouched. |
| `core/crates/grid/src/lib.rs` | Modified (`+1` line) | Register `pub mod bundle;`. |

### Flags, Bounds, and Work Caps

- **No new CLI flags:** `square_bundles` runs inside `grid::octilinear_watch`, which is already gated by `!args.any_angle` in `Tightening` and `Squaring`. When `find_groups` detects zero multi-net parallel off-45 groups on a board, `square_bundles` returns `0` immediately after scanning the `soft` chains.
- **No new `Deadline::seconds` sub-slices:** `square_bundles` checks the caller's stage `deadline.expired()` in every round, group, subgroup, and candidate loop, so it never trips `fired` on its own or makes output depend on machine speed unless the stage's own budget expires.
- **Hard work caps:**
  - `MAX_BUNDLE_ROUNDS = 2` (outer passes in `square_bundles`, plus a 2-round single-chain cleanup pass only when a group actually bends).
  - `MAX_GROUP_TRIES = 512` (hard cap on candidate ways evaluated per group, shared across the full group and any contiguous subgroup fallback).
  - `OCTILINEAR_SPAN = 8` (max chain segments replaced per member).
- **Trace support:** `ULTRAROUTE_SQUARE_TRACE=X,Y` (mm) prints every candidate bundle way within `1 mm` of `(X, Y)` (`bundle way (N nets): ... -> fit` or the first member and exact check that refused it).

---

## Part 1: Off-45 Parallel Groups in the Corpus

### Definition

Matching `tools/tidy.py` and the task specification, two off-45 straight segments (`min(angle_deg % 45, 45 - angle_deg % 45) > 0.5 deg`) belong to the same group when they:
1. Belong to **different nets** on the **same copper layer**.
2. Have headings parallel within **`1.0 deg`** (`sin |da| <= sin 1 deg`, direction-agnostic).
3. **Overlap** along their shared axis (`hi > lo` when projected onto segment `i`'s unit vector).
4. Have edge-to-edge perpendicular clearance gap **`<= 3.0 * max(board_min_clearance, 0.10 mm)`**.

Connected components of `2+` distinct nets form groups. Two length figures are reported for every run:
- **Whole-piece grouped length:** sum of full lengths of off-45 segments participating in a group.
- **Overlapping-span grouped length:** sum of the portions of those segments actually overlapped alongside a neighbour in the group (excluding ungrouped tails).

Because `stages.rs` keeps differential-pair coupled segments out of `soft` during `Tightening`, and `pairs::pair_names` pairs (`_P/_N`, `+`/`-`, `DP/DM`) are routed as coupled pairs rather than independent single-ended bus tracks, we report both **all `2+`-net groups** and the split between **diff-pair 2-net groups** and **single-ended (`SE`) groups**.

### Corpus Incidence Table

| Run | Subset (Boards) | Straight (mm) | Total Off-45 (mm / % straight) | All Groups (grps / boards) | All Grouped Piece (mm / % off-45) | All Grouped Overlap (mm / % off-45) | Alone Off-45 Piece (mm / %) | Diff-Pair Groups (grps / piece mm / %) | Single-Ended Groups (grps / boards / piece mm / % off-45 / overlap mm) |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `sq-new1` (28 worst boards, `2f44840`) | All = Clean (`28`) | `60,424.21` | `16,811.67` (`27.82%`) | `48` on `15/28` | `453.91` (`2.70%`) | `402.19` (`2.39%`) | `16,357.76` (`97.30%`) | `22` / `311.28` (`1.85%`) | **`26` on `13/28` / `142.63 mm` (`0.85%`) / `94.74 mm` (`0.56%`)** |
| `srv-goct-c8b9eb2` (gate, `c8b9eb2`) | All (`315`) | `309,793.24` | `60,074.11` (`19.39%`) | `331` on `97/315` | `2,485.46` (`4.14%`) | `2,317.43` (`3.86%`) | `57,588.65` (`95.86%`) | `240` / `2,019.82` (`3.36%`) | **`91` on `55/315` / `465.64 mm` (`0.78%`) / `328.81 mm` (`0.55%`)** |
| `srv-goct-c8b9eb2` (gate, `c8b9eb2`) | Clean (`272`) | `266,270.80` | `51,198.39` (`19.23%`) | `290` on `83/272` | `2,117.46` (`4.14%`) | `1,980.25` (`3.87%`) | `49,080.94` (`95.86%`) | `216` / `1,752.44` (`3.42%`) | **`74` on `43/272` / `365.01 mm` (`0.71%`) / `255.78 mm` (`0.50%`)** |
| `sqg-2f44840` (gate, `2f44840`) | All (`315`) | `306,555.49` | `81,107.23` (`26.46%`) | `358` on `108/315` | `2,759.61` (`3.40%`) | `2,500.33` (`3.08%`) | `78,347.61` (`96.60%`) | `242` / `2,050.49` (`2.53%`) | **`116` on `70/315` / `709.12 mm` (`0.87%`) / `488.30 mm` (`0.60%`)** |
| `sqg-2f44840` (gate, `2f44840`) | Clean (`275`) | `267,107.77` | `69,787.32` (`26.13%`) | `311` on `94/275` | `2,357.31` (`3.38%`) | `2,137.08` (`3.06%`) | `67,430.01` (`96.62%`) | `216` / `1,773.68` (`2.54%`) | **`95` on `59/275` / `583.63 mm` (`0.84%`) / `392.83 mm` (`0.56%`)** |
| `srv-tidy-gm` (gate, pre-squaring) | All (`315`) | `292,990.58` | `215,938.15` (`73.70%`) | `771` on `171/315` | `6,337.13` (`2.93%`) | `5,046.94` (`2.34%`) | `209,601.02` (`97.07%`) | `247` / `2,138.86` (`0.99%`) | **`524` on `157/315` / `4,198.27 mm` (`1.94%`) / `2,951.61 mm` (`1.37%`)** |
| `srv-tidy-gm` (gate, pre-squaring) | Clean (`272`) | `251,753.37` | `185,275.86` (`73.59%`) | `623` on `142/272` | `5,159.02` (`2.78%`) | `4,100.77` (`2.21%`) | `180,116.83` (`97.22%`) | `221` / `1,842.40` (`0.99%`) | **`402` on `129/272` / `3,316.62 mm` (`1.79%`) / `2,291.71 mm` (`1.24%`)** |

*(Note: 315 of the 316 gate boards produced `out.kicad_pcb` in `srv-goct-c8b9eb2`, `sqg-2f44840`, and `srv-tidy-gm`.)*

### Breakdown of Single-Ended Groups in `sqg-2f44840` (All 315 Gate Boards)

- **2 nets:** `108` groups, `559.47 mm` whole-piece (`0.69%` of off-45), `357.57 mm` overlap (`0.44%`).
- **3 nets:** `6` groups, `69.52 mm` whole-piece (`0.09%` of off-45), `52.40 mm` overlap (`0.06%`).
- **4+ nets:** `2` groups, `80.13 mm` whole-piece (`0.10%` of off-45), `78.33 mm` overlap (`0.10%`), dominated by the 5-net GPIO bus on `esp_hat_esp_hat` (`73.11 mm`).

### Corpus SVG Crops (`~/Software/ultraroute-lab/squaring/`)

1. **5-net GPIO bus (`esp_hat_esp_hat`, `F.Cu`, `73.11 mm` piece / `71.88 mm` overlap):** nets `/GPIO4`, `/GPIO13`, `/GPIO12`, `/GPIO14`, `/GPIO16` running side by side at `162.80 deg` (`17.20 deg` off horizontal) with min edge gap `0.565x` nominal clearance (`0.20 mm` netclass vs `0.254 mm` board max).
   - http://localhost:8065/squaring/example0_esp_hat_5net_fcu.svg
2. **3-net inner-layer bus (`f4fc_f4fc`, `In2.Cu`, `14.37 mm` piece / `13.68 mm` overlap):** nets `/OSD_CS`, `/TELEM_TX`, `/MPU_INT` running at `136.68 deg` (`1.68 deg` off `135 deg`), min edge gap `1.007x` clearance.
   - http://localhost:8065/squaring/example1_f4fc_in2.svg
3. **3-net front-layer bus (`kitspace_hack`, `F.Cu`, `11.43 mm` piece / `8.44 mm` overlap):** nets `/VIN_P`, `/VOUT_P`, `/ENABLE` running at `1.98 deg`, min edge gap `1.547x` clearance.
   - http://localhost:8065/squaring/example2_kitspace_hack_fcu.svg
4. **2-net shield bus (`APM-RPi-Shield_APM-RPi-Shield`, `F.Cu`, `17.75 mm` piece / `17.75 mm` overlap):** nets `/RC_IN_5` and `/RC_IN_7` running at `153.20 deg` (`26.80 deg` off horizontal), min edge gap `1.158x` clearance.
   - http://localhost:8065/squaring/example3_apm_rpi_shield_fcu.svg

---

## Part 2: Coordinated Group Move (`core/crates/grid/src/bundle.rs`)

### Geometry: Preserving Run Spacing Across the Bend

Each off-45 span lies between a cardinal axis and a 45-degree diagonal. By applying one of the 8 dihedral isometries `(sx, sy, swap)` (`sx, sy in {-1, +1}`, `swap in {false, true}`), `build_ways_for_ends` maps the group's mean vector into the canonical octant `dx >= dy > 0` (`0 deg < theta < 45 deg`). For member `k` (`k = 0 .. m-1`, sorted across the bus by perpendicular signed offset), let:
- `S_k = (x_{s,k}, y_{s,k})` and `E_k = (x_{e,k}, y_{e,k})` be its transformed start and end points (`dx_k = x_{e,k} - x_{s,k} > dy_k = y_{e,k} - y_{s,k} >= 0`),
- `M_k = x_{e,k} - x_{s,k} - (y_{e,k} - y_{s,k}) >= 0` be the horizontal excess,
- `N_k = y_{e,k} - y_{s,k} >= 0` be the diagonal rise.

We generate four coordinated families across all members simultaneously:

1. **Family A (Horizontal -> +45 deg -> Horizontal, staggered Z):**
   Choose a shared parameter `u` (or staggered knee line) such that member `k` turns onto `+45 deg` at `(x_{k,1}, y_{s,k})`, climbs `(N_k, N_k)` to `(x_{k,1} + N_k, y_{e,k})`, and finishes horizontally to `E_k`. Along the `+45 deg` diagonal segments `y - x = c_k` (where `c_k = y_{s,k} - x_{k,1}`), the perpendicular centre-to-centre spacing between adjacent members `k` and `k+1` is `|c_{k+1} - c_k| / sqrt(2)`. Setting `c_{k+1} - c_k = sqrt(2) * pitch_perp_k`, where `pitch_perp_k` is the exact perpendicular centre-to-centre spacing between members `k` and `k+1` along the slanted run (clamped at `r_k + r_{k+1} + clearance(net_k, net_{k+1}) + TAUT_MARGIN`), makes **the edge-to-edge gap across the 45-degree bend equal the edge-to-edge gap along the slanted run** (`gap_diag == gap_run`). Five anchor positions along the shared feasible interval `[lo, hi]` (`mid`, `lo`, `hi`, `25%`, `75%`) plus uniform `u in {0.5, 0.0, 1.0, 0.25, 0.75}` are tried.
2. **Family B (+45 deg -> Horizontal -> +45 deg, staggered Z):**
   Dual construction splitting the diagonal rise `N_k = b_k + (N_k - b_k)` with horizontal middle segment at `y_mid_k = y_{s,k} + b_k` spaced by `pitch_perp_k` between adjacent tracks, plus uniform `v in {0.5, 0.0, 1.0}`.
3. **2-Piece Coordinated Knees (`axis_first` and `diag_first`):**
   Every member bends once (`u = 1` or `u = 0`), turning all `m` tracks in the same bend direction without middle Z-segments when endpoints permit.
4. **Pad-Centre & Pad-Slide Endpoint Variants + Contiguous Subgroup Fallback:**
   When a member's outer endpoint sits alone inside a pad, `group_ways` tries pad-centre endpoints (`pad_centre`) and inward pad slides (`100%`, `50%`, `0%`) just as `octilinear_round` does. If an obstacle blocks one outer track of an `m`-track group (`m >= 3`), `square_one_group` falls back to contiguous subgroups of size `m-1` down to `2` within `MAX_GROUP_TRIES`.

### All-or-None Validation (`try_bundle`)

For a candidate group way:
1. All members' old span tracks are removed together (saving exact `(ItemId, Item)` pairs for rollback).
2. Every member's new octilinear segments (collinear hinges merged via `same_heading`, sub-`0.5 mm` Z-kinks rejected) are checked against `board.ShoveChallenger` with `TAUT_MARGIN = 2_000 nm` (`2 um`) and inserted sequentially so inter-member clearances are verified directly by the board's spatial index and custom DRC rules.
3. Every member is checked for:
   - Endpoint joins (`joins` at non-free chain ends, or remaining inside its sole pad `pad_alone_at`),
   - No `grazes` against same-net copper,
   - No `leaves_slot` acid-trap slots,
   - No `slants_into` diagonal crossings of rectangular/roundrect/oval pad rims,
   - Non-increasing `doglegs` along the member's full chain (`doglegs_after <= doglegs_before`),
   - Non-increasing `net_connectivity(net).1` airline count (`airlines_after <= airlines_before`) so a T-junction branch along a span can never be orphaned.
4. If **any** member fails **any** check, all newly inserted tracks are removed and every original `(ItemId, Item)` is restored identically (`all-or-none`).

### Unit Tests (`core/crates/grid/src/bundle.rs`)

All 3 unit tests in `grid::bundle::tests` assert directly that the stage acted (or rolled back cleanly):
1. `eight_bit_qfp_bus_squares_together_with_equal_bend_gap_where_single_chain_cannot`: builds an 8-net bus at `15.5 deg` (`0.25 mm` width, `0.20 mm` clearance, `0.47 mm` perpendicular pitch = `1.10x` clearance), asserts 3 rounds of `octilinear_round` bend **`0`** tracks (`100%` off-45), then calls `square_bundles` directly and asserts `bent > 0`, **`0` off-45 tracks remaining**, **`0` DRC violations**, `0` open nets, and that every adjacent pair's 45-degree diagonal edge-to-edge gap matches its horizontal gap within `5 um`.
2. `bundle_move_is_all_or_none_when_an_obstacle_blocks_every_group_way`: places blocking copper across member `4` (`net 5`) in a 4-net tightly-packed bus (`0.452 mm` pitch = `1.01x` clearance where subgroups also collide with unmoved neighbours) and asserts `square_bundles == 0` with every `ItemId` and segment endpoint identical to before.
3. `multi_segment_slanted_fanout_curves_expand_and_square_into_aligned_knees`: builds a 4-net 3-segment curved slanted fanout (`19.5 deg -> 14.9 deg -> 19.3 deg`) and asserts `expand_span` + `square_bundles` replaces all 4 multi-segment chains with clean octilinear knees (`0` off-45 segments, `0` DRC violations, `0` open nets).

Full workspace test suite (`cargo test --release` across `board`, `grid`, `kicad-pcb`, `kicad-text`, `morph`, `ultraroute` unit + all 11 integration test binaries) passed with **0 failures**; `cargo clippy --workspace --all-targets -- -D warnings` passed with **0 warnings**.

---

## Part 3: Experimental Results (Exact Numbers)

### 3A. Synthetic 8-Bit Odd-Angle Bus Boards (`runs/synth_bus` -> `~/Software/ultraroute-lab/squaring/`)

Three synthetic 2-layer KiCad boards were generated (`100 x 80 mm`, 8-bit bus `D0..D7` at odd angles) and routed from bare pads (`--strip-all --recipe default --seed 0 --uuid-seed 0 --threads 1 --timeout 60 --cpu-clock`) with baseline `~/Software/ultraroute-lab/bin/sq-2f44840/ultraroute` vs worktree `core/target/release/ultraroute`, judged with `kicad-cli pcb drc --severity-all`, `tools/dfm.py`, and `tools/tidy.py`.

*(Note on `tidy.py --clearance`: `pcbench.py` passes `--clearance <board_rule_mm>` to `tidy.py`, whereas standalone `tidy.py` defaults to `--clearance 0.1` mm. Both figures are given below: `rule clr` first, `[default 0.1 mm clr]` in brackets where different.)*

| Board & Geometry | Binary | DRC Err / Open | Slots / Acute | Off-45 Ratio (`off45_mm / straight_mm`) | `bundled` (rule clr `[0.1mm]`) | `bus_bundled` (rule clr `[0.1mm]`) | Doglegs (`/m`) | Turns (`/10mm`) | Detour | Wirelength (mm) | Vias | CPU (s) | `tightening` / `squaring` Stage CPU (ms) |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| **`tqfp08_bus8`**<br>Two 32-pin `0.80 mm` TQFPs, bus at `15.52 deg`, `w=0.25`, `clr=0.20 mm` | `sq-2f44840` | `0 / 0` | `0 / 0` | **`1.000`** (`174.324 / 174.324`) | `0.992` `[0.000]` | `0.992` (`172.875 / 174.324`) `[0.000]` | `0` (`0.0`) | `0.000` | `0.905` | `174.324` | `0` | `0.0316` | `2.321 / 2.095` (`bent=0`) |
| **`tqfp08_bus8`** | **`square-bundles`** | **`0 / 0`** | **`0 / 0`** | **`0.000`** (`0.0 / 192.682`) | **`0.993`** `[0.000]` | **`0.993`** (`191.381 / 192.682`) `[0.000]` | **`0` (`0.0`)** | `0.415` | `1.000` | `192.682` | `0` | `0.0308` | `2.932 / 0.036` (all 8 bent in `tightening`) |
| **`hdr_bus8`**<br>Two 1x08 `0.85 mm` headers, bus at `20.56 deg`, `w=0.25`, `clr=0.20 mm` | `sq-2f44840` | `0 / 0` | `0 / 0` | **`0.863`** (`116.348 / 134.833`) | `0.858` `[0.000]` | `0.858` (`115.752 / 134.833`) `[0.000]` | `0` (`0.0`) | `0.037` | `0.912` | `134.833` | `0` | `0.0309` | `5.708 / 1.502` (`bent=0`) |
| **`hdr_bus8`** | **`square-bundles`** | **`0 / 0`** | **`0 / 0`** | **`0.000`** (`0.0 / 147.882`) | **`0.907`** `[0.000]` | **`0.907`** (`134.136 / 147.882`) `[0.000]` | **`0` (`0.0`)** | `0.507` | `1.000` | `147.882` | `0` | `0.0439` | `17.562 / 1.942` (`bent=7` in `squaring`) |
| **`qfp_bus8`**<br>Two 32-pin `0.50 mm` QFPs, bus at `17.35 deg`, `w=0.20`, `clr=0.15 mm` | `sq-2f44840` | `0 / 0` | `0 / 0` | `0.000` (`0.0 / 163.962`) | `0.996` `[0.888]` | `0.996` (`163.255 / 163.962`) `[0.888]` | `0` (`0.0`) | `0.488` | `0.945` | `163.962` | `0` | `0.3716` | `0.753 / 0.021` (`bent=0`) |
| **`qfp_bus8`** | **`square-bundles`** | `0 / 0` | `0 / 0` | `0.000` (`0.0 / 163.962`) | `0.995` `[0.887]` | `0.995` (`163.161 / 163.962`) `[0.887]` | `0` (`0.0`) | `0.488` | `0.945` | `163.962` | `0` | `0.3809` | `0.864 / 0.058` (`bent=0`) |

- Why `qfp_bus8` already reached `0.000` on baseline: on `0.50 mm` fine-pitch QFPs, `prefanout` lays horizontal escape stubs on all 16 pads before routing, giving the outer tracks room to bend one at a time from the outside in during `OCTILINEAR_ROUNDS`. Where tracks land directly on SMD/PTH pads without `prefanout` escapes (`tqfp08_bus8`, `hdr_bus8`), `sq-2f44840` jams completely (`100.0%` and `86.3%` off-45) and `square-bundles` squares all 8 nets cleanly (`0.0%` off-45).
- Synthetic SVG crops:
  - `tqfp08_bus8`: http://localhost:8065/squaring/synth_tqfp08_bus8_base.svg vs http://localhost:8065/squaring/synth_tqfp08_bus8_new.svg
  - `hdr_bus8`: http://localhost:8065/squaring/synth_hdr_bus8_base.svg vs http://localhost:8065/squaring/synth_hdr_bus8_new.svg
  - `qfp_bus8`: http://localhost:8065/squaring/synth_qfp_bus8_base.svg vs http://localhost:8065/squaring/synth_qfp_bus8_new.svg

---

### 3B. PCBench A/B on the 76 Boards With Single-Ended Off-45 Groups (`sqbun-grp-base` vs `sqbun-grp-new`)

Command run (`--only` union of all 76 boards with single-ended off-45 groups in `sqg-2f44840` or `sq-new1`):
`python3 tools/pcbench.py ab sqbun-grp-base sqbun-grp-new --base-bin ~/Software/ultraroute-lab/bin/sq-2f44840/ultraroute --only <76 IDs> -j 6`

| Metric | `sqbun-grp-base` (`sq-2f44840`) | `sqbun-grp-new` (`square-bundles`) | Delta / Notes |
|---|---:|---:|---|
| **Clean boards (of 76)** | **`65 / 76`** | **`64 / 76`** | **`lost 1` (`f4fc_f4fc`: `open 2`), `won 0`** (`noise floor = 3`; see Costs below) |
| Router / referee failures | `0` | `0` | Error types gained: `0` |
| **Time-limited boards (all 76)** | **`8`** | **`6`** | `prefanout: 4 -> 4`, `pairs: 4 -> 4`, `shove: 5 -> 3`, `vias: 2 -> 2`, **`tightening: 0 -> 0`**, **`squaring: 0 -> 0`** |
| **Off-45 (`64` both-clean)** | **`25.6%`** (`24,408.792 / 95,520.389 mm`) | **`25.3%`** (`24,193.948 / 95,550.576 mm`) | **`-214.844 mm` off-45** (`tidier on 11` boards, worse on `4`) |
| **Off-45 (`all 76` boards)** | `25.76%` (`27,949.29 / 108,506.60 mm`) | `25.40%` (`27,554.96 / 108,499.09 mm`) | `-394.33 mm` |
| **`bundled` (`64` both-clean)** | `6.9%` (`6,625.108 / 95,520.389 mm`) | `6.9%` (`6,594.148 / 95,550.576 mm`) | `-30.960 mm` (`tidier on 6`) |
| **`bus_bundled` (`64` both-clean)** | **`8.0%`** (`791.696 / 9,862.411 mm`) | **`8.1%`** (`798.952 / 9,867.065 mm`) | **`+7.256 mm` (`+0.1 pp`, `tidier on 4`)** |
| **Doglegs /m (`64` both-clean)** | `3.800 /m` (`363` doglegs) | `3.841 /m` (`367` doglegs) | **`+4` doglegs (`+0.041 /m`)**, `tidier on 8` |
| Turns `/10 mm` (`64` both-clean) | `0.799` (`7,635.04` qtrs) | `0.800` (`7,641.84` qtrs) | `+0.001` |
| Detour (`64` both-clean) | `1.027` | `1.027` | `0.000` |
| **Slots (`64` both-clean / `all 76`)** | **`1,274`** / `1,556` | **`1,272`** / `1,561` | **`-2` on clean boards** (`+5` on all 76 from time-limited `f4fc_f4fc` `+4` and `Own-Mailbox-Hardware_pierre` `+3`) |
| **Acute (`64` both-clean / `all 76`)** | `172` / `202` | `172` / `204` | **`0` net on clean boards** (`medusa` `-1`, `arf154` `+1`) |
| Necks (`64` both-clean / `all 76`) | `3` / `4` | `3` / `4` | `0` |
| **Wirelength (`64` both-clean)** | `95,520.387 mm` | `95,550.575 mm` | **`+30.188 mm` (`+0.032%`)** |
| **Vias (`64` both-clean)** | `3,435` | `3,434` | **`-1` via** |
| **Router CPU s (`64` both-clean / `all 76`)** | **`1,235.0 s`** / `2,144.0 s` | **`1,231.2 s`** / `2,174.4 s` | **`-3.8 s` on clean boards** (`+30.4 s` on all 76 entirely from `f4fc_f4fc` `62.5 -> 117.9 s`) |

#### Per-Board Changes on the 64 Both-Clean Group Boards (All 16 Boards Where Any Metric Moved)

| Board ID | Off-45 Base -> New (mm) | Delta Off-45 (mm) | Off-45 % | `bundled` | `bus_bundled` | Doglegs | Slots | Acute | Wirelength Base -> New (mm) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `medusa_medusa_cape` | `857.270 -> 808.381` | **`-48.889`** | `31.7% -> 29.8%` | `5.6% -> 5.5%` | **`9.2% -> 9.9%`** | `11 -> 11` | **`27 -> 26`** | **`8 -> 7`** | `2,707.583 -> 2,714.891` |
| `protoarray_proto_array` | `559.560 -> 524.873` | **`-34.687`** | `30.2% -> 28.3%` | `2.2% -> 2.2%` | **`4.7% -> 4.8%`** | `2 -> 2` | `13 -> 13` | `3 -> 3` | `1,850.004 -> 1,851.856` |
| `CAL430FR_CAL430F` | `248.893 -> 224.702` | **`-24.191`** | `22.2% -> 20.1%` | **`13.3% -> 15.1%`** | **`1.9% -> 12.3%`** | `10 -> 10` | `27 -> 28` (`+1`) | `5 -> 5` | `1,119.231 -> 1,119.998` |
| `arf154_arf154` | `219.797 -> 197.989` | **`-21.808`** | `42.0% -> 37.7%` | `9.8% -> 9.7%` | `13.0% -> 13.0%` | `5 -> 6` (`+1`) | `5 -> 5` | `2 -> 3` (`+1`) | `522.936 -> 524.893` |
| `stubby_stubby` | `326.340 -> 305.076` | **`-21.264`** | `24.5% -> 22.9%` | **`7.1% -> 7.2%`** | `13.4% -> 13.4%` | `4 -> 6` (`+2`) | `11 -> 11` | `3 -> 3` | `1,329.721 -> 1,330.730` |
| `type5_type5` | `301.350 -> 280.777` | **`-20.573`** | `16.7% -> 15.5%` | `16.1% -> 15.3%` | n/a | `1 -> 1` | `0 -> 0` | `0 -> 0` | `1,802.093 -> 1,809.857` |
| `Lys_Lys` | `443.382 -> 424.502` | **`-18.880`** | `28.7% -> 27.4%` | `6.4% -> 6.2%` | n/a | `4 -> 4` | `17 -> 18` (`+1`) | `1 -> 1` | `1,543.119 -> 1,551.861` |
| `navelino_navelino` | `286.806 -> 271.952` | **`-14.854`** | `31.6% -> 29.9%` | `3.6% -> 3.5%` | n/a | `7 -> 7` | `13 -> 13` | `4 -> 4` | `908.533 -> 909.187` |
| `kitspace_T32_ref` | `301.218 -> 288.027` | **`-13.191`** | `16.8% -> 16.0%` | `7.5% -> 6.6%` | n/a | **`14 -> 13`** | `11 -> 11` | `2 -> 2` | `1,795.149 -> 1,796.171` |
| `autohat-board_autohat-rig` | `725.301 -> 714.682` | **`-10.619`** | `28.0% -> 27.5%` | **`7.8% -> 7.9%`** | `7.1% -> 7.1%` | `4 -> 4` | `23 -> 23` | `4 -> 4` | `2,594.841 -> 2,595.209` |
| `MySensorIRBlaster_IR Blaster` | `246.820 -> 243.796` | `-3.024` | `28.9% -> 28.5%` | `3.1% -> 3.1%` | n/a | `1 -> 2` (`+1`) | `11 -> 11` | `1 -> 1` | `854.083 -> 854.308` |
| `iCE40-IO_ICE40-IO_Rev_A` | `435.652 -> 435.518` | `-0.134` | `23.7% -> 23.7%` | `5.5% -> 5.5%` | `7.8% -> 7.8%` | `6 -> 6` | `13 -> 13` | `0 -> 0` | `1,839.905 -> 1,839.961` |
| `f.60_keyboard` | `499.903 -> 499.958` | `+0.055` | `13.0% -> 13.0%` | `8.2% -> 8.2%` | n/a | `1 -> 1` | `11 -> 11` | `1 -> 1` | `3,855.679 -> 3,855.683` |
| `Horticulture_Light_EQ_3-Band_light` | `924.542 -> 924.657` | `+0.115` | `21.1% -> 21.1%` | `28.2% -> 28.2%` | n/a | `2 -> 2` | `138 -> 138` | `0 -> 0` | `4,381.151 -> 4,381.111` |
| `bobc_MS-F100` *(regression)* | `270.541 -> 277.349` | **`+6.808`** | `30.6% -> 31.4%` | `11.5% -> 8.8%` | `9.7% -> 3.9%` | `6 -> 7` (`+1`) | **`34 -> 31`** (`-3`) | `6 -> 6` | `884.083 -> 884.180` |
| `avr-fuser-32_hvpp` *(regression)* | `329.056 -> 339.348` | **`+10.292`** | `20.7% -> 21.4%` | `4.5% -> 3.7%` | n/a | `3 -> 3` | `6 -> 6` | `0 -> 0` | `1,585.905 -> 1,585.211` |

Real-board before/after SVG crops:
- `stubby_stubby` (`/PWM04` & `/PWM05`, `21.264 -> 0.000 mm` off-45): http://localhost:8065/squaring/pcbench_stubby_base.svg vs http://localhost:8065/squaring/pcbench_stubby_new.svg
- `CAL430FR_CAL430F` (`B.Cu` staggered knees on `CS2` & `P1.4`, `bus_bundled` `1.9% -> 12.3%`): http://localhost:8065/squaring/pcbench_cal430fr_base.svg vs http://localhost:8065/squaring/pcbench_cal430fr_new.svg

---

### 3C. PCBench `--tier smoke` A/B (`sqbun-smoke-base` vs `sqbun-smoke-new`, 48 Boards)

Command: `python3 tools/pcbench.py ab sqbun-smoke-base sqbun-smoke-new --base-bin ~/Software/ultraroute-lab/bin/sq-2f44840/ultraroute --tier smoke -j 8`

| Metric | `sqbun-smoke-base` (`sq-2f44840`) | `sqbun-smoke-new` (`square-bundles`) | Delta / Verdict |
|---|---:|---:|---|
| **Verdict / Clean (of 48)** | **`48 / 48`** | **`48 / 48`** | **`PASS` (`lost 0`, `won 0`, `error types gained 0`, `failures 0`)** |
| **Time-limited boards** | **`0`** | **`0`** | `0` in every stage |
| **Off-45** | **`29.0%`** (`5,307.409 / 18,314.419 mm`) | **`28.9%`** (`5,295.093 / 18,313.788 mm`) | **`-12.316 mm`** (`R1002_R1002`: `78.407 -> 66.091 mm`, `19.0% -> 16.0%`) |
| **`bundled` / `bus_bundled`** | `2.5%` / `3.0%` | `2.5%` / `3.0%` | `451.456 -> 451.867 mm` / `18.311 -> 18.311 mm` |
| **Doglegs /m** | **`1.802 /m`** (`33` doglegs) | **`1.747 /m`** (`32` doglegs) | **`-1` dogleg (`-0.055 /m`)** (`R1002_R1002`: `1 -> 0`) |
| **Slots / Acute / Necks** | **`251`** / `22` / `2` | **`250`** / `22` / `2` | **`-1` slot** (`R1002_R1002`: `2 -> 1`, `more on 0`) |
| Turns `/10 mm` / Detour | `0.724` / `0.969` | `0.724` / `0.969` | `0.000` / `0.000` |
| Wirelength / Vias | `18,314.419 mm` / `462` | `18,313.786 mm` / `462` | `-0.632 mm` / `0` |
| **Router CPU s (all 48 / 45 group-free)** | **`71.5 s`** / **`65.5 s`** | **`71.7 s`** / **`65.8 s`** | **`+0.2 s` total** (`+0.3 s` = **`+6.7 ms/board`** across the 45 group-free boards) |

Smoke before/after SVG crop (`R1002_R1002` `PX2/PX3/PX4` on `F.Cu`):
- http://localhost:8065/squaring/pcbench_r1002_base.svg vs http://localhost:8065/squaring/pcbench_r1002_new.svg

### 3D. Stage-Level Overhead on Boards Without Groups (`--report`, Seed 0, Single Thread)

To verify `router-change-rules.md` point 3 (stage time on boards where case X does not occur), three group-free smoke boards were run with `--report`:

| Group-Free Smoke Board | `tightening` Base -> New (ms) | `squaring` Base -> New (ms) | `squaring` Bent |
|---|---:|---:|---:|
| `CompactFlashBreakout_CompactFlashBreakout` | `67.218 -> 68.218` (`+1.000 ms`) | `16.530 -> 17.104` (`+0.574 ms`) | `25 -> 25` |
| `SimpleCPLD_SimpleCPLD` | `145.821 -> 154.913` (`+9.092 ms`) | `50.630 -> 54.807` (`+4.177 ms`) | `51 -> 51` |
| `PiZeroHub_PiZeroHub` | `51.613 -> 51.626` (`+0.013 ms`) | `51.100 -> 52.544` (`+1.444 ms`) | `44 -> 44` |

The 2 mm grid scan in `find_groups` adds `0.6` to `4.2 ms` per `octilinear_watch` call when no group is present.

---

## Costs and Regressions

1. **`f4fc_f4fc` (`clean -> open 2` in `sqbun-grp`, CPU-clock deadline jitter in `prefanout`):**
   `f4fc_f4fc` was already `time_limited: ["prefanout", "pairs"]` on baseline (`62.5 s` CPU). In `sqbun-grp-new`, `prefanout` expired 1 escape earlier (`65/79` vs `66/79`, before `negotiation`, `tightening`, or `squaring` ever ran), causing `shove` to spend its full time slice (`117.9 s` CPU, `unconnected: 2`). This is standard CPU-clock deadline noise (PCBench's `noise` floor is `3` boards; overall `time_limited` boards in `sqbun-grp` improved `8 -> 6`), not a connection lost by `square_bundles`.
2. **Wirelength increase on octilinear bends (`+30.188 mm` = `+0.032%` on 64 clean group boards; `+18.358 mm` on `tqfp08_bus8`, `+13.049 mm` on `hdr_bus8`):**
   Replacing a straight Euclidean diagonal at `15-21 deg` (`detour ~ 0.91`) with an octilinear horizontal + 45-degree path (`detour = 1.00`) lengthens the path by `cos(theta) + (sqrt(2) - 1) sin(theta) - 1` (up to `+8.2%` at `22.5 deg`). This is inherent to 45-degree routing.
3. **Pipeline interaction between `Tightening` and `Vias` (`bobc_MS-F100` `+6.808 mm` off-45, `avr-fuser-32_hvpp` `+10.292 mm` off-45, `type5_type5` `28 -> 30` vias):**
   In `stages.rs`, `Tightening` calls `octilinear_watch` *before* `Vias`. When `Vias` drops `>= 1` via, `Vias` re-runs `tighten()` (`pull_tight` + `pull_taut`) without calling `octilinear_watch` again until `Squaring` (after `Cleaning`). On `bobc_MS-F100` and `avr-fuser-32_hvpp`, bending a pair in `Tightening` slightly altered which vias `reduce_vias_all` dropped and how the second `tighten()` pulled neighbouring chains taut, leaving `+6.8 mm` and `+10.3 mm` more off-45 length in `Squaring` (though `bobc_MS-F100` also dropped `3` slots, `34 -> 31`).
4. **Small local DFM / dogleg trade-offs on clean boards:**
   - **Doglegs (`363 -> 367`, `+0.041 /m`):** `try_bundle` forbids adding doglegs along each member's chain (`doglegs_after <= doglegs_before`), and `SHORT_KINK = 0.5 mm` rejects tiny Z-middle segments, but a Z middle segment between `0.5 mm` and `1.0 mm` (`DOGLEG = 1.0 mm` in `cleanup.rs`) joined to a neighbouring segment of the same heading after hinge merging or subsequent `Vias` re-tightening added `+2` doglegs on `stubby_stubby` and `+1` each on `arf154_arf154`, `MySensorIRBlaster_IR Blaster`, and `bobc_MS-F100` (`-1` on `kitspace_T32_ref`).
   - **Acute angles (`172 -> 172` net on clean boards; `+1` on `arf154_arf154`, `-1` on `medusa_medusa_cape`):** On `arf154_arf154`, squaring a segment that met a third T-junction branch of the same net left an `84.9 deg` angle against the unmoved off-45 T-branch (`try_way` and `try_bundle` check `leaves_slot` and `slants_into`, not T-junction angles against other chains of the same net).
   - **Slots (`1,274 -> 1,272` net on clean boards; `+1` on `CAL430FR_CAL430F` and `Lys_Lys`, `-3` on `bobc_MS-F100`, `-1` on `medusa_medusa_cape`):** `leaves_slot` checks `pieces` against same-net copper within `1.0x` board clearance, whereas `tools/dfm.py` checks `max(board_min_clearance, 0.1 mm)` after `write`'s pad/joint mends.

---

## What Did Not Work and Why

1. **The 5-net `R4..R8 -> U1` bus on `esp_hat_esp_hat` (`73.11 mm` off-45, refused cleanly):**
   Running `ULTRAROUTE_SQUARE_TRACE=95.0,101.0` revealed why all 2-direction octilinear families fail on `esp_hat_esp_hat` (`example0_esp_hat_5net_fcu.svg`):
   - **Horizontal -> 45 deg -> Horizontal (Family A):** `R4..R8` are vertical SMD pads arranged in a *horizontal* row (`y ~ 98.2..99.0 mm`, pitch `2.0 mm`). Leaving `R4` horizontally to the left immediately crosses `R5`'s pad (`gap = 34..183 um < 201 um`).
   - **45 deg -> Horizontal -> 45 deg (Family B):** At `U1` (`x = 86.32 mm`), the 5 pads sit in a *vertical* column at `0.50 mm` pitch (`0.25 mm` tracks, `0.20 mm` clearance). Entering `U1` at `45 deg` compresses perpendicular centre-to-centre spacing to `0.50 / sqrt(2) = 0.3536 mm`, leaving only `0.0996 mm` edge-to-edge gap (`< 0.200 mm` clearance).
   - **2-piece `diag_first` (-135 deg from `R4..R8`, then horizontal into `U1`):** Geometrically fits both the channel and `U1`'s `0.50 mm` vertical pitch (`bundle.rs` generated and tried it), **but `leaves_slot` refused `net 17`** because leaving `R4`'s vertical rectangular pad at `-135 deg` creates a pad-rim acid trap (`leaves_slot` / `slants_into`).
   - **Why:** Squaring `esp_hat_esp_hat` requires a **3-direction `vertical -> -135 deg -> horizontal` way** (leaving `R4..R8` vertically along `+y`, bending `45 deg` down-left in staggered parallel diagonals, then turning horizontal `-x` into `U1`), whereas both `octilinear_ways` and `bundle.rs` only combine **one** cardinal axis with **one** diagonal per replacement.
2. **Corpus ceiling of post-hoc bundle squaring (`0.87%` single-ended group share):**
   Upstream A* negotiation (`route.rs`) and `pull_taut` optimize each net independently, fanning bus nets onto slightly different angles (`> 1 deg` apart) or splitting adjacent bus pins across `F.Cu` and `B.Cu`. Post-hoc squaring can only coordinate buses that arrived at `octilinear_watch` on the same layer within `1 deg` and `3x` clearance.

---

## What Was Not Done / Not Verified

- **Full 315-board `gate` A/B was not run:** Following the task instructions (`--only ID,ID,...` on the boards with groups, then `--tier smoke`), and because uncommitted worktree builds must run `--here` (`<= 10` cores) while `sequoia-server` had no free space earlier today, we ran the full **76-board group subset from `gate` + `sq-new1`** (`sqbun-grp-{base,new}`) and the **48-board `smoke` tier** (`sqbun-smoke-{base,new}`), plus 3 synthetic 8-bit bus boards and full workspace unit/integration tests, rather than re-running the remaining ~240 group-free `gate` boards.

---

## What to Try Next

1. **Orthogonal-end 3-direction bundle ways (`Axis_1 -> 45 deg -> Axis_2`):** When start pads sit in a horizontal row (`R4..R8` on `esp_hat_esp_hat`) and end pads sit in a vertical column (`U1`), generate a short vertical escape stub out of each start pad into a staggered 45-degree bus trunk and horizontal entry into the destination pads. That directly unlocks `esp_hat_esp_hat`'s `73.11 mm` 5-net bus without triggering `leaves_slot` or `slants_into`.
2. **Raise `SHORT_KINK` in `bundle.rs` from `0.5 mm` (`500_000 nm`) to `1.0 mm` (`DOGLEG`):** Matching `cleanup.rs`'s `1.0 mm` `DOGLEG` constant would eliminate the `+4` dogleg creep (`363 -> 367`) from sub-millimetre middle Z segments.
3. **Check acute angles at T-junction endpoints:** Reject ways that meet an existing segment of the same net outside `chain` at `< 89 deg` (fixes the `+1` acute angle on `arf154_arf154`).
4. **Upstream bus affinity in negotiation / `pull_taut`:** The real leverage for `bus_bundled` on PCBench (`8.1%` vs `> 90%` on synthetic buses) is keeping bus nets on the same layer and hugging neighbours during A* negotiation (`tidy-parallel-goal.md`), after which `grid::bundle::square_bundles` will already be in place to square them together.

---

## For the Project Memory

- **Corpus share of jammed parallel off-45 groups (`sqg-2f44840`, 315 gate boards):** `3.40%` (`2,759.61 mm`) of off-45 length is in `2+`-net parallel groups (`<= 1 deg`, `<= 3x` clearance), but **`2.53%` (`2,050.49 mm`, `242` groups) is coupled differential pairs** (excluded from `soft` in `Tightening`). Only **`0.87%` (`709.12 mm` piece / `488.30 mm` overlap, `116` groups on `70` boards)** is single-ended (`1.94%` / `4,198.27 mm` before squaring in `srv-tidy-gm`), because negotiation/taut separates single-ended bus nets in angle or layer.
- **Orthogonal pad orientations block 2-direction Z-ways on buses (`esp_hat_esp_hat`):** When a bus connects a horizontal row of vertical SMD pads (`R4..R8`) to a vertical column of fine-pitch horizontal SMD pads (`U1` at `0.50 mm` pitch), neither `H -> 45 -> H` (hits neighbour pad) nor `45 -> H -> 45` (`0.50 / sqrt(2) = 0.354 mm` pitch violates clearance at `U1`) nor `45 -> H` (`leaves_slot` at rectangular pad rim) works; squaring requires a **3-direction `V -> 45 -> H` way**.
- **`Tightening` vs `Vias` squaring gap (`stages.rs`):** `Tightening` runs `octilinear_watch` before `Vias`, and when `Vias` drops vias (`v.dropped > 0`), `Vias` re-runs `tighten()` without `octilinear_watch`, deferring squaring until `Squaring` after `Cleaning`. Changes inside `octilinear_watch` therefore slightly perturb `reduce_vias_all` on boards that drop vias.
- **`tools/tidy.py` `--clearance` default:** Standalone `tidy.py` defaults `--clearance` to `0.10 mm` unless `--clearance <mm>` is passed (`pcbench.py` passes the board's rule clearance from `bare.json`); on `0.20 mm`-clearance boards, `bundled` and `bus_bundled` read `0.0` unless `--clearance 0.20` is supplied.
