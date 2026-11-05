# Board Compaction v2 (`ultraroute compact` and `ultraroute scale`)

## Executive Summary

Compaction v2 (code commit [`6a98397`](https://github.com/sequoia-hope/ultraroute/commit/6a98397) on `explore/compaction`, merged with `main` [`8b71912`](https://github.com/sequoia-hope/ultraroute/commit/8b71912)) has been compiled, debugged across iterative fix commits (`2bf3234`, `8ab60c3`, `a4fceaf`, `7487bae`, `16cdd97`, `9cd2a92`, `cdec619`, `5bee4a0`, `6a98397`), verified for `ultraroute route` byte-identity against `main` (`8b71912`), tested against `v1` (`7d0462d`), and benchmarked end-to-end on `sequoia-server` across Experiments A, B, and C plus a 24-board PCBench generalization suite.

Every feature rule holds across all runs without exception:
- **No ripping or rerouting**: `compact` preserves route homotopy; its sole topological move is [`slide_pinched_vias`](core/crates/morph/src/compact.rs#L3770-L4408) along a via's own net.
- **Zero added KiCad 9 DRC errors and zero added unconnected items — both before and after refilling zones** (`added_errors_pre_refill <= 0`, `added_unconnected_pre_refill <= 0`, `added_errors <= 0`, `added_unconnected <= 0` on every Experiment A/B/C and PCBench board).
- **Locked parts stay exact (`0 nm`) and output is byte-identical across repeated runs**: verified by unit test and empirically (`det = True`, identical SHA-256) on Experiments A, B, C, and all 24 PCBench boards.
- **Work bounded strictly by counts, never wall-clock time slices**: iteration, pass, and step counts determine all work; `cpu_s` is measured via `grid::clock` solely for reporting.
- **`ultraroute route` is byte-identical to `main` (`8b71912`)**: verified across 6 boards unmodified (`deadline_fired = false`) and on `earring` and `Starling_Starling_V1` with `Deadline::seconds` lifted to `1e6`.

### Headline Outcomes (`v1` vs `v2`)

- **Experiment A (`earring` -> `0.85` / `0.98`)**: `A1` advances from `v1`'s `0.99` (`1/15`, `1.00%` linear / `1.98%` area, `1,164` segs) to **`0.97` (`3/15`, `3.00%` linear / `5.91%` area, `1,001` segs, `0/10` pre- and post-refill DRC)**; `A1-fine` (`-> 0.98`, `10` steps) reaches **`0.98` (`10/10` steps, vs `v1` `6/10`)**; `A2` (`0` unconnected) advances from `0.99` (`2,677` segs, `148` acid traps, `2/9` pre-refill DRC) to **`0.98` (`2/15`, `2.00%` linear / `3.96%` area, `1,292` segs, `71` acid traps, `0/0` pre- and post-refill DRC)**.
- **Experiment B (`earring` `110% -> route -> compact 1/1.1`, seeds `0..3`)**: `v2 scale 1.1` eliminates **all `11` post-refill (`12` pre-refill) static DRC errors** of `v1 scale 1.1`; on `v1`'s `routed_110_s{0..3}` inputs, `v2` advances from `[5, 3, 1, 1]/10` to **`[9, 8, 9, 8]/10` steps (`101.00%` of `100%` size on Seed `0`)**; end-to-end `v2` achieves **`0/0` pre- and post-refill DRC across all `4` seeds** (`[4, 9, 7, 7]/10` steps, reaching **`0.9181818181818182` = `101.00%` of `100%` size**, `15.69%` area saved).
- **Experiment C (PCBench `110% -> route -> compact 1/1.1`)**: **`BMS-bq76940_EvaluationsBoard`** (which still fails direct `100%` routing with `3` unconnected items) routes cleanly at `110%` and compacts **`10/10` steps (`0.9090909090909091`, `67.945 x 75.565 mm`) with `0` unconnected and `0/0` DRC** (vs `v1` `3/10` `106.99%` and `4` unconnected); **`AmpOne_dev-AmpOne`** compacts **`10/10` steps (`100.0 x 100.0 mm`, `943` segs) end-to-end** (`1/10`, `747` segs vs `v1` `1/10`, `2,835` segs on `v1`'s `110%` route) and **`20/20` steps (`80.0 x 80.0 mm`, `36.00%` area saved)** from `v1`'s `100%` route; **`Starling_Starling_V1`** reaches **`7/10` (`102.99%` vs `v1` `105.99%`)** and **`0.96`** from `100%`.
- **24-Board PCBench Generalization (`-> 0.80`)**: **`0` pre- or post-refill DRC/unconnected regressions (`24/24`)**, **`24/24` byte-identical determinism**, median linear shrink **`9.50%`** (mean **`9.91%`**), median area saved **`18.09%`** (mean **`18.39%`**), **`5/24`** reaching the full `0.80` target.

*Reporting note*: No number in this report is rounded up. Floating-point figures are either printed at full JSON precision or truncated toward $-\infty$ (`Decimal(..., ROUND_FLOOR)`).

---

## 1. Step 1–3 Verification: Initial Breakage, `main` Byte-Identity, and `v1` Test Failure Check

### 1.1 Step 1: What Broke When the Previous Agent's Untested Code (`1a865d5`) First Compiled and Ran

Building the merged untested tree (`1a865d5` = `30328a0` + `main` `8b71912`) on `sequoia-server` succeeded cleanly under `cargo build --release -p ultraroute -p kicad-text` and `cargo clippy --workspace --all-targets -- -D warnings` (`0` warnings), and 386 of 392 workspace tests passed, **but 6 of 18 unit tests in `morph` failed** (`exit code 101`):

1. `scale::tests::scale_expands_outline_parts_and_copper_while_keeping_part_sizes_and_joints` (`zero DRC violations after 1.1x scale`)
2. `compact::tests::compact_bends_a_straight_track_around_an_obstacle_pad_without_ripping` (`steps_accepted: 0 != 15`)
3. `compact::tests::compact_stops_at_the_last_legal_step_when_a_channel_jams` (`output at last legal step has 0 violations`)
4. `compact::tests::compact_stops_opposite_side_via_pinch_locally_while_board_shrinks_to_target` (`steps_accepted: 0 != 14`)
5. `compact::tests::compact_slides_pinched_through_via_along_its_track_so_front_and_back_parts_close` (`steps_accepted: 0 != 14`)
6. `compact::tests::compact_keeps_locked_parts_exact_and_produces_byte_identical_output` (`steps_accepted: 0 != 10`)

**Root causes and fixes (`2bf3234`)**:
1. **Canonical-order vs file-order index mismatch after merging `main` (`95a470a` / `f2d4b9e`)**: `main` changed [`kicad_pcb::parse_board`](core/crates/kicad-pcb/src/lib.rs) (`canon.rs`) to sort `pcb.footprints`, `copper.zones`, and `pcb.nets` by reference/name/geometry regardless of file order, whereas [`kicad_pcb::write::scale_edits`](core/crates/kicad-pcb/src/write.rs#L171-L245) walks raw S-expression nodes in file encounter order (`fp_idx`, `zone_idx`) and [`Edit`](core/crates/kicad-pcb/src/write.rs) expects file net numbers (`orig_pcb.net_file_ids`). Passing canonical indices to `scale_edits` and `Edit` scrambled footprint positions, zone polygon warps, kept-zone fills, and net IDs whenever canonical order differed from file order, and re-parsing scaled boards could permute footprints whose tie-break coordinates changed. Fixed by adding [`footprint_file_ordinals`](core/crates/morph/src/scale.rs#L331-L340), [`zone_file_ordinals`](core/crates/morph/src/scale.rs#L342-L351), and [`align_parsed_to_orig`](core/crates/morph/src/scale.rs#L353-L396) in `scale.rs`, and mapping track/via nets through `orig_pcb.net_file_ids` in `scale.rs` and `compact.rs`.
2. **Test fixture bugs in the previous agent's untested tests**: Two new tests in `compact.rs` indexed `pcb.footprints[0]` / `pcb.footprints[1]` assuming file order instead of looking up by reference (`fp("U_FRONT")`), and `compact_keeps_locked_parts_exact_and_produces_byte_identical_output` asserted `14.5e-3` instead of `10.0 + 0.75 * (16.5 - 10.0) = 14.875e-3`.
3. **Missing top-level JSON report keys in `main.rs`**: Added top-level `segments`, `part_stops`, and `via_slides` to `--report` / `--stats` JSON in [`run_compact`](core/crates/ultraroute/src/main.rs#L340-L361).

On final commit `6a98397`, **`cargo test --release --workspace --no-fail-fast` passes all 396 tests (`0` failures, including all `22` `morph` unit tests)** and **`cargo clippy --workspace --all-targets -- -D warnings` passes with `0` warnings**.

### 1.2 Step 2: `ultraroute route` Byte-Identity Against `main` (`8b71912`)

`main` (`8b71912`) and `explore/compaction` (`6a98397`) were built in separate detached worktrees on `sequoia-server` (`/home/sequoia/ab/8b71912` and `/home/sequoia/ab/compact-6a98397`), each with its own `CARGO_BUILD_BUILD_DIR=$PWD/target/build`:
- `main` (`8b71912`): inode `6564235`, `5,311,864` bytes, SHA-256 `2f01283ea24176bbb64388b88acaeac6a46d003eae8e67bdc5249f26f58c206d`
- `explore/compaction` (`6a98397`): inode `6698913`, `6,265,432` bytes, SHA-256 `2bd76fada9238bde5d518c6dae064ff924837347ad8d9973ff2ea1f0ec47b5d4` (`cmp -s` confirmed distinct binaries)

| Check Mode | Board | Output Bytes | Output SHA-256 (`main` == `branch`) | `deadline_fired` | Wall `s` (`main` / `branch`, floor) |
| --- | --- | ---: | --- | :---: | --- |
| Unmodified (`--seed 1 --uuid-seed 1 --threads 1 --timeout 600 --cpu-clock`) | `LED_port-status_LED_port-status` | `116,397` | `5b7a51fbdd5b1933720792a9fa7ab3ab05641866de98d8b05578177327c3d189` (`True`) | `False` | `0.958 / 0.956` |
| Unmodified | `1-Wire-Wing-pcb_1-Wire_Wing` | `97,961` | `6da742401d85ec6e17dfb39da9dd6f292d8fe69420bda26dcc2fa464b91c7ebe` (`True`) | `False` | `1.340 / 1.262` |
| Unmodified | `ESP07-Breakout_ESP07-Breakout` | `127,259` | `05649030e5117edcf0c7614eca7b56c1399f813002dd2eebc4f1139ab50ebb5c` (`True`) | `False` | `2.242 / 2.476` |
| Unmodified | `DasBlinkinput_Das Blinkinput` | `161,720` | `954cdd792e00017a241d65d9af300b7b9c2147723d6297b5236ab5f586de65e0` (`True`) | `False` | `2.506 / 2.450` |
| Unmodified | `DA_Lamp_attiny44a_servo_i2c` | `44,655` | `4da711cf60e2fb9a1bba792299c428e7b5e22dcd2ec3bd1ff9cec75cbace5d0c` (`True`) | `False` | `0.372 / 0.364` |
| Unmodified | `earring` (`jobs/earring-kicad/input/earring.kicad_pcb`) | `969,282` | `b97355dfd600483531aa99721efedf0ea00fd8fa3640f3901c115f7b07d02b34` (`True`) | `False` | `25.648 / 25.045` |
| Rule 6 (`Deadline::seconds` patched to `1e6` in `clock.rs`, inodes `6564118 != 6702145`) | `earring` | `969,282` | `b97355dfd600483531aa99721efedf0ea00fd8fa3640f3901c115f7b07d02b34` (`True`) | `False` | `25.1 / 25.4` |
| Rule 6 (`Deadline::seconds` patched to `1e6` in `clock.rs`, inodes `6564118 != 6702145`) | `Starling_Starling_V1` | `86,875` | `a2539edd6557af82743413792db5dac72757448d0ec226914d7205086d7f5ebe` (`True`) | `False` | `26.0 / 25.3` |

*(Full 64-char SHA-256 digests for all 6 unmodified boards are recorded in [`runs/v2/byte_identity.json`](runs/v2/byte_identity.json); `grid/` and `topo/` have `0` lines changed relative to `main` `8b71912`, and `board/src/check.rs` adds only `v1`'s 4 read-only rule-query helpers uncalled by `ultraroute route`.)*

### 1.3 Step 3: Running the v2 Unit Tests Against `v1` (`7d0462d`)

Appending the 4 new v2 unit tests onto a detached `7d0462d` checkout (`~/ab/compact-v1-7d0462d`) and running `cargo test --release -p morph --lib -- --no-capture`:
- All **13** original `v1` unit tests passed.
- **All 4 new v2 unit tests failed (`exit code 101`)**:
  1. `compact_stops_opposite_side_via_pinch_locally_while_board_shrinks_to_target`: **FAILED** (`steps_accepted == 0` vs `14`, jammed immediately at step 1 on `Clearance on B.Cu: U_BACK-1 (BACK_OBS) at (10.00, 8.90) mm vs via (SIG) at (10.00, 8.00) mm (gap 0.194 mm < required 0.200 mm)`).
  2. `compact_slides_pinched_through_via_along_its_track_so_front_and_back_parts_close`: **FAILED** (`steps_accepted == 7` vs `14`, jammed on `Clearance on B.Cu: U_BACK-1 (BACK_OBS) at (10.00, 8.88) mm vs via (SIG) at (10.00, 8.02) mm (gap 0.160 mm < required 0.200 mm)` because `v1` had no via-slide escape).
  3. `scale_anchors_offset_origin_parts_and_carries_passives_inside_connector_bosses`: **FAILED** (`BT1 shifted north with its physical pads: y=0.1` — `BT1` stayed stuck at `y = 100.0 mm` instead of moving north to `< 98.6 mm`).
  4. `compact_keeps_locked_parts_exact_and_produces_byte_identical_output`: **FAILED** (`locked footprint J_LOCK must not move by 1 nm: left [0.006077735, 0.006] != right [0.0052, 0.006]` — `v1` moved a `--lock` part by **`0.877735 mm`**!).

---

## 2. Designs That Did Not Work on Real Boards and Fixes Made (`2bf3234 -> 8ab60c3 -> a4fceaf -> 7487bae -> 16cdd97 -> 9cd2a92 -> 6a98397`)

Running the previous agent's untested v2 code against `earring` and PCBench boards with pre- and post-refill KiCad 9 DRC exposed **nine real-board bugs**, including three designs that had to be replaced outright:

1. **Previous agent's grazing split/tether design in [`CopperGraph::from_board`](core/crates/morph/src/compact.rs#L722-L1274) shattered staircases (`2.28x` segment bloat) and disabled `slide_pinched_vias` (Replaced in `8ab60c3`)**:
   - *Why it failed*: `from_board` split every raw segment and added `via_tether` links whenever *any* same-net vertex sat within `r + extra_r - 500 nm` of the segment, without checking graph distance. On routed boards (`direct_100_s0`), 45-degree staircases and short dogbone fanouts have adjacent bend vertices within `150..250 um` of neighbouring segments of the **same** trace, splitting `1,296` segments into `2,966` micro-segments at step `0` before compaction even began, and tethering nearly every escape via to its own dogbone bend `1` hop away (which caused `slide_pinched_vias` to skip the via).
   - *Replacement*: Rewrote `from_board` as a two-pass builder with [`within_hops`](core/crates/morph/src/compact.rs#L693-L719): Pass 1 splits only at exact collinear junctions (`d <= 2 um`), builds the explicit trace adjacency graph `adj`, and Pass 2 splits/tethers off-centre grazing T-junctions and cap-overlap joints **only** when `!within_hops(&adj, ..., 4)`. On `compact_clean_s0_085`, segment count dropped from `2,677` (`v1`) to **`1,292`** (`4` fewer segments than the `1,296`-segment input!), and `slide_pinched_vias` became active across `earring` and PCBench.
2. **Multi-rectangle `(filled_polygon ...)` emission in [`PourRaster`](core/crates/morph/src/scale.rs#L1311-L1497) caused spurious pre-refill `unconnected_items` in KiCad 9 DRC (Replaced in `7487bae`)**:
   - *Why it failed*: Both `v1` and the previous agent's `v2` decomposed recarved kept block zones (`rp2350_power` on `earring`) into `15..48` horizontal `(filled_polygon ...)` rectangles (`rects()`). Auditing `v1` (`runs/v2/v1_prerefill.json`) and `a4fceaf` with pre-refill `kicad-cli pcb drc` revealed that KiCad 9's pre-refill connectivity engine treats each `(filled_polygon ...)` S-expression in a zone as a **distinct zone island** without unioning abutting (or even `2 um`-overlapping!) strips — producing `+9` spurious pre-refill unconnected items in `v1` on `compact_clean_s0_085` (`+2` on `compact_085`, `9..10` on `v1` Exp B, and `+7` on `a4fceaf` `compact_clean_s0_085`) that vanished only after `refill_zones.py`.
   - *Replacement*: Replaced `PourRaster::rects()` with [`PourRaster::polygons()`](core/crates/morph/src/scale.rs#L1311-L1497) and updated [`ZoneFill`](core/crates/kicad-pcb/src/write.rs#L155-L159) / [`drop_and_fill_zone`](core/crates/kicad-pcb/src/write.rs#L297-L339) to trace each 4-connected `PourRaster` island into **one** orthogonal contour, splicing interior holes via horizontal keyhole cuts (horizontal cuts have `a.y == b.y`, so `board::inside`'s `(a.y > p.y) != (b.y > p.y)` ray-cast ignores them identically). Tested in [`pour_raster_emits_single_keyhole_polygon_per_connected_island`](core/crates/morph/src/scale.rs#L1960-L1977); pre-refill unconnected regressions dropped to **`0`** on every board.
3. **Cross-footprint `solder_mask_bridge` guard skipped `solder_mask_min_width == 0` boards and copperless `NPTH` mask apertures (Replaced in `16cdd97` / `9cd2a92`)**:
   - *Why it failed*: Running the Step 5 PCBench suite on `7487bae` exposed cross-footprint pad-to-pad `solder_mask_bridge` DRC errors on `5/16` boards (`AzizLight` `+1`, `1-Wire-Wing` `+2`, `QRPCard` `+2`, `655_testboard` `+8`, `BUF-DiffToSE-ADAU1966` `+20`) for three reasons: (a) the previous agent gated `da + db + board.mask_web` behind `if board.mask_web > 0`, returning only `da + mask_margin` instead of `da + db + mask_margin` whenever `(solder_mask_min_width ...)` was `0` (the default on most PCBench boards); (b) `Board::new` stores `board.mask_openings` only for copper-bearing pads with `margin > 0`, omitting copperless `np_thru_hole` (`NPTH`) pads (`layers == 0` in `holes_slot`/`unslotted`) that carry `*.Mask` openings (`1.0..1.5 mm` `solder_mask_margin` on `BUF-DiffToSE` `MNT202` vs `<no net>` `FID1`, and `655_testboard` `SW0:0`); and (c) `FP_STEP_CAP_NM = 22 um` copper pushes on the final `relax_core` iteration could overshoot static `2 um` buffers without proactive barriers or post-loop static cleanup.
   - *Replacement*: Added [`PadMaskTable`](core/crates/morph/src/compact.rs#L2145-L2183) and [`cross_fp_pad_mask_gap_side`](core/crates/morph/src/compact.rs#L2193-L2239) indexed directly from `(orig_pcb, orig_copper)` (including `0`-margin and copperless `NPTH` pads queried via `board.overlapping(...)`), removed the `board.mask_web > 0` gate, added `25 um` proactive static barriers in [`append_static_fp_guard_constraints`](core/crates/morph/src/compact.rs#L2547-L3072), and added post-`relax_core` static guard cleanup, verified by [`compact_stops_before_cross_footprint_solder_mask_bridge_and_npth_aperture`](core/crates/morph/src/compact.rs#L7552-L7645).
4. **Stale pre-relaxation kept-zone pours, zone-to-zone diagonal corner clearances, and over-erosion of `0.20 mm` kept zones (`8ab60c3`)**:
   - `compact` previously wrote `cur_bare_text` carved *before* copper relaxation, leaving recarved `rp2350_power` pours overlapping moved tracks/vias before `refill_zones.py` (`2` pre-refill DRC errors on `v1` A2, `6` vs `3` post-refill on `v1` Exp B `s0`, and `4..12` on `2bf3234` once `v2` compacted further). [`compact`](core/crates/morph/src/compact.rs#L630-L661) now recarves `cur_bare_text` *after* the final accepted step passing all final `Kind::Track | Kind::Via` items as `extra_obstacles`, [`carve_one_zone`](core/crates/morph/src/scale.rs#L1510-L1677) pads zone-to-zone clearances by `POUR_CORNER_PAD` (`19 um`), and morphological `open` uses `(min_thickness - POUR_STEP).max(0) / 2` so `200 um` minimum-thickness zones are not eroded empty on the `25 um` grid.
5. **Untethered pad-grazing vias detaching when footprints shift (`8ab60c3`)**:
   - Unlocked vias whose annular ring overlaps a same-net SMD pad with their centre outside the pad (`PWR_EN` on `R18-2`, `VLED_L` on `C20-1` on `earring`) and without a short (`<= 3`-hop) explicit track into that pad were previously treated as free `Anchor::Via` particles and pulled off the pad during compaction. [`CopperGraph::from_board`](core/crates/morph/src/compact.rs#L1187-L1271) now promotes them to `Anchor::PadVia { centre_in_pad: false, min_overlap, .. }`, tested in [`compact_preserves_pad_grazing_via_and_staircase_segments`](core/crates/morph/src/compact.rs#L7494-L7549).
6. **Degree-1 pour-landing track stubs pulling out of `rp2350_power` / plane zones (`8ab60c3`)**:
   - [`record_via_zones`](core/crates/morph/src/compact.rs#L3632-L3689) and [`check_via_zones`](core/crates/morph/src/compact.rs#L3691-L3736) now retain untethered degree-1 track endpoints inside their baseline kept/plane zone outlines (`(z, er, min_ov)`).
7. **Sub-micrometre 3-body courtyard penetration under asymmetric pushes (`8ab60c3`)**:
   - On `earring`, 3-body squeezes (`D6` / `R18` / `U1`) left a `0.8 um` courtyard overlap past KiCad's `5 um` per-part tolerance. Added proactive `0 <= gap < 15 um` guard half-planes in [`append_static_fp_guard_constraints`](core/crates/morph/src/compact.rs#L2660-L2704) plus `36` passes with pure stop-mode (`cm = (0, 0)` after pass 11) and exact `prev_pos` revert backstop in [`resolve_courtyard_stops`](core/crates/morph/src/compact.rs#L1874-L2111).
8. **Pre-existing positive-gap static clearance deficits worsening into `shorting_items` & cross-footprint NPTH `hole_clearance` (`a4fceaf`)**:
   - On `v1`'s `routed_110_s{0..3}` inputs, `BT1-1` (`VBATT`) and `R14-2` (`3V3`) start with a pre-existing static clearance violation (`gap0 = 17.9 um < 150 um`, non-overlapping). Binary exemption allowed `BT1-1` and `R14-2` to touch (`gap <= 0`), creating a new `shorting_items` DRC error. [`collect_static_violations`](core/crates/morph/src/compact.rs#L3283-L3559) now records `baseline: HashMap<StaticViolKey, f64>` so any pair with `gap0 > 500 nm` may never cross into overlap (`gap <= 500 nm`) or worsen by `> 1.5 um`, and checks cross-footprint unplated NPTH holes against foreign pads (`min_hole_clearance`, catching `J1` NPTH vs `R4`/`R5`).
9. **Rigid board-level drawn copper (`gr_text` on `F.Cu`) crossing `min_copper_edge_clearance` as `Edge.Cuts` shrinks (`cdec619` / `5bee4a0` / `6a98397`)**:
   - *Why it failed*: Expanding the PCBench batch to 24 boards exposed `+1` `copper_edge_clearance` (`0.3671 mm < 0.5000 mm`) on `DA_Lamp_attiny44a_servo_i2c` between `"PCB text 'servo_44_I2C' on F.Cu"` (`gr_text` rotated `270` deg whose rigid glyph strokes stay fixed in font size while its anchor scales with `Edge.Cuts`) and `Edge.Cuts`, because [`collect_static_violations`](core/crates/morph/src/compact.rs#L3283-L3559) walked only `Kind::Pad` and ignored `Kind::Pour { zone: board::DRAWN, kept: true }`. Added [`drawn_ordinals_on_board`](core/crates/morph/src/compact.rs#L3274-L3281) and static `DRAWN` checks against `Edge.Cuts`, `Keepout`, unplated `NPTH` holes, and distinct-net drawn copper (while skipping `<no net>` internal glyph stroke crossings), plus returning exact `viol_desc.or(yard_err)` from [`resolve_static_step_board`](core/crates/morph/src/compact.rs#L2373-L2450), verified in [`compact_stops_before_drawn_copper_graphic_hits_board_edge`](core/crates/morph/src/compact.rs#L7648-L7724).

---

## 3. Architecture of Compaction v2 and Unit Tests

### 3.1 Files Modified and Added

| File | Summary of Changes |
| --- | --- |
| [`core/crates/kicad-pcb/src/write.rs`](core/crates/kicad-pcb/src/write.rs#L155-L339) | Updated [`ZoneFill`](core/crates/kicad-pcb/src/write.rs#L155-L159) (`polygons: Vec<Vec<[f64; 2]>>`), [`ZonePointMap`](core/crates/kicad-pcb/src/write.rs#L162), [`ScaleEdits`](core/crates/kicad-pcb/src/write.rs#L165), [`scale_edits`](core/crates/kicad-pcb/src/write.rs#L171-L245), and [`drop_and_fill_zone`](core/crates/kicad-pcb/src/write.rs#L297-L339) to emit single keyhole-fractured polygons per kept-zone island with zero Clippy warnings. |
| [`core/crates/morph/src/scale.rs`](core/crates/morph/src/scale.rs#L71-L1978) | Added [`nominal_scaled_positions`](core/crates/morph/src/scale.rs#L154-L176), [`compute_footprint_envelopes`](core/crates/morph/src/scale.rs#L178-L329) (pad/courtyard physical anchors + pocket child adoption), canonical-to-file index alignment ([`footprint_file_ordinals`](core/crates/morph/src/scale.rs#L331-L340), [`zone_file_ordinals`](core/crates/morph/src/scale.rs#L342-L351), [`align_parsed_to_orig`](core/crates/morph/src/scale.rs#L353-L396)), multi-island courtyard extraction ([`extract_footprint_yards`](core/crates/morph/src/scale.rs#L398-L565)), single-polygon keyhole contour tracing ([`PourRaster::polygons`](core/crates/morph/src/scale.rs#L1311-L1497)), and `POUR_CORNER_PAD` zone-to-zone clearance in [`carve_one_zone`](core/crates/morph/src/scale.rs#L1510-L1677); added 2 new unit tests. |
| [`core/crates/morph/src/compact.rs`](core/crates/morph/src/compact.rs#L197-L7725) | Implemented catch-up per-part targets, two-pass [`CopperGraph::from_board`](core/crates/morph/src/compact.rs#L722-L1274) (`within_hops` grazing guard + pad-grazing `Anchor::PadVia`), [`PadMaskTable`](core/crates/morph/src/compact.rs#L2145-L2183), [`project_scaled_footprints`](core/crates/morph/src/compact.rs#L2284-L2370), [`resolve_static_step_board`](core/crates/morph/src/compact.rs#L2373-L2450), baseline gap-capped [`collect_static_violations`](core/crates/morph/src/compact.rs#L3283-L3559) (including `board::DRAWN` copper graphics), joint copper + rigid-footprint family relaxation ([`relax_core`](core/crates/morph/src/compact.rs#L4465-L5894)), topological via sliding ([`slide_pinched_vias`](core/crates/morph/src/compact.rs#L3770-L4408) Modes A/B/C), 10-pass homotopic [`shorten_taut_chains`](core/crates/morph/src/compact.rs#L6248-L6408), post-relaxation kept-zone recarving, and 6 new unit tests. |
| [`core/crates/ultraroute/src/main.rs`](core/crates/ultraroute/src/main.rs#L215-L371) | Recorded CPU seconds via `grid::clock` (`use_cpu_time`, `now`, `since`) and emitted `cpu_s`, `segments`, `part_stops`, and `via_slides` on stderr and in `--report` / `--stats` JSON. |
| [`core/README.md`](core/README.md#L11) | Documented `ultraroute scale` and `ultraroute compact` in the crate summary table. |

### 3.2 Core Mechanisms

1. **Catch-up nominal targets + per-part stops (`compact` + `relax_core`)**:
   - Step `k` computes nominal positions directly from `orig_pcb` at `step_f = 1.0 + (factor - 1.0) * (k / steps)` rather than compounding exclusively from `cur_pcb`. When an interior pair stops (same-side courtyard contact, locked part, cross-footprint mask/hole rule, or opposite-side via/track channel pinch), only that blocked family holds back while unblocked parts elsewhere on the board keep moving inward on subsequent steps.
   - Pass `0` of [`relax_core`](core/crates/morph/src/compact.rs#L4465-L5894) relaxes copper tracks and vias (including sub-`1.2 mm` pad-to-pad segment splitting down to `180 um` and [`slide_pinched_vias`](core/crates/morph/src/compact.rs#L3770-L4408)) with footprints fixed. Only if Pass `0` leaves dynamic violations (`count_dynamic_violations > 0`) do Passes `1..5` activate rigid-body footprint family motion (`22 um` cap per iteration via `solve_half_planes_2d`) coupled to cross-layer copper/hole/mask/spoke pushes and [`append_static_fp_guard_constraints`](core/crates/morph/src/compact.rs#L2547-L3072), followed by bare-board rebuild and `settle_copper_only`.
2. **Legal topological via slide ([`slide_pinched_vias`](core/crates/morph/src/compact.rs#L3770-L4408))**:
   - Triggered only on unlocked `Anchor::Via` particles still failing `check_item` after Pass `0` continuous relaxation, subject to `implicit_static_ties` and `plane_zones` containment guards:
     - **Mode A**: Slides the via `100 um..1.60 mm` along a same-layer, same-width degree-2 chain prefix (`<= 8` hops, lateral offsets `{0, +-80, +-160} um`), collapsing traversed segments and reconnecting remaining edges directly if legal.
     - **Mode B**: For a layer-transitioning via (`l_other != l_fwd`), leaves a degree-2 bend vertex `W` near `old_pt` on `l_other`, slides the via along `l_fwd`, and bridges `(W, cand)` on `l_other`, preserving homotopy on both layers.
     - **Mode C**: Short-range radial escape (`12` angles, radii `100..650 um`) keeping all incident edges attached.
   - Every candidate must avoid acute track junctions (`cos <= 0.008`, `>= 89.5 deg`) and pass `check_item(board, tid).is_empty()` on the via and every modified/bridge segment.
3. **Physical footprint anchors, multi-island courtyards, and pocket children (`scale.rs`)**:
   - Each footprint scales about the centre of its physical pad/hole/courtyard envelope rather than `fp.at` (`BT1` moves north with its `y ~ 83.65 mm` pads instead of staying pinned at `(100, 100)`).
   - Disjoint courtyard shapes (`BT1`'s two `B.CrtYd` tab rectangles) remain separate islands instead of forming a false convex hull across open cavities.
   - Enclosing footprints with `>= 3` interacting pads/holes (`J1`'s 4 shield tabs + 2 NPTH bosses) adopt enclosed small parts (`R4`, `R5`) as rigid pocket children (`parent = Some(p)`), while open 2-pad cavities (`BT1`) leave interior parts free to compact.
4. **10-pass homotopic chain merging ([`shorten_taut_chains`](core/crates/morph/src/compact.rs#L6248-L6408))**:
   - Replaces degree-2 triples `(pu, pv, pw)` with chord `(pu, pw)` whenever altitude `dev <= 200 um` (triangle inradius `< 100 um`, strictly below any pad/via radius + clearance, guaranteeing homotopy), `check_item` is clean, no acute bend is created, and outer-layer same-net slot clearances do not worsen by `> 3 um`.

### 3.3 Unit Tests (`22/22` Passing in `morph`)

1. [`compact_stops_opposite_side_via_pinch_locally_while_board_shrinks_to_target`](core/crates/morph/src/compact.rs#L7257-L7344): Asserts `14/14` steps to `0.65` (`35%` linear shrink, `57.75%` area saved), outer span `< 10.0 mm` (from `15.0 mm`), pinched `F.Cu`/`B.Cu` parts held at `>= 1.795 mm` across a `0.60 mm` through-via (`part_stops > 0`), `0` DRC violations, `0` airlines.
2. [`compact_slides_pinched_through_via_along_its_track_so_front_and_back_parts_close`](core/crates/morph/src/compact.rs#L7347-L7421): Asserts `14/14` steps to `0.65`, `via_slides > 0`, via displaced `> 0.85 mm` out of the `x = 10 mm` throat, `F.Cu` and `B.Cu` parts closed past `1.80 mm` to `< 1.50 mm`, `0` DRC violations, `0` airlines.
3. [`compact_keeps_locked_parts_exact_and_produces_byte_identical_output`](core/crates/morph/src/compact.rs#L7424-L7491): Asserts `rep1.text == rep2.text`, `--lock J_LOCK` moved `0 nm`, `R_FAR` moved to `14.875 mm`, `R_NEAR` stopped cleanly against `J_LOCK`, `0` DRC violations, `0` airlines.
4. [`compact_preserves_pad_grazing_via_and_staircase_segments`](core/crates/morph/src/compact.rs#L7494-L7549): Asserts `rep.steps[0].segments == 2` (dogbone bend not shattered), `10/10` steps to `0.75`, pad-grazing via stays attached to `C_BACK-1`, `0` DRC violations, `0` airlines.
5. [`compact_stops_before_cross_footprint_solder_mask_bridge_and_npth_aperture`](core/crates/morph/src/compact.rs#L7552-L7645): Asserts `10/10` steps to `0.65`, `R_FAR` hits full `11.25 mm` shrink target, `0`-web SMD pads keep `>= 0.400 mm` copper gap (`pad_to_mask_clearance = 0.2`), copperless `NPTH` (`solder_mask_margin = 1.0`) vs `<no net>` fiducial (`solder_mask_margin = 0.5`) keeps `>= 1.500 mm` shape gap, `0` DRC violations, `0` airlines.
6. [`compact_stops_before_drawn_copper_graphic_hits_board_edge`](core/crates/morph/src/compact.rs#L7648-L7724): Asserts a `20 x 30 mm` board with `270`-deg `gr_text "SERVO_44"` on `F.Cu` accepts `3/20` steps (`0.94`, `6.00%` linear shrink, `11.64%` area saved) without self-jamming on `<no net>` glyph stroke crossings and stops cleanly with `EdgeClearance` on drawn copper before crossing `0.500 mm` `min_copper_edge_clearance`, `0` added violations, `0` airlines.
7. [`scale_anchors_offset_origin_parts_and_carries_passives_inside_connector_bosses`](core/crates/morph/src/scale.rs#L1888-L1957): Asserts `BT1` shifts north (`y < 98.6 mm`) under `1.1x` scale, `R5` translates rigidly with enclosing `J1` (`|dj_y - dr_y| < 2 um`), and `0` DRC violations.
8. [`pour_raster_emits_single_keyhole_polygon_per_connected_island`](core/crates/morph/src/scale.rs#L1960-L1977): Asserts a 4-connected ring with an interior hole emits `1` keyhole polygon with exact cell containment.

---

## 4. Experiment A: Routed Earring Toward `0.85` (`v1` vs `v2` Side-by-Side)

Inputs match [`REPORT-v1.md`](REPORT-v1.md) byte-for-byte:
- **A1 (`compact_085`)**: `runs/expA/baseline.kicad_pcb` (`30.5 x 35.75 mm`, `10` pre-existing unconnected items, `0` DRC errors), `--factor 0.85 --steps 15`.
- **A1-fine (`compact_fine`)**: `runs/expA/baseline.kicad_pcb`, `--factor 0.98 --steps 10` (`0.2%` steps).
- **A2 (`compact_clean_s0_085`)**: `runs/expB/direct_100_s0.kicad_pcb` (`30.5 x 35.75 mm`, `0` unconnected, `0` DRC errors, `.kicad_pro` + `.kicad_dru` active), `--factor 0.85 --steps 15`.

### 4.1 KiCad-Judged Summary (`v1` vs `v2`, Pre- and Post-Refill)

| Case & Version | Achieved Factor (`steps`) | Linear Shrink / Area Saved (floor) | Board Size (`mm`) | Pre-Refill DRC (`err` / `unconn`) | Post-Refill DRC (`err` / `unconn`) | Vias | Segments | Track Length (`mm`) | Acid Traps (`acute`/`slots`/`tot`) | Stops / Slides | CPU `s` / Elapsed `s` | What Stopped It |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- | --- | --- | --- |
| **Input (A1 scratchpad)** | `1.0` | -- | `30.5 x 35.75` | `0 / 10` | `0 / 10` | `283` | `945` | `1032.609016` | `11 / 75 / 86` | -- | -- | -- |
| **A1 `v1`** (`-> 0.85`, `15` steps) | `0.99` (`1/15`) | `1.00% / 1.98%` | `30.195002 x 35.392501` | `0 / 12` (`+2`) | `0 / 10` | `283` | `1164` | `1014.612556` | `7 / 72 / 79` | -- | `-- / 10.159658397` | `Clearance on F.Cu: U1-34 (LED_EN_C) vs via (BTN_SENSE)` |
| **A1 `v2` (`-> 0.85`, `15` steps)** | **`0.97` (`3/15`)** | **`3.00% / 5.91%`** | `29.585 x 34.6775` | **`0 / 10` (`0`)** | **`0 / 10` (`0`)** | `283` | **`1001`** | **`981.441305`** | `5 / 82 / 87` | `96 / 0` | `26.853893795 / 26.859928793` | `CourtyardOverlap on B.CrtYd: R14 vs R15 (overlap 0.000 mm)` |
| **A1-fine `v1`** (`-> 0.98`, `10` steps) | `0.988` (`6/10`) | `1.20% / 2.38%` | `30.134 x 35.321` | `0 / 12` (`+2`) | `0 / 10` | `283` | `1213` | `1002.938346` | `8 / 70 / 78` | -- | `-- / 26.291489997` | `Clearance on F.Cu: U1-31 (CHRG) vs via (PWR_HOLD)` |
| **A1-fine `v2` (`-> 0.98`, `10` steps)** | **`0.98` (`10/10`)** | **`2.00% / 3.96%`** | `29.889998 x 35.034999` | **`0 / 10` (`0`)** | **`0 / 10` (`0`)** | `283` | **`1003`** | **`988.271588`** | `3 / 74 / 77` | `80 / 0` | `37.755626992 / 37.76273412` | `Reached target` |
| **Input (A2 clean `100%` s0)** | `1.0` | -- | `30.5 x 35.75` | `0 / 0` | `0 / 0` | `161` | `1296` | `1327.619626` | `62 / 55 / 117` | -- | -- | -- |
| **A2 `v1`** (`-> 0.85`, `15` steps) | `0.99` (`1/15`) | `1.00% / 1.98%` | `30.195002 x 35.392501` | `2 / 9` (`+2/+9`) | `0 / 0` | `161` | `2677` | `1298.941937` | `87 / 61 / 148` | -- | `-- / 18.619641603` | `Clearance on F.Cu: C5-1 (VDD_CORE) vs via (VBUS_SENSE)` |
| **A2 `v2` (`-> 0.85`, `15` steps)** | **`0.98` (`2/15`)** | **`2.00% / 3.96%`** | `29.889998 x 35.034999` | **`0 / 0` (`0`)** | **`0 / 0` (`0`)** | `161` | **`1292`** | **`1268.208625`** | `13 / 58 / 71` | `61 / 1` | `23.176935852 / 23.18150337` | `Clearance on B.Cu: J1-A6 (USB_C_DP) at (99.75, 86.38) mm vs track (USB_C_DN) near (99.57, 87.07) mm (gap 0.000 mm < required 0.127 mm)` |

### 4.2 v2 Per-Step Compaction Trajectories (Experiment A)

**A1 `v2` ([`runs/v2/expA/compact_085.kicad_pcb`](runs/v2/expA/compact_085.kicad_pcb), `det = True`, peak RSS `222 MB`)**:

| Step | Factor | Accepted | Internal Outline Size (`mm`) | Violations | Segments | Vias | Track Length (`mm`) | Courtyard Stops | Part Stops | Via Slides | Pre-Pour Acid (`acute`/`slots`) |
| ---: | --- | :---: | --- | ---: | ---: | ---: | --- | ---: | ---: | ---: | --- |
| `0` | `1.0` | yes | `30.491843 x 35.745092` | `0` | `1002` | `283` | `1032.609016470518` | `0` | `0` | `0` | `9 / 104` |
| `1` | `0.99` | yes | `30.188360999999997 x 35.3925` | `0` | `979` | `283` | `1014.5525420405321` | `12` | `6` | `0` | `9 / 103` |
| `2` | `0.98` | yes | `29.883426999999998 x 35.035` | `0` | `993` | `283` | `995.6930039422344` | `36` | `76` | `0` | `6 / 105` |
| `3` | `0.97` | yes | `29.582030999999997 x 34.672506999999996` | `0` | `1001` | `283` | `981.4413048996986` | `50` | `96` | `0` | `5 / 111` |
| `4` | `0.96` | **no (jam)** | `29.277061 x 34.315058` | `18` | `1435` | `283` | `1027.56726571106` | `65` | `125` | `4` | `11 / 134` |

**A2 `v2` ([`runs/v2/expA/compact_clean_s0_085.kicad_pcb`](runs/v2/expA/compact_clean_s0_085.kicad_pcb), `det = True`, peak RSS `255 MB`)**:

| Step | Factor | Accepted | Internal Outline Size (`mm`) | Violations | Segments | Vias | Track Length (`mm`) | Courtyard Stops | Part Stops | Via Slides | Pre-Pour Acid (`acute`/`slots`) |
| ---: | --- | :---: | --- | ---: | ---: | ---: | --- | ---: | ---: | ---: | --- |
| `0` | `1.0` | yes | `30.491843 x 35.745092` | `0` | `1751` | `161` | `1327.6691359108606` | `0` | `0` | `0` | `19 / 77` |
| `1` | `0.99` | yes | `30.188360999999997 x 35.3925` | `0` | `1297` | `161` | `1294.7627336950468` | `12` | `6` | `0` | `18 / 92` |
| `2` | `0.98` | yes | `29.883426999999998 x 35.035` | `0` | `1292` | `161` | `1268.208625286766` | `36` | `61` | `1` | `13 / 85` |
| `3` | `0.97` | **no (jam)** | `29.582030999999997 x 34.672506999999996` | `18` | `1872` | `161` | `1267.7712215741783` | `49` | `111` | `1` | `16 / 118` |

**Key findings on Experiment A**:
- On **A1** and **A1-fine**, per-part stops (`pstops` reaching `96` on `A1` and `80` on `A1-fine`) carry compaction straight past `v1`'s `0.986` `U1` vs `PWR_HOLD`/`BTN_SENSE` opposite-side via pinch to **`0.97` (`3/15` steps, `3.00%` linear shrink, `5.91%` area saved)** on `A1` (`CourtyardOverlap on B.CrtYd: R14 vs R15 (overlap 0.000 mm)`) and **`0.98` (`10/10` steps)** on `A1-fine` (`Reached target`).
- On **A2** (`0` unconnected, `.kicad_dru` active), `v2` advances to **`0.98` (`2/15` steps, `2.00%` linear shrink, `3.96%` area saved)**, eliminates all `2` pre-refill DRC errors and all `9` pre-refill unconnected items present in `v1`, reduces post-pour acid traps from `117` (`148` in `v1`) to **`71`**, and finishes with **`1292` segments** (vs `2,677` in `v1`).

---

## 5. Experiment B: `earring` `110% -> Route -> Compact 1/1.1` (`v1` vs `v2`, Seeds `0..3`)

To separate the effect of `v2` compaction alone from the effect of `v2` scaling (`BT1` physical-pad anchor + disjoint courtyards + `J1` pocket adoption of `R4`/`R5`), we ran **two** side-by-side comparisons against [`REPORT-v1.md`](REPORT-v1.md):
1. **Part B1 (`v2` compact on `v1`'s exact `runs/expB/routed_110_s{0..3}.kicad_pcb`)**: Same input boards as `v1` (which already carry the `6` static `v1 scale 1.1` footprint errors on `BT1`/`R14`/`SW2` and `J1`/`R4`/`R5`).
2. **Part B2 (End-to-end `v2` pipeline: `v2 scale 1.1 -> route -> v2 compact 1/1.1`)**: Scaling [`jobs/earring-kicad/input/earring.kicad_pcb`](jobs/earring-kicad/input/earring.kicad_pcb) with `v2 scale 1.1` ([`runs/v2/expB/bare_110.kicad_pcb`](runs/v2/expB/bare_110.kicad_pcb)), routing seeds `0..3` (`--strip-all --recipe default --timeout 120 --cpu-clock`), and compacting toward `1/1.1` (`0.9090909090909091`) in `10` steps.

### 5.1 Static `bare_110` Check (`v1 scale 1.1` vs `v2 scale 1.1`)

| Stage | Size (`mm`) | Pre-Refill DRC (`err` / `unconn`) | Post-Refill DRC (`err` / `unconn`) | Post-Refill Errors by Type | Scale Wall `s` |
| --- | --- | --- | --- | --- | ---: |
| `bare_100` (input) | `30.5 x 35.75` | `0 / 274` | `0 / 274` | `{}` | -- |
| `bare_110` (`v1 scale 1.1`) | `33.55 x 39.325` | `12 / 435` | `11 / 274` | `{clearance: 3, courtyards_overlap: 2, hole_clearance: 3, shorting_items: 1, solder_mask_bridge: 2}` | `0.256891` |
| **`bare_110` (`v2 scale 1.1`)** | `33.55 x 39.325` | **`0` / `430`** | **`0` / `275`** | **`{}` (`0` errors!)** | `0.4635` |

`v2 scale --factor 1.1` eliminates **all `11` post-refill (`12` pre-refill) DRC errors** that `v1 scale 1.1` introduced on `earring` (`BT1` vs `R14`/`SW2`, `J1` NPTH vs `R4`/`R5`, and pre-existing `VBUS` stub collisions).

### 5.2 Seeds `0..3` Side-by-Side (`100%` Direct vs `v1` vs `v2` on `v1_r110` vs End-to-End `v2`)

| Seed | Pipeline / Version | Achieved Factor (`steps`) | Linear Shrink / Area Saved (floor) | Size vs `100%` (`mm`) | Pre-Refill DRC (`err`/`unconn`) | Post-Refill DRC (`err`/`unconn`) | Vias | Segs | Track Length (`mm`) | Acid Traps (`acute`/`slots`/`tot`) | Stops / Slides | CPU `s` / Wall `s` | Jam / Stop Reason |
| :---: | --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- | --- | --- | --- |
| **`0`** | `100%` direct route | `1.0` | -- | `30.5 x 35.75` (`1.0000x`) | `0 / 0` | `0 / 0` | `161` | `1296` | `1327.619626` | `62 / 55 / 117` | -- | `-- / 23.179905145` | -- |
| **`0`** | `v1` `110%` route | `1.0` | -- | `33.55 x 39.325` (`1.1000x`) | `7 / 6` | `6 / 0` | `148` | `1291` | `1423.773465` | `43 / 51 / 94` | -- | `-- / 24.789368452` | -- |
| **`0`** | `v1` compact (`v1_r110`) | `0.9545454545454546` (`5/10`) | `4.54% / 8.88%` | `32.025 x 37.5375` (`1.0500x`) | `6 / 10` | `3 / 0` | `148` | `2728` | `1321.341878` | `47 / 57 / 104` | -- | `-- / 40.371898164` | `Clearance on B.Cu: R25-2 (GND) at (94.53, 97.77) mm vs track (PWR_HOLD) near (94.41, 97.25) mm (gap 0.123 mm < required 0.127 mm)` |
| **`0`** | **`v2` compact (on `v1_r110`)** | **`0.9181818181818182` (`9/10`)** | **`8.18% / 15.69%`** | `30.805 x 36.1075` (**`1.0100x`**) | `6 / 0` | `6 / 0` (`0` added) | `148` | **`1474`** | **`1265.18029`** | `12 / 70 / 82` | `87 / 5` | `72.048 / 72.067` | `EdgeClearance: J1-2 (GND) at (104.40, 80.69) mm vs board edge / rule area (gap 0.146 mm < required 0.200 mm)` |
| **`0`** | `v2` `110%` route | `1.0` | -- | `33.55 x 39.325` (`1.1000x`) | **`0 / 0`** | **`0 / 0`** | `152` | `1410` | `1435.614596` | `12 / 57 / 69` | -- | `-- / 28.2424` | -- |
| **`0`** | **`v2` compact (end-to-end, `det=True`)** | **`0.9636363636363636` (`4/10`)** | **`3.63% / 7.14%`** | `32.33 x 37.895` (**`1.0599x`**) | **`0 / 0`** | **`0 / 0`** | `152` | **`1565`** | **`1353.36859`** | **`9 / 51 / 60`** | `62 / 0` | `30.453 / 30.458` | `Clearance on B.Cu: J1-A8 () at (101.26, 85.08) mm vs via (CC2) at (101.73, 85.76) mm (gap 0.126 mm < required 0.127 mm)` |
| **`1`** | `100%` direct route | `1.0` | -- | `30.5 x 35.75` (`1.0000x`) | `0 / 0` | `0 / 0` | `158` | `1174` | `1319.288328` | `53 / 58 / 111` | -- | `-- / 31.294046390801668` | -- |
| **`1`** | `v1` `110%` route | `1.0` | -- | `33.55 x 39.325` (`1.1000x`) | `7 / 6` | `6 / 0` | `150` | `1366` | `1414.653438` | `66 / 65 / 131` | -- | `-- / 28.091408455744386` | -- |
| **`1`** | `v1` compact (`v1_r110`) | `0.9727272727272727` (`3/10`) | `2.72% / 5.38%` | `32.635 x 38.2525` (`1.0699x`) | `4 / 9` | `4 / 0` | `150` | `2737` | `1347.933496` | `59 / 57 / 116` | -- | `-- / 26.901802546344697` | `Clearance on B.Cu: R8-1 (PROG) at (103.66, 107.70) mm vs via (3V3) at (103.01, 107.84) mm (gap 0.126 mm < required 0.127 mm)` |
| **`1`** | **`v2` compact (on `v1_r110`)** | **`0.9272727272727272` (`8/10`)** | **`7.27% / 14.01%`** | `31.110002 x 36.465001` (**`1.0199x`**) | `6 / 0` | `6 / 0` (`0` added) | `150` | **`1495`** | **`1263.947186`** | `18 / 65 / 83` | `85 / 0` | `54.520 / 54.535` | `EdgeClearance: SW2-1 (GND) at (90.81, 87.67) mm vs board edge / rule area (gap 0.064 mm < required 0.200 mm)` |
| **`1`** | `v2` `110%` route | `1.0` | -- | `33.55 x 39.325` (`1.1000x`) | **`0 / 0`** | **`0 / 0`** | `159` | `1370` | `1411.099103` | `14 / 55 / 69` | -- | `-- / 26.8346` | -- |
| **`1`** | **`v2` compact (end-to-end)** | **`0.9181818181818182` (`9/10`)** | **`8.18% / 15.69%`** | `30.805 x 36.1075` (**`1.0100x`**) | **`0 / 0`** | **`0 / 0`** | `159` | **`1604`** | **`1259.11987`** | **`11 / 78 / 89`** | `108 / 4` | `86.538 / 86.556` | `Clearance on F.Cu: C15-1 (XTAL_R) at (97.60, 94.59) mm vs via (LED_DATA_L) at (97.73, 95.23) mm (gap 0.124 mm < required 0.127 mm)` |
| **`2`** | `100%` direct route | `1.0` | -- | `30.5 x 35.75` (`1.0000x`) | `0 / 0` | `0 / 0` | `152` | `1267` | `1351.520643` | `58 / 58 / 116` | -- | `-- / 28.478713741526008` | -- |
| **`2`** | `v1` `110%` route | `1.0` | -- | `33.55 x 39.325` (`1.1000x`) | `7 / 7` | `6 / 0` | `147` | `1371` | `1428.548734` | `57 / 53 / 110` | -- | `-- / 28.95048468373716` | -- |
| **`2`** | `v1` compact (`v1_r110`) | `0.990909090909091` (`1/10`) | `0.90% / 1.80%` | `33.245 x 38.9675` (`1.0900x`) | `5 / 9` | `5 / 0` | `147` | `2499` | `1398.824452` | `54 / 48 / 102` | -- | `-- / 16.860203887335956` | `Clearance on B.Cu: R20-1 (VBUS_SENSE) at (109.43, 100.43) mm vs via (LEDC_D6) at (108.84, 100.13) mm (gap 0.124 mm < required 0.127 mm)` |
| **`2`** | **`v2` compact (on `v1_r110`)** | **`0.9181818181818182` (`9/10`)** | **`8.18% / 15.69%`** | `30.805 x 36.1075` (**`1.0100x`**) | `4 / 0` | `4 / 0` (`0` added) | `147` | **`1499`** | **`1269.930774`** | `11 / 65 / 76` | `99 / 3` | `75.898 / 75.919` | `EdgeClearance: J1-2 (GND) at (104.40, 80.61) mm vs board edge / rule area (gap 0.097 mm < required 0.200 mm)` |
| **`2`** | `v2` `110%` route | `1.0` | -- | `33.55 x 39.325` (`1.1000x`) | **`0 / 0`** | **`0 / 0`** | `153` | `1406` | `1429.702028` | `10 / 62 / 72` | -- | `-- / 28.9027` | -- |
| **`2`** | **`v2` compact (end-to-end)** | **`0.9363636363636363` (`7/10`)** | **`6.36% / 12.32%`** | `31.415 x 36.8225` (**`1.0299x`**) | **`0 / 0`** | **`0 / 0`** | `153` | **`1636`** | **`1304.780405`** | **`14 / 59 / 73`** | `103 / 4` | `60.638 / 60.651` | `Clearance on F.Cu: U1-10 () at (103.72, 98.41) mm vs via (IMU_SDA) at (104.10, 98.79) mm (gap 0.072 mm < required 0.127 mm)` |
| **`3`** | `100%` direct route | `1.0` | -- | `30.5 x 35.75` (`1.0000x`) | `0 / 0` | `0 / 0` | `165` | `1201` | `1318.655564` | `44 / 63 / 107` | -- | `-- / 22.957267371937633` | -- |
| **`3`** | `v1` `110%` route | `1.0` | -- | `33.55 x 39.325` (`1.1000x`) | `7 / 7` | `6 / 0` | `155` | `1315` | `1411.466533` | `46 / 60 / 106` | -- | `-- / 26.506549478508532` | -- |
| **`3`** | `v1` compact (`v1_r110`) | `0.990909090909091` (`1/10`) | `0.90% / 1.80%` | `33.245 x 38.9675` (`1.0900x`) | `5 / 9` | `5 / 0` | `155` | `2465` | `1384.124524` | `53 / 58 / 111` | -- | `-- / 14.756275289691985` | `Clearance on F.Cu: U1-29 (LED_DATA_L) at (97.60, 96.23) mm vs via (3V3) at (97.18, 95.80) mm (gap 0.118 mm < required 0.127 mm)` |
| **`3`** | **`v2` compact (on `v1_r110`)** | **`0.9272727272727272` (`8/10`)** | **`7.27% / 14.01%`** | `31.110002 x 36.465001` (**`1.0199x`**) | `6 / 0` | `6 / 0` (`0` added) | `155` | **`1458`** | **`1265.094581`** | `15 / 73 / 88` | `75 / 1` | `64.551 / 64.570` | `CourtyardOverlap on B.CrtYd: BT1 vs J1 (overlap 0.044 mm)` |
| **`3`** | `v2` `110%` route | `1.0` | -- | `33.55 x 39.325` (`1.1000x`) | **`0 / 0`** | **`0 / 0`** | `152` | `1395` | `1455.608167` | `17 / 56 / 73` | -- | `-- / 28.878` | -- |
| **`3`** | **`v2` compact (end-to-end)** | **`0.9363636363636363` (`7/10`)** | **`6.36% / 12.32%`** | `31.415 x 36.8225` (**`1.0299x`**) | **`0 / 0`** | **`0 / 0`** | `152` | **`1611`** | **`1320.664668`** | **`9 / 51 / 60`** | `72 / 0` | `59.569 / 59.582` | `Clearance on B.Cu: D4-3 (BTN_N) at (93.00, 93.37) mm vs track (VBATT) near (92.43, 93.74) mm (gap 0.036 mm < required 0.127 mm)` |

**Key findings on Experiment B**:
- **Even on `v1`'s broken `routed_110_s{0..3}` inputs (Part B1)**, `v2` improves every seed (`v1` `[5, 3, 1, 1]/10` -> `v2` **`[9, 8, 9, 8]/10`** steps, reaching `101.00%` of original linear size on Seed `0`), while cutting segment counts by `40.01%..45.96%`.
- **End-to-end `v2` (`v2 scale 1.1 -> route -> v2 compact 1/1.1`, Part B2)** achieves **`0` DRC errors and `0` unconnected items (both pre-refill and post-refill) on all 4 seeds** (`[4, 9, 7, 7]/10` steps), with **`22..57` fewer post-pour acid traps** than `100%` direct routing.

---

## 6. Experiment C: PCBench Congestion & Compaction (`Starling_Starling_V1`, `BMS-bq76940_EvaluationsBoard`, `AmpOne_dev-AmpOne`)

We ran `v2` against all three Experiment C boards from [`REPORT-v1.md`](REPORT-v1.md) (`--seed 0`, `--timeout 120 --cpu-clock`), comparing:
1. **`v1` `110% -> 100%`** (`runs/expC/<id>/routed_110_s0.kicad_pcb` -> `v1 compact 1/1.1`).
2. **`v2` on `v1`'s exact `routed_110_s0.kicad_pcb`** (`from_v1_routed_110`) and **`v2` on `v1`'s `direct_100_s0.kicad_pcb` toward `0.80`** (`from_v1_direct_100_to_080`).
3. **End-to-end `v2` (`v2 scale 1.1 -> ultraroute route -> v2 compact 1/1.1`, `det = True`)** plus current `main`-equivalent `100%` direct route (`v2_direct_100_s0`) and `100% -> 0.80` compaction (`compact_080_from_100_s0`).

### 6.1 Side-by-Side Results (`v1` vs `v2`, Pre- and Post-Refill)

| Board | Stage / Pipeline | Achieved Factor (`steps`) | Linear Shrink / Area Saved (floor) | Size (`mm`, vs `100%`) | Pre-Refill DRC (`err`/`unconn`) | Post-Refill DRC (`err`/`unconn`) | Vias | Segs | Track Length (`mm`) | Acid Traps (`acute`/`slots`/`tot`) | Stops / Slides | CPU `s` / Wall `s` | Jam / Stop Reason |
| --- | --- | --- | --- | --- | :---: | :---: | ---: | ---: | ---: | --- | :---: | --- | --- |
| **`Starling_Starling_V1`** (`25` fp) | `v1` `100%` direct route | `1.0` | -- | `60.1 x 23.8` (`1.0000x`) | `0 / 3` | `0 / 3` | `79` | `500` | `1075.924803` | `28 / 30 / 58` | -- | `-- / 108.728` | -- |
| **`Starling_Starling_V1`** (`25` fp) | `v1` `110%` route | `1.0` | -- | `66.11 x 26.18` (`1.1000x`) | `0 / 0` | `0 / 0` | `70` | `447` | `1248.019024` | `16 / 17 / 33` | -- | `-- / 9.126` | -- |
| **`Starling_Starling_V1`** (`25` fp) | `v1` compact (`v1_r110`) | `0.9636363636363636` (`4/10`) | `3.63% / 7.14%` | `63.706 x 25.228` (`1.0599x`) | `0 / 0` | `0 / 0` | `70` | `994` | `1234.416791` | `16 / 40 / 56` | -- | `-- / 27.009` | `EdgeClearance: via (/SCL) at (143.84, 88.86) mm vs board edge / rule area (gap 0.491 mm < required 0.500 mm)` |
| **`Starling_Starling_V1`** (`25` fp) | **`v2` compact (on `v1_r110`)** | **`0.9636363636363636` (`4/10`)** | **`3.63% / 7.14%`** | `63.706 x 25.228` (**`1.0599x`**) | **`0 / 0`** | **`0 / 0`** | `70` | **`510`** | `1210.502482` | `0 / 13 / 13` | `8 / 0` | `34.570 / 34.585` | `Clearance on F.Cu: P6-4 (/H_RX) at (145.16, 87.60) mm vs track (/ADC7) near (144.16, 88.36) mm (gap 0.174 mm < required 0.178 mm)` |
| **`Starling_Starling_V1`** (`25` fp) | `v2` `110%` route | `1.0` | -- | `66.11 x 26.18` (`1.1000x`) | `0 / 0` | `0 / 0` | `60` | `524` | `1351.653868` | `5 / 24 / 29` | -- | `-- / 6.801` | -- |
| **`Starling_Starling_V1`** (`25` fp) | **`v2` compact (end-to-end, `det=True`)** | **`0.9363636363636363` (`7/10`)** | **`6.36% / 12.32%`** | `61.903 x 24.514` (**`1.0299x`**) | **`0 / 0`** | **`0 / 0`** | `60` | **`591`** | `1298.931459` | `4 / 15 / 19` | `17 / 0` | `55.041 / 55.056` | `EdgeClearance: track (/H_RX) near (129.19, 88.81) mm vs board edge / rule area (gap 0.495 mm < required 0.500 mm)` |
| **`Starling_Starling_V1`** (`25` fp) | `v2` `100%` direct route | `1.0` | -- | `60.1 x 23.8` (`1.0000x`) | `0 / 0` | `0 / 0` | `64` | `484` | `1223.17059` | `6 / 31 / 37` | -- | `-- / 13.844` | -- |
| **`Starling_Starling_V1`** (`25` fp) | **`v2` `100% -> 0.80` compact** | **`0.96` (`4/20`)** | **`4.00% / 7.84%`** | `57.696 x 22.848` (**`0.9600x`**) | **`0 / 0`** | **`0 / 0`** | `64` | **`567`** | `1193.295679` | `4 / 13 / 17` | `13 / 0` | `33.649 / 33.658` | `Clearance on B.Cu: P3-2 (+5C) at (140.48, 68.77) mm vs track (GND) near (140.09, 67.48) mm (gap 0.172 mm < required 0.178 mm)` |
| **`Starling_Starling_V1`** (`25` fp) | **`v2` compact (`v1_d100 -> 0.80`)** | **`0.96` (`4/20`)** | **`4.00% / 7.84%`** | `57.696 x 22.848` (**`0.9600x`**) | **`0 / 3`** | **`0 / 3`** | `79` | **`541`** | `1045.296304` | `2 / 18 / 20` | `14 / 0` | `33.802 / 33.816` | `Clearance on F.Cu: P3-3 (/RST) at (143.05, 68.77) mm vs track (/SCK) near (143.11, 67.45) mm (gap 0.175 mm < required 0.178 mm)` |
| **`BMS-bq76940`** (`134` fp) | `v1` `100%` direct route | `1.0` | -- | `67.945 x 75.565` (`1.0000x`) | `0 / 4` | `0 / 4` | `50` | `723` | `1573.602596` | `37 / 33 / 70` | -- | `-- / 108.501` | -- |
| **`BMS-bq76940`** (`134` fp) | `v1` `110%` route | `1.0` | -- | `74.7395 x 83.1215` (`1.1000x`) | `0 / 4` | `0 / 4` | `62` | `662` | `1779.898276` | `27 / 30 / 57` | -- | `-- / 110.145` | -- |
| **`BMS-bq76940`** (`134` fp) | `v1` compact (`v1_r110`) | `0.9727272727272727` (`3/10`) | `2.72% / 5.38%` | `72.70115 x 80.85455` (`1.0699x`) | `0 / 4` | `0 / 4` | `62` | `1172` | `1721.80958` | `27 / 55 / 82` | -- | `-- / 28.227` | `Clearance on F.Cu: C10-2 (/BatPack/VC11) at (189.72, 107.82) mm vs via (/BatPack/VC7) at (189.97, 109.12) mm (gap 0.165 mm < required 0.200 mm)` |
| **`BMS-bq76940`** (`134` fp) | **`v2` compact (on `v1_r110`)** | **`0.9090909090909091` (`10/10`)** | **`9.09% / 17.35%`** | `67.945 x 75.565` (**`1.0000x`**) | **`0 / 3`** | **`0 / 3`** | `62` | **`759`** | `1554.644381` | `4 / 32 / 36` | `41 / 0` | `82.493 / 82.546` | **Reached full target `1/1.1`** |
| **`BMS-bq76940`** (`134` fp) | `v2` `110%` route | `1.0` | -- | `74.7395 x 83.1215` (`1.1000x`) | `0 / 0` | `0 / 0` | `61` | `1009` | `1877.772963` | `1 / 23 / 24` | -- | `-- / 92.766` | -- |
| **`BMS-bq76940`** (`134` fp) | **`v2` compact (end-to-end, `det=True`)** | **`0.9090909090909091` (`10/10`)** | **`9.09% / 17.35%`** | `67.945 x 75.565` (**`1.0000x`**) | **`0 / 0`** | **`0 / 0`** | `61` | **`1099`** | `1651.745619` | `0 / 20 / 20` | `59 / 0` | `77.554 / 77.604` | **Reached full target `1/1.1`** |
| **`BMS-bq76940`** (`134` fp) | `v2` `100%` direct route | `1.0` | -- | `67.945 x 75.565` (`1.0000x`) | `0 / 3` | `0 / 3` | `49` | `894` | `1568.245339` | `1 / 33 / 34` | -- | `-- / 109.665` | -- |
| **`BMS-bq76940`** (`134` fp) | **`v2` compact (`v1_d100 -> 0.80`)** | **`0.9` (`10/20`)** | **`10.00% / 19.00%`** | `61.1505 x 68.0085` (**`0.9000x`**) | **`0 / 4`** | **`0 / 4`** | `50` | **`800`** | `1347.170343` | `5 / 49 / 54` | `86 / 1` | `73.694 / 73.754` | `Clearance on F.Cu: track (B) near (186.36, 116.10) mm vs Q6-1 (N-000006) at (186.73, 116.81) mm (gap 0.067 mm < required 0.200 mm)` |
| **`AmpOne_dev-AmpOne`** (`164` fp) | `v1` `100%` direct route | `1.0` | -- | `100.0 x 100.0` (`1.0000x`) | `0 / 0` | `0 / 0` | `37` | `707` | `2165.034581` | `29 / 21 / 50` | -- | `-- / 78.384` | -- |
| **`AmpOne_dev-AmpOne`** (`164` fp) | `v1` `110%` route | `1.0` | -- | `110.0 x 110.0` (`1.1000x`) | `0 / 0` | `0 / 0` | `54` | `804` | `2439.937209` | `51 / 13 / 64` | -- | `-- / 86.815` | -- |
| **`AmpOne_dev-AmpOne`** (`164` fp) | `v1` compact (`v1_r110`) | `0.990909090909091` (`1/10`) | `0.90% / 1.80%` | `109.0 x 109.0` (`1.0900x`) | `0 / 0` | `0 / 0` | `54` | `2835` | `2426.503029` | `36 / 16 / 52` | -- | `-- / 29.108` | `Clearance on B.Cu: U301-23 (/InputStage/P2) at (130.40, 110.75) mm vs via (Net-(C324-Pad1)) at (128.82, 111.18) mm (gap 0.147 mm < required 0.150 mm)` |
| **`AmpOne_dev-AmpOne`** (`164` fp) | **`v2` compact (on `v1_r110`)** | **`0.990909090909091` (`1/10`)** | **`0.90% / 1.80%`** | `109.0 x 109.0` (**`1.0900x`**) | **`0 / 0`** | **`0 / 0`** | `54` | **`747`** | `2398.122156` | `1 / 11 / 12` | `0 / 0` | `32.676 / 32.686` | `Clearance on B.Cu: U301-32 (/InputStage/N1) at (130.42, 115.27) mm vs via (/TDA7498/GAIN1A) at (131.95, 114.78) mm (gap 0.141 mm < required 0.150 mm)` |
| **`AmpOne_dev-AmpOne`** (`164` fp) | `v2` `110%` route | `1.0` | -- | `110.0 x 110.0` (`1.1000x`) | `0 / 0` | `0 / 0` | `49` | `888` | `2443.193722` | `2 / 27 / 29` | -- | `-- / 47.953` | -- |
| **`AmpOne_dev-AmpOne`** (`164` fp) | **`v2` compact (end-to-end, `det=True`)** | **`0.9090909090909091` (`10/10`)** | **`9.09% / 17.35%`** | `100.0 x 100.0` (**`1.0000x`**) | **`0 / 0`** | **`0 / 0`** | `49` | **`943`** | `2169.572772` | `2 / 25 / 27` | `36 / 0` | `137.338 / 137.371` | **Reached full target `1/1.1`** |
| **`AmpOne_dev-AmpOne`** (`164` fp) | `v2` `100%` direct route | `1.0` | -- | `100.0 x 100.0` (`1.0000x`) | `0 / 0` | `0 / 0` | `42` | `993` | `2235.903517` | `4 / 30 / 34` | -- | `-- / 107.286` | -- |
| **`AmpOne_dev-AmpOne`** (`164` fp) | **`v2` `100% -> 0.80` compact** | **`0.8` (`20/20`)** | **`20.00% / 36.00%`** | `80.0 x 80.0` (**`0.8000x`**) | **`0 / 0`** | **`0 / 0`** | `42` | **`1199`** | `1757.730814` | `11 / 94 / 105` | `111 / 5` | `237.296 / 237.352` | **Reached full target `0.80`** |
| **`AmpOne_dev-AmpOne`** (`164` fp) | **`v2` compact (`v1_d100 -> 0.80`)** | **`0.8` (`20/20`)** | **`20.00% / 36.00%`** | `80.0 x 80.0` (**`0.8000x`**) | **`0 / 0`** | **`0 / 0`** | `37` | **`884`** | `1684.045326` | `10 / 78 / 88` | `100 / 3` | `218.228 / 218.280` | **Reached full target `0.80`** |

### 6.2 Key Findings on Experiment C

- **`BMS-bq76940_EvaluationsBoard` (`134` footprints) — Complete `110% -> 100.00%` End-to-End Solve**:
  - Direct `100%` routing still fails on current `main` (`8b71912`) after `109.665 s` with **`3` unconnected items**.
  - Scaling to `110%` (`v2 scale 1.1`, `0` DRC errors) allows `ultraroute route` to solve **`0` unconnected in `92.766 s`**, and **`v2 compact 1/1.1` (`77.554 cpu s`) accepts all `10/10` steps (`0.9090909090909091`) back to `67.945 x 75.565 mm` (`100.00%` original board outline) with `0` unconnected items, `0` pre-refill DRC errors, `0` post-refill DRC errors, and `20` post-pour acid traps** (vs `v1`'s `3/10` steps to `72.70115 x 80.85455 mm`, `4` unconnected, and `82` acid traps).
  - Even on `v1`'s `routed_110_s0` input, `v2` compacts **`10/10` steps (`0.9090909090909091`, `759` segments vs `1,172` in `v1`)**.
- **`Starling_Starling_V1` (`25` footprints)**:
  - End-to-end `v2` `110% -> 100%` advances from `v1`'s `4/10` (`63.706 x 25.228 mm`, `105.99%`, `994` segs, `56` acid traps) to **`7/10` steps (`0.9363636363636363`, `61.903 x 24.514 mm` = `102.99%` of original size, `591` segs, `19` acid traps, `0/0` DRC)** (`EdgeClearance: track (/H_RX) near (129.19, 88.81) mm vs board edge / rule area (gap 0.495 mm < required 0.500 mm)`).
  - Compacting the clean `100%` route toward `0.80` shrinks `Starling` below its original bare dimensions to **`57.696 x 22.848 mm` (`0.96`, `4.00%` linear shrink, `7.84%` area saved, `0/0` DRC)**.
- **`AmpOne_dev-AmpOne` (`164` footprints, `100 x 100 mm`)**:
  - On `v1`'s `routed_110_s0` input (`from_v1_routed_110`), `v2` cuts segment count from `2,835` (`v1`) to **`747`** (`52 -> 12` post-pour acid traps) at `1/10` steps (`0.990909090909091`); end-to-end **`v2 scale 1.1 -> route -> v2 compact 1/1.1` reaches `10/10` steps (`0.9090909090909091`, `100.0 x 100.0 mm`, `943` segments, `27` acid traps, `0/0` pre- and post-refill DRC)**.
  - Compacting `v1`'s `100%` routed board toward `0.80` reaches **`20/20` steps (`0.8`, `80.0 x 80.0 mm`, `20.00%` linear shrink, `36.00%` area saved, `100` part stops, `3` via slides, `884` segments, `0/0` DRC)**; compacting current `main`'s `100%` route reaches **`0.8` (`20/20` steps, `80.0 x 80.0 mm`, `20.00%` linear shrink, `36.00%` area saved, `0/0` DRC)**.


---

## 7. Generalization Batch: 24 Stratified Routed PCBench Boards (`--factor 0.80 --steps 20`)

Each of the 24 PCBench boards below was routed from `board.kicad_pcb` (`ultraroute route --strip-all --recipe default --seed 1 --uuid-seed 1 --threads 1 --timeout 120 --cpu-clock`), judged with KiCad 9 DRC **both before and after `tools/refill_zones.py`**, compacted toward `--factor 0.80 --steps 20 --uuid-seed 1` (`RAYON_NUM_THREADS=1`), re-run a second time from the same routed input to verify **byte-identical output (`SHA-256` + full byte comparison)**, and re-judged with KiCad 9 DRC before and after zone refill.

### 7.1 Aggregate Outcomes Across All 24 Boards

- **DRC & connectivity safety (pre-refill and post-refill)**: **`0` boards added a pre-refill DRC error, `0` added a pre-refill unconnected item, `0` added a post-refill DRC error, and `0` added a post-refill unconnected item** (`24/24` clean of any regression).
- **Determinism (`compact` run twice on identical input)**: **`24/24` byte-identical (`100%`)**.
- **Achieved linear shrink**: median **`9.50%`**, mean **`9.91%`**, range **`1.00%` – `20.00%`** (**`5/24`** boards reached the full `0.80` target in `20/20` steps).
- **Board area saved**: median **`18.09%`**, mean **`18.39%`**, range **`1.99%` – `36.00%`**.
- **Per-part stops & topological via slides**: **`22/24`** boards engaged per-part stops (`part_stops > 0`); **`3/24`** boards executed topological via slides (`6` total via slides).
- **Track segment counts & wirelength**: total segments across all 24 boards went from **`9649` -> `10561`** (`1.094x`), while total track length dropped from **`19836.60 mm` -> `17617.57 mm`**.
- **Compaction CPU time (`RAYON_NUM_THREADS=1`)**: median **`31.537 s`**, mean **`54.238 s`**, total **`1301.717 s`** across all 24 boards.
- **What stopped shrink across the 24 boards**:
  - `EdgeClearance` (outline vs stopped part/copper): **`9/24`**
  - Copper pad/track-to-track channel capacity: **`6/24`**
  - Reached full target `0.80`: **`5/24`**
  - Static / hole / mask clearance limit: **`2/24`**
  - Courtyard / static envelope limit: **`1/24`**
  - Copper pad/track-to-via channel capacity: **`1/24`**

### 7.2 Per-Board Results (`Before -> After`, Unrounded / Floor-Truncated)

| Board (`PCBench`) | Layers | Footprints (`F/B`) | Size `Before -> After` (`mm`) | Achieved Factor (`steps`) | Linear Shrink | Area Saved | Pre-Refill DRC `err/unconn` (`Before -> After`) | Post-Refill DRC `err/unconn` (`Before -> After`) | Track Segs (`Before -> After`) | Vias (`Before -> After`) | `part_stops / via_slides` | CPU `s` | Byte-Identical `2x` | What Stopped It |
| --- | :---: | :---: | --- | --- | ---: | ---: | :---: | :---: | :---: | :---: | :---: | ---: | :---: | --- |
| `LED_port-status_LED_port-status` | `2L` | `21` (`17/4`) | `23.749 x 12.4968` -> `23.03653 x 12.121896` | `0.97` (`3/20`) | `3.00%` | `5.91%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `120 -> 124` | `16 -> 16` | `4 / 0` | `10.887` | `True` | `EdgeClearance: P1-1 (Net-(D1-Pad1)) at (101.59, 91.31) mm vs board edge / rule area (gap 0.383 mm < required 0.500 mm)` |
| `ESP-Breakout_ESP-Breakout` | `2L` | `17` (`4/13`) | `50.8 x 20.32` -> `50.292 x 20.1168` | `0.99` (`1/20`) | `1.00%` | `1.99%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `127 -> 126` | `17 -> 17` | `0 / 0` | `2.752` | `True` | `EdgeClearance: M1-1 (/RESET) at (124.31, 136.27) mm vs board edge / rule area (gap 0.457 mm < required 0.500 mm)` |
| `ESP07-Breakout_ESP07-Breakout` | `2L` | `25` (`10/15`) | `31.75 x 49.53` -> `31.115 x 48.5394` | `0.98` (`2/20`) | `2.00%` | `3.96%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `183 -> 191` | `15 -> 15` | `3 / 0` | `12.135` | `True` | `EdgeClearance: P1-1 (GND) at (193.79, 142.88) mm vs board edge / rule area (gap 0.298 mm < required 0.500 mm)` |
| `ESP8266-MQTT-battery-monitor-hw_battery-monitor` | `2L` | `49` (`38/11`) | `50.0 x 50.0` -> `40.0 x 40.0` | `0.8` (`20/20`) | `20.00%` | `36.00%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `258 -> 298` | `20 -> 20` | `15 / 0` | `32.105` | `True` | **Reached target `0.80`** (`20/20`) |
| `KiwiSDR_PCB_ant` | `2L` | `85` (`76/9`) | `138.0 x 38.0` -> `135.24 x 37.24` | `0.98` (`2/20`) | `2.00%` | `3.96%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `552 -> 580` | `24 -> 24` | `1 / 0` | `10.318` | `True` | `EdgeClearance: EP501-1 (/EPROBE) at (110.88, 69.00) mm vs board edge / rule area (gap 0.000 mm < required 0.500 mm)` |
| `Biscay_Blueeye_sipm-comp` | `2L` | `57` (`49/8`) | `50.0 x 50.0` -> `45.5 x 45.5` | `0.91` (`9/20`) | `9.00%` | `17.19%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `709 -> 778` | `37 -> 37` | `33 / 0` | `51.455` | `True` | `Clearance on F.Cu: C1-1 (+5VA) at (79.99, 59.84) mm vs track (/COMP_12) near (80.18, 60.45) mm (gap 0.105 mm < required 0.200 mm)` |
| `BUF-DiffToSE-ADAU1966_BUF-DiffToSE-ADAU1966` | `2L` | `165` (`145/20`) | `100.0 x 100.0` -> `84.0 x 84.0` | `0.8400000000000001` (`16/20`) | `15.99%` | `29.44%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `711 -> 733` | `29 -> 29` | `27 / 0` | `108.550` | `True` | `SolderMaskBridge on F.Cu: R309-1 vs R314-1 (gap 0.395 mm < required 0.401 mm)` |
| `avr_ledprojector_avr_ledprojection-0402` | `2L` | `91` (`87/4`) | `25.4 x 34.544` -> `21.59 x 29.3624` | `0.8500000000000001` (`15/20`) | `14.99%` | `27.75%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `649 -> 724` | `56 -> 56` | `56 / 0` | `30.036` | `True` | `Clearance on F.Cu: C2-1 (+5V) at (75.62, 70.51) mm vs track (GND) near (76.50, 71.02) mm (gap 0.182 mm < required 0.203 mm)` |
| `AzizLight_AzizLight` | `2L` | `55` (`55/0`) | `70.104 x 29.21` -> `64.49568 x 26.8732` | `0.92` (`8/20`) | `8.00%` | `15.36%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `274 -> 292` | `22 -> 22` | `23 / 0` | `17.019` | `True` | `Clearance on F.Cu: D1-2 (Net-(D1-Pad2)) at (148.46, 92.40) mm vs track (GND) near (148.46, 91.78) mm (gap 0.191 mm < required 0.201 mm)` |
| `QRPCard_QRPCard` | `2L` | `33` (`33/0`) | `87.122 x 76.708` -> `69.6976 x 61.3664` | `0.8` (`20/20`) | `20.00%` | `36.00%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `238 -> 269` | `17 -> 17` | `12 / 2` | `325.417` | `True` | **Reached target `0.80`** (`20/20`) |
| `1-Wire-Wing-pcb_1-Wire_Wing` | `2L` | `23` (`12/11`) | `69.85 x 24.13` -> `55.88 x 19.304` | `0.8` (`20/20`) | `20.00%` | `36.00%` | `11/0 -> 11/0` | `11/0 -> 11/0` | `145 -> 171` | `15 -> 15` | `16 / 0` | `66.927` | `True` | **Reached target `0.80`** (`20/20`) |
| `655_testboard` | `2L` | `30` (`21/9`) | `39.1668 x 62.9158` -> `33.683448 x 54.107588` | `0.8600000000000001` (`14/20`) | `13.99%` | `26.04%` | `52/0 -> 52/0` | `52/0 -> 52/0` | `347 -> 390` | `37 -> 37` | `19 / 0` | `124.932` | `True` | `Clearance on B.Cu: S0-2 () at (225.68, 157.88) mm vs track (VCC) near (226.61, 157.63) mm (gap 0.198 mm < required 0.255 mm)` |
| `6volt-5W-solar-cc_6vleadacidsolar` | `2L` | `65` (`54/11`) | `50.0 x 50.0` -> `40.0 x 40.0` | `0.8` (`20/20`) | `20.00%` | `36.00%` | `4/0 -> 4/0` | `4/0 -> 4/0` | `337 -> 384` | `25 -> 25` | `25 / 0` | `70.546` | `True` | **Reached target `0.80`** (`20/20`) |
| `Brushless_ESC_Brushless_ESC` | `2L` | `77` (`42/35`) | `63.0 x 36.0` -> `56.7 x 32.4` | `0.9` (`10/20`) | `10.00%` | `19.00%` | `30/0 -> 30/0` | `30/0 -> 30/0` | `551 -> 637` | `50 -> 50` | `51 / 2` | `44.196` | `True` | `CourtyardOverlap on F.CrtYd: C19 vs C3 (overlap 0.002 mm)` |
| `FlashProgrammer_flash_programmer` | `2L` | `49` (`37/12`) | `60.96 x 38.1` -> `57.912 x 36.195` | `0.95` (`5/20`) | `5.00%` | `9.75%` | `5/0 -> 5/0` | `5/0 -> 5/0` | `548 -> 601` | `59 -> 59` | `18 / 0` | `43.174` | `True` | `EdgeClearance: track (/Control/ISP_MISO) near (96.64, 58.48) mm vs board edge / rule area (gap 0.477 mm < required 0.500 mm)` |
| `EncoderBoard_Enc_Pan_Led` | `2L` | `58` (`30/28`) | `42.0 x 34.0` -> `36.54 x 29.58` | `0.87` (`13/20`) | `13.00%` | `24.31%` | `39/0 -> 39/0` | `39/0 -> 39/0` | `453 -> 618` | `43 -> 43` | `45 / 2` | `81.899` | `True` | `Clearance on F.Cu: D12-1 (Net-(D12-Pad1)) at (143.41, 79.63) mm vs track (/BTN0) near (142.99, 81.13) mm (gap 0.200 mm < required 0.201 mm)` |
| `DasBlinkinput_Das Blinkinput` | `4L` | `36` (`26/10`) | `25.4 x 50.8` -> `23.876 x 47.752` | `0.9400000000000001` (`6/20`) | `5.99%` | `11.64%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `207 -> 211` | `20 -> 20` | `18 / 0` | `18.161` | `True` | `EdgeClearance: P101-1 (GND) at (96.81, 70.61) mm vs board edge / rule area (gap 0.248 mm < required 0.500 mm)` |
| `PDB_OSD_HARDWARE_Quadcopter Power Board` | `4L` | `140` (`138/2`) | `38.0 x 44.6` -> `33.82 x 39.694` | `0.89` (`11/20`) | `11.00%` | `20.79%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `379 -> 418` | `10 -> 10` | `25 / 0` | `30.969` | `True` | `EdgeClearance: 3S_4S-1 (LIPO_IN) at (139.00, 71.41) mm vs board edge / rule area (gap 0.414 mm < required 0.500 mm)` |
| `4_digit_hex_display_4_digit_display` | `4L` | `46` (`36/10`) | `62.23 x 21.59` -> `61.6077 x 21.3741` | `0.99` (`1/20`) | `1.00%` | `1.99%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `518 -> 539` | `52 -> 52` | `31 / 0` | `24.432` | `True` | `Clearance on B.Cu: via (/D13) at (112.32, 114.36) mm vs track (GNDREF) near (111.64, 114.61) mm (gap 0.220 mm < required 0.229 mm)` |
| `a123-battery-integration_BCM` | `4L` | `67` (`67/0`) | `66.04 x 73.66` -> `52.832 x 58.928` | `0.8` (`20/20`) | `20.00%` | `36.00%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `485 -> 508` | `51 -> 51` | `39 / 0` | `129.117` | `True` | **Reached target `0.80`** (`20/20`) |
| `FMCW_RADAR_Radar MCU` | `4L` | `104` (`70/34`) | `36.9062 x 32.9438` -> `35.06089 x 31.29665` | `0.95` (`5/20`) | `5.00%` | `9.74%` | `12/0 -> 12/0` | `12/0 -> 12/0` | `817 -> 811` | `87 -> 87` | `39 / 0` | `36.620` | `True` | `Clearance on F.Cu: U4-8 (Net-(U4-Pad8)) at (137.40, 63.47) mm vs via (RESET) at (136.79, 62.98) mm (gap 0.140 mm < required 0.150 mm)` |
| `DA_Lamp_attiny44a_servo_i2c` | `2L` | `7` (`7/0`) | `22.0472 x 31.3817` -> `19.84248 x 28.24353` | `0.9` (`10/20`) | `10.00%` | `19.00%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `71 -> 86` | `6 -> 6` | `5 / 0` | `7.313` | `True` | `EdgeClearance: drawn copper () near (173.33, 120.05) mm vs board edge / rule area (gap 0.453 mm < required 0.500 mm)` |
| `LT3652EvalBoard_LT3652EvalBoard` | `2L` | `47` (`47/0`) | `49.8 x 49.8` -> `48.804 x 48.804` | `0.98` (`2/20`) | `2.00%` | `3.96%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `194 -> 215` | `6 -> 6` | `1 / 0` | `6.063` | `True` | `EdgeClearance: REF**- () at (112.57, 70.35) mm vs board edge / rule area (gap 0.253 mm < required 0.500 mm)` |
| `kitspace_120-channel-test-board` | `2L` | `258` (`258/0`) | `76.0062 x 50.7872` -> `72.20589 x 48.24784` | `0.95` (`5/20`) | `5.00%` | `9.75%` | `0/0 -> 0/0` | `0/0 -> 0/0` | `776 -> 857` | `238 -> 238` | `0 / 0` | `16.685` | `True` | `SolderMaskBridge on F.Cu: P10-1 vs P14-1 (gap 0.495 mm < required 0.509 mm)` |

---

## 8. Costs, Regressions / Tradeoffs, and What Physically Limits Shrink Now

### 8.1 Runtime Cost and Tradeoffs (`router-change-rules.md` Rule 5 & 10)

1. **`ultraroute route` cost**: **`0` overhead** — `ultraroute route` shares zero runtime execution paths with `morph::compact` or `morph::scale` and produces byte-identical output to `main` (`8b71912`) across all tested boards.
2. **Per-step `compact` CPU cost on unblocked vs pinched steps**:
   - Steps where Pass `0` copper relaxation + `slide_pinched_vias` clears all dynamic violations (`count_dynamic_violations(board).0 == 0`) exit before Pass `1` and run at roughly `0.5..1.5 cpu s` per step on small/medium PCBench boards (`1.2..2.5 cpu s` on `earring`), comparable to `v1` while cutting output segment counts roughly in half via 10-pass `shorten_taut_chains`.
   - Steps where Pass `0` leaves cross-layer or channel pinches enter Passes `1..5` (joint rigid-footprint family + copper half-plane solve + cross-footprint mask/hole guards), rebuild `step_bare_board` when footprints shift, and run `settle_copper_only` (`3` passes). On heavily pinched runs that now accept up to `10x` more steps than `v1` (`4..9/10` steps vs `1..5/10` in Experiment B; `10/10` steps vs `3/10` on `BMS-bq76940`; `10/10` steps vs `1/10` on `AmpOne`), total run time increases proportionally to the additional accepted steps + joint relaxation (`14.75..40.37 s` in `v1` vs `30.45..86.55 s` in end-to-end `v2` Exp B, `28.22 -> 77.60 s` on `BMS-bq76940` for `10/10` vs `3/10` steps, and `10.15 -> 26.85 s` on `A1` for `3/15` vs `1/15` steps).
   - Every loop in `scale` and `compact` is strictly bounded by deterministic pass/iteration/step counts (`0` `Deadline::seconds` time slices).
3. **Post-pour narrow-slot DFM tradeoff at deep compression**:
   - Post-pour acute-angle traps (`acute`) drop sharply across every experiment compared to `v1` (`A2`: `87 -> 13`, `Exp B`: `47..59 -> 9..14`, `Starling`: `16 -> 0..4`, `BMS-bq76940`: `27 -> 0..4`, `AmpOne`: `36 -> 1..2`) because spurious grazing splits were eliminated and `shorten_taut_chains` pulls zig-zags straight.
   - When a board compresses `7%..20%` linearly, parallel same-net serpentine/bus traces and zone-pour fingers naturally sit closer together on outer layers, which can modestly raise `tools/dfm.py` same-net narrow-`slots` count on deeply compacted boards (`A1`: `72 -> 82` slots at `3.00%` shrink vs `1.00%` in `v1`; `Exp B s1`: `57 -> 78` slots at `8.18%` shrink vs `2.72%` in `v1`), while total post-pour acid traps (`acute + slots`) are lower than `v1` on `A2` (`71` vs `148`), all `4` seeds of `Exp B` (`60..89` vs `102..116`), `Starling` (`13..19` vs `56`), `BMS-bq76940` (`20..36` vs `82`), and `AmpOne` (`12..27` vs `52`).

### 8.2 What Physically Limits Shrink Now

With opposite-side stationary-via pinches (`Goal 1`), offset/enclosing footprint collisions (`Goal 2`), and staircase shattering (`Goal 3`) resolved, compaction across `earring` and the 24 PCBench boards stops for four concrete physical reasons:

1. **Uniform `Edge.Cuts` outline catching stopped perimeter parts or rigid copper labels (`EdgeClearance`)**:
   - Once interior courtyards, mask webs, or copper channels saturate and hold perimeter footprints back (`part_stops > 0`), or when a rigid board-level copper graphic (`gr_text` on `DA_Lamp_attiny44a_servo_i2c` after `10.00%` linear shrink) meets the shrinking board boundary, the uniformly scaled `Edge.Cuts` polygon (`step_f` about `centre`) meets the stopped perimeter pad, mounting hole, copper label, or perimeter trace at `min_copper_edge_clearance` (`0.20..0.50 mm`). This stops `9/24` PCBench generalization boards as well as `Starling` `v2_compact_100` (`0.9363636363636363`).
2. **Board-spanning rigid footprints and courtyard/mask packing (`CourtyardOverlap` / `SolderMaskBridge` / `HoleClearance`)**:
   - Rigid mechanical footprints cannot shrink their own pad/courtyard span (`BT1` is `28 x 32 mm` inside `earring`'s `30.5 x 35.75 mm` outline; wide connectors/headers fix inter-row pitch), and adjacent SMD footprints cannot close past their courtyard envelope or cross-footprint solder-mask opening clearance (`2 * pad_to_mask_clearance + solder_mask_min_width`). Once a continuous chain of touching courtyards or mask apertures spans from the board centre to the outline (`earring` `A1` at `0.97` on `R14 vs R15`), no rigid translation without part rotation or re-placement can shrink that axis further.
3. **Multi-item homotopic channel capacity (`K` tracks + vias between locked/courtyard-stopped pads)**:
   - On `earring` `110% -> 100%` (`4..9/10` steps across Seeds `0..3`), the router at `110%` packed tracks/vias through inter-pad gaps near `U1` (`0.4 mm`-pitch QFN-60), `J1`, and the perimeter LED ring whose sum of copper widths + `.kicad_dru` clearances exceeds the `100%` pad gap (`101.00%..105.99%` of original linear size across Seeds `0..3`). `slide_pinched_vias` slides vias along their own trace out of narrow throats (`0..4` via slides per seed), and `part_stops` (`62..108` stopped parts) holds saturated pockets open locally, until those held-back pockets span to the perimeter. Passing the final `1.00%..5.99%` back to `100.00%` on `earring` requires either routing `110%` with clearances scaled by `1.1x` (`--clearance-scale 1.1`) so over-capacity channels are never populated, or local rip-and-reroute.
4. **No non-homotopic via-past-pad hop**:
   - `slide_pinched_vias` slides vias strictly along their own net's tracks (`Modes A/B/C`) and never jumps a via across a foreign pad or trace, preserving homotopy without ripping or rerouting.

### 8.3 What to Try Next

1. **Anisotropic (`fx, fy`) or offset-polygon `Edge.Cuts` shrink**: On elongated boards or boards whose horizontal span is pinned by a single wide part (`BT1` on `earring` or wide headers on PCBench breakouts), shrinking `fx` and `fy` independently or offsetting `Edge.Cuts` around the achieved footprint/copper hull would recover area along the unblocked axis after the narrower axis saturates.
2. **Channel-capacity-aware expanded routing (`--clearance-scale f`)**: Inflating track/via clearances by `f` during the `110%` route pass would prevent the router from populating inter-pad channels with `K` items that fit at `110%` (`f = 1.1`) but exceed `100%` channel capacity.
3. **Topological rubber-band arcs (`core/crates/topo`)**: Replacing piecewise-linear capsule chains with exact rubber-band tangents around pad/via discs would pull detours taut in one geometric pass.

---

## For the project memory

- **Canonical vs file ordering in `kicad-pcb` (`canon.rs` vs `write.rs`)**: Since `95a470a` / `f2d4b9e`, `kicad_pcb::parse_board` canonicalizes `pcb.footprints`, `copper.zones`, and `pcb.nets` (sorting by reference/name/geometry) and stores original file indices in `Footprint::order`, `Zone::order`, and `Pcb::net_file_ids`. However, `kicad_pcb::write::scale_edits` indexes footprints and zones by raw S-expression encounter order (`0..N` in file order), and `kicad_pcb::write::Edit` (`Track.net`, `Via.net`) writes raw file net IDs directly to `(net ...)`. Any caller of `scale_edits` or `apply_edits` must map canonical indices through `Footprint::order`, `Zone::order`, and `pcb.net_file_ids` (and re-align re-parsed boards by `Footprint::order` when coordinates change).
- **KiCad 9 pre-refill DRC treats multiple `(filled_polygon ...)` blocks in one `(zone ...)` as disconnected islands**: Even if `(filled_polygon ...)` strips abut seamlessly or overlap by `2 um`, running `kicad-cli pcb drc` *before* `refill_zones.py` flags pads/vias on different strips of the same kept zone as `unconnected_items`. Tracing each 4-connected `PourRaster` component into a single polygon with horizontal keyhole cuts (`PourRaster::polygons()` in `core/crates/morph/src/scale.rs`) makes pre-refill KiCad 9 DRC see exact zone connectivity while remaining compatible with `board::inside` ray-casting (`(a.y > p.y) != (b.y > p.y)` ignores horizontal keyhole edges).
- **Cross-footprint `solder_mask_bridge` applies when `solder_mask_min_width == 0` and on copperless `NPTH` mask apertures**: KiCad 9 checks cross-footprint mask openings whenever any `*.Mask` shapes overlap (`da + db + mask_web` even when `mask_web == 0`), including copperless `np_thru_hole` pads (`layers == 0`) with positive `solder_mask_margin` that `Board::new` omits from `board.mask_openings`.
- **Drawn copper (`gr_text`, `gr_line`, `fp_text`, `fp_line` on `*.Cu`) in `Board::new` (`Kind::Pour { zone: board::DRAWN, kept: true }`)**: Rigid board-level `gr_text` glyphs scale their anchor `(at x y)` with `Edge.Cuts` without shrinking their font size, so static step validation must check `zone == board::DRAWN` against `Edge.Cuts` (`copper_edge_clearance`), `Keepout`, unplated `NPTH` `hole_clearance`, and distinct-net (`oit.net != it.net`) drawn copper, while ignoring same-label `<no net>` (`net == 0`) stroke crossings that `board::check_item` flags as `Rule::Clearance`.
- **Footprint `(locked yes)` vs `--lock` in `morph`**: KiCad's file-level `(locked yes)` flag on footprints is set on 60 of 167 footprints in `earring.kicad_pcb` (all 58 LEDs, `BT1`, and `J1`) to prevent accidental GUI drags, whereas `ScaleOptions::lock` / `CompactOptions::lock` (`--lock REF,...`) is the explicit compaction lock set. Track and via `(locked yes)` (`item.locked`) is respected as fixed copper.
- **`~/.claude/bin/reserve` leaks queue entries when killed by `SIGTERM` while waiting**: `~/.claude/bin/reserve` installs its `SIGINT`/`SIGTERM`/`SIGHUP` signal handlers after the `while e["state"] == "waiting":` loop. Wrapping `reserve` in default `timeout N` sends `SIGTERM`, killing the client without calling `/api/release/<eid>`; always use `timeout --signal=INT` around `reserve`.
- **Unnamed NPTH mounting bosses share `pad.number == ""`**: Connectors like `earring`'s `J1` have multiple unnamed (`""`) `np_thru_hole` pads on the same footprint. Baseline DRC maps and pad tables must key pads by `(footprint_idx, pad_ordinal)` rather than `(footprint_idx, pad.number)`.
