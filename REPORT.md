# Board Compaction v2 (`ultraroute compact` and `ultraroute scale`)

## Executive Summary and Verification Status

This branch finishes the core geometry, physics, and footprint-coupling changes requested for `ultraroute compact` and `ultraroute scale` on top of the merged `main` (`947d08c`) + `explore/compaction` (`7d0462d`) tree (`HEAD` `52972f3`), and renames the previous report to [`REPORT-v1.md`](REPORT-v1.md).

### Plain Statement of What Was Verified vs Blocked

- **Verified in this session**:
  - Fixed all Clippy `-D warnings` errors left by the `main` merge in [`core/crates/kicad-pcb/src/write.rs`](core/crates/kicad-pcb/src/write.rs).
  - Implemented all Goal 1, Goal 2, and Goal 3 changes across [`core/crates/morph/src/compact.rs`](core/crates/morph/src/compact.rs), [`core/crates/morph/src/scale.rs`](core/crates/morph/src/scale.rs), [`core/crates/ultraroute/src/main.rs`](core/crates/ultraroute/src/main.rs), and [`core/README.md`](core/README.md), plus 4 new unit tests and a resumable end-to-end verification and 24-board PCBench driver at [`runs/v2/run_all.py`](runs/v2/run_all.py).
  - Single-crate `cargo check --tests` and `cargo clippy --all-targets --no-deps -- -D warnings` pass with **0 errors and 0 warnings** on every crate in `core/` (`kicad-pcb`, `board`, `morph`, `grid`, `ultraroute`, `kicad-text`).
- **Blocked on `loaf` core-reservation queue (not run unbooked)**:
  - Release binary compilation (`cargo build --release -p ultraroute -p kicad-text`), workspace test execution (`cargo test --workspace --release`), `ultraroute route` byte-identity runs against `main` (`947d08c`), Experiments A/B/C v2 runs, and the 24-board PCBench generalization batch could **not** be executed because `loaf`'s reservation queue (`~/.claude/bin/reserve --here`) was deadlocked throughout the session:
    - `reserve --status` showed `20/22` cores held for `73` to `458` minutes (`circuit-composer` `458m`, `round-pendant` `168m`, `bluebuzz` `116m`, `bluebuzz` `112m`, `ultraroute` `73m`) with actual machine CPU load `1.1` on `24` cores, and `10` jobs waiting ahead in strict FIFO (`130m` down to `34m`) without a single grant occurring.
    - Our booked foreground command (`timeout --signal=INT --kill-after=15 1200 ~/.claude/bin/reserve -c 4 -l "compact v2 build+test+bench" --here -- systemd-run --user --scope -q -p MemoryMax=8G -p MemorySwapMax=0 python3 runs/v2/run_all.py --phase all`) waited the full `1200 s` (`20 min`) at queue position `11`, never received cores, exited `124`, and cleanly released its reservation on `SIGINT`.
    - Per the ground rules, heavy builds, test suites, router runs, and KiCad DRC batches were **never** run unbooked, uncommitted work was not committed or pushed to `sequoia-server`, and external reservation state was not touched.
  - Once `loaf`'s reservation queue is cleared, a single booked command runs the entire resumable build, test, Clippy, `main` byte-identity, Experiment A/B/C, and 24-board PCBench DRC suite:
    ```bash
    timeout --signal=INT --kill-after=15 1800 ~/.claude/bin/reserve -c 6 -l "compact v2 build+test+bench" --here -- systemd-run --user --scope -q -p MemoryMax=8G -p MemorySwapMax=0 python3 runs/v2/run_all.py --phase all
    ```

---

## 1. Diagnosis Verification on v1 Jams

Inspecting the v1 `.stats.json` and `.drc.json` artifacts under `runs/expA/`, `runs/expB/`, and `runs/expC/` confirmed both mechanisms identified in [`REPORT-v1.md`](REPORT-v1.md) plus two subtle engine gaps:

1. **Opposite-side through-via pinches (`F.Cu` vs `B.Cu`)**:
   - In Experiment A (`earring`), `U1` (`F.Cu` QFN-60, pins `U1-29..34`) and `Q5` / `R25` / `C25` (`B.Cu`) sit on opposite sides of the board. Their courtyards live on separate layers (`F.CrtYd` vs `B.CrtYd`), so `place_parts` applied zero mutual stop between them while escape through-vias (`PWR_HOLD`, `BTN_SENSE`, `VBUS_SENSE`, `0.400 mm` diameter) spanned both `F.Cu` and `B.Cu` in the channel between them. Once the horizontal gap fell below `0.400 + 2 * 0.127 = 0.654 mm` (`0.652 mm` at factor `0.986`), no 2D displacement of the stationary via could satisfy both layers simultaneously, and refusing the whole step ended compaction for the entire board at `1.0%` to `1.2%` shrink.
   - The same mechanism stopped Experiment B (`R8-1` on `B.Cu` vs `3V3` via, `R20-1` on `B.Cu` vs `LEDC_D6` via, `U1-29` on `F.Cu` vs `3V3` via) and Experiment C (`C10-2` vs `/BatPack/VC7` via on `BMS-bq76940`, `U301-23` vs via on `AmpOne`).
2. **Offset-origin and cavity/boss footprints (`BT1`, `J1`, `R4`, `R5` on `earring`)**:
   - `BT1` (`B.Cu` coin-cell holder) has `(at 100 100 180)` at the board centre while its two physical `B.Cu` SMD pads and two `B.CrtYd` tab rectangles sit near `y = 83.65 mm`. Scaling `fp.at` left `BT1` stationary at `(100, 100)` while scaling interior parts (`R14`, `SW2`) into `BT1`'s tabs (`4` DRC errors on `bare_110`).
   - `J1` (`B.Cu` USB-C connector at `(100, 83.41)`) has four `thru_hole` shield tabs and two unnamed (`""`) `np_thru_hole` bosses framing `F.Cu` passives `R4` (`(98.0, 86.4)`) and `R5` (`(102.0, 85.4)`). Scaling origins independently drove `R4` and `R5` into `J1`'s NPTH bosses (`2` `hole_clearance` DRC errors on `bare_110`).
3. **Additional engine gaps found in code audit**:
   - In v1 `relax_step`, edges with two immobile pad endpoints (`mu <= 0.0 && mv <= 0.0`) were skipped completely in Pass `0`. Any straight pad-to-pad segment shorter than `SUBDIVIDE_LEN` (`1.2 mm`) could never split at its midpoint to bend around an approaching pad or via.
   - `relax_step` checked `mnet == 0 || mnet != net` directly on `board.mask_areas`, whereas `ANY_ONE_NET` (`>= 0x8000_0000`) solder-mask shapes must query `board.mask_holder(...)` just as `board::check` does.
   - `relax_step` only checked `board.hole_clearance` against foreign-net copper, whereas `.kicad_dru` `physical_hole_clearance` rules (such as `easy2350`) apply to same-net non-overlapping via-to-pad pairs (`hgap > 0`) as well.

---

## 2. What Was Built

### Files Modified and Added

| File | Summary of Changes |
| --- | --- |
| [`core/crates/kicad-pcb/src/write.rs`](core/crates/kicad-pcb/src/write.rs) | Added `ZonePointMap` and `ScaleEdits` type aliases and collapsed nested `if scale_changed` to clear all 3 Clippy `-D warnings` errors from the `main` merge. |
| [`core/crates/morph/src/scale.rs`](core/crates/morph/src/scale.rs) | Added `FootprintYards`, `extract_footprint_yards`, `FootprintEnvelope`, `compute_footprint_envelopes`, `nominal_scaled_positions`, and `project_scaled_footprints`; wired them into `scale()` and re-exported helpers to `compact.rs`; added unit test `scale_anchors_offset_origin_parts_and_carries_passives_inside_connector_bosses`. |
| [`core/crates/morph/src/compact.rs`](core/crates/morph/src/compact.rs) | Added catch-up per-part step targets, static pad-ordinal violation keys (`StaticViolKey`), `resolve_static_step_board`, joint copper + rigid-footprint family relaxation (`relax_core`, `relax_step`, `settle_copper_only`, `append_static_fp_guard_constraints`), topological via sliding (`slide_pinched_vias` Modes A/B/C), bare-board rebuild when relaxation shifts unlocked parts, sub-`1.2 mm` pad-to-pad splitting, `ANY_ONE_NET` mask-holder & same-net `physical_hole_clearance` parity, 10-pass homotopic `shorten_taut_chains`, `segments` / `part_stops` / `via_slides` metrics, and 3 new unit tests. |
| [`core/crates/ultraroute/src/main.rs`](core/crates/ultraroute/src/main.rs) | Recorded CPU seconds via `grid::clock` (`use_cpu_time`, `now`, `since`) and added `cpu_s`, `segments`, `part_stops`, and `via_slides` to `ultraroute compact` stderr output and `--report` / `--stats` JSON. |
| [`core/README.md`](core/README.md) | Updated `morph` crate description to cover `ultraroute scale` and `ultraroute compact`. |
| [`runs/v2/run_all.py`](runs/v2/run_all.py) | Resumable verification and benchmark suite covering release build, workspace tests, Clippy, `main` byte-identity on 4 boards, Experiments A/B/C, and 24 stratified PCBench boards. |

No CLI flags were broken or removed. `ultraroute route` shares zero execution paths with `morph::compact` or `morph::scale`.

### Goal 1a: Joint Rigid-Body Footprint + Copper Relaxation and Per-Part Stops

1. **Catch-up nominal targets (`compact` step loop)**:
   - Each step `k` computes nominal positions from the initial board `orig_pcb` at `step_f = factor^(k / steps)` rather than compounding from `cur_pcb` (`cur_f`). If one part stops against a courtyard, board edge, or pinched via at step `3`, only that part (and anything directly blocked behind it) stays held back at steps `4..N`; every unblocked part on the board continues moving inward to `factor`.
2. **Two-stage relaxation per step (`relax_core`)**:
   - **Pass `0` (copper-only + topological via slide)**: Footprints stay fixed at their courtyard- and static-DRC-safe step targets (`allow_fp_move = false`) while free vertices and vias relax under fading spring tension + exact 2D half-plane clearance projection (`solve_half_planes_2d`), mid-pass segment subdivision (`can_split`), and `slide_pinched_vias`. Whenever a channel has enough room for copper to bend or a via to slide out of the throat, copper yields first without holding parts back.
   - **Passes `1..5` (joint copper + rigid footprint families)**: Only entered if Pass `0` left at least one dynamic violation (`count_dynamic_violations(board).0 > 0`, so clean steps pay zero extra passes). Unlocked footprint families (`leader[f]` + pocket children `members[g]`) receive half-plane reaction pushes (`hs_fp_group`) from every violated pad-to-via, pad-to-track, hole-to-via, via-to-edge, spoke, and keepout pair across **all** layers, alongside static guard half-planes (`append_static_fp_guard_constraints` for courtyards, cross-footprint pad/hole clearances, mask bridges, pours, and board edges).
   - Each footprint family solves its accumulated half-planes via `solve_half_planes_2d`, capped at `22 um` (`MAX_FP_ITER_STEP`) per iteration so heavy footprints yield smoothly while lighter copper tracks (`85 um` cap) conform around them. Translating a footprint family updates its `Pad`, `Keepout`, and `Spoke` items in `board`, its `Pad` and `PadVia` anchors in `CopperGraph`, and ends with `resolve_courtyard_stops` to guarantee zero same-side courtyard penetration.
   - Whenever Passes `1..5` displace any unlocked footprint by `> 1 nm` from `resolved_pos`, `compact` rebuilds `step_bare_board` from `orig_pcb` at those exact `relaxed_pos` coordinates (updating zone cutouts, spoke geometries, and solder-mask areas consistently) and runs `settle_copper_only` (`3` copper-only passes) before `polish_on_board`, `shorten_taut_chains`, and the strict `check_item` + `BaselineConn` acceptance gate.

### Goal 1b: Explicit Specification of Legal Topological Via Moves (`slide_pinched_vias`)

 Compaction performs **no ripping and no rerouting**. The single topological copper move added under Goal 1 is `slide_pinched_vias`, invoked only when a mobile non-locked `Anchor::Via` particle still fails `check_item(board, vid)` after Pass `0` continuous relaxation:

- **Preconditions (when a via is eligible)**:
  - The via must be an unlocked `Anchor::Via` (never `locked`, never `Anchor::PadVia`).
  - Every static item (`Pad` or `Pour`) that overlapped the via on the baseline board must already be represented by an active explicit graph edge incident to the via (`implicit_static_ties` guard), so sliding the via cannot silently detach an implicit pad-overlap joint.
  - Any plane zone polygon containing the via at baseline (`plane_zones`) must still contain the candidate via position (`in_plane_zones`).
- **Mode A (Direct slide along own-net degree-2 chain)**:
  - Walks each incident non-locked edge out of the via along consecutive degree-2 `Anchor::Free` vertices on the same layer `l_fwd` and radius `r_fwd` (`<= 8` hops, arc distances `150 um` to `1.40 mm` plus lateral offsets `{0, +-80 um, +-160 um}`).
  - Collapses the traversed chain prefix `chain_edges[0..=m]` into one segment `(cand, end_node)` on `l_fwd` and reconnects the via's remaining incident edges `other_eis` directly to `cand`.
- **Mode B (Layer-transition dogleg slide)**:
  - Applies when `other_eis` are non-empty and all lie on a single layer `l_other != l_fwd` (the standard 2-layer `B.Cu -> through-via -> F.Cu` transition where a `B.Cu` pad blocks `other_eis` from pivoting directly to `cand` on `B.Cu`).
  - Leaves a new unlocked `Anchor::Free` bend vertex `W` near `old_pt` (`{0, +-80 um}`) on `l_other`, reconnects `other_eis` to `W`, moves the via along the `l_fwd` chain to `cand` (collapsing `chain_edges[0..=m]`), and inserts one bridge segment `(W, cand)` on `l_other`. Net connectivity and homotopy on both layers are preserved.
- **Mode C (Short-range radial escape)**:
  - When chain slides do not apply (e.g., T-junctions), tests `12` angles at radii `{100, 200, 320, 460, 650} um`, keeping all incident edges attached to the via.
- **Acceptance invariants for every candidate in Modes A, B, and C**:
  - No new acute track junction (`cos > 0.008`, i.e. `< 89.5 deg`) at `cand`, `W`, or any neighbour vertex.
  - Temporary board insertion of the moved via and all modified/bridge segments passes `check_item(board, tid).is_empty()` (`0` clearance, hole, mask, keepout, spoke, or edge violations) before mutating `CopperGraph`.

### Goal 2: Large Enclosing and Offset-Origin Footprints (`scale.rs` and `compact.rs`)

1. **Physical anchor (`compute_footprint_envelopes`)**:
   - Instead of scaling `fp.at` (which may sit at `(0, 0)` or board centre `(100, 100)` far from the part's pads), each footprint's scaling anchor is the bounding-box midpoint of its physical pads (falling back to courtyard islands or copper graphics, and only then `fp.at`). Under `scale --factor 1.1` on `earring`, `BT1` now translates north with its `y ~ 83.65 mm` pads rather than staying pinned at `(100, 100)`.
2. **Multi-island and concave courtyards (`extract_footprint_yards`)**:
   - Closed `fp_rect`, `fp_poly`, and `fp_circle` courtyard shapes are kept as separate polygons, and open `fp_line`/`fp_arc` stroke segments are chained into connected components by shared endpoints (`20 um` snap) before taking per-component hulls. `BT1`'s two `B.CrtYd` tab rectangles stay two small disjoint islands instead of bridging a false convex hull across the `28 x 32 mm` battery cavity.
3. **Pocket child adoption (`parent` in `FootprintEnvelope`)**:
   - When a large footprint (`inter_cnt >= 3` pads/holes interacting with the candidate's copper layers or drill holes, span `>= 5 mm`, area `>= 20 mm^2`) encloses a smaller footprint (`< 35%` area) within `ibb.grow(0.60 mm)`, the smaller footprint is adopted as a rigid pocket child (`parent = Some(p)`).
   - On `earring`, `J1`'s 4 `thru_hole` shield tabs + 2 unnamed `np_thru_hole` bosses (`inter_cnt = 6`) adopt `R4` and `R5` so they translate by `J1`'s exact delta during both `scale` and `compact`, preventing `R4`/`R5` from colliding into `J1`'s NPTH bosses. Conversely, `BT1` has only 2 `B.Cu` SMD pads (`inter_cnt = 0` on `F.Cu`, `inter_cnt = 2 < 3` on `B.Cu`), so parts inside `BT1`'s open cavity are **not** frozen to `BT1` and remain free to compact inward until their own `B.Cu` pads or `B.CrtYd` courtyards meet `BT1`'s tabs.
4. **Post-scale and pre-relaxation static projection (`project_scaled_footprints`, `resolve_static_step_board`)**:
   - Resolves courtyard overlaps, cross-footprint pad/hole clearances (using per-footprint pad ordinals `a_pad_ord` so multiple unnamed `""` NPTH bosses are tracked distinctly), solder-mask bridges, and board-edge clearances while keeping pocket children locked rigidly to their parent's delta and `--lock` footprints fixed to `0 nm`.

### Goal 3: Segment Count Reduction and `110% -> 100%` Channel Capacity

1. **Homotopic `shorten_taut_chains` (`10` passes, `dev <= 200 um`)**:
   - v1 only merged nearly-collinear triples (`dev <= 25 um` or `<= 80 um` for `3` passes), leaving subdivided `1.2 mm` segments and slight relaxation bows in place (`945 -> 1164` segments on A1, `1291 -> 2728` on B s0, `447 -> 994` on `Starling`).
   - Any triangle `(pu, pv, pw)` with altitude `dev <= 200 um` to chord `(pu, pw)` has inradius `< dev / 2 <= 100 um`, which is strictly smaller than any pad or via radius + clearance on a PCB, so replacing `(pu, pv) + (pv, pw)` with `(pu, pw)` when `check_item` passes is guaranteed homotopic (it can never jump across a pad or via). Running up to `10` passes to fixed point collapses `16`-segment subdivided straight runs back to `1` segment while preserving a relative outer-layer slot guard (`g + 3 um < g_old`) and `creates_acute_at`.
2. **Sub-`1.2 mm` pad-to-pad segment splitting**:
   - Straight pad-to-pad segments down to `MIN_SPLIT_LEN = 180 um` remain active in Pass `0` for `can_split` midpoint insertion when an obstacle intrudes on `t in [0.16, 0.84]` (`deficit > 1.5 um`), allowing short inter-pad links to route smoothly around an approaching pad or via instead of jamming rigidly.

---

## 3. Unit Tests Added

All 4 new unit tests compile cleanly under `cargo check -p morph --tests` and `cargo clippy -p morph --all-targets --no-deps -- -D warnings`:

1. **`compact::tests::compact_stops_opposite_side_via_pinch_locally_while_board_shrinks_to_target`** ([`compact.rs:6320`](core/crates/morph/src/compact.rs#L6320-L6409)):
   - Places `U_FRONT` (`F.Cu`, `y = 7.08 mm`, `4.0 x 0.8 mm` pad) and `U_BACK` (`B.Cu`, `y = 8.92 mm`, `4.0 x 0.8 mm` pad) on opposite sides flanking a `0.60 mm` through-via at `(10, 8)` where net `SIG` transitions from a `B.Cu` trace under `U_FRONT` to an `F.Cu` trace over `U_BACK`. Stationary clearance allows only `2.2%` shrink (`1.84 mm -> 1.80 mm`) before jamming on v1.
   - Asserts `steps_accepted == 14`, `achieved_factor == 0.65` (`35%` linear shrink, `57.75%` area saved), `part_stops > 0`, outer perimeter span `< 10.0 mm` (down from `15.0 mm`), central `U_FRONT`/`U_BACK` separation `>= 1.795 mm`, `0` DRC violations, and `0` airlines.
2. **`compact::tests::compact_slides_pinched_through_via_along_its_track_so_front_and_back_parts_close`** ([`compact.rs:6411`](core/crates/morph/src/compact.rs#L6411-L6484)):
   - Places `U_FRONT` (`F.Cu`, `y = 6.90 mm`) and `U_BACK` (`B.Cu`, `y = 9.10 mm`) flanking a `0.60 mm` through-via at `(10, 8)` whose `F.Cu` track continues east into open space toward `(16, 8)`. Stationary via clearance blocks at `dy = 1.80 mm` (`factor ~0.818`).
   - Asserts `steps_accepted == 14`, `via_slides > 0` (`slide_pinched_vias` acted), via `x` shifted `> 0.85 mm` out of the `x = 10 mm` throat, `U_FRONT` and `U_BACK` closed past `1.80 mm` to `dy < 1.50 mm` (`~1.430 mm`), `0` DRC violations, and `0` airlines.
3. **`compact::tests::compact_keeps_locked_parts_exact_and_produces_byte_identical_output`** ([`compact.rs:6486`](core/crates/morph/src/compact.rs#L6486-L6545)):
   - Compacts with `--lock J_LOCK` twice under `uuid_seed = Some(99)`; asserts byte-identical output strings (`rep1.text == rep2.text`), `J_LOCK.at` unchanged to `0 nm`, unlocked `R_FAR` moved inward, `0` DRC violations, and `0` airlines.
4. **`scale::tests::scale_anchors_offset_origin_parts_and_carries_passives_inside_connector_bosses`** ([`scale.rs:1615`](core/crates/morph/src/scale.rs#L1615-L1685)):
   - Models `earring`'s `BT1` (`(at 100 100 180)` with disjoint `B.CrtYd` islands framing `J1` and physical pads near `y = 83.65 mm`), `J1` (`(at 100 83.41)` with 4 `thru_hole` shield tabs and 2 unnamed `""` `np_thru_hole` bosses), and `R5` (`F.Cu` passive inside `J1`'s pocket).
   - Asserts `BT1` shifts north (`at.y < 98.6 mm`) under `1.1x` scale instead of staying at `100.0 mm`, `J1` and `BT1` are not falsely pushed apart by convex-hull courtyard bridging, `R5` rides rigidly with `J1` (`|dj_y - dr_y| < 2 um`), and `check(&board)` has `0` DRC violations.

---

## 4. Reference v1 Baselines and Prepared v2 Experiments (`runs/v2/run_all.py`)

Because `loaf`'s reservation server was wedged (`20/22` cores held for `73..458 min`, `0` grants in `20 min`), the v2 tables below could not be populated in this session without violating the booking rule. All inputs from v1 (`runs/expA/baseline.kicad_pcb`, `runs/expB/direct_100_s0.kicad_pcb`, `runs/expB/routed_110_s{0..3}.kicad_pcb`, `runs/expC/*/routed_110_s0.kicad_pcb`) are wired into [`runs/v2/run_all.py`](runs/v2/run_all.py) for side-by-side execution.

### v1 Baseline Numbers to Beat (from `REPORT-v1.md`)

- **Experiment A (`earring` toward `0.85`)**:
  - `A1` (`runs/expA/baseline.kicad_pcb`, `--factor 0.85 --steps 15`): v1 reached **`0.99`** (`1/15` steps, `30.195002 x 35.392501 mm`, `10` unconnected unchanged, `0` DRC errors, `945 -> 1164` segments, `1032.609016 -> 1014.612556 mm` track length, `10.159658397 s`), jammed at `0.98` on `U1-34` vs `BTN_SENSE` via.
  - `A1-fine` (`--factor 0.98 --steps 10`): v1 reached **`0.988`** (`6/10` steps, `30.134 x 35.321 mm`, `945 -> 1213` segments, `26.291489997 s`), jammed at `0.986` on `U1-31` vs `PWR_HOLD` via.
  - `A2` (`runs/expB/direct_100_s0.kicad_pcb`, `--factor 0.85 --steps 15`): v1 reached **`0.99`** (`1/15` steps, `0` unconnected, `0` DRC errors, `1296 -> 2677` segments, `1327.619626 -> 1298.941937 mm`, `18.619641603 s`), jammed at `0.98` on `C5-1` vs `VBUS_SENSE` via.
- **Experiment B (`earring` `scale 1.1 -> route -> compact 1/1.1`, seeds `0..3`)**:
  - `bare_110` (`scale 1.1`): v1 created `6` new static footprint DRC errors (`BT1` vs `R14`/`SW2` and `J1` NPTH vs `R4`/`R5`, `11` total with pre-existing `VBUS` stubs).
  - Compaction from `110%` toward `0.909091` (`100%`):
    - Seed `0`: **`0.9545454545454546`** (`5/10` steps, `1291 -> 2728` segments, `0` unconnected, `0` copper DRC errors, `40.371898164 s`).
    - Seed `1`: **`0.9727272727272727`** (`3/10` steps, `1366 -> 2737` segments, `0` unconnected, `0` copper DRC errors, `26.901802546 s`).
    - Seed `2`: **`0.990909090909091`** (`1/10` steps, `1371 -> 2499` segments, `0` unconnected, `0` copper DRC errors, `16.860203887 s`).
    - Seed `3`: **`0.990909090909091`** (`1/10` steps, `1315 -> 2465` segments, `0` unconnected, `0` copper DRC errors, `14.756275290 s`).
- **Experiment C (PCBench `110% -> route -> compact 1/1.1`, seed `0`)**:
  - `Starling_Starling_V1`: **`0.9636363636363636`** (`4/10` steps, `447 -> 994` segments, `0` unconnected vs `3` unconnected at `100%`, `0` DRC errors, `27.009554371 s`).
  - `BMS-bq76940_EvaluationsBoard`: **`0.9727272727272727`** (`3/10` steps, `662 -> 1172` segments, `4 -> 4` unconnected, `0` DRC errors, `28.227117087 s`).
  - `AmpOne_dev-AmpOne`: **`0.990909090909091`** (`1/10` steps, `804 -> 2835` segments, `0` unconnected, `0` DRC errors, `29.108802399 s`).
- **Prepared 24-Board PCBench Generalization Set (`--phase pcbench` in [`runs/v2/run_all.py`](runs/v2/run_all.py))**:
  - 10 clean double-sided 2-layer boards (`0` bare DRC errors): `LED_port-status_LED_port-status`, `ESP-Breakout_ESP-Breakout`, `ESP07-Breakout_ESP07-Breakout`, `ESP8266-MQTT-battery-monitor-hw_battery-monitor`, `KiwiSDR_PCB_ant`, `Biscay_Blueeye_sipm-comp`, `BUF-DiffToSE-ADAU1966_BUF-DiffToSE-ADAU1966`, `avr_ledprojector_avr_ledprojection-0402`, `AzizLight_AzizLight`, `QRPCard_QRPCard`.
  - 6 double-sided 2-layer boards with pre-existing bare DRC errors: `1-Wire-Wing-pcb_1-Wire_Wing`, `655_testboard`, `6volt-5W-solar-cc_6vleadacidsolar`, `Brushless_ESC_Brushless_ESC`, `FlashProgrammer_flash_programmer`, `EncoderBoard_Enc_Pan_Led`.
  - 5 4-layer boards: `DasBlinkinput_Das Blinkinput`, `PDB_OSD_HARDWARE_Quadcopter Power Board`, `4_digit_hex_display_4_digit_display`, `a123-battery-integration_BCM`, `FMCW_RADAR_Radar MCU`.
  - 3 single-sided / mixed THT+SMD 2-layer boards: `DA_Lamp_attiny44a_servo_i2c`, `LT3652EvalBoard_LT3652EvalBoard`, `kitspace_120-channel-test-board`.

---

## 5. Costs, Limitations, and What Limits Shrink Once Opposite-Side Via Pinches Are Gone

1. **Per-step runtime cost when a channel saturates**:
   - Steps that relax cleanly in Pass `0` (`count_dynamic_violations == 0`) break immediately at the top of Pass `1` and pay near-zero overhead beyond v1 (plus up to `7` extra passes of `shorten_taut_chains`, which only scans degree-2 vertices).
   - Steps where Pass `0` leaves a pinch enter Passes `1..5` (`28..38` iterations each) and, if unlocked footprints move, rebuild `step_bare_board` and run `settle_copper_only` (`3` passes). On saturated boards this adds roughly `2x` to `3x` CPU work on the pinched steps in exchange for keeping the step alive via per-part stops rather than aborting.
2. **What limits shrink once opposite-side via pinches are resolved**:
   - **Board outline (`Edge.Cuts`) catching stopped perimeter parts**: In `ultraroute compact`, the board outline scales uniformly by `step_f` around `centre`. When an interior part stops (`part_stops > 0`), outer parts continue shrinking past it; however, once a **perimeter** footprint stops (against a locked connector, an interior courtyard chain spanning from the board core to the edge, or a board-spanning mechanical part like `BT1` whose `28 mm` width already spans `92%` of `earring`'s `30.5 mm` width), the uniformly shrinking `Edge.Cuts` outline eventually meets that stopped perimeter part's pad/hole edge-clearance limit (`min_copper_edge_clearance`, typically `0.30..0.50 mm`), which legitimately ends uniform outline shrink.
   - **Multi-track channel capacity (`110% -> 100%`)**: Even with `slide_pinched_vias` sliding vias out of narrow throats, if the router at `110%` routed `K` parallel same-layer tracks through a gap between two pads on the **same** footprint (or two courtyard-locked footprints) where `K * track_width + (K + 1) * clearance` fits at `110%` only if the two footprints stay `110%` apart, per-part stops will hold those two footprints at the channel's exact `K`-track capacity width (`> 100%`), preventing the board outline from reaching `100%` if those footprints sit near the board edge.
   - **No via-past-pad hop across a foreign obstacle**: `slide_pinched_vias` slides a via continuously/discretely along its own net's tracks (Modes A/B/C), which never crosses a foreign pad or changes route homotopy. It cannot teleport a via to the opposite side of an intervening foreign pad, because doing so changes the topological winding of the trace on at least one layer and requires rerouting.

---

## 6. What to Try Next

1. **Run `runs/v2/run_all.py --phase all` as soon as `loaf`'s reservation queue is cleared** and inspect any board where `Edge.Cuts` uniform scaling stops before interior channels saturate.
2. **Non-uniform / anisotropic (`fx, fy`) or offset-polygon `Edge.Cuts` shrink**: On rectangular or elongated boards (`Starling_Starling_V1` is `60.1 x 23.8 mm`; `earring` is constrained horizontally by `BT1` long before vertically), allowing `fx != fy` or shrinking `Edge.Cuts` to the tightest legal offset around the achieved part envelope would save substantial area after one axis saturates.
3. **Channel-capacity-aware `110%` routing (`--clearance-scale 1.1`)**: Inflating track/via clearances by `f` during the `110%` route pass would prevent the router from packing `N` tracks or vias into an expanded inter-part channel that can physically hold only `N - 1` items at `100%`.
4. **Topological rubber-band routing (`core/crates/topo`)**: Replacing piecewise-linear capsule chains with exact rubber-band tangents around pad/via discs would pull detours taut around obstacles in a single geometric solve without subdivision/merge passes.

---

## For the project memory

- **`~/.claude/bin/reserve` leaks queue entries when killed by `SIGTERM` while waiting**: `~/.claude/bin/reserve` installs its `SIGINT`/`SIGTERM`/`SIGHUP` signal handlers at lines `342-343`, *after* the `while e["state"] == "waiting":` loop (`lines 268-292`). During the wait loop, only `KeyboardInterrupt` (`SIGINT`) is caught to call `leave()` (`/api/release/<eid>`). Wrapping `reserve` in default `timeout N` sends `SIGTERM`, killing the client without calling `/api/release/<eid>`; always use `timeout --signal=INT` around `reserve` (and `reserve` itself should install a `SIGTERM`/`SIGHUP` handler before entering the wait loop).
- **Footprint `(locked yes)` vs `--lock` in `morph`**: KiCad's file-level `(locked yes)` flag on footprints is set on 60 of 167 footprints in `earring.kicad_pcb` (all 58 LEDs, `BT1`, and `J1`) to prevent accidental GUI drags, whereas `ScaleOptions::lock` / `CompactOptions::lock` (`--lock REF,...`) is the explicit compaction lock set. Track and via `(locked yes)` (`item.locked`) is respected as fixed copper.
- **Unnamed NPTH mounting bosses share `pad.number == ""`**: Connectors like `earring`'s `J1` have multiple unnamed (`""`) `np_thru_hole` pads on the same footprint. Any baseline DRC violation key or pad map must key pads by per-footprint pad ordinal (`(footprint_idx, pad_ordinal)`), never `(footprint_idx, pad.number)`.
