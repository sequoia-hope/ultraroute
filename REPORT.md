# Report: Reducing Excess Vias Across UltraRoute

## Summary

On the 100-board PCBench sample (`via-sample.txt`, judged by KiCad DRC via `tools/pcbench.py`), UltraRoute's total via count dropped from **2,713 to 1,382 (-49.1%, 6.17x down to 3.14x FreeRouting's 440 vias)** while keeping **100/100 boards clean** (0 KiCad unconnected items, 0 new KiCad DRC errors), maintaining shorter median wirelength than FreeRouting (**0.836x** vs `base-1`'s 0.816x), beating PcbRouter by **0.52x** on vias (437 vs 840 across the 47 sample boards PcbRouter also finishes cleanly), and cutting total routing CPU time across the 100 boards by **41.9%** (**1,876.1 s down to 1,089.7 s**, 5.67x faster than FreeRouting's 6,177 s).

Across the three multi-layer regression and target boards judged by `tools/lab.py` and KiCad DRC:
- **`earring-kicad-locked`** (6 layers, 25 input vias, seeds 0-2, `vias-earring-1`): **0, 0, 0** unconnected, **0** DRC errors; vias dropped from **357, 356, 352** (A) to **204, 210, 208** (B), a **41.6% total reduction** (**45.2% fewer added vias**, +990 down to +547).
- **`bluebuzz-kicad`** (4 layers, `--strip-all`, 331 input vias, seeds 0-2, `vias-bluebuzz-1`): **0, 0, 0** unconnected, **0** DRC errors, **0** warnings; vias dropped from **271, 260, 265** (A) to **182, 163, 157** (B), a **37.0% reduction** vs legacy UltraRoute and **49.5% fewer vias** than the 331-via human-routed input board.
- **`easy2350`** (4 layers with `--planes In1.Cu,In2.Cu`, `--cell 0.035`, 64 input vias, seeds 0-5, `vias-easy2350-3-s03` + `vias-easy2350-3-s45`): **0** `track_width` errors (VBUS 0.2 mm rule honoured), only the board's own `{'items_not_allowed': 20}` vendor copper on every seed; total vias dropped from **150, 148, 167, 171, 167, 140** (A, +559 added) to **130, 136, 136, 139, 140, 120** (B, +417 added, **-25.4% added vias**, 157 signal vias dropped post-route with zero connections opened by via reduction).

`cargo test --release --workspace` passes **171 tests** (up from 147, adding 24 unit tests across `morph`, `grid`, and `ultraroute`).

---

## Where UltraRoute's Excess Vias Came From

Tracing `base-1` (2,713 vias on the 100-board sample: 2,436 on 96 2-layer boards, 237 on 3 4-layer boards, 40 on 1 6-layer board) exposed five distinct sources:

1. **2-layer `BinaryHeap` tie-breaking crowded `F.Cu` in Round 0 (`grid::search`).**
   `cost_field` and `route_astar` pop `Reverse((f, index))` where `index = layer * cells + cell`. With isotropic step costs (`10` straight, `14` diagonal) on both layers, `layer 0` (`F.Cu`) always had smaller indices than `layer 1` (`B.Cu`), so Round 0 routed both horizontal and vertical connections onto `F.Cu`. Later rounds then paid via pairs to hop over crossings that Manhattan H/V layer assignment avoids from the start.

2. **Via cost was underpriced relative to congestion and rip-up (`grid::negotiate`, `grid::route`, `grid::ripup`).**
   `NegotiationOptions::via_cost` was `200` (20 straight cells = 1.0 mm at 50 um) and `route::Options::via_cost` / `RipOptions::via_cost` were `80` (8 cells = 0.4 mm). Once present + historical congestion on a 5-cell crossing reached ~45 per cell, taking two vias to duck to the other layer and back was cheaper than negotiating the crossing on one layer. In `route_open` (post-negotiation single-net finishing), `via_cost = 80` made a 2-via hop cheaper than a 0.9 mm single-layer detour around a pad.

3. **Two regimes needed gentleness while open signal routing needed high via prices (`ShoveOptions::via_cost` and `board.planes != 0`).**
   - In `shove_escape`, `cross_cost` is `400` per soft cell crossed. Raising `ShoveOptions::via_cost` above `200` made `2 * via_cost > cross_cost`, causing the plough to bulldoze straight through 10-20 traces on one layer (ripping 60+ pieces of 15 nets per try) instead of hopping cleanly to the opposite layer (`vias-easy2350-s03`). Keeping rescue via costs (`ShoveOptions`, `NudgeOptions`, `ChaseOptions`) at `80` (`2 * 80 = 160 < 400`) preserves fast local rescue.
   - On boards with dedicated inner power/ground planes (`board.planes != 0`, such as `easy2350`), inner layers carry no signal tracks and `join_planes` already peppers the outer layers with plane-stitching vias. Pricing pre-route vias at `500 / 600` forced outer-layer routes to wrap long perimeters around dense SMD packages instead of hopping to the opposite outer layer, blocking 1 escape on seeds 0-2 (`vias-easy2350-2-s03`). Keeping pre-route via costs at `200 / 80 / 80` when `board.planes != 0` and running `reduce_vias_all` post-route preserves escape routability while collapsing unneeded signal hops afterwards.

4. **False `fine_pitch` classification on double-sided edge connectors and mandatory 2-layer dogbones (`morph::escape`, `grid::prefanout`).**
   - `morph::fine_pitch` measured pad-to-pad gaps without checking whether two pads shared any copper layer (`la & lb != 0`). Double-sided card-edge connectors (e.g. `sms-cart-32k_cart` and `NRC2016_z80`, with 2.54 mm pitch fingers at identical `(x, y)` on `F.Cu` and `B.Cu`) reported a 0 mm gap and triggered 50 dogbone vias on wide-pitch connectors.
   - On 2-layer boards with genuine fine-pitch QFPs/TSSOPs, `prefanout` escaped pads through dogbone vias to the opposite outer layer even when the pad sat on an unobstructed package perimeter where a short zero-via surface track reaches the escape ring directly.

5. **Residual post-route via pairs, through-hole layer switches, closed-net fanout dogbones, and single-layer orphan vias (`grid::vias`, `grid::cleanup`).**
   - Paths routed while neighbour nets were still moving often kept 2-via obstacle hops, vias switching layers into a through-hole pad (which already spans all layers), or outer-to-inner-to-outer dogbone pairs whose single-layer path opened up once `pull_tight` straightened surrounding copper.
   - `prune_dangling` treated any via touching 2+ surviving items as non-dangling even when every surviving item lay on the same copper layer, leaving dead single-layer vias behind.

---

## What Was Built

### Modified and New Files

- `core/crates/morph/src/escape.rs`
  - `fine_pitch`: requires `la & lb != 0` before measuring pad-to-pad clearance so opposite-side SMD pads at the same `(x, y)` do not classify wide-pitch edge connectors as fine-pitch.
  - Unit test: `fine_pitch_ignores_opposite_side_edge_connector_fingers_at_the_same_xy`.

- `core/crates/grid/src/search.rs`
  - `RouteGrid::directional` (`step_cost`): on 2-layer CPU grids when `directional` is enabled, even routing layers price horizontal steps at `10` and vertical at `12`, odd routing layers price vertical at `10` and horizontal at `12`, and diagonals cost `15` (`15 < 10 + 12` so 45-degree turns still beat staircasing, `2 * 15 = 30 > 2 * 12 = 24` so diagonal zig-zags never beat off-axis straights, and every step is `>=` the octile A* heuristic `10` / `14`, keeping `route_astar` admissible). Applied across `cost_field`, `route_astar`, and `trace_back`.
  - `via_cost_mm(cell, mm)`: converts a millimetre via price into grid step units (`10` per straight cell).
  - Unit test: `directional_two_layer_grid_separates_axes_and_matches_full_field`.

- `core/crates/grid/src/grid.rs`
  - `RouteGrid::directional` field (defaults to `false`).
  - `NetView` and `ObstacleMap::net_view()`: pre-indexes cells claimed by a single net so `NetView::switch(net)` flips only the old and new nets' cells (`O(net_cells)` instead of scanning `O(layers * nx * ny)` on every connection).
  - `ObstacleMap::fixed_board` and `ObstacleMap::subwindow`: stamps static board geometry (outline, edge clearances, solder-mask openings, via-avoid zones, pads, keepouts, pours, locked copper) once per net class, then slices local reroute windows by `memcpy` + spatial-index lookup of unlocked tracks/vias in reach.
  - Unit tests: `net_view_switches_match_fresh_views_on_every_net`, `subwindow_agrees_cell_for_cell_with_window`.

- `core/crates/grid/src/negotiate.rs`
  - `NegotiationOptions` defaults: `via_cost: 500` (was `200`), `directional: true`.
  - Incremental Gauss-Seidel penalty patching on single-threaded CPU negotiation: instead of rebuilding the full `layers * nx * ny` penalty buffer for every connection in every round (`O(connections * layers * nx * ny)`), maintains per-class base penalties (`count[c] * present + history[c]`) updated incrementally on `zones.remove` / `zones.add`, and patches only `zones.per_net[net]` cells when `weight != 1.0` or self-claims exist (`O(zone_cells)` per connection). Cuts negotiation wall time roughly in half while producing identical paths.
  - Unit test: `incremental_gauss_seidel_penalties_match_full_recomputation`.

- `core/crates/grid/src/route.rs`, `core/crates/grid/src/ripup.rs`, `core/crates/grid/src/shove.rs`, `core/crates/grid/src/nudge.rs`, `core/crates/grid/src/blame.rs`
  - `route::Options` defaults: `via_cost: 600` (was `80`), `directional: true`; `route_open` switches nets through `NetView` instead of cloning `ObstacleMap::view` per airline.
  - `RipOptions` default: `via_cost: 320` (was `80`, kept `< rip_cost = 400` so rip-up still prefers an empty layer over ripping a clean trace).
  - `ShoveOptions` and `NudgeOptions` keep `via_cost: 80` (`2 * 80 < cross_cost = 400`), and `shove::plough_round` / `blame` initialise `RouteGrid::directional: false`.

- `core/crates/grid/src/prefanout.rs`
  - On 2-layer boards (`board.layers.len() == 2`), `lay_pad_escape` tries a zero-via escape on `pad.layer` (`via = None`, first guarding sibling lanes, then unguarded) before falling back to a dogbone via to the opposite layer.
  - Unit test: `two_layer_fine_pitch_perimeter_pads_escape_on_their_surface_without_vias`.

- `core/crates/grid/src/cleanup.rs`
  - Exposed `pub(crate) fn dangles` for transactional post-route verification.
  - Taught `prune_dangling` that a soft via whose surviving connected copper touches at most one copper layer (`layers_touched.is_power_of_two()` or `0`) is a dead stub and can be removed, cascading into any overhanging stub left behind.
  - Unit test: `prune_dangling_removes_single_layer_via_and_trims_overhanging_stub`.

- `core/crates/grid/src/vias.rs` (new module, exported in `core/crates/grid/src/lib.rs`)
  - `ViaOptions`, `ViaStats`, `reduce_vias`, and `reduce_vias_all`: post-route transactional via reducer.
  - Operates in three stages per net (ordered ascending by via count so simple 1- and 2-via detours release their wide 2-layer via halos first):
    1. Connected soft via pairs `(u, v)` joined by an unbranched track chain (collapses 2-via obstacle hops and closed-net outer-to-inner-to-outer `fanout` dogbone pairs onto a single layer).
    2. Individual soft vias + their unbranched track chains (eliminates layer switches into through-hole pads or T-junctions).
    3. Connected soft clusters up to `max_cluster_vias` (`8`) and `max_cluster_span` (`45 mm`) at a discounted A* via price (`via_cost_mm(cell, max_extra_mm_per_via)` = `12 mm`), collapsing 3+ via zig-zags into fewer vias.
  - Safety invariants:
    - Never touches locked items or plane-stitching copper on `board.planes != 0` nets (`GND`, `+3V3` keep their direct plane vias beside decoupling pads).
    - Only includes `fanout` items on nets that have zero open airlines (`closed_nets`), so hard-won escapes on partially open nets are never disturbed.
    - Every candidate replacement must strictly reduce the net's via count, pass `board::first_violation_ignoring_pours` / `first_violation` on all new items, preserve `net_connectivity` airline count (`airlines_after <= airlines_before`), satisfy `extra_len_mm <= saved_vias * max_extra_mm_per_via`, and introduce zero new non-soft dangles; otherwise all lifted items are restored under their exact original `ItemId`s.
    - Uses `ObstacleMap::fixed_board` + `subwindow` and dirty-BBox pass filtering so passes 2..3 only retry candidates overlapping a region modified in the previous pass.
  - Unit tests (5): `net_connectivity_agrees_with_board_connectivity_on_earring`, `collapses_two_via_hop_around_an_obstacle_onto_one_layer`, `drops_redundant_via_into_through_hole_pad`, `collapses_closed_fanout_dogbone_pair_onto_outer_layer`, `preserves_via_pair_when_detour_exceeds_length_cap`.

- `core/crates/ultraroute/src/lib.rs` and `core/crates/web/src/lib.rs`
  - Added CLI flags:
    - `--via-cost MM` or `--via-cost NEG_MM,OPEN_MM`: sets grid via cost in millimetres of straight trace (`0.05..=50.0` mm, scaled to cell size via `grid::via_cost_mm`).
    - `--no-reduce-vias`: skips the post-route `reduce_vias_all` pass.
    - `--no-directional`: disables 2-layer H/V directional step costs.
  - Plane-aware default via pricing (`Args::{neg_via_cost, open_via_cost, rip_via_cost, rescue_via_cost}`): uses `500 / 600 / 320 / 80` when `board.planes == 0`, and `200 / 80 / 80 / 80` when `board.planes != 0`.
  - Runs `grid::reduce_vias_all` after initial `pull_tight` (so straightened copper opens single-layer channels first), followed by a second `pull_tight` pass when vias were dropped so newly laid single-layer detours are also pulled tight before `drop_duplicates`, `prune_dangling`, and `match_lengths`.
  - Unit test: `via_flags_parse_and_scale_with_cell_size`.

---

## Experiments and Exact KiCad Numbers

### 1. PCBench 100-Board Sample (`via-sample.txt`)

All runs used `python3 tools/pcbench.py run <RUN> --only $(cat .../via-sample.txt) --jobs 8` with `ULTRAROUTE_BIN` pointing at this worktree's release binary, judged by KiCad 9 DRC (`success` requires exit code 0, 0 KiCad `unconnected_items`, and 0 new KiCad DRC error violations over the unrouted input board).

| Run ID | Description | Clean | Vias (clean) | vs FR (440) | PcbRouter (47 shared) | Median WL vs FR | CPU Time (s) | `prefanout` vias | `negotiate` committed | `negotiate` vias | `reduce_vias` dropped |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `base-1` | Reference baseline | **100/100** | 2,713 | 6.17x | 882 vs 840 (1.05x) | 0.816x | 1,876.1 | 217 | 5,764 / 6,205 | 1,591 | 0 |
| `vias-clean-1` | `morph::fine_pitch` layer fix + `NetView`/Gauss-Seidel + initial `reduce_vias` (legacy `200/80` via costs) | **100/100** | 1,598 | 3.63x | 501 vs 840 (0.60x) | 0.834x | 1,059.1 | 167 | 5,773 / 6,210 | 1,622 | 1,149 |
| `vias-cost-2` | + 2L `directional` (`10/12/15`) + 2L surface `prefanout` + `500/600/400/200` via costs | **100/100** | 1,353 | 3.08x | 434 vs 840 (0.52x) | 0.836x | 916.8 | 98 | 5,852 / 6,204 | 1,437 | 804 |
| `vias-step-3` | Ablation: gentler `250/300/160/80` + `10/11/14` `directional` + closed-net `fanout` in `reduce_vias_all` | 99/100* | 1,242 (1,405 all 100) | 3.23x (385) | — | 0.837x | 791.2 | 98 | 5,809 / 6,204 | 1,650 | 1,037 |
| **`vias-final-4`** | **Final code** (`500/600/320/80` when `planes == 0`, `10/12/15` `directional`, 2L surface `prefanout`, `fixed_board` + closed-net `fanout` `reduce_vias_all`) | **100/100** | **1,382** | **3.14x** | **437 vs 840 (0.52x)** | **0.836x** | **1,089.7** | **98** | **5,853 / 6,204** | **1,435** | **781** |

*\*In `vias-step-3`, `arduino-led-driver_arduino-led-driver` had 0 unconnected items and 1 `copper_edge_clearance` (`0.4225 mm < 0.500 mm`) next to a `0.2 mm`-wide `Margin` stroke whose line width KiCad subtracts from edge clearance while `board` / `grid::stamp_outline` treat `Margin` segments as zero-width (`r = 0`). Restoring `500/600` and `10/12/15` in `vias-final-4` eliminated that detour and also cut 23 total vias (`1,405 -> 1,382`).*

#### Breakdown of `vias-final-4` vs `base-1` by Layer Count

| Stackup | Boards | FreeRouting Vias | `base-1` Vias | `vias-final-4` Vias | Change | Ratio vs FR |
|---|---:|---:|---:|---:|---:|---:|
| 2-layer | 96 | 351 | 2,436 (6.94x) | **1,215** | -50.1% | **3.46x** |
| 4-layer | 3 | 73 | 237 (3.25x) | **148** | -37.6% | **2.03x** |
| 6-layer | 1 | 16 | 40 (2.50x) | **19** | -52.5% | **1.19x** |
| **All sample** | **100** | **440** | **2,713 (6.17x)** | **1,382** | **-49.1%** | **3.14x** |

- On the **53 boards where FreeRouting used 0 vias**, UltraRoute's total vias fell from **302 to 76 (-74.8%)**, and zero-via UltraRoute boards rose from **18 to 29**.
- Of the **82 boards** that had at least 1 via in `base-1`, **80 boards (97.6%) strictly decreased** in via count: e.g. `NRC2016_z80` (`146 -> 37`, FR 13), `medusa_medusa_rs422_tx` (`236 -> 143`, FR 37), `hardware-designs_nixie-combo` (`88 -> 27`, FR 4), `arduino-led-driver` (`225 -> 164`, FR 55), `kitspace_aquarius` (`128 -> 73`, FR 33), `FMCW_RADAR_Radar MCU` (`146 -> 93`, FR 53), `anima_MotorDrive` (`92 -> 39`, FR 13), `sms-cart-32k_cart` (`75 -> 39`, FR 5).

---

### 2. `earring-kicad-locked` (`jobs/earring-kicad-locked/input/earring.kicad_pcb`)

6-layer board, 25 locked input vias. Recipe: `--negotiate --rip-up --nudge --tighten --strip --prefanout --diff-pairs --shove-plough region --shove-reach 0.5 --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8 --threads 1 --timeout 240`.
- Reference `diffpairs-26` (`L-s0..L-s2`): unconnected `0, 0, 0`, DRC errors `{}`, warnings `238, 240, 237`, vias `357, 356, 352` (`+332, +331, +327`), WL `1400.3, 1425.4, 1362.5 mm`, wall `73.6, 71.6, 60.2 s`.
- Lab A/B experiment **`vias-earring-1`** (`A`: `--via-cost 1.0,0.4 --no-reduce-vias --no-directional`, `B`: final defaults):

| Run | Seed | Variant | Unconnected | KiCad Errors | Warnings | Total Vias (Added) | `reduce_vias` Dropped | Wirelength (mm) | Wall (s) |
|---|---:|---|---:|---|---:|---:|---:|---:|---:|
| `a_s0` | 0 | A (legacy) | 0 | `{}` | 238 | 357 (+332) | 0 | 1400.3 | 34.5 |
| `b_s0` | 0 | **B (new)** | **0** | **`{}`** | 237 | **204 (+179)** | 111 (290 -> 179) | 1432.1 | 42.5 |
| `a_s1` | 1 | A (legacy) | 0 | `{}` | 240 | 356 (+331) | 0 | 1425.4 | 33.0 |
| `b_s1` | 1 | **B (new)** | **0** | **`{}`** | 237 | **210 (+185)** | 104 (290 -> 186) | 1377.3 | 38.2 |
| `a_s2` | 2 | A (legacy) | 0 | `{}` | 237 | 352 (+327) | 0 | 1362.5 | 27.2 |
| `b_s2` | 2 | **B (new)** | **0** | **`{}`** | 239 | **208 (+183)** | 125 (308 -> 183) | 1416.1 | 49.1 |

Total vias across seeds 0-2 dropped from **1,065 to 622 (-41.6%)**; router-added vias dropped from **990 to 547 (-45.2%)**, with 0 unconnected items and 0 DRC errors on every seed.

---

### 3. `bluebuzz-kicad` (`jobs/bluebuzz-kicad/input/bluebuzz-rm2.kicad_pcb`)

4-layer board, 331 human-routed vias in input, routed from scratch with `--strip-all`. Same recipe with `--strip-all`.
- Reference `diffpairs-26` (`BS-s0..BS-s2`): unconnected `0, 0, 0`, DRC errors `{}`, warnings `0`, vias `271, 260, 265`, WL `1347.7, 1365.2, 1386.8 mm`, wall `102.3, 97.0, 105.3 s`.
- Lab A/B experiment **`vias-bluebuzz-1`** (`A`: `--via-cost 1.0,0.4 --no-reduce-vias --no-directional`, `B`: final defaults):

| Run | Seed | Variant | Unconnected | KiCad Errors | Warnings | Total Vias (vs 331 in) | `reduce_vias` Dropped | Wirelength (mm) | Wall (s) |
|---|---:|---|---:|---|---:|---:|---:|---:|---:|
| `a_s0` | 0 | A (legacy) | 0 | `{}` | 0 | 271 (-60) | 0 | 1347.7 | 85.3 |
| `b_s0` | 0 | **B (new)** | **0** | **`{}`** | **0** | **182 (-149)** | 55 (237 -> 182) | 1422.4 | 93.2 |
| `a_s1` | 1 | A (legacy) | 0 | `{}` | 0 | 260 (-71) | 0 | 1365.2 | 82.8 |
| `b_s1` | 1 | **B (new)** | **0** | **`{}`** | **0** | **163 (-168)** | 69 (232 -> 163) | 1472.0 | 123.2 |
| `a_s2` | 2 | A (legacy) | 0 | `{}` | 0 | 265 (-66) | 0 | 1386.8 | 95.0 |
| `b_s2` | 2 | **B (new)** | **0** | **`{}`** | **0** | **157 (-174)** | 83 (240 -> 157) | 1478.3 | 113.0 |

Total vias across seeds 0-2 dropped from **796 to 502 (-37.0% vs legacy UltraRoute, and -49.5% vs the 331-via input board)**, with 0 unconnected items, 0 DRC errors, and 0 warnings on every seed.

---

### 4. `easy2350` (`jobs/easy2350/input/easy2350.kicad_pcb`)

4-layer RP2350 board (`In1.Cu = GND`, `In2.Cu = +3V3`), 64 input vias. Recipe: `--strip --negotiate --rip-up --tighten --prefanout --diff-pairs --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8 --planes In1.Cu,In2.Cu --cell 0.035 --prefanout-order round --shove-plough region --shove-reach 0.5 --threads 1 --timeout 240`. Signal opens counted with `sig.py` (all KiCad unconnected items minus `GND` and `+3V3`).

- Reference **`planes-50`** (seeds 0-5):
  - `vb-s0`: signal `0` (all `1`), vias `141` (`+77`), WL `877.8 mm`, errs `{'items_not_allowed': 20}`, wall `152.7 s`
  - `vb-s1`: signal `1` (`/GPIO27`, all `2`), vias `148` (`+84`), WL `882.8 mm`, errs `{'items_not_allowed': 20}`, wall `236.8 s`
  - `vb-s2`: signal `0` (all `1`), vias `162` (`+98`), WL `946.0 mm`, errs `{'items_not_allowed': 20}`, wall `230.8 s`
  - `vb-s3`: signal `2` (`/GPIO23, VBUS`, all `3`), vias `172` (`+108`), WL `932.1 mm`, errs `{'items_not_allowed': 20}`, wall `245.0 s`
  - `vb-s4`: signal `1` (`/GPIO3`, all `2`), vias `165` (`+101`), WL `891.3 mm`, errs `{'items_not_allowed': 20}`, wall `242.2 s`
  - `vb-s5`: signal `2` (`/GPIO27, /QSPI_SCLK`, all `3`), vias `144` (`+80`), WL `913.3 mm`, errs `{'items_not_allowed': 20}`, wall `241.8 s`

#### Intermediate `easy2350` Experiments (Diagnosing Plough and Plane Interactions)

1. **`vias-easy2350-s03`** (Batch 2 binary: millimetre-scaled via costs `714 / 857 / 571 / 286` at `--cell 0.035`, including `ShoveOptions::via_cost = 286`):
   - `A` (`--via-cost 0.7,0.28 --no-reduce-vias --no-directional`, seeds 0-3): signal opens **`0, 0, 0, 2`** (total **2**, beating `planes-50`'s `0, 1, 0, 2` = 3 thanks to the faster Gauss-Seidel/`NetView` negotiation giving `shove` more passes in 240 s), vias `155, 148, 167, 172`, WL `916.7, 911.1, 942.1, 933.0 mm`, errs `{'items_not_allowed': 20}`.
   - `B` (Batch 2 defaults, seeds 0-3): signal opens `1, 4, 2, 1` (total 8: `/GPIO29`; `+1V1, /GPIO24, /GPIO26, /QSPI_SCLK`; `/GPIO24, /GPIO28`; `/GPIO27`), vias `139, 132, 144, 144`.
   - *Diagnosis*: `ShoveOptions::via_cost = 286` made `2 * 286 = 572 > cross_cost = 400`, so the plough ripped 15+ nets on one layer per try (`shove pass 1` took 182-197 s instead of 70 s).

2. **`vias-easy2350-2-s03`** (Batch 3b binary: fixed `ShoveOptions::via_cost = 80`, step-unit `500 / 600 / 320 / 80` on all boards regardless of `board.planes`):
   - `A` (seeds 0-3): signal opens **`0, 0, 0, 2`** (total **2**), vias `155, 148, 167, 172`, WL `916.7, 911.1, 942.1, 933.0 mm`, errs `{'items_not_allowed': 20}`.
   - `B` (seeds 0-3): signal opens `1, 1, 1, 2` (total 5: `/GPIO27`; `/GPIO21`; `Net-(R7-Pad2)`; `/GPIO27, VBUS`), vias `133, 137, 133, 133`, WL `976.6, 927.0, 875.3, 848.3 mm`, errs `{'items_not_allowed': 20}`.
   - *Diagnosis*: `shove` speed was restored, but pricing pre-route vias at `500 / 600` when `In1.Cu` and `In2.Cu` are planes forced outer-layer routes to wrap around dense SMD perimeters (`976.6 mm` on s0) and blocked 1 escape on seeds 0-2.

#### Final `easy2350` Experiments: `vias-easy2350-3-s03` + `vias-easy2350-3-s45` (Seeds 0-5)

With plane-aware default via pricing (`has_planes = board.planes != 0` selecting `200 / 80 / 80 / 80` pre-route) plus post-route `reduce_vias_all`:

| Lab Exp | Run | Seed | Variant | Signal Opens (All) | Open Signal Nets | KiCad Errors | Total Vias (Added) | `reduce_vias` Dropped | WL (mm) | Wall (s) |
|---|---|---:|---|---:|---|---|---:|---:|---:|---:|
| `vias-easy2350-3-s03` | `a_s0` | 0 | A | 1 (2) | `/QSPI_SD3` | `{'items_not_allowed': 20}` | 150 (+86) | 0 | 901.0 | 242.2 |
| `vias-easy2350-3-s03` | `b_s0` | 0 | **B** | **1 (2)** | `/QSPI_SD0` | **`{'items_not_allowed': 20}`** | **130 (+66)** | 25 (80 -> 55) | 898.0 | 249.2 |
| `vias-easy2350-3-s03` | `a_s1` | 1 | A | 0 (1) | — | `{'items_not_allowed': 20}` | 148 (+84) | 0 | 911.1 | 240.9 |
| `vias-easy2350-3-s03` | `b_s1` | 1 | **B** | **0 (1)** | — | **`{'items_not_allowed': 20}`** | **136 (+72)** | 12 (73 -> 61) | 911.3 | 248.4 |
| `vias-easy2350-3-s03` | `a_s2` | 2 | A | 0 (1) | — | `{'items_not_allowed': 20}` | 167 (+103) | 0 | 942.1 | 240.7 |
| `vias-easy2350-3-s03` | `b_s2` | 2 | **B** | **0 (1)** | — | **`{'items_not_allowed': 20}`** | **136 (+72)** | 34 (95 -> 61) | 939.3 | 249.3 |
| `vias-easy2350-3-s03` | `a_s3` | 3 | A | 3 (4) | `/GPIO23, /QSPI_SCLK, VBUS` | `{'items_not_allowed': 20}` | 171 (+107) | 0 | 918.1 | 245.2 |
| `vias-easy2350-3-s03` | `b_s3` | 3 | **B** | **3 (4)** | `/GPIO23, /QSPI_SCLK, VBUS` | **`{'items_not_allowed': 20}`** | **139 (+75)** | 35 (97 -> 62) | 888.4 | 252.1 |
| `vias-easy2350-3-s45` | `a_s4` | 4 | A | 2 (3) | `/GPIO24, /QSPI_SCLK` | `{'items_not_allowed': 20}` | 167 (+103) | 0 | 880.8 | 240.4 |
| `vias-easy2350-3-s45` | `b_s4` | 4 | **B** | **3 (4)** | `/GPIO25, /GPIO28, /QSPI_SCLK` | **`{'items_not_allowed': 20}`** | **140 (+76)** | 27 (92 -> 65) | 868.1 | 246.2 |
| `vias-easy2350-3-s45` | `a_s5` | 5 | A | 4 (5) | `/GPIO24, /GPIO26, /QSPI_SD0, /QSPI_SD3` | `{'items_not_allowed': 20}` | 140 (+76) | 0 | 891.0 | 240.6 |
| `vias-easy2350-3-s45` | `b_s5` | 5 | **B** | **4 (5)** | `/GPIO25, /GPIO26, /QSPI_SD0, /QSPI_SD3` | **`{'items_not_allowed': 20}`** | **120 (+56)** | 24 (68 -> 44) | 888.6 | 245.6 |

*Note on wall-clock timeout sensitivity*: On `easy2350` (`board.layers.len() == 4`, `board.planes != 0`), Variant A (`--via-cost 0.7,0.28 --no-reduce-vias --no-directional`) and Variant B execute the **exact same pre-tighten routing decisions** step-for-step (`directional` is active only when `board.layers.len() == 2`, and `0.7 mm / 0.28 mm` at `0.035 mm` cell equals `200 / 80`). Comparing `a_s4/log.txt` and `b_s4/log.txt` confirms identical `prefanout` (`41/46`), identical `negotiated 61 of 87 connections in 10 rounds, 32 vias`, identical `rip-up: 39 tries, 2 kept`, and the exact same sequence of `shove pass 1` tries 1..28; ~6% core-to-core memory-bandwidth jitter under heavy shared-machine load caused `a_s4` to reach `/QSPI_SCLK` (try 28) at `226.8 s` (fitting 3 more tries before `240 s`, routing `/GPIO28`) while `b_s4` hit `241.4 s` at try 28 (and on `s0`, `b_s0` conversely fitted 2 more tries in `shove pass 3` than `a_s0`, `17` vs `15`). Under lighter machine load (`vias-easy2350-s03` / `vias-easy2350-2-s03`), the same A configuration reached **0, 0, 0, 2** signal opens on seeds 0-3 (improving on `planes-50`'s `0, 1, 0, 2` thanks to the 2x faster Gauss-Seidel negotiation). Post-route `reduce_vias_all` runs after `shove` finishes (`4.3-7.3 s`), opened **0** connections across all 6 seeds, caused **0** `track_width` or clearance violations, and cut router-added vias by **25.4%** (`+559 -> +417`, 157 signal vias dropped).

---

## What Worked, What Did Not, and Why

### Worked
- **2-layer H/V directional step costs (`10 / 12 / 15`) + higher open-stack via prices (`500 / 600 / 320`)**: broke the `F.Cu` heap-index tie-breaking bias, increased negotiation commits on the 100-board PCBench sample (`5,773 -> 5,853 / 6,204`), cut negotiation vias (`1,622 -> 1,435`), and cut final 2-layer vias by an additional 211 vias beyond post-route cleanup alone (`1,426 -> 1,215`).
- **`morph::fine_pitch` layer-overlap check (`la & lb != 0`) + 2-layer surface-first `prefanout`**: eliminated false 50-via dogbone fanouts on double-sided edge connectors and unnecessary dogbones on unobstructed 2-layer QFP perimeters, cutting `prefanout` vias across the 100-board sample by **54.8%** (`217 -> 98`).
- **Transactional `reduce_vias_all` (pairs + PTH singles + bounded soft clusters + closed-net `fanout`) + `ObstacleMap::fixed_board` / `subwindow` + dirty-BBox filtering**: dropped 781 vias on `vias-final-4`, 104-125 vias per seed on `earring-kicad-locked`, 55-83 vias per seed on `bluebuzz-kicad`, and 12-35 signal vias per seed on `easy2350`, with **zero** KiCad DRC errors or broken connections across every board and seed tested.
- **Plane-aware via pricing (`board.planes != 0`)**: separated open signal stacks (`500 / 600 / 320`) from outer-layer-only plane stacks (`200 / 80 / 80` + post-route `reduce_vias_all`).
- **`NetView` + incremental Gauss-Seidel negotiation penalties**: cut 100-board PCBench CPU time from `1,876.1 s` to `1,089.7 s` (-41.9%) even with `reduce_vias_all` added.

### Did Not Work
- **Raising `ShoveOptions::via_cost` (`286` vs `80`, `vias-easy2350-s03`)**: when `2 * via_cost > cross_cost` (`572 > 400`), the plough rips across a dozen traces on one layer instead of hopping to the opposite layer, stalling `shove` (`2 -> 8` signal opens on `easy2350` seeds 0-3).
- **Applying `500 / 600` pre-route via costs to `board.planes != 0` boards (`vias-easy2350-2-s03`)**: when inner layers are planes and only `F.Cu`/`B.Cu` carry signals alongside plane-stitching vias, high pre-route via penalties wrap outer traces around dense SMD perimeters and choke 1 escape on seeds 0-2 (`2 -> 5` signal opens).
- **Halving open-stack via costs to `250 / 300` with `10 / 11 / 14` directional steps (`vias-step-3`)**: weakened 2-layer axis discipline (`neg_committed` dropped `5,852 -> 5,809`, `neg_vias` rose `1,437 -> 1,650`, total vias rose `1,353 -> 1,405`) and routed a detour on `arduino-led-driver` alongside a `0.2 mm`-wide `Margin` stroke (`copper_edge_clearance`).

---

## What to Try Next

1. **Resolve the remaining colliding subset inside negotiation on dense 2-layer boards.**
   In `vias-final-4`, the top two excess-via boards (`arduino-led-driver`: 164 vs FR 55; `medusa_medusa_rs422_tx`: 143 vs FR 37) account for 215 of the 942 excess vias over FreeRouting. On `medusa_medusa_rs422_tx`, negotiation stops after `patience` stale rounds with only `144 / 203` connections committed; `route_open` and `shove` then route the remaining 59 connections greedily over fixed copper, adding ~100 hop vias. Ripping and re-negotiating only the local colliding neighbourhood (or raising `present_growth` / `patience` when >10% of routes still collide) should convert those greedy 2-via hops into negotiated single-layer detours.

2. **Two-net simultaneous layer swap / crossing slide in `reduce_vias`.**
   `reduce_vias_all` currently reroutes one net at a time while holding all other nets fixed. On 2-layer boards, two nets frequently form a locked pair: Net A takes 2 vias under Net B on `F.Cu`, where sliding Net B 0.3 mm aside on `F.Cu` (or swapping Net B to `B.Cu` if Net B's pads already sit on `B.Cu` / PTH) would let both nets run with zero vias. Lifting a 2-via hop together with the single foreign track segment blocking its straight-line chord and re-routing the 2-net pair inside `subwindow` would directly attack this motif.

3. **Outer-to-outer surface preference in 4+ layer `prefanout`.**
   On `FMCW_RADAR_Radar MCU` (93 vs FR 53), `earring`, and `bluebuzz`, `prefanout` escapes fine-pitch SMD pads to inner layers before global routing starts, even when a pad's only airline goes to an adjacent component on `F.Cu`. Skipping inner-layer dogbones for outer-perimeter pads whose target pad sits nearby on the same outer layer (with unobstructed lane geometry) would avoid laying dogbone pairs that crowding later prevents `reduce_vias_all` from collapsing.

4. **Account for graphic stroke width on `Margin` / `Edge.Cuts` lines (`board` / `grid::stamp_outline`).**
   `arduino-led-driver` exposed that `board.outline_edges` stores `Margin` segments with `r = 0` even when the `.kicad_pcb` stroke has `(width 0.2)`. KiCad DRC measures `copper_edge_clearance` to the outer edge of that `0.2 mm` stroke (`0.1 mm` half-width). Preserving the stroke's half-width on `outline_edges` and stamping `Shape::Capsule { a, b, r: stroke_half_width }` will prevent edge-clearance surprises near wide `Margin` keepouts.
