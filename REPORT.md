# Report: Differential Pairs Routed Coupled End to End

## 1. Summary of What Was Built

We redesigned `--diff-pairs` (`core/crates/grid/src/pairs.rs` and pipeline integration in
`core/crates/ultraroute/src/lib.rs`) so differential pairs route coupled end to end from pad to
pad, including symmetric leg convergence from spread connector/ESD pins, coupled layer changes
through paired vias, matched bridges across short spans, and full locking of pair copper through
subsequent single-ended routing stages.

### Modified Files and Flags

- `core/crates/ultraroute/src/lib.rs`:
  - Added `--diff-pairs-legacy` (`Args::diff_pairs_legacy`) to run the previous single-layer,
    legless pair stage (`PairOptions { legacy: true, .. }`) for A/B experiments in `tools/lab.py`.
  - Coordinated `prefanout` with `route_pairs`:
    - Before `route_pairs`, `prefanout` skips a differential pair only when all of its pads share a
      layer and it still has open runs (`grid::shares_layer(&board, p) && grid::pair_has_open_runs(&board, p, a.pours_block)`).
      Pairs whose pads span multiple layers (such as `USB_DP`/`USB_DM` on `earring`, where `U1` and
      `R9`/`R11` sit on `F.Cu` and locked test points `TP2`/`TP3` sit on `B.Cu`) still get early
      `prefanout` dogbones in harmony with their footprint's other pins, and `route_pairs` connects
      directly to those inner-layer dogbones and locks them.
    - After `route_pairs`, `fanout_laid.retain(|&id| board.item(id).is_some_and(|it| !it.locked))`
      removes any `prefanout` escape items that `route_pairs` locked into a coupled run so
      `shove_escape` never rips them up, and a second `prefanout` pass escapes any pair pads that
      still have open runs before `negotiate`.
- `core/crates/grid/src/pairs.rs`:
  - **Net-class impedance and via spacing (`diff_pairs`, `via_pitches`)**:
    - Track width and edge-to-edge gap come from the positive net's class (`diff_pair_width`,
      `diff_pair_gap`), clamped to `min_track_width` and `clearance`.
    - Paired via centre-to-centre pitch (`pref_vp`, `min_vp`) honours `diff_pair_via_gap` when set
      on the net class, bounded below by copper clearance, solder-mask margin, and drill
      hole-to-hole rules (`VIA_SLACK = 4_000 nm`).
  - **End-to-end coupled centreline search (`couple_end_to_end`, `coupled_grid`, `path_stretches`)**:
    - Searches five concentric rings (`END_RINGS`, `0..3.4 mm`) around each pin pair's midpoint in
      increasing order of `ia + ib`, evaluating both single-layer centrelines on every non-plane
      layer and multi-layer centrelines with paired layer-change vias.
    - Scales soft inner-pour region penalties on the centreline grid so board-wide inner pour
      layers act as a gentle tie-breaker rather than forcing pairs onto crowded outer layers.
    - When a connector has duplicate pin pairs already jumpered together (such as USB-C `A6/B6` and
      `A7/B7`), evaluates both attachment sites and keeps the lowest-cost end-to-end candidate.
  - **Coupled stretches and paired vias (`lay_coupled_stretches`, `evaluate_stretches`)**:
    - Places paired vias at each layer transition along the centreline, keeping track polarity
      (`p_left`) unchanged on forward turns and flipping cleanly on foldbacks
      (`dot(n_in, n_out) < 0`) so offset tracks never cross on either layer.
    - Trims stretch ends by the minimum lead-in needed for each offset track to clear its partner's
      via and mask opening before joining its own via.
  - **Spread-pin leg convergence (`route_pair_legs`, `chamfer_paths`, `try_window_leg_pair`, `route_single_leg_to_point`, `tighten_leg_items`)**:
    - Tier 1 tries direct and symmetric 45-degree chamfered legs on `entry_layer` (straight-out
      elbow first so the bay between spread pads stays open), scoring by `len_p + len_n + 3 * |len_p - len_n|`.
    - Tier 2 runs a local `25 um` (`LEG_CELL`) window A* search, always trying `allow_vias = false`
      before `allow_vias = true` and both net orders (`p_first`), then pulling consecutive collinear
      segments tight (`tighten_leg_items`). Trying `allow_vias = false` first reuses existing
      inner-layer escape/cluster vias (`cluster_sources` within `1.8 mm`) with zero new vias and
      zero outer-layer wrap-around tracks.
  - **Middle blocker pad escape protection (`pad_outward_dir`, `middle_blocker_pads`, `middle_pad_can_escape`, `reserve_middle_pad_escapes`)**:
    - Detects pads of other nets sitting between `end.p` and `end.n` (such as middle `GND` and
      `VBUS` pads on 6-pin USB ESD arrays like `U10` on `bluebuzz` or `U3` on `earring`).
    - Computes the outward unit direction `pad_outward_dir` away from the footprint's pad centroid,
      reserves temporary outward surface tracks or outward dogbone vias during window leg routing
      (`reserve_middle_pad_escapes`), and requires in `middle_pad_can_escape` that each middle pad
      retain either a `1.50 mm` straight outward surface escape or a `0.40..1.00 mm` outward
      dogbone via with a `1.10 mm` non-inward inner-layer continuation.
  - **Short matched bridges (`route_matched_bridge`, `bridge_siblings`)**:
    - Pin pairs closer than `SHORTEST_RUN` (`1.5 mm`, such as series resistors beside an ESD chip or
      stripped USB-C `A/B` pads) and unpaired same-footprint sibling pads within `PIN_PAIR_SPAN`
      are bridged at `pair.width` and locked with the pair.
  - **Locking and functional-block pour compliance (`commit_candidate`, `legal_pair_item`)**:
    - Marks every committed coupled track, paired via, end leg, short bridge, and used local-cluster
      escape item with `it.locked = true`, protecting them from `rip_up`, `shove` (`Plough::Region`),
      `nudge`, and `pull_tight`.
    - Rejects any track or via that intrudes into a foreign kept block pour (`!it.refilled()`) even
      when `--pours-block` is not set.
- `core/crates/grid/src/grid.rs` & `core/crates/grid/src/lib.rs`:
  - Updated `ObstacleMap` pour stamping so kept functional-block pours (`!it.refilled()`) of other
    nets always block tracks on their layer and vias across layers.
  - Exported `pair_has_open_runs` and `shares_layer`.

---

## 2. Unit Tests

Added 6 unit tests in `core/crates/grid/src/pairs.rs` (`pairs::tests`):
1. `spread_pins_converge_into_an_end_to_end_coupled_run_with_matched_legs`
2. `coupled_run_changes_layers_through_paired_vias_and_respects_via_gap`
3. `locked_pair_keeps_coupling_through_rip_up_shove_and_tightening`
4. `already_connected_pair_lays_no_duplicate_copper`
5. `both_earring_usb_pairs_route_coupled_end_to_end`
6. `both_bluebuzz_usb_pairs_route_coupled_end_to_end`

All **153 workspace tests** (147 baseline + 6 new) pass in release mode:
`cargo test --release --manifest-path core/Cargo.toml --workspace`.

---

## 3. Lab Experiments and Per-Seed Results

All experiments were run via `python3 tools/lab.py run` with `--threads 1 --timeout 240`, comparing
**A (`--diff-pairs-legacy`)** and **B (`--diff-pairs`)** in the same `lab.py run`. Coupling was
measured with `core/crates/grid/examples/pair_check.rs`, block intrusions with
`core/crates/board/examples/block_check.rs`, and DRC/unconnected counts with KiCad DRC (`drc.json`).

### 3.1 Iterative Progression (v1 -> v2 -> v3)

- **v1 (`pairs-earring-1`, `pairs-bluebuzz-1`, `pairs-earring-plain-1`, `pairs-easy2350-1`)**:
  - End-to-end coupling worked on all runs (`3/3` on `earring`, `4/4` on `bluebuzz`), but
    `legal_pair_item` only checked `Kind::Pour` when `pours_block` was true, allowing inner-layer
    pair runs and leg vias to cross kept functional-block pours (`blocks=6..7` on `earring`,
    `blocks=2..3` on `bluebuzz`). In addition, `middle_pad_can_escape` was not yet enforced on
    `bluebuzz` `U10`, leaving `4` unconnected items around `U10`.
- **v2 (`pairs-earring-2`, `pairs-bluebuzz-2`, `pairs-earring-plain-2`)**:
  - Enforced `!it.refilled()` block-pour clearance in `legal_pair_item` and `ObstacleMap`,
    eliminating block intrusions (**`blocks=0` across all runs and seeds** on every board).
  - `pairs-earring-2` achieved **`unconn=0, errors={}, blocks=0` on all seeds `0-2`**, but
    `USB_DP/USB_DM` did not couple on `In2.Cu` because `prefanout` had skipped `U1-51/52` while
    escaping the other 58 pins of `U1`.
  - On `pairs-bluebuzz-2`, `U10` unconnected items dropped from `4` to `2` (`U10-2 GND` and
    `U10-5 VBUS`) because `middle_pad_can_escape` still tested inward directions (into `U10`'s
    interior pocket) and `step_nm = 0.0` (via-in-pad), and `route_pair_legs` only tried
    `allow_vias = true` when `entry_layer` was an inner layer.
- **v3 (`pairs-earring-3`, `pairs-bluebuzz-3`, `pairs-earring-plain-3`)**:
  - Restored `shares_layer(&board, p)` in pre-pair `prefanout`, tried `allow_vias = false` first in
    `route_pair_legs` so inner-layer runs reuse existing cluster vias with zero outer-layer
    wrap-arounds, locked used cluster escapes in `commit_candidate`, and restricted
    `middle_pad_can_escape` + `reserve_middle_pad_escapes` to the outward footprint direction.
  - Result: **`3/3` coupled runs on `earring` (`84%/91%` coupled on `USB_DP/DM`)**, **`4/4` coupled
    runs on `bluebuzz` (`78%/79%` on `USB_DP/DM`, `67%/59%` on `USB_DP_C/DM_C`)**, **`0` signal
    opens and `0` DRC errors on every seed of every board**, and **`unconn=0` on every seed of
    `pairs-bluebuzz-3` and `pairs-earring-plain-3`**.

---

### 3.2 Primary Board: `earring` with `--planes In1.Cu --pour-cost 3` (`pairs-earring-3`)

Recipe:
`--negotiate --rip-up --nudge --tighten --prefanout --shove-plough region --shove-reach 0.5 --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8 --strip --planes In1.Cu --pour-cost 3`

| Run | Variant | Seed | Coupled Runs (Log) | `USB_C_DP/DN` Runs (mm) | `USB_C_DP/DN` Coupled % (`pair_check`) | `USB_C_DP/DN` Skew Pre -> Post (mm) | `USB_DP/DM` Runs (mm) | `USB_DP/DM` Coupled % (`pair_check`) | `USB_DP/DM` Vias (P/N) | `USB_DP/DM` Skew Pre -> Post (mm) | KiCad Unconn (Signal) | KiCad Errors | Block Intrusions |
| :--- | :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| `a-s0` | Legacy (A) | 0 | 2/3 (16.8 mm) | `0.98` (In2) | 2% / 1% | 6.87 -> 4.35 | `15.81` (In2) | 67% / 71% | 2 / 2 | 3.80 -> 0.00 | 0 (0) | 0 | 0 |
| `a-s1` | Legacy (A) | 1 | 2/3 (16.8 mm) | `0.98` (In2) | 3% / 1% | 0.87 -> 0.00 | `15.81` (In2) | 35% / 33% | 2 / 4 | 2.27 -> 0.00 | 0 (0) | 0 | 0 |
| `a-s2` | Legacy (A) | 2 | 2/3 (16.8 mm) | `0.98` (In2) | 3% / 1% | 1.08 -> 0.00 | `15.81` (In2) | 10% / 9% | 10 / 2 | 35.39 -> 0.00 | 0 (0) | 0 | 0 |
| `b-s0` | Coupled (B) | 0 | **3/3 (23.9 mm)** | **`3.53` (In2) + `0.70` (F)** | **27% / 24%** | 3.35 -> 2.88 | **`19.66` (In2)** | **84% / 91%** | **2 / 2** | 2.33 -> **0.00** | 1 (**0 signal**, 1 GND) | 0 | 0 |
| `b-s1` | Coupled (B) | 1 | **3/3 (23.9 mm)** | **`3.53` (In2) + `0.70` (F)** | **27% / 18%** | 3.35 -> **0.15** | **`19.66` (In2)** | **84% / 91%** | **2 / 2** | 2.33 -> **0.00** | **0 (0)** | 0 | 0 |
| `b-s2` | Coupled (B) | 2 | **3/3 (23.9 mm)** | **`3.53` (In2) + `0.70` (F)** | **27% / 18%** | 3.35 -> **0.15** | **`19.66` (In2)** | **84% / 91%** | **2 / 2** | 2.33 -> **0.00** | **0 (0)** | 0 | 0 |

*(Note: In `pairs-earring-2`, B achieved `unconn=0` on all 3 seeds `b-s0`, `b-s1`, `b-s2` with `50%/48%` coupling and `0.00 mm` skew on `USB_C_DP/DN` (`3.53 mm` In2 + `1.08 mm` F), while `pairs-earring-3` coupled all `3/3` runs (`23.9 mm`) including `USB_DP/DM` at `84%/91%` across all seeds.)*

---

### 3.3 Earring Plain Regression Recipe (`pairs-earring-plain-3`)

Recipe:
`--negotiate --rip-up --nudge --tighten --strip --prefanout --shove-plough region --shove-reach 0.5 --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8`

| Run | Variant | Seed | Coupled Runs (Log) | `USB_C_DP/DN` Runs (mm) | `USB_C_DP/DN` Coupled % (`pair_check`) | `USB_C_DP/DN` Skew Pre -> Post (mm) | `USB_DP/DM` Runs (mm) | `USB_DP/DM` Coupled % (`pair_check`) | `USB_DP/DM` Vias (P/N) | `USB_DP/DM` Skew Pre -> Post (mm) | KiCad Unconn | KiCad Errors | Block Intrusions |
| :--- | :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| `a-s0` | Legacy (A) | 0 | 2/3 (14.9 mm) | `1.00` (B) | 17% / 17% | 1.54 -> 0.16 | `13.90` (In1) | 71% / 73% | 2 / 2 | 4.49 -> 0.00 | 0 | 0 | 0 |
| `a-s1` | Legacy (A) | 1 | 2/3 (14.9 mm) | `1.00` (B) | 14% / 9% | 2.28 -> 1.48 | `13.90` (In1) | 32% / 35% | 2 / 4 | 3.97 -> 0.00 | 0 | 0 | 0 |
| `a-s2` | Legacy (A) | 2 | 2/3 (14.9 mm) | `1.00` (B) | 17% / 17% | 1.54 -> 0.16 | `13.90` (In1) | 20% / 23% | 5 / 4 | 3.05 -> 0.00 | 0 | 0 | 0 |
| `b-s0` | Coupled (B) | 0 | **3/3 (21.5 mm)** | **`3.53` (In1) + `0.70` (F)** | **27% / 17%** | 3.35 -> **0.15** | **`17.30` (In2)** | **73% / 86%** | **2 / 2** | 3.59 -> **0.00** | **0** | 0 | 0 |
| `b-s1` | Coupled (B) | 1 | **3/3 (21.5 mm)** | **`3.53` (In1) + `0.70` (F)** | **27% / 17%** | 3.35 -> **0.15** | **`17.30` (In2)** | **73% / 86%** | **2 / 2** | 3.59 -> **0.00** | **0** | 0 | 0 |
| `b-s2` | Coupled (B) | 2 | **3/3 (21.5 mm)** | **`3.53` (In1) + `0.70` (F)** | **27% / 17%** | 3.35 -> **0.15** | **`17.30` (In2)** | **73% / 86%** | **2 / 2** | 3.59 -> **0.00** | **0** | 0 | 0 |

---

### 3.4 Bluebuzz Regression and Coupling Benchmark (`pairs-bluebuzz-3`)

Recipe:
`--negotiate --rip-up --nudge --tighten --strip-all --prefanout --shove-plough region --shove-reach 0.5 --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8`

| Run | Variant | Seed | Coupled Runs (Log) | `USB_DP/DM` Runs (mm) | `USB_DP/DM` Coupled % (`pair_check`) | `USB_DP/DM` Skew Pre -> Post (mm) | `USB_DP_C/DM_C` Runs (mm) | `USB_DP_C/DM_C` Coupled % (`pair_check`) | `USB_DP_C/DM_C` Vias (P/N) | `USB_DP_C/DM_C` Skew Pre -> Post (mm) | KiCad Unconn | KiCad Errors | Block Intrusions |
| :--- | :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| `a-s0` | Legacy (A) | 0 | 4/4 (24.2 mm) | `1.15` (In1) + `1.01` (F) + `19.03` (In1) | 35% / 33% | 0.03 -> 0.03 | `3.05` (In2) | 27% / 26% | 2 / 2 | 0.71 -> 0.15 | 0 | 0 | 0 |
| `a-s1` | Legacy (A) | 1 | 4/4 (24.2 mm) | `1.15` (In1) + `1.01` (F) + `19.03` (In1) | 34% / 34% | 1.23 -> 0.00 | `3.05` (In2) | 3% / 2% | 2 / 2 | 0.88 -> 0.18 | 0 | 0 | 0 |
| `a-s2` | Legacy (A) | 2 | 4/4 (24.2 mm) | `1.15` (In1) + `1.01` (F) + `19.03` (In1) | 25% / 25% | 0.53 -> 0.00 | `3.05` (In2) | 27% / 30% | 2 / 0 | 0.18 -> 0.18 | 0 | 0 | 0 |
| `b-s0` | Coupled (B) | 0 | **4/4 (23.8 mm)** | **`1.70` (F) + `18.44` (In4) + `1.15` (In4)** | **78% / 79%** | 0.52 -> 0.17 | **`2.52` (F)** | **67% / 59%** | **0 / 0** | 0.48 -> **0.00** | **0** | 0 | 0 |
| `b-s1` | Coupled (B) | 1 | **4/4 (23.8 mm)** | **`1.70` (F) + `18.44` (In4) + `1.15` (In4)** | **78% / 79%** | 0.52 -> 0.17 | **`2.52` (F)** | **67% / 59%** | **0 / 0** | 0.48 -> **0.00** | **0** | 0 | 0 |
| `b-s2` | Coupled (B) | 2 | **4/4 (23.8 mm)** | **`1.70` (F) + `18.44` (In4) + `1.15` (In4)** | **78% / 79%** | 0.52 -> 0.17 | **`2.52` (F)** | **67% / 59%** | **0 / 0** | 0.48 -> **0.00** | **0** | 0 | 0 |

---

### 3.5 Target Board: `easy2350` (`pairs-easy2350-1`, seeds 0-5)

Recipe:
`--strip --negotiate --rip-up --tighten --prefanout --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8 --planes In1.Cu,In2.Cu --cell 0.035 --prefanout-order round --shove-plough region --shove-reach 0.5`

| Seed | Legacy (A) Signal Opens (`sig.py`) | Coupled (B) Signal Opens (`sig.py`) | Legacy (A) `USB_D+/D-` Coupled (`pair_check`) | Coupled (B) `USB_D+/D-` Coupled (`pair_check`) | Post-Tune Skew (A / B) | Extra DRC Errors Beyond Vendor Copper (A / B) |
| :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| 0 | 0 | 0 | 2.66 mm (97% / 91%) | 2.45 mm (90% / 90%) | 0.00 / 0.00 mm | 0 / 0 (`track_width=0`) |
| 1 | 1 (`GPIO23`) | 1 (`GPIO23`) | 2.66 mm (97% / 91%) | 2.45 mm (90% / 90%) | 0.00 / 0.00 mm | 0 / 0 (`track_width=0`) |
| 2 | 0 | 0 | 2.66 mm (97% / 91%) | 2.45 mm (90% / 90%) | 0.00 / 0.00 mm | 0 / 0 (`track_width=0`) |
| 3 | 2 (`GPIO23`, `VBUS`) | 2 (`GPIO23`, `VBUS`) | 2.66 mm (97% / 91%) | 2.45 mm (90% / 90%) | 0.00 / 0.00 mm | 0 / 0 (`track_width=0`) |
| 4 | 1 (`QSPI_SCLK`) | 1 (`QSPI_SCLK`) | 2.66 mm (97% / 91%) | 2.45 mm (90% / 90%) | 0.00 / 0.00 mm | 0 / 0 (`track_width=0`) |
| 5 | 2 (`GPIO3`, `GPIO27`) | 2 (`GPIO3`, `GPIO27`) | 2.66 mm (97% / 91%) | 2.45 mm (90% / 90%) | 0.00 / 0.00 mm | 0 / 0 (`track_width=0`) |

---

### 3.6 Earlier Experiments (`pairs-earring-1..2`, `pairs-bluebuzz-1..2`, `pairs-earring-plain-1..2`)

| Experiment ID | Board | Variant B Coupled Runs | Variant B Unconnected (Seeds 0, 1, 2) | Variant B Block Intrusions (Seeds 0, 1, 2) | Notes |
| :--- | :--- | :---: | :---: | :---: | :--- |
| `pairs-earring-1` | `earring` (`--planes In1.Cu`) | 3/3 (24.0 mm) | 1, 1, 1 (GND) | 6, 7, 7 | Before `!it.refilled()` block-pour check in `legal_pair_item` |
| `pairs-earring-2` | `earring` (`--planes In1.Cu`) | 2/3 (4.6 mm) | **0, 0, 0** | **0, 0, 0** | `prefanout` skipped `U1-51/52` before `shares_layer` check was restored |
| `pairs-earring-plain-1` | `earring` (plain) | 3/3 (24.0 mm) | 0, 0, 0 | 6, 6, 6 | Before `!it.refilled()` block-pour check |
| `pairs-earring-plain-2` | `earring` (plain) | 3/3 (24.8 mm) | 2, 2, 2 (`RUN`, `GND`) | **0, 0, 0** | `U1-51/52` exited on `F.Cu` before `shares_layer` restored `In2.Cu` dogbone |
| `pairs-bluebuzz-1` | `bluebuzz` (`--strip-all`) | 4/4 (23.7 mm) | 4, 4, 4 | 3, 2, 3 | Before `middle_pad_can_escape` and `!it.refilled()` block-pour check |
| `pairs-bluebuzz-2` | `bluebuzz` (`--strip-all`) | 4/4 (23.2 mm) | 2, 2, 2 (`VBUS`, `GND`) | **0, 0, 0** | Before `allow_vias = false` cluster reuse and outward-only `middle_pad_can_escape` |

---

## 4. What Worked, What Did Not, and Why

### What Worked
1. **Concentric ring search with end-to-end leg verification (`couple_end_to_end` + `evaluate_stretches`)**:
   Searching candidate centrelines across `END_RINGS` (`0..3.4 mm`) and verifying that both end
   legs (`route_pair_legs`) and coupled stretches (`lay_coupled_stretches`) can be legally placed on
   the board before accepting a ring tier solved spread-pin convergence (such as `U3` <-> `J1` on
   `earring` and `U10` <-> `J1` on `bluebuzz`) where the centreline cannot start at `0.3 mm`.
2. **Trying `allow_vias = false` first in `route_pair_legs` regardless of `end.layers`**:
   Previously, `allow_vias = false` was only tried when the SMD pad itself sat on `entry_layer`.
   Always trying `allow_vias = false` first lets an inner-layer coupled stretch connect directly on
   `entry_layer` to existing vias in `local_cluster(board, pad_id)` (`prefanout` dogbones at `U1`,
   test-point vias at `R9`/`R11`, or an earlier pair run's vias at `U10`), eliminating redundant
   outer-layer wrap-around tracks and extra vias.
3. **Outward-only `middle_pad_can_escape` and temporary `reserve_middle_pad_escapes`**:
   On 6-pin USB ESD protection arrays (`U10` on `bluebuzz`, `U3` on `earring`), the middle `GND` and
   `VBUS` pads sit between `D+` and `D-` on each row. Testing all 8 compass directions in
   `middle_pad_can_escape` gave false positives because a track going *inward* into the `1.85 mm`
   pocket between the two pad rows (or a `0.0 mm` via-in-pad) passed `legal_pair_item` even when the
   pad was completely boxed in on the outside. Restricting middle-pad escapes to the outward
   direction (`pad_outward_dir`) and reserving that outward chute during window leg routing reduced
   `bluebuzz` unconnected items from `4` -> `2` -> **`0` on all seeds**.
4. **Locking pair copper and used cluster escapes (`it.locked = true`)**:
   Locking every coupled stretch, paired via, end leg, short bridge, and used `prefanout` escape kept
   `USB_DP`/`USB_DM` coupling at **`84%/91%`** (`pairs-earring-3`), **`73%/86%`**
   (`pairs-earring-plain-3`), and **`78%/79%`** (`pairs-bluebuzz-3`) across every seed, completely
   preventing the single-ended rip-up and `B.Cu` detours seen in legacy mode (`10%/9%` on `a-s2` of
   `pairs-earring-3` and `25-35%` on `pairs-bluebuzz-3`).

### What Did Not Work and Why
1. **Skipping pre-pair `prefanout` unconditionally for all differential pairs**:
   When we skipped `prefanout` for `USB_DP`/`USB_DM` on `earring` (`pairs-earring-2`), `prefanout`
   escaped the other 58 pins of `U1` (RP2350) first, surrounding `U1-51`/`52` with dogbone vias on
   `In2.Cu` or forcing `U1-51`/`52` to exit on `F.Cu` across `RUN` (`U1-26`). Conditioning the skip
   on `grid::shares_layer(&board, p)` gave the best of both worlds: outer-layer-only pairs converge
   directly from their pads, while pairs with multi-layer attachments get coordinated `prefanout`
   dogbones first.
2. **Excluding `local_cluster` items from `ObstacleMap` in `coupled_grid`**:
   When we excluded pre-existing connector jumper tracks (`J1` on `earring`) from `ObstacleMap` in
   `coupled_grid`, the coarse centreline A* routed right over `J1`'s `USB_C_DP` jumper track,
   causing the partner `USB_C_DN` offset track to fail `legal_pair_item`. Keeping non-pad copper at
   full `fat.half_width + clearance` in `coupled_grid` while letting `route_pair_legs` bridge the
   last `0.3..1.8 mm` on the `25 um` window grid avoided those false A* paths.

---

## 5. What to Try Next

1. **Joint two-net state-space search for end legs (`route_pair_legs`)**:
   Currently, Tier 2 routes one leg and then the partner leg on the `25 um` window (trying both
   orders `p_first` and penalizing `3.0 * |lp - ln|`). A joint chamfer/wavefront search that advances
   both legs synchronously from `entry_p`/`entry_n` into `end.p`/`end.n` could further reduce leg
   skew on asymmetric connector footprints (such as `J1`'s `A6/B6` and `A7/B7` jumpers on
   `earring`, where one pin has a shorter jumper path than its partner).
2. **Paired-via nudge during `tune::match_lengths`**:
   When a coupled run changes layers through paired vias or terminates at pre-jumpered connector
   vias with fixed asymmetry (`~1.5 mm` inside `J1` on `earring`), allowing `tune::match_lengths` to
   insert a small symmetric tromboned phase bump right at the uncoupled connector leg before the
   coupled stretch starts would bring end-to-end skew to `0.00 mm` without disturbing the coupled
   stretch itself.
