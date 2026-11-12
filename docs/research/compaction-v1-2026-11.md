# Routed Board Scaling and Spring Compaction (`ultraroute scale` & `ultraroute compact`)

## 1. What Was Built

### Modified and Created Files

- [`core/crates/board/src/check.rs`](core/crates/board/src/check.rs)
  - Added [`Board::is_tied`](core/crates/board/src/check.rs) (exposing net-tie membership to external crates), [`Board::hole_clearance`](core/crates/board/src/check.rs) (drill-to-copper rule lookup matching `check_item`), [`Board::hole_to_hole_clearance`](core/crates/board/src/check.rs) (drill-to-drill rule lookup), and [`Board::max_rule_reach`](core/crates/board/src/check.rs) (spatial query radius bound covering copper clearance, hole clearance, and solder-mask web reach).
  - Unit test: `hole_clearances_follow_the_rules_and_let_joined_tracks_in`.
- [`core/crates/kicad-pcb/src/lib.rs`](core/crates/kicad-pcb/src/lib.rs) and [`core/crates/kicad-pcb/src/write.rs`](core/crates/kicad-pcb/src/write.rs)
  - Added [`ZoneFill`](core/crates/kicad-pcb/src/write.rs) and [`scale_edits`](core/crates/kicad-pcb/src/write.rs) to rewrite `Edge.Cuts` and board-level graphics (`gr_line`, `gr_arc`, `gr_circle`, `gr_rect`, `gr_poly`), top-level and footprint-local `(zone ...)` outline polygons, footprint `(at x y [rot])` origins (preserving footprint size and rotation), and recarved `(filled_polygon ...)` blocks on kept local zones while stripping stale board-plane fill polygons before re-pouring.
  - Unit test: `scale_edits_rewrites_outline_zones_drawings_and_footprints`.
- [`core/crates/morph/src/warp.rs`](core/crates/morph/src/warp.rs)
  - Added [`WarpField::set_border`](core/crates/morph/src/warp.rs) so the harmonic CG solver accepts non-zero Dirichlet boundary displacements `u(p) = (F - 1)(p - c)` on the domain perimeter instead of clamping the border to zero.
  - Unit test: `a_scaled_border_reproduces_uniform_scaling_and_blends_around_rigid_parts`.
- [`core/crates/morph/src/scale.rs`](core/crates/morph/src/scale.rs) and [`core/crates/morph/src/lib.rs`](core/crates/morph/src/lib.rs)
  - Implemented [`scale`](core/crates/morph/src/scale.rs), [`ScaleOptions`](core/crates/morph/src/scale.rs), and [`ScaleReport`](core/crates/morph/src/scale.rs), plus helpers shared with compaction (`build_scale_field`, `build_scaled_bare_board`, `strip_routed_copper`, `is_board_plane_zone`, `strip_kept_zone_pours`, `recarve_kept_zones_in_board`, and `carve_kept_zones`).
  - Scales every footprint origin about the `Edge.Cuts` bounding-box centre `c` by `c + F * (p - c)`, scales the board outline, cutouts, keepouts, and zone polygons by `F` about `c`, solves a `250_000 nm` (`0.25 mm`) harmonic [`WarpField`](core/crates/morph/src/warp.rs) with rigid footprint inclusions and scaled border condition, advects free tracks and vias through [`flow::flow`](core/crates/morph/src/flow.rs), and recarves kept local block-zone pours (such as `rp2350_power` on `earring`) against opposite-net copper and drills.
  - Unit test: `scale_expands_outline_parts_and_copper_while_keeping_part_sizes_and_joints`.
- [`core/crates/morph/src/compact.rs`](core/crates/morph/src/compact.rs)
  - Implemented [`compact`](core/crates/morph/src/compact.rs), [`CompactOptions`](core/crates/morph/src/compact.rs), [`CompactReport`](core/crates/morph/src/compact.rs), [`StepReport`](core/crates/morph/src/compact.rs), [`AcidTraps`](core/crates/morph/src/compact.rs), and [`dfm_counts`](core/crates/morph/src/compact.rs).
  - Architecture of each incremental step `k = 1..=steps` toward target factor `F`:
    1. **Courtyard-limited part placement (`place_parts`)**: Moves footprint origins incrementally toward `c + f_k * (p_0 - c)`, clamps footprints inside the scaled `Edge.Cuts` outline, and resolves pairwise `F.CrtYd` and `B.CrtYd` polygon overlaps (deflated by `KICAD_COURTYARD_TOL_NM = 10_000 nm` pairwise, matching KiCad 9's `5_000 nm` per-footprint courtyard tolerance) by decomposing motion into common-mode translation, tangential sliding, and normal stop projection.
    2. **Harmonic copper advection**: Solves an incremental [`WarpField`](core/crates/morph/src/warp.rs) from the previous accepted step's footprint positions and border to the new step's positions and border, carrying free vertices and vias smoothly with their surrounding parts.
    3. **Topological graph & exact tether preservation (`CopperGraph`)**: Extracts routed tracks and vias into a particle-and-edge graph where pad endpoints move rigidly with their footprints (`Anchor::Pad`), pad-stacked vias move rigidly (`Anchor::PadVia`), free vias (`Anchor::Via`) move as net-bound particles shared across layers, and mid-segment T-junctions, cap-overlap joints, and off-centre via landings are preserved as exact tether offsets without inserting synthetic hook segments.
    4. **Spring + rule-aware relaxation (`relax_step`)**: Iteratively relaxes particle positions under constant-tension unit-direction springs on `Free` bend vertices (`w_mob = 0.0` on vias so vias do not drift off channels unless pushed by clearance or pulled by taut tracks), exact rule repulsion against foreign copper (`Board::clearance`), drill holes (`Board::hole_clearance`, `Board::hole_to_hole_clearance`), board edges/cutouts (`edge_clearance`), keepout zones, poured-zone thermal spokes, and solder-mask bridge webs (`min_web`). Dynamically subdivides long segments where an obstacle pushes into the interior of a span, projects multi-constraint conflicts with an exact 2D active-set half-plane solver (`solve_half_planes_2d`, computing pairwise wedge apices and cycle-averaged pinch midpoints rather than under-projecting obtuse wedges with sequential projections), runs recursive tether-group coordinate polish directly against `check_item` (`polish_on_board`), and collapses taut or collinear bend chains (`shorten_taut_chains`).
    5. **Strict acceptance gate (`check_item` + `BaselineConn`)**: Rebuilds the candidate `Board`, recarves kept block zones, verifies that `check_item` reports zero net rule violations above baseline on any track or via, and verifies via `BaselineConn::added_airlines` that zero copper-connected terminal clusters split apart (while allowing cross-footprint pads that only overlapped accidentally inside a shared copper pour zone at `110%` to un-overlap inside that zone). Any step that leaves a single violation or broken net is rejected, leaving the previous clean board intact. Never rips, never reroutes.
  - Unit tests (6): `compact_bends_a_straight_track_around_an_obstacle_pad_without_ripping`, `compact_stops_footprints_short_of_courtyard_overlap`, `compact_stops_at_the_last_legal_step_when_a_channel_jams`, `compact_keeps_off_centre_via_joints_without_adding_acute_hooks`, `compact_caps_preexisting_subtolerance_courtyard_graze`, and `solve_half_planes_2d_resolves_acute_wedges_and_antiparallel_pinches`.
- [`core/crates/ultraroute/src/lib.rs`](core/crates/ultraroute/src/lib.rs), [`core/crates/ultraroute/src/main.rs`](core/crates/ultraroute/src/main.rs), and [`core/crates/ultraroute/tests/cli.rs`](core/crates/ultraroute/tests/cli.rs)
  - Wired `ultraroute scale` and `ultraroute compact` into CLI argument parsing, execution, per-step stderr reporting, JSON stats output, and KiCad PCB writing.
  - Unit/CLI tests in `lib.rs` and `tests/cli.rs`.
- [`.gitignore`](.gitignore)
  - Ignored `/runs/`.

### CLI Flags

```text
ultraroute scale <input.kicad_pcb> -o <output.kicad_pcb> --factor <F> [--lock|--no-lock] [--uuid-seed <u64>]
ultraroute compact <input.kicad_pcb> -o <output.kicad_pcb> --factor <F> [--steps <N>] [--lock|--no-lock] [--uuid-seed <u64>] [--stats <stats.json>] [-v|--verbose]
```

- `--factor <F>` accepts either a floating-point number (e.g. `1.1`, `0.85`) or a ratio `A/B` (e.g. `1/1.1`), and rejects non-positive or non-finite values.
- `--steps <N>` defaults to `ceil(|F - 1.0| / 0.01)` (`1%` steps when omitted, e.g. `15` steps for `0.85`, `10` steps for `1/1.1 = 0.9090909090909091`).
- `--stats <FILE>` (alias `--report <FILE>`) writes the full JSON step log (`achieved_factor`, `steps_accepted`, `elapsed_s`, `peak_memory_mb`, `jam`, and per-step `size_mm`, `violations`, `track_length_mm`, `vias`, `courtyard_stops`, and pre-pour `acid_traps`).

---

## 2. Judging Protocol

Every board reported below was judged under `runs/` with KiCad 9 (`kicad-cli`) and `tools/dfm.py`:

1. Copy the input board's `.kicad_pro` and `.kicad_dru` (when present) beside the candidate `.kicad_pcb` inside `runs/`.
2. Refill all copper zones with `python3 tools/refill_zones.py IN OUT.filled.kicad_pcb`.
3. Run `kicad-cli pcb drc --format json --severity-all -o OUT.drc.json OUT.filled.kicad_pcb` and count:
   - `unconnected_count = len(drc["unconnected_items"])`
   - `error_count` and `errors_by_type` over `drc["violations"]` entries with `"severity": "error"`.
4. Run `python3 tools/dfm.py OUT.filled.kicad_pcb --json` on the zone-refilled board to count post-pour acid traps (`acute` angles + narrow same-net `slots`).
   - Note on acid-trap columns: `compact` also logs an internal per-step `acid_traps` count before KiCad zone refill (computed on unpoured tracks, pads, and kept block zones). Both the KiCad post-refill DFM counts and the internal per-step counts are given below without rounding.

---

## 3. Experiment A: Compacting the Routed Earring Toward `0.85`

### Sanity Check on the Scratchpad Input Board

Before trusting the compacted output, we judged the provided scratchpad board (`<scratchpad>/earring-routed.kicad_pcb`) directly with KiCad:

- It has **`0` KiCad DRC errors**, **`283` vias**, **`945` segments**, **`1032.609016 mm` track length**, and **`86` post-pour acid traps** (`11` acute, `75` slots), on a **`30.5 x 35.75 mm`** outline.
- However, the scratchpad input board **already has `10` unconnected items** (`20` unconnected endpoints across `SWCLK`, `SWDIO`, `USB_DM`, `USB_DP`, `LED_EN_L`, `QSPI_SD2`, `CHRG`, `VBUS`, `LIGHT`, and `VSYS`). Note also that the scratchpad directory only has `earring-routed.kicad_pro` (no `.kicad_dru` beside it).

Because `compact` never rips or reroutes, it preserves those exact `10` pre-existing unconnected nets (`0` added unconnected items). To also answer the question strictly at `0` total KiCad errors and `0` total unconnected items (with `.kicad_dru` rules active), we ran three Experiment A cases:

1. **A1 (`compact_085`)**: Scratchpad `earring-routed.kicad_pcb`, `--factor 0.85 --steps 15` (`1%` steps).
2. **A1-fine (`compact_fine`)**: Scratchpad `earring-routed.kicad_pcb`, `--factor 0.98 --steps 10` (`0.2%` steps) to locate the exact continuous jam threshold between `0.990` and `0.980`.
3. **A2 (`compact_clean_s0_085`)**: Cleanly routed `100%` earring (`runs/expB/direct_100_s0.kicad_pcb`, which starts at `0` unconnected and `0` DRC errors with `.kicad_pro` + `.kicad_dru`), `--factor 0.85 --steps 15` (`1%` steps).

### KiCad-Judged Summary (Experiment A)

| Case | Board | Achieved Factor | Steps Accepted | Board Size (`mm`) | Unconnected (Added) | KiCad Errors by Type | Vias | Segments | Track Length (`mm`) | Acid Traps (`acute` / `slots` / `total`) | Run Time (`s`) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Input (scratchpad) | `earring-routed.kicad_pcb` | `1.0` | -- | `30.5 x 35.75` | `10` (`--`) | `0` `{}` | `283` | `945` | `1032.609016` | `11 / 75 / 86` | -- |
| **A1 (`1%` steps -> `0.85`)** | `runs/expA/compact_085.kicad_pcb` | **`0.99`** | `1 / 15` | `30.195002 x 35.392501` | `10` (`0`) | **`0` `{}`** | `283` | `1164` | `1014.612556` | `7 / 72 / 79` | `10.159658397` |
| **A1-fine (`0.2%` steps -> `0.98`)** | `runs/expA/compact_fine.kicad_pcb` | **`0.988`** | `6 / 10` | `30.134 x 35.321` | `10` (`0`) | **`0` `{}`** | `283` | `1213` | `1002.938346` | `8 / 70 / 78` | `26.291489997` |
| Input (clean `100%` s0) | `runs/expB/direct_100_s0.kicad_pcb` | `1.0` | -- | `30.5 x 35.75` | `0` (`--`) | `0` `{}` | `161` | `1296` | `1327.619626` | `62 / 55 / 117` | -- |
| **A2 (`1%` steps -> `0.85`, clean)** | `runs/expA/compact_clean_s0_085.kicad_pcb` | **`0.99`** | `1 / 15` | `30.195002 x 35.392501` | **`0` (`0`)** | **`0` `{}`** | `161` | `2677` | `1298.941937` | `87 / 61 / 148` | `18.619641603` |

*(Note: KiCad's `Edge.Cuts` arc midpoint rounding yields `30.195002 x 35.392501 mm` at `0.99`; internal outline polygon bbox is `30.188360999999997 x 35.3925 mm`.)*

### Per-Step Compaction Logs (Experiment A)

**A1 (`--factor 0.85 --steps 15`, peak RSS `181 MB`)**:

| Step | Factor | Accepted | Internal Outline Size (`mm`) | Violations Left | Courtyard Stops | Track Length (`mm`) | Vias | Pre-Pour Acid Traps (`acute` / `slots`) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `0` | `1.0` | yes | `30.491843 x 35.745092` | `0` | `0` | `1032.6090164705258` | `283` | `12 / 104` |
| `1` | `0.99` | yes | `30.188360999999997 x 35.3925` | `0` | `12` | `1014.612556328658` | `283` | `7 / 109` |
| `2` | `0.98` | **no (jam)** | `29.883426999999998 x 35.035` | `4` | `37` | `997.9331606589411` | `283` | `8 / 102` |

**A1-fine (`--factor 0.98 --steps 10`, peak RSS `182 MB`)**:

| Step | Factor | Accepted | Internal Outline Size (`mm`) | Violations Left | Courtyard Stops | Track Length (`mm`) | Vias | Pre-Pour Acid Traps (`acute` / `slots`) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `0` | `1.0` | yes | `30.491843 x 35.745092` | `0` | `0` | `1032.6090164705258` | `283` | `12 / 104` |
| `1` | `0.998` | yes | `30.430858999999998 x 35.673601999999995` | `0` | `4` | `1024.397399359311` | `283` | `7 / 108` |
| `2` | `0.996` | yes | `30.369875 x 35.602112` | `0` | `5` | `1018.5318937177905` | `283` | `7 / 106` |
| `3` | `0.994` | yes | `30.310333 x 35.5355` | `0` | `9` | `1013.6337337763538` | `283` | `8 / 92` |
| `4` | `0.992` | yes | `30.249347 x 35.464` | `0` | `9` | `1009.494421656969` | `283` | `6 / 90` |
| `5` | `0.99` | yes | `30.188360999999997 x 35.3925` | `0` | `12` | `1006.0137233553468` | `283` | `7 / 116` |
| `6` | `0.988` | yes | `30.127374999999997 x 35.321` | `0` | `20` | `1002.9383463903962` | `283` | `8 / 110` |
| `7` | `0.986` | **no (jam)** | `30.066388999999997 x 35.2495` | `4` | `25` | `999.4831515893051` | `283` | `9 / 103` |

**A2 (`direct_100_s0.kicad_pcb`, `--factor 0.85 --steps 15`, peak RSS `194 MB`)**:

| Step | Factor | Accepted | Internal Outline Size (`mm`) | Violations Left | Courtyard Stops | Track Length (`mm`) | Vias | Pre-Pour Acid Traps (`acute` / `slots`) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `0` | `1.0` | yes | `30.491843 x 35.745092` | `0` | `0` | `1327.7391298276477` | `161` | `78 / 77` |
| `1` | `0.99` | yes | `30.188360999999997 x 35.3925` | `0` | `12` | `1298.941937233927` | `161` | `74 / 92` |
| `2` | `0.98` | **no (jam)** | `29.883426999999998 x 35.035` | `12` | `37` | `1280.8926459618608` | `161` | `59 / 87` |

### Where It Jams First and Why

- **In `1%` steps (A1)**, compaction reaches **`0.99`** (`1%` linear shrink, `1.99%` area shrink, reducing wirelength by `17.996460 mm` and post-pour acid traps from `86` to `79`) and jams at **`0.98`** on:
  `Clearance on F.Cu: U1-34 (LED_EN_C) at (96.57, 97.85) mm vs via (BTN_SENSE) at (96.08, 97.49) mm (gap 0.075 mm < required 0.127 mm)`.
- **In `0.2%` steps (A1-fine)**, compaction reaches **`0.988`** (`1.2%` linear shrink, reducing wirelength by `29.670670 mm` and post-pour acid traps from `86` to `78`) and jams at **`0.986`** by **`0.001 mm`** (`1 um`) on the adjacent pin of the same QFN edge:
  `Clearance on F.Cu: U1-31 (CHRG) at (96.56, 96.67) mm vs via (PWR_HOLD) at (95.91, 96.98) mm (gap 0.126 mm < required 0.127 mm)`.
- **On the `0`-unconnected board (A2)**, compaction also reaches **`0.99`** (`0` DRC errors, `0` unconnected, saving `28.677689 mm` of copper) and jams at **`0.98`** on the same west-of-`U1` escape ring:
  `Clearance on F.Cu: C5-1 (VDD_CORE) at (95.30, 100.57) mm vs via (VBUS_SENSE) at (95.90, 100.75) mm (gap 0.100 mm < required 0.127 mm)`.

**Physical mechanism of the jam**:
`earring` is a tightly packed double-sided wearable (`167` footprints in `30.5 x 35.75 mm`). At `1.00`, `12` courtyard pairs are within `1%` of contact; by `0.98`, `37` courtyard pairs have engaged. Crucially, courtyards only oppose footprints on the **same** side (`F.CrtYd` vs `F.CrtYd` and `B.CrtYd` vs `B.CrtYd`). Just west of `U1` (RP2350 QFN-60 on `F.Cu`, pins `U1-31..34` at `x = 96.56..96.57 mm`), components on `B.Cu` (`Q5` pad 2, `R25`, `C25`) move inward from the west as the board scales down. Because `U1` (`F.Cu`) and `Q5` (`B.Cu`) are on opposite sides of the PCB, they experience zero courtyard repulsion against each other, while the `0.400 mm` outer-diameter through-hole escape vias (`PWR_HOLD`, `BTN_SENSE`, `VBUS_SENSE`) span **both** `F.Cu` and `B.Cu`. At `0.986`, the 2D horizontal gap between `U1-31` on `F.Cu` and `Q5-2` / adjacent `B.Cu` pads shrinks below `0.400 + 2 * 0.127 = 0.654 mm` (`0.652 mm` actual, leaving `0.126 mm < 0.127 mm` per side). No 2D displacement of the through-via can satisfy both layers simultaneously without either moving `Q5` relative to `U1` (a cross-layer via-channel footprint stop) or rerouting the via outside the gap.

---

## 4. Experiment B: Route at `110%` + Compact to `100%` vs Direct `100%` (`earring`, Seeds `0..3`)

### Setup and Static Check on `bare_110`

- Input bare board: [`jobs/earring-kicad/input/earring.kicad_pcb`](jobs/earring-kicad/input/earring.kicad_pcb) (`30.5 x 35.75 mm`, `167` footprints, `274` unconnected items, `0` KiCad DRC errors).
- `ultraroute scale jobs/earring-kicad/input/earring.kicad_pcb -o runs/expB/bare_110.kicad_pcb --factor 1.1` ran in **`0.256891 s`**, producing a **`33.55 x 39.325 mm`** board.
- **Static footprint check on `bare_110`**: Judging `bare_110.kicad_pcb` before routing shows `11` DRC errors: `5` are from pre-existing locked `VBUS` stubs near `R4`/`R5` that `ultraroute route --strip-all` removes, and **`6` are static footprint-to-footprint errors created by rigid `1.1x` origin scaling itself**:
  1. `courtyards_overlap`: `BT1` vs `R14`
  2. `courtyards_overlap`: `BT1` vs `SW2`
  3. `clearance`: `BT1` pad 1 (`VBATT`, `B.Cu`) vs `R14` pad 2 (`3V3`, `B.Cu`) (`0.0179 mm < 0.1500 mm`)
  4. `solder_mask_bridge`: `BT1` pad 1 vs `R14` pad 2 on `B.Mask`
  5. `hole_clearance`: `J1` NPTH mounting hole vs `R5` pad 2 (`GND`, `F.Cu`) (`0.1038 mm < 0.2000 mm`)
  6. `hole_clearance`: `J1` NPTH mounting hole vs `R4` pad 2 (`GND`, `F.Cu`) (`0.1038 mm < 0.2000 mm`)
  - **Why `1.1x` scaling creates footprint collisions on `earring`**: `BT1` is a `28 x 32 mm` coin-cell holder whose origin sits almost at the board centre `(100.0, 100.0)`, so its pads barely move under `c + 1.1 * (p - c)`, while small parts tucked inside `BT1`'s cavity (`R14`, `SW2`) scale outward by `10%` of their offset from `(100, 100)` and collide into `BT1`'s outer perimeter pads. Likewise, USB-C connector `J1` has wide-set NPTH bosses whose offset from `J1`'s origin stays fixed while `R4` and `R5` scale outward into those bosses.
  - Routing `bare_110` with `--strip-all --recipe default --seed S --timeout 120` solves **`0` unconnected on all 4 seeds** with **zero new DRC errors** (only the `6` static `bare_110` footprint errors remain). Compacting back toward `1/1.1` **heals** `1` to `3` of those static footprint errors as the parts scale back inward and introduces **zero routed-copper DRC errors** and **zero unconnected items** on any seed.

### Exact Results Across Seeds `0..3`

| Seed | Pipeline Stage | Achieved Factor | Board Size (`mm`) | Unconnected | KiCad Errors (`total`: breakdown) | Vias | Segments | Track Length (`mm`) | Acid Traps (`acute` / `slots` / `total`) | Run Time (`s`) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| -- | `bare_100` | `1.0` | `30.5 x 35.75` | `274` | `0`: `{}` | `25` | `20` | `18.192043` | `1 / 4 / 5` | -- |
| -- | `bare_110` (`scale 1.1`) | `1.1` | `33.55 x 39.325` | `274` | `11`: `{clearance: 3, courtyards_overlap: 2, hole_clearance: 3, shorting_items: 1, solder_mask_bridge: 2}` | `25` | `31` | `19.869298` | `3 / 6 / 9` | `0.256891` |
| **`0`** | `100%` direct route | `1.0` | `30.5 x 35.75` | **`0`** | **`0`**: `{}` | `161` | `1296` | `1327.619626` | `62 / 55 / 117` | `23.179905145` |
| **`0`** | `110%` route | `1.0` (at `110%`) | `33.55 x 39.325` | **`0`** | `6`: `{clearance: 1, courtyards_overlap: 2, hole_clearance: 2, solder_mask_bridge: 1}` | **`148`** | `1291` | `1423.773465` | `43 / 51 / 94` | `24.789368452` |
| **`0`** | `110% -> 100%` compact (`5/10` steps) | **`0.9545454545454546`** | `32.025 x 37.5375` | **`0`** | `3`: `{courtyards_overlap: 1, hole_clearance: 2}` (`0` copper) | **`148`** | `2728` | **`1321.341878`** | **`47 / 57 / 104`** | `40.371898164` |
| **`1`** | `100%` direct route | `1.0` | `30.5 x 35.75` | **`0`** | **`0`**: `{}` | `158` | `1174` | `1319.288328` | `53 / 58 / 111` | `31.294046391` |
| **`1`** | `110%` route | `1.0` (at `110%`) | `33.55 x 39.325` | **`0`** | `6`: `{clearance: 1, courtyards_overlap: 2, hole_clearance: 2, solder_mask_bridge: 1}` | **`150`** | `1366` | `1414.653438` | `66 / 65 / 131` | `28.091408456` |
| **`1`** | `110% -> 100%` compact (`3/10` steps) | **`0.9727272727272727`** | `32.635 x 38.2525` | **`0`** | `4`: `{courtyards_overlap: 2, hole_clearance: 2}` (`0` copper) | **`150`** | `2737` | `1347.933496` | `59 / 57 / 116` | `26.901802546` |
| **`2`** | `100%` direct route | `1.0` | `30.5 x 35.75` | **`0`** | **`0`**: `{}` | `152` | `1267` | `1351.520643` | `58 / 58 / 116` | `28.478713742` |
| **`2`** | `110%` route | `1.0` (at `110%`) | `33.55 x 39.325` | **`0`** | `6`: `{clearance: 1, courtyards_overlap: 2, hole_clearance: 2, solder_mask_bridge: 1}` | **`147`** | `1371` | `1428.548734` | `57 / 53 / 110` | `28.950484684` |
| **`2`** | `110% -> 100%` compact (`1/10` steps) | **`0.990909090909091`** | `33.245 x 38.9675` | **`0`** | `5`: `{clearance: 1, courtyards_overlap: 2, hole_clearance: 2}` (`0` copper) | **`147`** | `2499` | `1398.824452` | **`54 / 48 / 102`** | `16.860203887` |
| **`3`** | `100%` direct route | `1.0` | `30.5 x 35.75` | **`0`** | **`0`**: `{}` | `165` | `1201` | `1318.655564` | `44 / 63 / 107` | `22.957267372` |
| **`3`** | `110%` route | `1.0` (at `110%`) | `33.55 x 39.325` | **`0`** | `6`: `{clearance: 1, courtyards_overlap: 2, hole_clearance: 2, solder_mask_bridge: 1}` | **`155`** | `1315` | `1411.466533` | `46 / 60 / 106` | `26.506549479` |
| **`3`** | `110% -> 100%` compact (`1/10` steps) | **`0.990909090909091`** | `33.245 x 38.9675` | **`0`** | `5`: `{clearance: 1, courtyards_overlap: 2, hole_clearance: 2}` (`0` copper) | **`155`** | `2465` | `1384.124524` | `53 / 58 / 111` | `14.756275290` |

### Per-Step Compaction Trajectories and Jam Reasons (Experiment B)

- **Seed `0` (`runs/expB/compact_100_s0.stats.json`, `elapsed_s = 40.371898164`, peak RSS `212 MB`)**:
  - Accepted steps: `1.0` (`1423.869847 mm`, `64/78` pre-pour acid) -> `0.990909090909091` (`1396.119885 mm`, `22/86`) -> `0.9818181818181818` (`1375.029123 mm`, `21/87`) -> `0.9727272727272727` (`1356.054871 mm`, `36/95`) -> `0.9636363636363636` (`1336.625087 mm`, `38/89`) -> **`0.9545454545454546`** (`32.023665 x 37.532581 mm` internal / `32.025 x 37.5375 mm` KiCad, `1321.341878 mm`, `39/89`).
  - Jam at step `6` (`0.9454545454545454`, `2` violations): `Clearance on B.Cu: R25-2 (GND) at (94.53, 97.77) mm vs track (PWR_HOLD) near (94.41, 97.25) mm (gap 0.123 mm < required 0.127 mm)`.
  - Notably, even at `105.0%` of original linear board dimensions (`0.954545 * 1.1 = 1.05`), seed `0`'s compacted board has **`13` fewer vias (`148` vs `161`)**, **`6.277748 mm` less total track length (`1321.341878 mm` vs `1327.619626 mm`)**, and **`13` fewer post-pour acid traps (`104` vs `117`)** than routing directly at `100%`.
- **Seed `1` (`runs/expB/compact_100_s1.stats.json`, `elapsed_s = 26.893984336`, subprocess wall `26.901802546 s`, peak RSS `221 MB`)**:
  - Accepted steps: `1.0` (`1414.749820 mm`, `97/89`) -> `0.990909090909091` (`1385.961334 mm`, `47/94`) -> `0.9818181818181818` (`1365.535791 mm`, `43/94`) -> **`0.9727272727272727`** (`1347.933496 mm`, `42/100`).
  - Jam at step `4` (`0.9636363636363636`, `4` violations): `Clearance on B.Cu: R8-1 (PROG) at (103.66, 107.70) mm vs via (3V3) at (103.01, 107.84) mm (gap 0.126 mm < required 0.127 mm)`.
- **Seed `2` (`runs/expB/compact_100_s2.stats.json`, `elapsed_s = 16.853225555`, subprocess wall `16.860203887 s`, peak RSS `202 MB`)**:
  - Accepted steps: `1.0` (`1428.644991 mm`, `86/73`) -> **`0.990909090909091`** (`1398.824452 mm`, `39/77`).
  - Jam at step `2` (`0.9818181818181818`, `2` violations): `Clearance on B.Cu: R20-1 (VBUS_SENSE) at (109.43, 100.43) mm vs via (LEDC_D6) at (108.84, 100.13) mm (gap 0.124 mm < required 0.127 mm)`.
- **Seed `3` (`runs/expB/compact_100_s3.stats.json`, `elapsed_s = 14.751617855`, subprocess wall `14.756275290 s`, peak RSS `202 MB`)**:
  - Accepted steps: `1.0` (`1411.562790 mm`, `77/82`) -> **`0.990909090909091`** (`1384.124524 mm`, `44/85`).
  - Jam at step `2` (`0.9818181818181818`, `2` violations): `Clearance on F.Cu: U1-29 (LED_DATA_L) at (97.60, 96.23) mm vs via (3V3) at (97.18, 95.80) mm (gap 0.118 mm < required 0.127 mm)`.

**Why `110%` routing does not compact all the way back to `0.909091` (`100%`)**:
At `110%`, the router exploits the widened inter-part channels by dropping through-hole vias directly between `F.Cu` and `B.Cu` pads that were too narrow to host a `0.400 mm` via at `100%` (which is also why `110%` routing needs `8..13` fewer vias). Because compaction preserves route homotopy and cannot rip or relocate a via to the opposite side of a pad, any channel where the router placed `K` tracks + vias whose combined minimum width + clearances sits between `1.00x` and `1.10x` of the `100%` gap acts as a hard topological stop during pure spring compaction.

---

## 5. Experiment C: PCBench Boards (`120 s` Timeout, Seed `0`)

We evaluated two PCBench boards from `~/Software/ultraroute-lab/pcbench/boards/<id>/bare.kicad_pcb` that fail to route at `100%` within `--timeout 120` on current `HEAD` (`99ba56d`): **`Starling_Starling_V1`** (`3` unconnected at `100%`) and **`BMS-bq76940_EvaluationsBoard`** (`4` unconnected at `100%`). We also included **`AmpOne_dev-AmpOne`** (which failed in an older `srvfull-a7831ee` lab baseline and now solves on `99ba56d`) as a third point of comparison.

### KiCad-Judged Summary (Experiment C, Seed `0`)

| Board | Stage | Achieved Factor | Board Size (`mm`) | Unconnected | KiCad Errors by Type | Vias | Segments | Track Length (`mm`) | Acid Traps (`acute` / `slots` / `total`) | Run Time (`s`) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **`Starling_Starling_V1`** (`25` parts) | `bare_100` | `1.0` | `60.1 x 23.8` | `79` | `0` `{}` | `0` | `0` | `0.0` | `0 / 0 / 0` | -- |
| **`Starling_Starling_V1`** | `bare_110` (`scale 1.1`) | `1.1` | `66.11 x 26.18` | `79` | `0` `{}` | `0` | `0` | `0.0` | `0 / 0 / 0` | `0.326988359` |
| **`Starling_Starling_V1`** | `100%` direct route | `1.0` | `60.1 x 23.8` | **`3` (FAIL)** | `0` `{}` | `79` | `500` | `1075.924803` | `28 / 30 / 58` | `108.728075464` |
| **`Starling_Starling_V1`** | `110%` route | `1.0` (at `110%`) | `66.11 x 26.18` | **`0` (SOLVED)** | **`0` `{}`** | **`70`** | `447` | `1248.019024` | **`16 / 17 / 33`** | **`9.126007704`** |
| **`Starling_Starling_V1`** | `110% -> 100%` compact (`4/10` steps) | **`0.9636363636363636`** | `63.706 x 25.228` | **`0` (SOLVED)** | **`0` `{}`** | **`70`** | `994` | `1234.416791` | `16 / 40 / 56` | `27.009554371` |
| **`BMS-bq76940_EvaluationsBoard`** (`134` parts) | `bare_100` | `1.0` | `67.945 x 75.565` | `263` | `0` `{}` | `0` | `0` | `0.0` | `0 / 0 / 0` | -- |
| **`BMS-bq76940_EvaluationsBoard`** | `bare_110` (`scale 1.1`) | `1.1` | `74.7395 x 83.1215` | `264` | `0` `{}` | `0` | `0` | `0.0` | `0 / 0 / 0` | `0.770442596` |
| **`BMS-bq76940_EvaluationsBoard`** | `100%` direct route | `1.0` | `67.945 x 75.565` | **`4` (FAIL)** | `0` `{}` | `50` | `723` | `1573.602596` | `37 / 33 / 70` | `108.501748681` |
| **`BMS-bq76940_EvaluationsBoard`** | `110%` route | `1.0` (at `110%`) | `74.7395 x 83.1215` | **`4` (FAIL)** | `0` `{}` | `62` | `662` | `1779.898276` | `27 / 30 / 57` | `110.145436209` |
| **`BMS-bq76940_EvaluationsBoard`** | `110% -> 100%` compact (`3/10` steps) | **`0.9727272727272727`** | `72.70115 x 80.85455` | `4` (`0` added) | **`0` `{}`** | `62` | `1172` | `1721.80958` | `27 / 55 / 82` | `28.227117087` |
| **`AmpOne_dev-AmpOne`** (`164` parts) | `bare_100` | `1.0` | `100.0 x 100.0` | `321` | `0` `{}` | `0` | `0` | `0.0` | `0 / 0 / 0` | -- |
| **`AmpOne_dev-AmpOne`** | `bare_110` (`scale 1.1`) | `1.1` | `110.0 x 110.0` | `321` | `0` `{}` | `0` | `0` | `0.0` | `0 / 0 / 0` | `1.483155346` |
| **`AmpOne_dev-AmpOne`** | `100%` direct route | `1.0` | `100.0 x 100.0` | `0` | `0` `{}` | `37` | `707` | `2165.034581` | `29 / 21 / 50` | `78.384263854` |
| **`AmpOne_dev-AmpOne`** | `110%` route | `1.0` (at `110%`) | `110.0 x 110.0` | `0` | `0` `{}` | `54` | `804` | `2439.937209` | `51 / 13 / 64` | `86.815254503` |
| **`AmpOne_dev-AmpOne`** | `110% -> 100%` compact (`1/10` steps) | **`0.990909090909091`** | `109.0 x 109.0` | **`0`** | **`0` `{}`** | `54` | `2835` | `2426.503029` | `36 / 16 / 52` | `29.108802399` |

### Analysis of Experiment C

1. **`Starling_Starling_V1` (Inter-Part Channel Bottleneck -> Complete Solve, `11.9x` Faster Routing)**:
   - At `100%`, `Starling_Starling_V1` fails after `108.728075464 s` with `3` unconnected nets (`/ADC7`, `/S_TX`, `Net-(IC1-Pad2)`) jammed between `IC1` and through-hole headers `P5`/`P6`.
   - Scaling to `110%` introduces `0` DRC errors and opens the inter-component channels just enough that `ultraroute route` finishes **`0` unconnected in `9.126007704 s`** (`11.91x` faster) with `70` vias instead of `79`.
   - `ultraroute compact` (`elapsed_s = 26.986019991`, subprocess wall `27.009554371 s`, peak RSS `242 MB`) accepts `4/10` steps (`1.0 -> 0.990909 -> 0.981818 -> 0.972727 -> 0.963636`, `63.706 x 25.228 mm`, `106.0%` of original linear dimensions) with **`0` unconnected and `0` KiCad DRC errors**, jamming at step `5` (`0.954545`, `1` violation):
     `EdgeClearance: via (/SCL) at (143.84, 88.86) mm vs board edge / rule area (gap 0.491 mm < required 0.500 mm)`.
   - Total `scale + route + compact` time is `36.462550434 s`, beating the failing `108.728075464 s` direct run in both completion (`0` vs `3` unconnected) and wall time (`3.0x` faster), at a `6.0%` linear board-size penalty.
2. **`BMS-bq76940_EvaluationsBoard` (Intra-Footprint Pin-Escape Bottleneck -> Unchanged Unconnected)**:
   - Both `100%` (`108.501748681 s`) and `110%` (`110.145436209 s`) leave `4` unconnected items (`0` DRC errors), all attached to `BMS1` (a fine-pitch 44-pin TSSOP IC: pads `2, 27, 35` at `100%`; pads `3, 4, 13` plus `SRN` at `110%`). Because `scale` keeps footprint pad pitches rigid, scaling the board by `1.1x` does not widen the `0.65 mm` pin pitch of `BMS1` itself.
   - Compaction (`elapsed_s = 28.222363391`, subprocess wall `28.227117087 s`, peak RSS `217 MB`) cleanly accepts `3/10` steps to `0.9727272727272727` (`72.70115 x 80.85455 mm`) with `0` DRC errors and `0` added unconnected items (`4 -> 4`), cutting wirelength by `58.088696 mm` before jamming at step `4` (`0.963636`) on `Clearance on F.Cu: C10-2 (/BatPack/VC11) at (189.72, 107.82) mm vs via (/BatPack/VC7) at (189.97, 109.12) mm (gap 0.165 mm < required 0.200 mm)`.

---

## 6. What Worked, What Did Not (and Why), and What We Would Try Next

### What Worked

1. **Zero-Error KiCad Rule Fidelity**: Across every single experiment (A1, A1-fine, A2, all 4 seeds of B, and all 3 PCBench boards in C -- 10 compaction runs in total), `compact` introduced **zero KiCad DRC errors** and **zero broken nets / unconnected items**. Combining `Board::clearance`, `hole_clearance`, `hole_to_hole_clearance`, `edge_clearance`, keepouts, thermal spoke checks, and `min_web` solder-mask checks inside the relaxation loop, gated by `check_item` and `BaselineConn`, matched `kicad-cli pcb drc` down to the micrometre.
2. **Harmonic Boundary-Scaled `WarpField`**: Extending [`WarpField`](core/crates/morph/src/warp.rs) with non-zero scaled boundary conditions `u(p) = (F - 1)(p - c)` blended uniform board compression smoothly around rigid multi-pad footprints and courtyard-stopped parts without shearing pad landings.
3. **Exact 2D Wedge & Pinch Projection (`solve_half_planes_2d`)**: Standard sequential POCS projections under-step obtuse wedges (such as a via pushed between two `0.4 mm`-pitch QFN pads at `~140 deg`) by `20-33%` per pass and bias antiparallel squeezes toward whichever constraint ran last. Solving pairwise 2D half-plane intersections directly + averaging conflicting cycles pushed Experiment B seed `0` from `4/10` steps (`0.9636`) to `5/10` steps (`0.9545`) and Experiment A fine steps from `0.990` to `0.988`.
4. **Zone-Bridged Cross-Footprint Overlap Tracking**: Detecting pad pairs across different footprints that accidentally overlapped inside a poured zone at `110%` (`BT1-2` and `J1-4` on `B.Cu` `GND`) allowed them to separate cleanly inside the `GND` pour during compaction (`s0` jumped from `2/10` to `5/10` steps with `0` unconnected in KiCad) without loosening strict connectivity on routed copper.
5. **Spreading Inter-Part Congested Boards (`Starling_Starling_V1`)**: On boards limited by inter-part routing channels rather than single-IC pin pitch, `scale 1.1 -> route -> compact` turned a `108.73 s` routing failure (`3` unconnected) into a `9.13 s` clean route (`0` unconnected, `0` DRC errors) compacted to `106%` size.

### What Did Not Work (and Why)

1. **Opposite-Side (`F.Cu` vs `B.Cu`) Through-Via Channel Pinches**: Every single copper jam in Experiments A, B, and C (`U1` vs `via` vs `Q5-2`, `R25-2` vs `track`, `R8-1` vs `via`, `R20-1` vs `via`, `C10-2` vs `via`, `U301-23` vs `via`) occurred because `place_parts` only stops footprints on **same-side courtyard overlap**. An `F.Cu` part and a `B.Cu` part slide freely toward each other in `(x, y)` during compaction even when a through-hole via sits between them on both layers, pinching the via below `d_via + 2 * clearance`.
2. **Uniform Origin Scaling on Giant Enclosing Footprints (`BT1`, `J1` on `earring`)**: Scaling footprint origins by `F` while keeping pad offsets rigid assumes footprints are small relative to the board and have convex non-nested extents. On `earring`, `BT1` (`28 x 32 mm`) spans nearly the entire board about `(100, 100)`, so `1.1x` scaling pushes interior parts (`R14`, `SW2`) outward into `BT1`'s outer tabs (`6` static DRC errors on `bare_110`).
3. **Segment Subdivision Inflates Segment Count and Narrow-Slot DFM Counts on Some Boards**: `flow::flow` and bend-vertex insertion split curved/bowed tracks into `~2x` more short capsule segments (`945 -> 1164`, `1291 -> 2728`, `447 -> 994`). While acute-angle traps generally improved or held steady, tight zig-zags compressed near pads occasionally raised `tools/dfm.py` same-net `slots` counts (`Starling`: `17 -> 40` slots; `A2`: `62 -> 87` acute angles at sharp pad exits).
4. **Topological Channel Capacity Mismatch Between `110%` and `100%`**: Without rerouting or sliding vias past pads, tracks and vias placed at `110%` into channels that can hold `N` items at `110%` but only `N - 1` items at `100%` cannot be compressed to `100%` by pure spring relaxation.

### What We Would Try Next

1. **Copper-Aware Cross-Layer Footprint Stops (Joint Part + Copper Relaxation)**: Coupling footprint positions `(dx_i, dy_i)` into `relax_step` as heavy rigid-body particles (pulled toward `target_pos` by weak placement springs, stopped by courtyards **and** pushed back by pad-to-via / pad-to-track clearance forces from `solve_half_planes_2d`) would keep `F.Cu` and `B.Cu` parts from crushing shared through-hole vias and let the rest of the board continue compacting around a locally saturated channel.
2. **Cavity- and Courtyard-Aware `scale` (`F > 1`)**: Running the courtyard + pad/hole clearance stop solver *after* `ultraroute scale --factor 1.1` (treating giant board-spanning mechanical footprints like `BT1` and `J1` as fixed or repulsive obstacles for small nested passives `R14`, `SW2`, `R4`, `R5`) would eliminate all `6` static `bare_110` DRC errors on `earring` before routing even begins.
3. **Rubber-Band Topological Representation (`core/crates/topo`)**: Replacing piecewise-linear capsule chains during compaction with exact rubber-band homotopy arcs around pad/via obstacles (using `core/crates/topo`) would eliminate segment-count doubling (`2500+` micro-segments), pull every trace taut to true Euclidean/octilinear tangents, and prevent acute pad-exit kinks.
4. **Channel-Capacity Guard During `110%` Routing**: Scaling design-rule clearances (or via outer keepout halos) proportionally by `1.1x` during the `110%` route pass (`--clearance-scale 1.1`) so the router never packs more tracks/vias into a widened channel than can physically fit once compacted back to `100%`.
