# Region Plough Renegotiation: Investigation, Fixes, and Lab Results

## Problem and Root-Cause Investigation

On `jobs/easy2350/input/easy2350.kicad_pcb`, the region plough (`Plough::Region` in `core/crates/grid/src/shove.rs`) repeatedly found a soft path for the last 1-2 open signal connections, ripped movable copper within `reach`, and ran `route_negotiated`, only to leave 1-2 other connections open (`routed; 10 -> 11 open, undone`) pass after pass until the 240 s timeout.

Inspecting verbose PathFinder round logs and S-expression copper geometry around the failing pockets across seeds 0-5 revealed six general algorithmic causes:

1. **Inflated 3D congestion zones and missing 2D via congestion zones (`negotiate.rs`, `search.rs`, `grid.rs`)**:
   - Standard `zone_discs` adds a full `+1 * cell` grid-quantisation pad on top of copper clearance (`0.285 mm` = 9 cells on a `0.035 mm` grid instead of the `0.250 mm` = 7.14 cells physical centre-to-centre clearance of two `0.15 mm` tracks at `0.10 mm` rule clearance). Four legal parallel tracks in a `0.85 mm` fine-pitch escape mouth overlapped in PathFinder even when `lay()` accepted them.
   - More critically, `zone_discs` only priced `(owner_trace -> viewer_trace)` and `(owner_via -> viewer_trace)` in the 3D `penalty` array. It had no `(owner_trace -> viewer_via)` (`0.45 mm` centre-to-centre) or `(owner_via -> viewer_via)` (`0.625 mm` centre-to-centre) zones, and no 2D `via_penalty` on layer transitions. Two `0.5 mm` vias could sit `0.46 mm` apart with `colliding = 0` and then fail `lay()` at commit; and a route dropping a `0.5 mm` via `0.35 mm` (10 cells) from a neighbour trace never saw the neighbour's 7-cell trace zone (`colliding = false`, `history = 0` across every round), stealing the neighbour's lane at commit.
2. **Greedy commit order, fixed `0.5` trapped-net weight, and best-round amnesia (`negotiate.rs`)**:
   - When `patience` expired with 1-2 routes still colliding, PathFinder committed the last round (after `present` had grown steep enough to trigger oscillation) instead of the minimum-collision round, and let a colliding route commit ahead of a collision-free route and block its corridor.
   - Holding the trapped net at `net_weight = 0.5` across all rounds also allowed it to sit permanently on the sole exit of a neighbour pad instead of yielding once history accumulated.
3. **Euclidean MST blindness on multi-component nets (`route.rs`, `negotiate.rs`)**:
   - `connectivity_without_pours` emits airlines along a Euclidean minimum spanning tree of a net's clusters. When a net has a pre-existing buried or vendor-locked island (such as a `+1V1` pad boxed in by locked copper), the Euclidean MST can step `C_left -- W_walled -- C_right`. Neither `route_negotiated` nor `route_open` ever tried connecting `C_left` directly to `C_right`, leaving a physically routable split open (seen on `+1V1` / `VBUS` on seed 5).
   - In addition, `cluster_terminals` previously skipped multi-pad rails (`pads.len() > 4`), starving nets like `+1V1` and `VBUS` when an airline endpoint was a congested cut track tip rather than the nearest pads of the cluster.
4. **Full-corridor ripping across clear spans, cut-stub ping-pong on `t.net`, and dangling ghost copper (`shove.rs`, `cleanup.rs`)**:
   - Ripping `reach` around the entire length of a 25 mm cross-board soft path tore up 15-28 unrelated parallel bus tracks along spans where the soft path already ran in free space (`Net-(R7-Pad2)` on seed 2).
   - When a tie-accepted try displaced a net (`B`) by cutting `B`'s track locally, `B` kept two long dangling soft stubs reaching right up to `A`'s newly laid track. When `B`'s turn came, `find_soft_path` stepped 0.3 mm across `A` between those two frozen cut tips instead of routing cleanly from `B`'s pads (`+1V1` vs `VBUS` on seed 5, `/GPIO27` vs `/GPIO28` on seed 3).
   - Tie swaps and local cuts accumulated 90-140 dangling/duplicate soft stubs on already-closed nets before end-of-run cleanup, blocking routing channels during shove passes.
5. **Movable plane-via over-pricing and ordering on `--planes` boards (`shove.rs`, `ripup.rs`)**:
   - On boards with inner power planes (`!pours_block` and `board.is_plane`), `join_planes` drops opportunistic movable vias from outer `+3V3` and `GND` pads (`U1-43`, decoupling caps) straight into tight escape corridors even though outer pours also join those pads.
   - `find_soft_path` and `rip_up` priced every cell of soft copper of those fat `0.6 mm` vias at `cross_cost = 400` (`~8,000` penalty vs `1,200` for crossing two `0.1 mm` signal tracks), trapping `/GPIO28` behind a movable `+3V3` via at `(113.94, 123.70)` on seed 3. Worse, if a try evicted a movable plane via whose pad could no longer fit a via, raw `connectivity_without_pours` counted the plane airline against `open_after` and rejected a try that strictly reduced signal opens.
   - Calling `join_planes` *before* `route_open(neg_nets)` inside `renegotiate_region` also let opportunistic plane vias reclaim corridor space before displaced signal nets finished reconnecting.
6. **Single-net sequential `rip_up` repair and wasted undone-try work (`shove.rs`, `ripup.rs`, `grid.rs`)**:
   - Post-try `rip_up` held `t.net` untouchable and rerouted ripped blockers one by one with `route_open`, so it could neither swap two nets across adjacent lanes nor pack 2-8 ripped corridor nets simultaneously.
   - Undone airlines tried both `from_item` and `to_item` at the same reach on an unchanged board (wasting half of every pass), and single-slot `stamp_outline` caching thrashed whenever `Default` and `VBUS` netclasses alternated.

---

## What Was Built

### Files Changed and Flags Added

- **`core/crates/grid/src/cleanup.rs`**:
  - Added `prune_dangling_filtered(board, soft, only_nets, skip_nets, watch)` so callers can either prune dangling stubs only on selected nets (`only_nets = Some(&[t.net])`) or prune closed nets while leaving currently open nets intact (`skip_nets = &open_nets`).
  - Added unit test `prune_dangling_filtered_honours_only_and_skip_net_sets`.
- **`core/crates/grid/src/grid.rs`**:
  - Expanded `stamp_outline` to a 4-entry cache (`OUTLINE_CACHE_CAP = 4`) and added a 4-entry static-geometry semilattice cache (`STATIC_CACHE` + `static_cache_key` in `ObstacleMap::without`) that caches rasterised outlines, pads, PTH holes, pours/planes, and keepouts across repeated `ObstacleMap` builds while producing cell-for-cell identical `owner` and `via_owner` arrays.
  - Added `ObstacleMap::via_blocked_at(net, cell)` to expose per-2D-cell soft via-clearance occupancy for layer-change pricing.
  - Added unit test `cached_static_obstacle_map_matches_direct_stamping_cell_for_cell`.
- **`core/crates/grid/src/search.rs`**:
  - Added `cost_field_via`, `trace_back_via`, and `route_astar_via(grid, sources, targets, via_cost, via_penalty)` to price 2D layer-change penalties (`via_penalty[cell]`) consistently in Dijkstra, A*, and traceback without enlarging `RouteGrid`.
  - Added unit test `a_star_via_respects_via_penalties_and_matches_the_full_field`.
- **`core/crates/grid/src/route.rs`**:
  - Added `FieldSolver::route_with_via_penalty` (implemented on `CpuSolver` via `route_astar_via`).
  - Added `other_cluster_terminals` and a multi-component MST fallback in `route_open` so `>= 3`-cluster nets with a walled middle island still bridge their reachable clusters.
  - Added unit test `route_open_bridges_reachable_clusters_past_a_walled_middle_island`.
- **`core/crates/grid/src/negotiate.rs`**:
  - Added `NegotiationOptions::tight_zones`: exact physical clearance discs (`zone_discs` without `+h` inflation) plus symmetric 2D via congestion zones (`via_zone_discs`, `via_zone_of`, `via_zones`, `via_history`) pricing `(owner_trace -> viewer_via)` and `(owner_via -> viewer_via)` on layer transitions.
  - Added nearest-4 pad terminal expansion (`cluster_terminals`) on every cluster regardless of total net pad count, pre-round walled-connection pruning, multi-component MST rebinding via `other_cluster_terminals`, late-round `net_weight` decay toward `1.0` after round 5, best-round snapshot restoration (`best_paths`), and collision-free-first commit ordering with single-net clean reroute fallback.
  - Added unit test `tight_zones_price_via_zones_and_commit_clean_crossings`.
- **`core/crates/grid/src/ripup.rs`**:
  - Added `RipOptions::region_repair`: charges `via_blocked_at` soft penalties on layer changes, expands cluster terminals to the nearest 4 pads, prices movable plane-net copper cheaply (`STRAIGHT / 2`) on `--planes` boards, re-runs `join_planes` after rerouting, falls back to equal-weight `route_negotiated` (`tight_zones: true`) when sequential `route_open` leaves 2-8 ripped signal nets colliding, and prunes closed-net dangling stubs on kept repairs.
  - Added unit test `region_repair_negotiates_multiple_ripped_nets_in_a_narrow_corridor`.
- **`core/crates/grid/src/shove.rs`**:
  - Added `ShoveOptions::region_whole` (`true` by default; `Plough::Region` dispatches to `renegotiate_region` when `region_whole` is true and preserves legacy `renegotiate` when false).
  - Implemented `FixedWalls` per-pass fixed-copper `ObstacleMap` caching, `terminals_through` mouth-unblocking terminal discovery, cheap movable plane-via pricing (`STRAIGHT / 2`) in `find_soft_path` plus direct `force_lay` + `join_planes` fast path when a soft path only touches movable plane copper, `t.net` cut-stub unzipping at try start, closed-net duplicate/dangling stub cleanup (`clean_closed_stubs`), pinch-window corridor ripping (`pinch` within `reach + 0.35 mm`), `route_open(neg_nets)` before `join_planes`, guarded Stage 2 (`!initial_pairs` displaced filter + equal-weight repack fallback), two-stage `rip_up` repair (held `t.net` then unlocked strict-improvement pass), signal-first `score_open` on plane boards, `far_item` deduplication on undone tries, `net_reaches` tracking, mid-pass reach reset on strict improvement, and tie ping-pong caps.
  - Added unit test `region_whole_skips_duplicate_far_end_when_walled_and_frees_multi_net_pocket`.
- **`core/crates/ultraroute/src/lib.rs`**:
  - Added CLI flags `--shove-whole` (default **on**), `--no-shove-whole` (switches back to legacy `Plough::Region` behaviour for A/B testing), and `--negotiate-tight`.
  - Updated `shove_pass` to propagate `net_reaches` alongside `reaches`, track signal-only open counts on `--planes` boards, and double reaches when a tie pass makes no net signal-open progress.
  - Added unit test `shove_whole_defaults_on_and_can_be_disabled`.

`cargo test --release --workspace` passes all **155** tests (up from 147 baseline, with 8 new unit tests covering the added logic).

---

## Lab Experiments and Exact KiCad Results

All `easy2350` experiments ran `jobs/easy2350/input/easy2350.kicad_pcb` via `tools/lab.py` with `--threads 1 --timeout 240` and base `RECIPE`:
`--strip --negotiate --rip-up --tighten --prefanout --diff-pairs --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8 --planes In1.Cu,In2.Cu --cell 0.035 --prefanout-order round --shove-plough region --shove-reach 0.5`.

Because `reserve` caps bookings at `<= 8` cores and `lab.py` launches every run in an invocation concurrently, 12-run A/B sweeps across seeds 0-5 were executed as paired 6-core `lab.py` runs (`-s02` for seeds 0-2 and `-s35` for seeds 3-5), always pairing A and B in the same `lab.py run`.

Signal opens are reported by `sig.py` (KiCad `unconnected_items` minus `GND` and `+3V3`). On `easy2350`, the input board's own locked vendor copper has `{"items_not_allowed": 20}` DRC errors; **every single run across every experiment below reported `drc.errors = {"items_not_allowed": 20}` (0 `track_width` errors on `VBUS`, 0 clearance errors, 0 shorts)**.

### 1. `region2-01-s02` & `region2-01-s35`: Whole-Branch Flood-Fill Rip + Locked Prefanout (Failed)

- **A**: Legacy `RECIPE`.
- **B**: `RECIPE --shove-whole` with whole-branch flood-fill (`rip_movable_branches`), locked `fanout_laid` and `pair_laid`, `rounds = 1`, and `freed_by_tie`.

| Seed | A Run | A Signal Opens | A All Unconn | A Wall (s) | B Run | B Signal Opens | B All Unconn | B Wall (s) | B Open Signal Nets |
| :---: | :--- | :---: | :---: | :---: | :--- | :---: | :---: | :---: | :--- |
| 0 | `a-s0` | **0** | 1 | 206.68 | `b-s0` | **2** | 3 | 244.26 | `/QSPI_SD3`, `VBUS` |
| 1 | `a-s1` | **1** (`/GPIO27`) | 2 | 240.67 | `b-s1` | **2** | 3 | 242.27 | `/GPIO24`, `/QSPI_SD3` |
| 2 | `a-s2` | **1** (`/GPIO27`) | 2 | 240.44 | `b-s2` | **2** | 3 | 240.46 | `/GPIO24`, `/QSPI_SD3` |
| 3 | `a-s3` | **2** (`/GPIO23`, `VBUS`) | 3 | 243.42 | `b-s3` | **3** | 4 | 240.98 | `/GPIO5`, `/GPIO24`, `/QSPI_SD3` |
| 4 | `a-s4` | **2** (`/GPIO24`, `/QSPI_SD2`) | 3 | 241.49 | `b-s4` | **2** | 3 | 241.79 | `/GPIO24`, `/QSPI_SD0` |
| 5 | `a-s5` | **2** (`/GPIO27`, `/QSPI_SCLK`) | 3 | 240.47 | `b-s5` | **1** | 2 | 243.01 | `/QSPI_SD3` |
| **Total** | | **8** | 14 | | | **12** | 18 | | |

- **Why it failed**: Flood-filling entire branches from pads tore up cross-board QSPI/GPIO routes 25 mm away into `U5`, turning a local 4-net pocket into a 15-net board-wide scramble, while locking `fanout_laid` froze `U1-56` (`/QSPI_SD2`) directly in front of unescaped `U1-55` (`/QSPI_SD3`). Reverted `prefanout.rs` and whole-branch flood-fill immediately.

### 2. `region2-02-s02` & `region2-02-s35`: Surgical Local Rip + Tight 3D Trace Zones + Plane Rejoin + Caching

- **A**: Legacy `RECIPE` (with 4-entry `stamp_outline` cache).
- **B**: `RECIPE --shove-whole` with surgical local corridor rip, `tight_zones` on 3D trace/via-trace discs, cluster pad terminals (when `pads.len() <= 4`), `best_paths` restoration, `join_planes` inside `renegotiate_region`, and `FixedWalls` caching.

| Seed | A Run | A Signal Opens | A All Unconn | A Wall (s) | B Run | B Signal Opens | B All Unconn | B Wall (s) | B Open Signal Nets |
| :---: | :--- | :---: | :---: | :---: | :--- | :---: | :---: | :---: | :--- |
| 0 | `a-s0` | **0** | 1 | 198.40 | `b-s0` | **0** | 1 | 122.57 | - |
| 1 | `a-s1` | **1** (`/GPIO27`) | 2 | 240.90 | `b-s1` | **0** | 1 | 183.74 | - |
| 2 | `a-s2` | **0** | 1 | 240.37 | `b-s2` | **1** | 2 | 244.23 | `/GPIO0` |
| 3 | `a-s3` | **2** (`/GPIO23`, `VBUS`) | 3 | 241.54 | `b-s3` | **1** | 2 | 242.61 | `/GPIO29` |
| 4 | `a-s4` | **2** (`/GPIO25`, `/QSPI_SD2`) | 3 | 241.02 | `b-s4` | **0** | 1 | 226.71 | - |
| 5 | `a-s5` | **2** (`/GPIO27`, `/QSPI_SCLK`) | 3 | 240.69 | `b-s5` | **1** | 2 | 210.13 | `+1V1` |
| **Total** | | **7** | 13 | | | **3** | 9 | | |

### 3. `region2-03-s02` & `region2-03-s35`: Guarded Stage 2 + Plane-Aware `rip_up` + Reach Reset

- **A**: Legacy `RECIPE`.
- **B**: `RECIPE --shove-whole` adding guarded Stage 2 hotspot expansion, `region_repair` plane-via re-joining in `ripup.rs`, mid-pass reach reset on strict improvement, and reach doubling when a tie pass fails to reduce opens.

| Seed | A Run | A Signal Opens | A All Unconn | A Wall (s) | B Run | B Signal Opens | B All Unconn | B Wall (s) | B Open Signal Nets |
| :---: | :--- | :---: | :---: | :---: | :--- | :---: | :---: | :---: | :--- |
| 0 | `a-s0` | **0** | 1 | 183.01 | `b-s0` | **0** | 1 | 97.94 | - |
| 1 | `a-s1` | **1** (`/GPIO27`) | 2 | 240.20 | `b-s1` | **0** | 1 | 147.91 | - |
| 2 | `a-s2` | **0** | 1 | 240.31 | `b-s2` | **1** | 2 | 242.89 | `Net-(R7-Pad2)` |
| 3 | `a-s3` | **2** (`/GPIO23`, `VBUS`) | 3 | 241.50 | `b-s3` | **1** | 2 | 240.66 | `/GPIO26` |
| 4 | `a-s4` | **2** (`/GPIO24`, `/QSPI_SD2`) | 3 | 240.17 | `b-s4` | **0** | 1 | 105.30 | - |
| 5 | `a-s5` | **2** (`/GPIO28`, `/QSPI_SCLK`) | 3 | 240.66 | `b-s5` | **1** | 2 | 240.93 | `VBUS` |
| **Total** | | **7** | 13 | | | **3** | 9 | | |

### 4. `region2-04-s02` & `region2-04-s35`: Symmetric 2D Via Zones + Multi-Cluster MST + Pinch-Window Rip + `t.net` Cut-Stub Unzip + Cheap Plane-Via Eviction + Two-Stage `rip_up`

- **A**: Legacy `RECIPE` (`region_whole = false`).
- **B**: `RECIPE --shove-whole` with the complete set of fixes.

| Seed | A Run | A Signal Opens | A All Unconn | A Viol | A Wall (s) | B Run | B Signal Opens | B All Unconn | B Viol | B Wall (s) |
| :---: | :--- | :---: | :---: | :---: | :---: | :--- | :---: | :---: | :---: | :---: |
| 0 | `a-s0` (`Net-(R7-Pad2)`) | **1** | 2 | 87 | 240.22 | `b-s0` | **0** | 1 (`+3V3`) | 86 | **103.80** |
| 1 | `a-s1` (`/GPIO27`) | **1** | 2 | 88 | 240.37 | `b-s1` | **0** | 1 (`+3V3`) | 86 | **36.34** |
| 2 | `a-s2` (`/QSPI_SD0`) | **1** | 2 | 86 | 240.24 | `b-s2` | **0** | 1 (`+3V3`) | 87 | **81.75** |
| 3 | `a-s3` (`/GPIO23`, `VBUS`) | **2** | 3 | 86 | 241.30 | `b-s3` | **0** | 1 (`+3V3`) | 86 | **229.87** |
| 4 | `a-s4` (`/GPIO0`) | **1** | 2 | 86 | 242.25 | `b-s4` | **0** | 1 (`+3V3`) | 86 | **111.48** |
| 5 | `a-s5` (`/GPIO25`, `/GPIO27`, `/QSPI_SCLK`) | **3** | 4 | 86 | 241.59 | `b-s5` | **0** | 1 (`+3V3`) | 86 | **116.09** |
| **Total** | | **9** | 15 | | | | **0** | 6 | | |

Every seed 0-5 finished under the 240 s timeout with **0 signal opens**, `drc.errors = {"items_not_allowed": 20}`, and **0 `track_width` errors**.

### 5. `region2-05-s02` & `region2-05-s35`: First Default-Enabled Verification (Exposed Seed-3 Timeout Margin)

- **A**: `RECIPE --no-shove-whole`.
- **B**: Exact unmodified `RECIPE` (`shove_whole = true` by default).

| Seed | A Run | A Signal Opens | A All Unconn | A Wall (s) | B Run | B Signal Opens | B All Unconn | B Wall (s) | B Open Signal Nets |
| :---: | :--- | :---: | :---: | :---: | :--- | :---: | :---: | :---: | :--- |
| 0 | `a-s0` (`Net-(R7-Pad2)`) | **1** | 2 | 241.52 | `b-s0` | **0** | 1 (`+3V3`) | 106.98 | - |
| 1 | `a-s1` (`/GPIO27`) | **1** | 2 | 240.49 | `b-s1` | **0** | 1 (`+3V3`) | 37.46 | - |
| 2 | `a-s2` (`/QSPI_SD2`) | **1** | 2 | 240.47 | `b-s2` | **0** | 1 (`+3V3`) | 84.71 | - |
| 3 | `a-s3` (`/GPIO23`, `VBUS`) | **2** | 3 | 240.69 | `b-s3` | **1** | 2 | 241.19 | `/GPIO1` |
| 4 | `a-s4` (`/GPIO0`) | **1** | 2 | 240.18 | `b-s4` | **0** | 1 (`+3V3`) | 120.78 | - |
| 5 | `a-s5` (`/GPIO22`, `/GPIO26`, `/QSPI_SCLK`) | **3** | 4 | 240.62 | `b-s5` | **0** | 1 (`+3V3`) | 112.10 | - |
| **Total** | | **9** | 15 | | | **1** | 7 | | |

- **What `region2-05` caught**: Under heavier shared-machine CPU load, `b-s3` (which solved in `229.87 s` in `region2-04`) reached the winning `/GPIO1` try at `224.50 s` and finished it at `241.13 s`, `1.13 s` past the `240 s` deadline (causing `!out_of_time()` to reject the try).

### 6. `region2-06-s02` & `region2-06-s35`: Skipping Pre-Existing Open Airlines in `rip_up` + Immediate Tie Reach Doubling (Rejected)

- **A**: `RECIPE --no-shove-whole`.
- **B**: Exact `RECIPE` with `RipOptions::skip_airlines` (skipping pre-existing open airlines on nets untouched by a shove try) and immediate reach doubling on kept ties.

| Seed | A Run | A Signal Opens | A All Unconn | A Wall (s) | B Run | B Signal Opens | B All Unconn | B Wall (s) | B Open Signal Nets |
| :---: | :--- | :---: | :---: | :---: | :--- | :---: | :---: | :---: | :--- |
| 0 | `a-s0` (`Net-(R7-Pad2)`) | **1** | 2 | 240.33 | `b-s0` | **0** | 1 (`+3V3`) | 133.83 | - |
| 1 | `a-s1` (`/GPIO27`) | **1** | 2 | 240.43 | `b-s1` | **0** | 1 (`+3V3`) | 65.25 | - |
| 2 | `a-s2` (`/QSPI_SD2`) | **1** | 2 | 240.29 | `b-s2` | **0** | 1 (`+3V3`) | 129.66 | - |
| 3 | `a-s3` (`/GPIO23`, `VBUS`) | **2** | 3 | 246.44 | `b-s3` | **0** | 1 (`+3V3`) | 122.81 | - |
| 4 | `a-s4` (`/GPIO0`) | **1** | 2 | 240.40 | `b-s4` | **1** | 2 | 241.12 | `VBUS` |
| 5 | `a-s5` (`/GPIO27`, `VBUS`) | **2** | 3 | 241.41 | `b-s5` | **0** | 1 (`+3V3`) | 206.66 | - |
| **Total** | | **8** | 14 | | | **1** | 7 | | |

- **Why `region2-06` was reverted**: Skipping pre-existing open airlines in post-try `rip_up` halved `b-s3`'s runtime (`122.81 s`), but on `b-s4` at `41.9 s` (`/GPIO25`), moving two adjacent nets had freed room for a nearby pre-existing open airline (`/GPIO3`); skipping `/GPIO3` degraded `/GPIO25` from `15 -> 14 open` to a `15 -> 15` tie, sending `b-s4` into a `VBUS <-> Net-(R7-Pad2)` loop.
- **Zero-behaviour-change fix (`region2-07`)**: Reverted `skip_airlines` and `tie_at_reach` (restoring the exact `region2-04` routing trajectory that solves all 6 seeds) and instead eliminated the pure CPU bottleneck: every `ObstacleMap` build in `route_negotiated`, `route_open`, and `rip_up` was re-rasterising full-board `In1.Cu` and `In2.Cu` plane polygons plus ~300 pads from scratch. Because cell claims (`FREE < net < SEVERAL`) form a commutative semilattice, caching static board geometry (`STATIC_CACHE` in `ObstacleMap::without`) produces cell-for-cell identical `ObstacleMap`s at a fraction of the CPU time.

### 7. `region2-07-s02` & `region2-07-s35`: Final Verification on Exact Unmodified `RECIPE`

- **A**: `RECIPE --no-shove-whole`.
- **B**: Exact unmodified `RECIPE` (`shove_whole = true` by default + `STATIC_CACHE` static-geometry caching).

| Seed | A Run | A Signal Opens | A All Unconn | A Viol | A Wall (s) | B Run | B Signal Opens | B All Unconn | B Viol | B Wall (s) |
| :---: | :--- | :---: | :---: | :---: | :---: | :--- | :---: | :---: | :---: | :---: |
| 0 | `a-s0` (`Net-(R7-Pad2)`) | **1** | 2 | 87 | 242.53 | `b-s0` | **0** | 1 (`+3V3`) | 86 | **114.65** |
| 1 | `a-s1` (`/GPIO27`) | **1** | 2 | 88 | 237.80 | `b-s1` | **0** | 1 (`+3V3`) | 86 | **46.61** |
| 2 | `a-s2` (`/QSPI_SD2`) | **1** | 2 | 86 | 240.34 | `b-s2` | **0** | 1 (`+3V3`) | 87 | **83.63** |
| 3 | `a-s3` (`/GPIO23`, `VBUS`) | **2** | 3 | 86 | 240.17 | `b-s3` | **0** | 1 (`+3V3`) | 86 | **185.35** |
| 4 | `a-s4` (`/GPIO1`) | **1** | 2 | 86 | 240.24 | `b-s4` | **0** | 1 (`+3V3`) | 86 | **117.57** |
| 5 | `a-s5` (`+1V1`, `/GPIO27`) | **2** | 3 | 86 | 240.63 | `b-s5` | **0** | 1 (`+3V3`) | 86 | **119.09** |
| **Total** | | **8** | 14 | | | | **0** | 6 | | |

Every seed 0-5 (`b-s0..b-s5`) on the unmodified `RECIPE` achieved **0 signal opens** well within the 240 s timeout (max wall time `185.35 s` on seed 3), with `drc.errors = {"items_not_allowed": 20}` (the board's own 20 vendor-copper items) and **0 `track_width`, clearance, or shorting errors**.

### 8. Regression Checks on Other Boards (`region2-earring`, `region2-earring-final`, `region2-bluebuzz`, `region2-bluebuzz-final`)

#### `region2-earring` & `region2-earring-final`: `jobs/earring-kicad-locked/input/earring.kicad_pcb` (Seeds 0-2)
Recipe: `--negotiate --rip-up --nudge --tighten --strip --prefanout --diff-pairs --shove-plough region --shove-reach 0.5 --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8` (`--threads 1 --timeout 240`).
- **A**: `RECIPE --no-shove-whole`
- **B**: Exact unmodified `RECIPE` (new default `shove_whole = true`)

| Experiment | Seed | A Run | A Unconnected | A DRC Errors | A Wall (s) | B Run | B Unconnected | B DRC Errors | B Wall (s) |
| :--- | :---: | :--- | :---: | :---: | :---: | :--- | :---: | :---: | :---: |
| `region2-earring` | 0 | `a-s0` | **0** | `{}` | 33.13 | `b-s0` | **0** | `{}` | 33.19 |
| `region2-earring` | 1 | `a-s1` | **0** | `{}` | 32.61 | `b-s1` | **0** | `{}` | 32.69 |
| `region2-earring` | 2 | `a-s2` | **0** | `{}` | 29.92 | `b-s2` | **0** | `{}` | 30.39 |
| `region2-earring-final` | 0 | `a-s0` | **0** | `{}` | 37.29 | `b-s0` | **0** | `{}` | 39.96 |
| `region2-earring-final` | 1 | `a-s1` | **0** | `{}` | 35.85 | `b-s1` | **0** | `{}` | 36.78 |
| `region2-earring-final` | 2 | `a-s2` | **0** | `{}` | 33.49 | `b-s2` | **0** | `{}` | 33.41 |

#### `region2-bluebuzz` & `region2-bluebuzz-final`: `jobs/bluebuzz-kicad/input/bluebuzz-rm2.kicad_pcb` (Seeds 0-2)
Recipe: `--negotiate --rip-up --nudge --tighten --strip-all --prefanout --diff-pairs --shove-plough region --shove-reach 0.5 --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8` (`--threads 1 --timeout 240`).
- **A**: `RECIPE --no-shove-whole`
- **B**: Exact unmodified `RECIPE` (new default `shove_whole = true`)

| Experiment | Seed | A Run | A Unconnected | A DRC Errors | A Wall (s) | B Run | B Unconnected | B DRC Errors | B Wall (s) |
| :--- | :---: | :--- | :---: | :---: | :---: | :--- | :---: | :---: | :---: |
| `region2-bluebuzz` | 0 | `a-s0` | **0** | `{}` | 54.22 | `b-s0` | **0** | `{}` | 54.24 |
| `region2-bluebuzz` | 1 | `a-s1` | **0** | `{}` | 51.88 | `b-s1` | **0** | `{}` | 51.32 |
| `region2-bluebuzz` | 2 | `a-s2` | **0** | `{}` | 57.56 | `b-s2` | **0** | `{}` | 57.73 |
| `region2-bluebuzz-final` | 0 | `a-s0` | **0** | `{}` | 97.90 | `b-s0` | **0** | `{}` | 97.87 |
| `region2-bluebuzz-final` | 1 | `a-s1` | **0** | `{}` | 88.49 | `b-s1` | **0** | `{}` | 88.83 |
| `region2-bluebuzz-final` | 2 | `a-s2` | **0** | `{}` | 100.44 | `b-s2` | **0** | `{}` | 99.91 |

---

## Summary of What Worked vs What Did Not

### What Worked
1. **Symmetric 2D via congestion zones (`via_zone_discs` + `route_astar_via`) and tight 3D zones**: Pricing `(trace -> via)` and `(via -> via)` collisions directly on layer changes eliminated silent commit failures where two vias or a via and a trace collided despite `colliding = 0` in 3D trace zones.
2. **Pinch-window corridor ripping**: Restricting Stage 1's rip along `t.net`'s soft path to spans within `reach + 0.35 mm` of actual soft-copper `pinch` points stopped cross-board nets (`Net-(R7-Pad2)` on seed 2) from ripping 15-28 unrelated parallel bus tracks.
3. **Unzipping `t.net`'s own dangling cut stubs at try start + closed-net stub cleanup**: Pruning `t.net`'s dangling soft tails before `find_soft_path` stopped nets cut in an earlier tie (`+1V1` / `VBUS` on seed 5, `/GPIO27` / `/GPIO28` on seed 3) from stepping right back across their displacer between frozen cut tips, while `clean_closed_stubs` cleared 90-140 ghost stubs during routing rather than after routing.
4. **Multi-component MST rebinding (`other_cluster_terminals`) + nearest-4 pad terminals**: Allowed multi-cluster nets with a walled/vendor-locked island (`+1V1` on `easy2350`) to bridge reachable clusters directly, and gave multi-pin power/signal nets pad-level detour terminals instead of forcing reconnection at a congested cut tip.
5. **Cheap movable plane-via eviction (`STRAIGHT / 2`) + `route_open` before `join_planes` + signal-first `score_open`**: Allowed trapped signals (`/GPIO28` on seed 3) to evict opportunistic movable outer-pad plane vias (`U1-43 +3V3`) cleanly without inflating open counts or letting plane vias steal signal lanes before `route_open` finished.
6. **Two-stage `rip_up` repair (held `t.net` then unlocked equal-weight negotiated `rip_up`)**: Let post-try repair resolve 2-net bottleneck swaps cleanly with equal PathFinder weights when holding `t.net` fixed could not fit both nets.
7. **Static-geometry semilattice caching (`STATIC_CACHE` in `ObstacleMap::without`)**: Caching rasterised outlines, pads, PTH holes, pours/planes, and keepouts across `ObstacleMap` builds preserved 100% cell-for-cell identical grids while removing repeated full-board plane-polygon rasterisation from the shove loop, giving seed 3 ample timeout margin under shared CPU load (`185.35 s` vs `240 s`).

### What Did Not Work and Why
1. **Whole-branch flood-fill ripping (`region2-01`)**: Ripping whole connected soft branches out to pads/junctions tore up clean cross-board QSPI/GPIO trunks 25 mm away and degraded signal opens from 8 to 12. Surgical pinch-window ripping combined with `cluster_terminals` gave PathFinder pad-escape freedom without destroying distant routing.
2. **Locking `prefanout` escapes (`region2-01`)**: Freezing `fanout_laid` prevented neighbour escapes (`U1-56`) from shifting half a lane to let an unescaped adjacent pad (`U1-55`) out.
3. **Pruning dangling stubs on *neighbour* ripped nets inside a union bounding box**: Tested during diagnosis on seed 2; when a long path ripped two spots 15 mm apart, the union bounding box spanned the middle of the board and stripped intact neighbour trunks (`50 -> 107` ripped pieces). Unzipping *only* `[t.net]`'s own dangling cut stubs before `find_soft_path` plus pruning *closed* nets (`skip_nets = open_nets`) avoided all collateral damage.
4. **Skipping pre-existing open airlines in post-try `rip_up` (`region2-06`)**: Skipping pre-existing open airlines on untouched nets halved seed 3's runtime (`122.81 s`), but on seed 4 prevented `rip_up` from opportunistically closing a nearby open airline (`/GPIO3`) whose corridor had just been freed by shifting two neighbours (`1` signal open on `b-s4`). Speeding up `ObstacleMap::without` via `STATIC_CACHE` achieved the runtime margin with zero behavioural change.

---

## What to Try Next

1. **Multi-pin Steiner tree / dynamic MST in `connectivity_without_pours`**: Currently `route_open` and `route_negotiated` recover from Euclidean MST edges touching walled islands via `other_cluster_terminals`. Computing obstacle-aware cluster distances when forming airlines in `connect.rs` would avoid generating walled MST edges in the first place.
2. **Soft-copper push/shove on `0.025 mm` local subgrids**: `renegotiate_region` runs on the board's `cell` (`0.035 mm`). Building a local `0.020-0.025 mm` subgrid purely inside the `pinch` bounding box (typically `3 x 3 mm`) would make fine-pitch QFN/BGA escape mouths exact to half a micrometre at negligible CPU cost.
3. **Opportunistic outer-pour awareness for plane pads**: `join_planes` drops vias on every non-plane pad of a plane net before routing, then `shove` evicts the ones blocking signals. Deferring opportunistic `join_planes` vias on pads that already sit inside an outer same-net pour outline until after signal routing would keep fine-pitch escape channels clear from round 1.
