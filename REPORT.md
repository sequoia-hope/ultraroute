# Negotiated Congestion Routing Report (`explore/negotiate-2`)

## Summary

On `jobs/easy2350/input/easy2350.kicad_pcb` (4-layer RP2350 board, `In1.Cu` and `In2.Cu` inner power planes, `0.035 mm` routing grid), the baseline negotiation stage committed only **55 to 64 of 87** open connections (`60, 64, 55, 59, 60, 59` across seeds `0..5` in `planes-50`), leaving 23 to 32 open connections to sequential `route_open`, `rip_up`, and the region plough, and ending with **`0, 1, 0, 2, 1, 2` signal opens** (6 total across seeds `0..5`).

Tracing the 87 open connections entering negotiation showed that:
- **15 connections** are structurally unroutable without ripping pre-existing copper: **9** are blocked by input routing keepouts (`+1V1`: 3, `+3V3`: 2, `/VREG_LX`: 1, `GND`: 3; the `+3V3` and `GND` ones are joined afterward by outer copper pours, except for one keepout-isolated `F.Cu` `+3V3` stub that is non-signal), and **6** are trapped in pockets behind pre-fanout, differential-pair, plane-via, or locked copper (`/GPIO23` `U1-35`, `/GPIO28` `J3-17`, `/QSPI_SCLK` `U5-6`, `/QSPI_SD3` `U5-7`, `/USB_DP_MCU` track-to-track, and `VLED` `R11-1`).
- The remaining **72 connections** are routable on the bare grid, yet legacy negotiation lost **8 to 17** of them every seed.

With the general improvements built in this worktree:
1. Negotiation commits **72, 72, 72, 72, 72, 71 of 87** connections across seeds `0..5` (**100%** of bare-grid-routable connections on seeds `0..4` and **98.6%** on seed `5`), cutting post-negotiation sequential `route_open` work to `0` connections and `rip_up` tries from `34..39` down to `29..31`.
2. Across `easy2350` seeds `0..5` (`negotiate2-v2-02` + `negotiate2-v2-35`, and verified again with lossless immovable-wall map caching in `negotiate2-v5-02` + `negotiate2-v5-35`), the router achieves **`0, 0, 0, 0, 0, 0` signal opens** (down from `0, 1, 0, 2, 1, 2` in `planes-50` and `0, 0, 1, 2, 2, 2` in same-run `--negotiate-legacy` controls), with **0 `track_width` violations** (`VBUS >= 0.2 mm` preserved on every segment), **0 inner-plane segments**, and **no new KiCad DRC violations** beyond the board's 20 pre-existing vendor-copper `items_not_allowed` errors.
3. On both regression boards (`earring` seeds `0..2` and `bluebuzz-rm2` seeds `0..2`), KiCad DRC reports **`0` unconnected items and `0` DRC errors** on every seed (`negotiate2-reg2`, `negotiate2-reg3`, `negotiate2-reg4`).
4. Workspace unit and integration tests grow from **147 to 158** (`+11` new unit tests) and all **158 pass** under `cargo test --release --workspace`.

---

## Root-Cause Diagnosis: Why Negotiation Stalled at 55 to 64 of 87

Instrumenting `negotiate_with_Order` on `easy2350` seeds `0..5` (`--cell 0.035 --planes In1.Cu,In2.Cu`) decomposed every seed's 87 connections at the end of legacy negotiation:

| Seed | Committed (`ok`) | Bare-grid unroutable (`no_path`) | Skipped as colliding (`zone_skip`) | Failed `lay()` (`lay_fail`) | Rounds | Colliding-route count per round |
| ---: | ---: | ---: | ---: | ---: | ---: | :--- |
| 0 | 60 | 17 | 3 | 7 | 8 | `[32, 20, 15, 14, 15, 17, 15, 15]` |
| 1 | 64 | 17 | 1 | 5 | 14 | `[31, 14, 14, 15, 12, 15, 12, 13, 10, 5, 8, 5, 9, 6]` |
| 2 | 55 | 17 | 7 | 8 | 9 | `[29, 13, 12, 14, 11, 16, 16, 17, 18]` |
| 3 | 59 | 17 | 5 | 6 | 7 | `[31, 15, 12, 13, 20, 21, 17]` |
| 4 | 60 | 17 | 2 | 8 | 10 | `[33, 20, 18, 18, 16, 13, 13, 16, 16, 19]` |
| 5 | 59 | 17 | 6 | 5 | 7 | `[31, 19, 14, 15, 16, 18, 17]` |

Four distinct mechanisms caused the 15 to 22 lost connections per seed:

### 1. Over-inflated segment clearance zones (`+ 2 * STEP_REACH * h`)
Legacy `zone_discs` expanded every path cell by `r_claim + clearance + r_view + 2 * STEP_REACH * h`, where `STEP_REACH = sqrt(2) / 2`. For two `0.14 mm` traces with `0.10 mm` clearance on a `0.035 mm` grid:
- Exact capsule-to-capsule centreline clearance is `70 + 100 + 70 = 240 um`.
- Worst-case distance from two stepped grid nodes to the continuous segments between them is bounded by `sqrt(d^2 + 0.5 * h^2) + 1 um = 244.6 um` (`6.99` cells).
- Legacy `zone_discs` used `240 + sqrt(2) * 35 = 289.5 um` (`315 um` with `0.125 mm` class clearance = `9.0` cells radius, a **19-cell-wide** forbidden swath!).

Around `U1` (RP2350 QFN-60, `0.4 mm` pad pitch), `U5` (QSPI flash), `R7`/`R8`, and the `J2`/`J3` header buses, legal parallel escape lanes sit `245 to 280 um` apart (`7 to 8` cells). Because legacy zones claimed 9 cells on each side, adjacent traces permanently saw themselves inside each other's collision zones even when routed cleanly side by side. Rising history penalties then pushed them into wide detours that collided with further buses, causing the round collision count to bounce (`14 -> 15 -> 17 -> 15`) until `patience` expired (`zone_skip = 1..7`).

### 2. Unpriced 2D via clearances and same-net hole-to-hole rules (`lay_fail = 5..8`)
Legacy negotiation stored congestion zones only on the path's trace layer and had no price channel for layer changes (`RouteGrid` charged flat scalar `via_cost` with zero congestion term). Consequently:
- A via dropped by route A on `(x, y)` spans **all copper layers** (`r_via = 0.25 mm` on `easy2350`, requiring `0.25 + 0.10 + 0.07 = 0.42 mm` trace-to-via spacing and `0.60 mm` via-to-via spacing), yet route B on F.Cu or B.Cu paid **zero** congestion penalty to run a trace right across `(x, y)` or drop a second via beside it.
- Sibling airlines of the same multi-pin net (`VBUS`, `/GPIO25`) dropped vias within `0 < gap < 0.25 mm` of each other, violating `Rule::HoleToHole`, or at `gap == 0` where `lay()` unconditionally inserted a duplicate `Via` item and rejected its own net for `HoleToHole` (`gap = 0 < 250,000 nm`).
- In seed 0 (`7` `lay_fail`) and seed 3 (`6` `lay_fail`), **12 of those 13** failing routes had `c.colliding == false`: negotiation believed they were completely collision-free, exited, and then watched `lay()` reject them one by one against vias committed milliseconds earlier.

### 3. Blind Euclidean MST chaining across pocketed multi-pin clusters (`no_path = 17 -> 15`)
`board.open_connections()` builds a Euclidean minimum spanning tree over all disconnected copper clusters of each net without checking whether a cluster is walled in. On `easy2350`, `/GPIO23` has three clusters (`U4-2`, pocketed `U1-35`, `J3-11`) and `VLED` has a pocketed middle pad `R11-1` between `D7-1` and `D4-4`. Euclidean MST chained **both** incident airlines through the pocketed middle pin (`U4-2 -> U1-35` and `U1-35 -> J3-11`; `D7-1 -> R11-1` and `R11-1 -> D4-4`), turning 1 pocketed pin into **2** dead airlines and leaving the two reachable outer clusters (`U4-2` and `J3-11`; `D7-1` and `D4-4`) disconnected during negotiation.

### 4. One-shot commit order and post-commit walling
At the end of negotiation, colliding routes were skipped entirely and committed routes were laid in input order rather than minimum-conflict order. Routes that lost a close race or got walled in by committed neighbours were left to sequential `route_open` (which has no congestion negotiation) instead of renegotiating the small leftover set against the newly laid copper or ripping a single blocking net.

---

## What Was Built

### Modified Files (16 files, `+2184 / -195` lines)

1. **`core/crates/grid/src/grid.rs`** (`RouteGrid`):
   - Added `pub via_penalty: Vec<u32>` (`w * h`, default empty = zero penalty) and `#[inline] pub fn via_penalty_at(&self, cell: usize) -> u64` so solvers can charge a per-(x, y) congestion surcharge whenever a step changes layer (`from_layer != to_layer`), without inflating trace steps along a layer.
2. **`core/crates/grid/src/search.rs`**, **`core/crates/grid/src/gpu.rs`**, **`core/crates/grid/src/relax.wgsl`**, **`core/crates/grid/src/sweep.rs`**, **`core/crates/grid/src/sweep.wgsl`**:
   - Added `grid.via_penalty_at(cell)` to CPU Dijkstra and A* (`search.rs`), uploaded `via_penalty` to GPU storage buffers (`gpu.rs`, `sweep.rs`), and added `via_penalties[cell]` to `via_cost` in both WGSL compute shaders (`relax.wgsl`, `sweep.wgsl`).
   - Verified GPU/CPU parity in `negotiate::tests::priced_batch_solver_matches_unpriced_batch_with_via_zones`.
3. **`core/crates/grid/src/blame.rs`**, **`core/crates/grid/src/prefanout.rs`**, **`core/crates/grid/src/ripup.rs`**:
   - Initialized `via_penalty: Vec::new()` on manual `RouteGrid` struct literals.
4. **`core/crates/grid/src/route.rs`**:
   - Updated `lay()` so that when a path changes layer at `(x, y)` where an existing same-net `Kind::Via` already sits at `(x, y)` covering both layers and has `r >= via_radius`, `lay()` reuses that via instead of inserting a duplicate via that fails `Rule::HoleToHole`.
5. **`core/crates/grid/src/lib.rs`** & **`core/crates/web/src/lib.rs`**:
   - Added `RoundPrices`, `PricedJob`, `BatchSolver::route_priced` (with default fallback to `route_batch`), `grid::via_price_into`, and WASM worker-pool support (`WebBatchSolver::route_priced`, `GridSolver::set_round`, `GridSolver::route_priced_job`) so browser workers apply per-(x, y) via prices and `SameNetHoles` hole-to-hole exclusions identically to native CPU/GPU solvers.
6. **`core/crates/grid/src/negotiate.rs`**:
   - **`NegotiationOptions` extensions**:
     - `via_zones: bool` (default `true`): tight segment-to-segment trace zones (`seg_reach(d) = sqrt(d^2 + 0.5 * h^2) + 1_000 nm`), 2D via-viewer congestion zones (`trace_to_via`, `via_to_via`), `SameNetHoles` annulus (`0 < gap < hole_to_hole`, excluding `gap == 0` where vias are shared), and full-width terminal leg claims (`leg_cells`).
     - `repasses: usize` (default `2`): after committing a negotiation pass, immediately re-runs negotiation on remaining open airlines against the newly laid copper (`soft` extended with all laid `ItemId`s).
     - `rip_blocked: bool` (default `true`): `rescue_walled_off` identifies leftover airlines that had a bare-grid path before negotiation (`had_initial_path`) and are now blocked by `<= 3` soft nets (`soft_Set`), rips all soft items of those victim nets together, and runs a focused negotiation sub-pass (`repasses: 0`, `rip_blocked: false`) over the freed + ripped airlines, keeping the change only on strict open-count reduction (`after_open < before_open`).
     - `reachable_tree: bool` (default `true`): `reachable_airlines` tests pairwise bare-grid reachability across clusters of multi-pin nets (`3..=24` clusters) and orders Kruskal spanning edges `( pocketed_flag, euclidean_dist )` so reachable pins connect directly past pocketed middle pins.
     - Sub-problem warm-up floor: when negotiating a small region/rescue sub-problem (`connections.len() <= 24`), `patience` floors at `options.patience.max(8)` so PathFinder history penalties can grow past initial trace/via cost ratios before stalling out.
   - **Incremental 8-direction rim stamping (`FastDisc`, `StampBuf`) & `commit_order`**:
     - Precomputed directional rim arcs (`FastDisc::rims[8]`) and epoch-based `StampBuf` (`seen_net`, `seen_route`, `seen_own`) cut dense zone stamping from `O(pi * R^2)` per step to `O(R)` per step after the start cap, keeping negotiation fast (`13 to 19 s` on `easy2350`).
     - `commit_order` sorts routes at commit time by `(colliding, overlap_peers, vias, path_len)` so clean, low-conflict, low-via routes claim board space first.
7. **`core/crates/grid/src/cleanup.rs`**:
   - Added `end_overlaps` matching `board::joined` and KiCad's strict cap-overlap rule (`gap < reach - 0.5` rather than `gap_within == 0.0` at exact `2 * r` kissing distance). Previously, a foreign or sibling track stub pruned back to exact `2 * r` clearance kissed a round pad or track cap (`gap == 0.0` but zero area overlap); `dangles` thought the stub joined the pad and spared it, leaving KiCad `track_dangling` errors (found on `bluebuzz` seed 0 in `negotiate2-reg`).
8. **`core/crates/grid/src/shove.rs`** & **`core/crates/ultraroute/src/lib.rs`**:
   - Added CLI flags:
     - `--negotiate-repasses N` (override `NegotiationOptions::repasses`, default `2`)
     - `--no-negotiate-vias` (`via_zones = false`)
     - `--no-negotiate-rip` (`rip_blocked = false`)
     - `--no-negotiate-tree` (`reachable_tree = false`)
     - `--negotiate-legacy` (all four reverts at once: `via_zones = false`, `repasses = 0`, `rip_blocked = false`, `reachable_tree = false`, reproducing baseline negotiation for clean A/B testing).
   - Improved region-shove interaction with fuller negotiated boards:
     - `ShoveOptions::net_reaches`: propagates reach widening (`0.5 -> 1.0 -> 2.0 -> 4.0 mm`) across a net's freshly re-laid `ItemId`s when a whole pass stalls (`after >= before`) on a kept `--shove-ties` swap (`stalled_tie`).
     - Cached immovable `ObstacleMap`s (`fixed_maps`), early unpenalised wall check before allocating `penalties` in `plough_round`, and skipping `route_open` when `!changed`, cutting keepout-trapped tries **100x** (from `~400 to 800 ms` down to `~6 ms` per try) with zero change to routing decisions.

### Added Unit Tests (11 new tests, `147 -> 158` total)

- `core/crates/grid/src/negotiate.rs` (6 tests):
  - `fast_disc_rims_match_full_disc_at_every_step`
  - `via_zones_keep_crossing_routes_clear_around_vias`
  - `same_net_routes_share_a_via_or_keep_hole_to_hole_clearance`
  - `reachable_tree_spans_open_pins_past_a_pocketed_middle_pin`
  - `repasses_and_rip_blocked_finish_routes_left_by_one_pass`
  - `priced_batch_solver_matches_unpriced_batch_with_via_zones`
- `core/crates/grid/src/cleanup.rs` (1 test):
  - `kissing_track_ends_count_as_dangling_and_are_pruned`
- `core/crates/grid/src/shove.rs` (2 tests):
  - `net_reaches_override_default_reach_for_any_item_on_the_net`
  - `immovable_wall_fast_path_leaves_board_untouched`
- `core/crates/ultraroute/src/lib.rs` (1 test):
  - `negotiate_flags_parse_and_validate`
- `core/crates/web/src/lib.rs` (1 test):
  - `set_round_and_route_priced_job_honour_via_prices_and_hole_ring`

---

## Experiments Run and Exact KiCad Numbers

All `easy2350` experiments used the exact target recipe (`--threads 1 --timeout 240`):
```
--strip --negotiate --rip-up --tighten --prefanout --diff-pairs --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8 --planes In1.Cu,In2.Cu --cell 0.035 --prefanout-order round --shove-plough region --shove-reach 0.5
```
All A/B experiments ran `a-sN` (`--negotiate-legacy`) and `b-sN` (default improved negotiation) side by side in the same `lab.py run` invocation so both variants experienced identical machine conditions. Signal opens were counted with `sig.py` (KiCad `unconnected_items` excluding `GND` and `+3V3`). Every `easy2350` run has `all_unc = sig + 1`, where the `+1` is the input board's keepout-isolated `Track [+3V3] on F.Cu, length 0.8400 mm`, and `err = {'items_not_allowed': 20}` (the input board's 20 vendor-copper items inside keepouts, 0 `track_width` or clearance errors).

### 0. Reference Baseline: `planes-50` (`easy2350`, seeds `0..5`)

| Run | Seed | Negotiated | `route_open` | `rip_up` | Shove passes | End open | Wall (s) | Signal opens | Unconnected signal nets | DRC errors |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | :--- | :--- |
| `vb-s0` | 0 | `60/87` (8r, 18v, 14.3s) | `7/27` | `1/37` | 11 | 9 | 152.68 | **0** | none | `items_not_allowed: 20` |
| `vb-s1` | 1 | `64/87` (14r, 24v, 24.1s) | `5/23` | `1/34` | 14 | 10 | 236.78 | **1** | `/GPIO27` | `items_not_allowed: 20` |
| `vb-s2` | 2 | `55/87` (9r, 34v, 16.6s) | `12/32` | `1/38` | 16 | 9 | 230.76 | **0** | none | `items_not_allowed: 20` |
| `vb-s3` | 3 | `59/87` (7r, 30v, 13.7s) | `8/28` | `2/37` | 6 | 11 | 244.99 | **2** | `/GPIO23`, `VBUS` | `items_not_allowed: 20` |
| `vb-s4` | 4 | `60/87` (10r, 32v, 17.1s) | `6/28` | `1/39` | 6 | 10 | 242.17 | **1** | `/GPIO3` | `items_not_allowed: 20` |
| `vb-s5` | 5 | `59/87` (7r, 21v, 13.4s) | `9/29` | `1/35` | 5 | 11 | 241.81 | **2** | `/GPIO27`, `/QSPI_SCLK` | `items_not_allowed: 20` |

Baseline total across seeds `0..5`: **6 signal opens** (`0, 1, 0, 2, 1, 2`).

---

### 1. Stage-Isolated Negotiation Ablation (`easy2350`, seeds `0..5`)

Before running full 240 s end-to-end routes, each negotiation mechanism was stacked incrementally on the exact post-prefanout/planes/pairs `easy2350` board state across seeds `0..5`:

| Variant | Committed (`s0..s5`, `/87`) | Remaining open (`s0..s5`) | Vias (`s0..s5`) | Time (`ms`, `s0..s5`) |
| :--- | :--- | :--- | :--- | :--- |
| `legacy` | `60, 64, 55, 59, 60, 59` | `27, 23, 32, 28, 27, 28` | `18, 24, 34, 30, 32, 21` | `14742, 24047, 18029, 13792, 18131, 14121` |
| `+via_zones` (tight `seg_reach` + 2D via zones + shared vias + `commit_order`) | `70, 68, 70, 70, 69, 70` | `17, 19, 17, 17, 18, 17` | `28, 24, 35, 32, 31, 32` | `13170, 16625, 16296, 18327, 22909, 12570` |
| `+tree` (`reachable_airlines` Kruskal order) | `72, 72, 72, 72, 72, 71` | `15, 15, 15, 15, 15, 16` | `37, 40, 38, 36, 33, 32` | `18375, 15700, 19471, 17090, 26641, 26050` |
| `+repasses2` (multi-pass renegotiation) | `72, 72, 72, 72, 72, 71` | `15, 15, 15, 15, 15, 16` | `37, 40, 38, 36, 33, 32` | `22143, 17915, 17896, 14093, 23127, 22638` |
| `+rip_blocked` (whole-victim-net rescue) | **`72, 72, 72, 72, 72, 71`** | **`15, 15, 15, 15, 15, 16`** | `37, 40, 38, 36, 33, 32` | `19351, 18853, 15911, 12963, 16349, 15041` |

On seeds `0..4`, the 15 remaining open connections after `+tree` are **exclusively** the 9 keepout-blocked connections and 6 pre-negotiation hard pockets; 72 of 72 routable connections are committed during negotiation. (`repasses` and `rip_blocked` do additional work inside region-shove sub-negotiations and on denser multi-net boards; see `negotiate::tests::repasses_and_rip_blocked_finish_routes_left_by_one_pass`.)

---

### 2. `negotiate2-ab-02` (v1: negotiation improvements only, before `net_reaches` & sub-problem warm-up, seeds `0..2`)

| Run | Variant | Seed | Negotiated | `route_open` | `rip_up` | Shove passes | End open | Wall (s) | Signal opens | Unconnected signal nets |
| :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | :--- |
| `a-s0` | `--negotiate-legacy` | 0 | `60/87` (8r, 18v, 12.9s) | `7/27` | `1/37` | 11 | 9 | 152.14 | **0** | none |
| `b-s0` | v1 | 0 | `72/87` (10r, 37v, 17.7s) | `0/15` | `1/29` | 11 | 10 | 211.85 | **1** | `/GPIO27` |
| `a-s1` | `--negotiate-legacy` | 1 | `64/87` (14r, 24v, 19.9s) | `5/23` | `1/34` | 11 | 9 | 119.89 | **0** | none |
| `b-s1` | v1 | 1 | `72/87` (9r, 40v, 17.3s) | `0/15` | `1/29` | 11 | 9 | 111.79 | **0** | none |
| `a-s2` | `--negotiate-legacy` | 2 | `55/87` (9r, 34v, 14.8s) | `12/32` | `1/38` | 6 | 10 | 240.85 | **1** | `/GPIO26` |
| `b-s2` | v1 | 2 | `72/87` (10r, 38v, 18.5s) | `0/15` | `1/29` | 11 | 10 | 199.52 | **1** | `/GPIO23` |

**Finding from `negotiate2-ab-02`**: Negotiation jumped from `60, 64, 55` to `72, 72, 72 / 87`, yet `b-s0` and `b-s2` stopped early at `t = 211.9 s` and `199.5 s` with 1 signal open (`stalled == 8`)! Inspection of `b-s0/log.txt` and `b-s2/log.txt` revealed two bugs in how `shove` interacted with the fuller negotiated board:
1. Region shove calls `negotiate` with `patience: 3` on `6..12` ripped connections; with 2D via zones active, 3 rounds was too short for history penalties to outgrow via/detour cost differences, so region renegotiations exited early before resolving 2-via swaps.
2. In `ultraroute/src/lib.rs`, `reaches: HashMap<ItemId, Nm>` was keyed only by `ItemId` and erased whenever `t.kept == true`. Under `--shove-ties`, two nets (`VBUS` and `+1V1`, or `/GPIO27` and `/GPIO28`) kept swapping places (`10 -> 10 open, kept`), creating brand-new track `ItemId`s every pass so `reaches` stayed stuck at `0.5 mm` for all 8 patience passes instead of widening to `1.0 / 2.0 / 4.0 mm`!

---

### 3. `negotiate2-v2-02` and `negotiate2-v2-35` (v2: `+ sub-problem patience floor + net_reaches on stalled ties`, seeds `0..5`)

Adding the sub-problem `patience.max(8)` floor (`connections.len() <= 24`) and `ShoveOptions::net_reaches` widening on stalled passes (`stalled_tie = tie_widens && after >= before && t.open_after >= t.open_before`) resolved every seed:

| Lab ID | Run | Variant | Seed | Negotiated | `route_open` | `rip_up` | Shove passes | End open | Wall (s) | Signal opens | Unconnected signal nets |
| :--- | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | :--- |
| `negotiate2-v2-02` | `a-s0` | `--negotiate-legacy` | 0 | `60/87` (8r, 18v, 14.1s) | `7/27` | `1/37` | 11 | 9 | 174.45 | **0** | none |
| `negotiate2-v2-02` | `b-s0` | v2 (improved) | 0 | `72/87` (10r, 37v, 19.1s) | `0/15` | `1/29` | 11 | 9 | 143.72 | **0** | none |
| `negotiate2-v2-02` | `a-s1` | `--negotiate-legacy` | 1 | `64/87` (14r, 24v, 21.4s) | `5/23` | `1/34` | 11 | 9 | 138.71 | **0** | none |
| `negotiate2-v2-02` | `b-s1` | v2 (improved) | 1 | `72/87` (9r, 40v, 18.0s) | `0/15` | `1/29` | 11 | 9 | 125.08 | **0** | none |
| `negotiate2-v2-02` | `a-s2` | `--negotiate-legacy` | 2 | `55/87` (9r, 34v, 16.0s) | `12/32` | `1/38` | 5 | 10 | 240.49 | **1** | `/GPIO27` |
| `negotiate2-v2-02` | `b-s2` | v2 (improved) | 2 | `72/87` (10r, 38v, 19.5s) | `0/15` | `1/29` | 12 | 9 | 240.29 | **0** | none |
| `negotiate2-v2-35` | `a-s3` | `--negotiate-legacy` | 3 | `59/87` (7r, 30v, 12.1s) | `8/28` | `2/37` | 4 | 11 | 240.31 | **2** | `/GPIO25`, `VBUS` |
| `negotiate2-v2-35` | `b-s3` | v2 (improved) | 3 | `72/87` (9r, 36v, 16.4s) | `0/15` | `1/29` | 12 | 9 | 171.95 | **0** | none |
| `negotiate2-v2-35` | `a-s4` | `--negotiate-legacy` | 4 | `60/87` (10r, 32v, 15.0s) | `6/28` | `1/39` | 4 | 11 | 240.58 | **2** | `/GPIO27`, `/QSPI_SD2` |
| `negotiate2-v2-35` | `b-s4` | v2 (improved) | 4 | `72/87` (12r, 33v, 19.6s) | `0/15` | `1/29` | 11 | 9 | 240.28 | **0** | none |
| `negotiate2-v2-35` | `a-s5` | `--negotiate-legacy` | 5 | `59/87` (7r, 21v, 11.8s) | `9/29` | `1/35` | 4 | 11 | 240.40 | **2** | `/GPIO27`, `/QSPI_SCLK` |
| `negotiate2-v2-35` | `b-s5` | v2 (improved) | 5 | `71/87` (12r, 32v, 19.9s) | `0/16` | `1/31` | 11 | 9 | 123.72 | **0** | none |

Totals across seeds `0..5` in `negotiate2-v2`:
- `a` (`--negotiate-legacy`): **`0, 0, 1, 2, 2, 2` = 7 signal opens**.
- `b` (improved): **`0, 0, 0, 0, 0, 0` = 0 signal opens** (`all_unc = 1` on every seed, `min_vbus_w = 0.2 mm`, `inner_segs = 0`, `err = {'items_not_allowed': 20}`).

---

### 4. Regression Boards: `negotiate2-reg` and `negotiate2-reg2` (`earring` & `bluebuzz`, seeds `0..2`)

Recipe: `--negotiate --rip-up --nudge --tighten --strip --prefanout --diff-pairs --shove-plough region --shove-reach 0.5 --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8` (`--strip` on `earring`, `--strip-all` on `bluebuzz-rm2`).

- In **`negotiate2-reg`** (before `cleanup.rs` kissing-end fix), all 6 runs achieved `sig = 0` and `all_unc = 0`, and 5 of 6 had `err = {}`, while `bb-s0` reported `err = {'track_dangling': 2}` (two `0.2 mm`-wide stubs whose trimmed ends kissed round pads at exact `2 * r` distance without cap overlap).
- Fixing `end_overlaps` in `core/crates/grid/src/cleanup.rs` eliminated `track_dangling` completely in **`negotiate2-reg2`** (and subsequently **`negotiate2-reg3`** and **`negotiate2-reg4`**):

| Lab ID | Run | Board & Seed | Negotiated | `route_open` | `rip_up` | Shove passes | Wall (s) | Unconnected (`all_unc`) | DRC errors |
| :--- | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: | :--- |
| `negotiate2-reg` | `ear-s0..2` | `earring` `0..2` | `413, 412, 412 / 414` | `0` | `1, 1, 2` | `8, 9, 8` | `28.3, 44.5, 42.3` | `0, 0, 0` | `{}, {}, {}` |
| `negotiate2-reg` | `bb-s0..2` | `bluebuzz` `0..2` | `299, 299, 299 / 299` | `0` | `0, 0, 0` | `8, 8, 8` | `42.8, 39.2, 42.8` | `0, 0, 0` | `{'track_dangling': 2}, {}, {}` |
| `negotiate2-reg2` | `ear-s0` | `earring` `0` | `413/414` (8r, 189v, 36.6s) | `0/1` | `1/1` | 8 | 37.24 | **0** | `{}` |
| `negotiate2-reg2` | `ear-s1` | `earring` `1` | `412/414` (13r, 202v, 52.5s) | `0/2` | `1/3` | 9 | 54.25 | **0** | `{}` |
| `negotiate2-reg2` | `ear-s2` | `earring` `2` | `412/414` (12r, 190v, 49.1s) | `0/2` | `2/2` | 8 | 50.11 | **0** | `{}` |
| `negotiate2-reg2` | `bb-s0` | `bluebuzz` `0` | `299/299` (6r, 180v, 51.2s) | `0/0` | `0/0` | 8 | 51.62 | **0** | `{}` |
| `negotiate2-reg2` | `bb-s1` | `bluebuzz` `1` | `299/299` (5r, 178v, 45.6s) | `0/0` | `0/0` | 8 | 46.07 | **0** | `{}` |
| `negotiate2-reg2` | `bb-s2` | `bluebuzz` `2` | `299/299` (6r, 182v, 52.0s) | `0/0` | `0/0` | 8 | 52.41 | **0** | `{}` |

---

### 5. Contended-CPU Runs and Shove Tie-Capping Experiments (`negotiate2-final-02/35`, `negotiate2-reg3`, `negotiate2-final2-02/35`, `negotiate2-reg4`, `negotiate2-v4-02/35`)

Re-running v2 after the `cleanup.rs` fix (`negotiate2-final-02` and `negotiate2-final-35`) coincided with heavy unbooked external CPU load on the shared 24-core host (`load 52 on 24`, CPU pressure `48%..58%`, slowing single-threaded negotiation from `16..19 s` to `47..98 s`). Because `--timeout 240` is wall-clock seconds, runs launched during high CPU pressure completed fewer shove passes before the 240 s cutoff. All per-seed results from these runs (including two experimental `tied_nets` variants, v3 and v4, that were tested and rejected) are recorded below:

| Lab ID | Variant tested | `a` signal opens (`s0..s2` or `s3..s5`) | `b` signal opens (`s0..s2` or `s3..s5`) | Exact `a` unconnected signal nets | Exact `b` unconnected signal nets |
| :--- | :--- | :--- | :--- | :--- | :--- |
| `negotiate2-final-02` | v2 + `cleanup.rs` (seeds `0..2`, moderate load) | `0, 0, 2` (2) | `0, 0, 1` (1) | `s2`: `/GPIO27`, `Net-(R7-Pad2)` | `s2`: `+1V1` (stopped pass 4 at 241.8s) |
| `negotiate2-final-35` | v2 + `cleanup.rs` (seeds `3..5`, high load) | `3, 4, 3` (10) | `1, 1, 0` (2) | `s3`: `/GPIO25`, `/QSPI_SD0`, `VBUS`; `s4`: `/GPIO23`, `/GPIO24`, `/GPIO29`, `/QSPI_SD0`; `s5`: `/GPIO24`, `/GPIO26`, `/QSPI_SD3` | `s3`: `/QSPI_SD0` (pass 4); `s4`: `/QSPI_SCLK` (pass 3) |
| `negotiate2-reg3` | v3 regression (`earring` `0..2`, `bluebuzz` `0..2`) | n/a | `0, 0, 0, 0, 0, 0` | n/a | none (`all_unc = 0`, `err = {}` on all 6) |
| `negotiate2-final2-02` | v3 (`fixed_maps` + unconditional tie-widen + `tied_nets`, seeds `0..2`, 4.7x load) | `3, 1, 3` (7) | `1, 0, 2` (3) | `s0`: `/GPIO27`, `/QSPI_SD3`, `VBUS`; `s1`: `/GPIO27`; `s2`: `/GPIO27`, `/QSPI_SD0`, `Net-(R7-Pad2)` | `s0`: `/GPIO27`; `s2`: `+1V1`, `/GPIO23` |
| `negotiate2-final2-35` | v3 (`fixed_maps` + unconditional tie-widen + `tied_nets`, seeds `3..5`, 3x load) | `3, 5, 3` (11) | **`0, 0, 0` (0)** | `s3`: `/GPIO25`, `/QSPI_SD0`, `VBUS`; `s4`: `/GPIO23`, `/GPIO24`, `/GPIO26`, `/GPIO28`, `/QSPI_SD3`; `s5`: `/GPIO24`, `/GPIO26`, `/QSPI_SD3` | none (0 signal opens on all 3 seeds: `147.3s`, `224.6s`, `112.4s`) |
| `negotiate2-reg4` | v4 regression (`earring` `0..2`, `bluebuzz` `0..2`) | n/a | `0, 0, 0, 0, 0, 0` | n/a | none (`all_unc = 0`, `err = {}` on all 6) |
| `negotiate2-v4-02` | v4 (`fixed_maps` + `stalled_tie` + `tied_nets` reset on drop, seeds `0..2`) | `0, 0, 1` (1) | `0, 0, 2` (2) | `s2`: `/QSPI_SCLK` | `s2`: `+1V1`, `/GPIO23` |
| `negotiate2-v4-35` | v4 (`fixed_maps` + `stalled_tie` + `tied_nets` reset on drop, seeds `3..5`, 5x load) | `3, 5, 3` (11) | `0, 1, 0` (1) | `s3`: `/GPIO25`, `/QSPI_SD0`, `VBUS`; `s4`: `/GPIO23`, `/GPIO24`, `/GPIO26`, `/GPIO28`, `/QSPI_SD3`; `s5`: `/GPIO24`, `/GPIO26`, `/QSPI_SD3` | `s4`: `/GPIO0` (`neg` alone took `98.6s` under 58% CPU pressure) |

**What these experiments taught us**:
1. **Caching `fixed_maps` + early unpenalised wall check + skipping `route_open` on `!changed` is a pure 100x speedup (`800 ms -> 6 ms` per keepout try, saving `50..67 s` per run) with zero behavioral change**: In `v4-35`, even when external CPU pressure stretched negotiation alone to `92..98 s` (`5x` slower than normal), `b-s3` and `b-s5` still solved to `0` signal opens in `198.8 s` and `135.8 s` (while `a-s3..5` left `3, 5, 3` signal opens).
2. **Per-net `tied_nets` skipping (tested in v3 and v4) is unsound for multi-cluster nets and was removed in v5**: Comparing `negotiate2-v2-02/b-s2/log.txt` (`0` signal opens) against `negotiate2-v4-02/b-s2/log.txt` (`2` signal opens) showed that multi-pin nets like `VBUS`, `+1V1`, `/GPIO23`, and `/GPIO24` have multiple distinct open track segments in different regions of the board. In `v2/b-s2` Pass 6, `VBUS`'s first track segment tied (`56 pieces of 3 nets`), and then `VBUS`'s *second* track segment in the same pass (`84 pieces of 12 nets`) cleared the corridor that allowed `/GPIO9` to solve `10 -> 9 open`. Blocking a net after its first tie (`tied_nets`) skipped `VBUS`'s second track segment and `/GPIO24`'s second escape shift, preventing `b-s2` from converging. Removing `tied_nets` in v5 restores `v2`'s exact routing decisions while keeping the 100x `fixed_maps` speedup.

---

### 6. Final Verification Suite: `negotiate2-v5-02` and `negotiate2-v5-35` (v5: `v2` routing logic + `cleanup.rs` + lossless `fixed_maps`)

With `tied_nets` removed and lossless `fixed_maps` + `!changed` skip active, `v5` reproduces `v2`'s exact routing decisions while cutting total wall-clock runtime nearly in half (mean **`89.48 s`** vs `174.17 s` in `v2` and `194.13 s` in same-run `--negotiate-legacy` controls):

| Lab ID | Run | Variant | Seed | Negotiated | `route_open` | `rip_up` | Shove passes | End open | Wall (s) | Signal opens | Unconnected signal nets |
| :--- | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | :--- |
| `negotiate2-v5-02` | `a-s0` | `--negotiate-legacy` | 0 | `60/87` (8r, 18v, 21.0s) | `7/27` | `1/37` | 11 | 9 | 112.56 | **0** | none |
| `negotiate2-v5-02` | `b-s0` | **v5 (default)** | 0 | **`72/87`** (10r, 37v, 28.3s) | `0/15` | `1/29` | 11 | 9 | **77.34** | **0** | none |
| `negotiate2-v5-02` | `a-s1` | `--negotiate-legacy` | 1 | `64/87` (14r, 24v, 32.2s) | `5/23` | `1/34` | 11 | 9 | 76.16 | **0** | none |
| `negotiate2-v5-02` | `b-s1` | **v5 (default)** | 1 | **`72/87`** (9r, 40v, 26.8s) | `0/15` | `1/29` | 11 | 9 | **62.01** | **0** | none |
| `negotiate2-v5-02` | `a-s2` | `--negotiate-legacy` | 2 | `55/87` (9r, 34v, 23.8s) | `12/32` | `1/38` | 13 | 10 | 240.93 | **1** | `/GPIO27` |
| `negotiate2-v5-02` | `b-s2` | **v5 (default)** | 2 | **`72/87`** (10r, 38v, 29.1s) | `0/15` | `1/29` | 14 | 9 | **175.63** | **0** | none |
| `negotiate2-v5-35` | `a-s3` | `--negotiate-legacy` | 3 | `59/87` (7r, 30v, 11.8s) | `8/28` | `2/37` | 7 | 11 | 247.70 | **2** | `/GPIO25`, `VBUS` |
| `negotiate2-v5-35` | `b-s3` | **v5 (default)** | 3 | **`72/87`** (9r, 36v, 16.3s) | `0/15` | `1/29` | 12 | 9 | **62.12** | **0** | none |
| `negotiate2-v5-35` | `a-s4` | `--negotiate-legacy` | 4 | `60/87` (10r, 32v, 14.7s) | `6/28` | `1/39` | 8 | 11 | 247.18 | **2** | `/GPIO27`, `/QSPI_SD2` |
| `negotiate2-v5-35` | `b-s4` | **v5 (default)** | 4 | **`72/87`** (12r, 33v, 19.6s) | `0/15` | `1/29` | 18 | 9 | **123.06** | **0** | none |
| `negotiate2-v5-35` | `a-s5` | `--negotiate-legacy` | 5 | `59/87` (7r, 21v, 11.7s) | `9/29` | `1/35` | 7 | 11 | 240.23 | **2** | `/GPIO28`, `/QSPI_SCLK` |
| `negotiate2-v5-35` | `b-s5` | **v5 (default)** | 5 | **`71/87`** (12r, 32v, 19.8s) | `0/16` | `1/31` | 11 | 9 | **36.71** | **0** | none |

**KiCad PCB and DRC audit across `b-s0..b-s5` (`negotiate2-v5-02` + `negotiate2-v5-35`)**:
- Signal opens: **`0, 0, 0, 0, 0, 0`** (0 total across seeds `0..5`, vs **`0, 0, 1, 2, 2, 2` = 7** on `--negotiate-legacy` and **`0, 1, 0, 2, 1, 2` = 6** in `planes-50`).
- All unconnected items: `all_unc = 1` on every seed (`Track [+3V3] on F.Cu, length 0.8400 mm`, isolated by an input keepout).
- `.kicad_dru` VBUS track width rule (`>= 0.2 mm`): `27, 32, 30, 27, 36, 33` `VBUS` segments laid, **`min_vbus_w = 0.2 mm` on every seed**, **0 `track_width` errors**.
- Inner power planes (`In1.Cu`, `In2.Cu`): **`0` routed segments** on inner layers across all 6 seeds.
- Footprints, pads, and zones preserved: `58` footprints, `279` pads, `63` zones on all 6 seeds.
- KiCad DRC error severities: `{'items_not_allowed': 20}` on every seed (0 clearance, shorting, hole-to-hole, track-width, or dangling-track errors).

---

## Summary of What Worked vs What Did Not

### What Worked
1. **Exact grid-segment zone radius (`seg_reach`) + 2D via congestion zones (`trace_to_via`, `via_to_via`, `SameNetHoles`) + shared same-net vias in `lay()`**: The single biggest win, lifting negotiation commits on `easy2350` from `55..64 / 87` (`avg 59.5`) to `68..70 / 87` (`avg 69.5`) and eliminating silent post-negotiation `lay()` failures against vias.
2. **Reachable-first Kruskal spanning (`reachable_airlines`)**: Prevented pocketed middle pins (`U1-35` on `/GPIO23`, `R11-1` on `VLED`) from splitting reachable outer clusters into two dead MST airlines, lifting commits to **`72, 72, 72, 72, 72, 71 / 87`** (100% of bare-grid-routable airlines on seeds `0..4`).
3. **Minimum-conflict commit order (`commit_order`), multi-pass renegotiation (`repasses: 2`), and whole-victim-net rescue (`rip_blocked`)**: Reduced negotiation runtime (`22 s -> 15..19 s`) and resolved multi-route region-shove sub-negotiations cleanly.
4. **Sub-problem warm-up floor (`patience.max(8)` for `<= 24` connections) + `net_reaches` widening on stalled passes (`after >= before`) + `fixed_maps` caching**: Allowed region shove to finish the 6 pre-negotiation hard pockets reliably across all seeds (`0, 0, 0, 0, 0, 0` signal opens) while cutting keepout-trapped checks 100x (`800 ms -> 6 ms`).
5. **Strict cap-overlap check (`end_overlaps`) in `cleanup.rs`**: Eliminated `track_dangling` DRC violations caused by pruned stubs kissing round pads/caps at exact `2 * r` clearance (`bluebuzz` `bb-s0`: `track_dangling: 2 -> 0`).

### What Did Not Work and Why
1. **Longer or gentler PathFinder parameters alone (`--negotiate-rounds`, `--negotiate-patience`, `--negotiate-growth`)**: Could not help because the collision geometry itself was wrong (over-inflated 19-cell trace zones falsely flagged legal adjacent `U1`/`U5` escape lanes, while 2D via clearances were completely unpriced at `0` congestion cost regardless of round count).
2. **Keying shove reach escalation only by `ItemId` (`negotiate2-ab-02`)**: Under `--shove-ties`, region renegotiation deletes and re-lays track items with fresh `ItemId`s on every tied swap, resetting `reaches` back to `0.5 mm` forever unless tracked by net (`net_reaches`) when a pass stalls.
3. **Unconditional tie-widening on improving passes (`negotiate2-final2-02`) and per-net `tied_nets` capping (`negotiate2-v4-02`)**: Doubling reaches during passes that already reduced open count (`after < before`) caused Pass 2 to rip `36..255` pieces instead of `5` pieces, and skipping a net after its first kept tie prevented multi-cluster nets (`VBUS`, `+1V1`, `/GPIO24`) from shoving their *other* disconnected track segments in separate regions of the board (`b-s2` Pass 6 needed `VBUS`'s second track segment to free `/GPIO9`). Restricting net-widening to stalled passes (`after >= before`) and keeping every open `ItemId` eligible once per pass (as in `v2` and `v5`) avoided both pitfalls.

---

## What To Try Next

1. **Negotiate pre-fanout escapes jointly with global airlines (soft pre-fanout)**: Today `prefanout` locks in escape directions and vias around `U1` (`0.4 mm` QFN-60) before global negotiation sees where those nets actually go, creating all **6** of `easy2350`'s non-keepout pockets (`/GPIO23` `U1-35`, `/GPIO28` `J3-17`, `/QSPI_SCLK` `U5-6`, `/QSPI_SD3` `U5-7`, `/USB_DP_MCU`, `VLED`). Allowing `rescue_walled_off` (or a second negotiation pass) to rip and re-negotiate soft pre-fanout stubs alongside global routes would let negotiation commit `77..78 / 87` directly without needing region-shove passes at all.
2. **Pre-filter structurally keepout-walled terminals before shove**: Even at `6 ms` per try, `easy2350` has 9 connections whose pads sit inside input keepouts with zero valid terminal cells on any routing layer (`src_terms == 0` or `tgt_terms == 0` against fixed obstacles). Detecting `terms == 0` once on the immovable obstacle map would skip them across `route_open`, `rip_up`, and `shove` alike and report exact keepout-blocked diagnostics immediately.
3. **Steiner tree attachment during negotiation**: `reachable_airlines` chooses a static Kruskal tree over reachable clusters before round 1; letting multi-pin nets attach dynamically to any already-routed sibling track segment during PathFinder rounds would shorten power/ground trees (`VBUS`, `+1V1`, `VLED`) and cut via count further.
