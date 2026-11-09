# Squaring Tracks Left Slanted Without Doglegs or Slots

## Summary

Commit `2f44840` (`feat/squaring-2`) added slot (`leaves_slot`, `slants_into`) and dogleg (`doglegs`) checks to `grid::octilinear_watch`. On the 28-board squaring set (`sq-base1` `0b6c76c` -> `sq-new1` `2f44840`), those checks cut acid-trap slots from `1486` to `927` and doglegs from `18.842 /m` (`1153`) to `4.137 /m` (`250`), at the cost of raising `off45` from `19.18%` (`11739.2 mm`) to `27.82%` (`16811.7 mm`).

This worktree lowers `off45` below the pre-check `0b6c76c` level while also reducing both `slots` and `doglegs/m` further below `2f44840`, with `lost = 0`, `0` boards gaining DRC error types, and `0` time-limited squaring or tightening runs:

| Benchmark | Run (`sq-2f44840` -> worktree) | Clean | Lost / Won | Necks | Slots | Acute | Doglegs (`Dog/m`) | `Off45%` (`Off45_mm`) | Wirelength (`mm`) | Vias | Total CPU (`s`) | `Sq_s` (`log.txt`, `s`) | Bent |
| :--- | :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| **10-board sample** | `sqpad-s10-base` -> `sqpad-s10-m4` | `10/10 -> 10/10` | `0 / 0` | `1 -> 1` | `304 -> 219` (`-85`) | `39 -> 33` | `78 -> 57` (`2.710 -> 1.923`) | `31.02% -> 16.27%` (`8930.4 -> 4823.8`) | `28786 -> 29647` (`+2.99%`) | `741 -> 737` | `182.4 -> 177.9` | `5.5 -> 10.9` | `2301 -> 3148` |
| **28-board squaring set** | `sqpad-28-base` -> `sqpad-28-final` | `28/28 -> 28/28` | `0 / 0` | `2 -> 1` | `928 -> 790` (`-138`) | `96 -> 85` | `251 -> 158` (`4.156 -> 2.534`) | `27.76% -> 16.80%` (`16765.5 -> 10477.3`) | `60390 -> 62358` (`+3.26%`) | `2265 -> 2276` | `499.7 -> 536.6` | `8.7 -> 23.2` | `4616 -> 5990` |
| **48-board `--tier smoke`** | `sqpad-smoke-base` -> `sqpad-smoke-final` | `48/48 -> 48/48` | `0 / 0` | `2 -> 2` | `251 -> 225` (`-26`) | `22 -> 21` | `33 -> 24` (`1.802 -> 1.256`) | `28.98% -> 17.07%` (`5307.4 -> 3262.7`) | `18314 -> 19114` (`+4.37%`) | `462 -> 456` | `73.6 -> 84.2` | `1.4 -> 5.0` | `922 -> 1350` |

Prominent costs and trade-offs:
- **Stage CPU cost**: `octilinear_watch` runs twice in the default recipe (at the end of `tightening` before `vias`, and as the `squaring` stage after `vias`). On the 10-board `--report` timing run, `squaring` stage CPU rose from `4.219 s` to `11.287 s` (`0.422 -> 1.129 s/board`, `+0.707 s/board`) and `tightening` stage CPU rose from `8.419 s` to `17.536 s` (`0.842 -> 1.754 s/board`, `+0.912 s/board`). On the 28-board A/B, total router CPU rose from `499.7 s` to `536.6 s` (`+36.9 s`, `+1.318 s/board`; `log.txt` `squaring` stage `8.7 -> 23.2 s`, `+0.518 s/board`). On the 48-board `--tier smoke` A/B, total router CPU rose from `73.6 s` to `84.2 s` (`+10.6 s`, `+0.221 s/board`; `log.txt` `squaring` stage `1.4 -> 5.0 s`, `+0.075 s/board`). Zero boards hit a deadline in `tightening` or `squaring` on any run.
- **Wirelength and turns**: replacing Euclidean taut chords with 45-degree knees, Zs, and 3-piece square pad exits raises wirelength by `+2.99%` on the 10-board sample (`detour` `0.992 -> 1.022`), `+3.26%` on the 28-board set (`detour` `0.980 -> 1.012`, `turns /10 mm` `0.822 -> 1.064`), and `+4.37%` on `--tier smoke` (`detour` `0.969 -> 1.011`, `turns /10 mm` `0.724 -> 0.991`).
- **Per-board slot outliers**: while net slots fell on all three benchmarks (`-85` on 10-board with `more on 0`, `-138` on 28-board, `-26` on smoke), 3 of 28 boards (`newer-motor-controllers_busparts` `17 -> 25`, `newer-motor-controllers_si31-3` `19 -> 27`, `4_digit_hex_display_4_digit_display` `7 -> 8`) and 6 of 48 smoke boards (`ISO-port_ch340-usb-serial-isolated` `6 -> 12`, `PDB_OSD_HARDWARE_Quadcopter Power Board` `15 -> 18`, `Radar-System_fmcw_radar` `8 -> 10`, `kitspace_led_driver` `2 -> 3`, `light-painting-wand_light-wand` `7 -> 8`, `thegrid_pole_panellised` `6 -> 7`) had higher slot counts, analyzed below.

---

## 1. Phase 1 Measurement and Refusal Taxonomy

### Sample boards measured

We measured 10 boards spanning dense 2-layer matrices, 4-layer MCU/RF boards, through-hole headers, and fine-pitch SMD packages:
1. `1Bitsy_1bitsy`
2. `PICA_pica`
3. `front-end-modules_LimeSDR_Sony`
4. `FireMC_rados`
5. `Atreus_54percent_rev`
6. `OpenAVRc_Mega_2560 core mini_full_2.0`
7. `timecircuits-hardware_BTTF-TimeCircuits`
8. `BUF-DiffToSE-ADAU1966_BUF-DiffToSE-ADAU1966`
9. `Biscay_Blueeye_sipm`
10. `SPEX_HAB_Mainboard_Hardware_Power-no-mask-text`

On `sq-2f44840` (`sqpad-s10-base`), `tools/tidy.py` measured `8930.4 mm` of `off45` copper out of `28786 mm` total wirelength (`31.02%`). Excluding coupled differential-pair runs (`50.4 mm`, left alone on purpose so coupled spacing and length matching stay intact) and short post-squaring `mend_joints` / locked segments (`285.3 mm`), our instrumentation of `octilinear_round` recorded every candidate way and exact refusal reason across all `2770` non-pair `off45` chain segments (`8594.66 mm` total, saved in `runs/diag_sample/<ID>/diag.jsonl`, alongside `ULTRAROUTE_SQUARE_TRACE` logs in `runs/trace_check/`).

### Per-board non-pair `off45` length before changes (`sq-2f44840`)

| Board | Non-pair `off45` segs | Non-pair `off45` (`mm`) | Total `tidy.py` `off45` (`mm`) | `tidy.py` `Off45%` |
| :--- | :---: | :---: | :---: | :---: |
| `Atreus_54percent_rev` | `443` | `2892.35` | `2923.1` | `45.50%` |
| `timecircuits-hardware_BTTF-TimeCircuits` | `451` | `1316.10` | `1341.1` | `27.40%` |
| `FireMC_rados` | `317` | `920.46` | `943.3` | `26.00%` |
| `OpenAVRc_Mega_2560 core mini_full_2.0` | `281` | `700.95` | `731.5` | `21.80%` |
| `PICA_pica` | `203` | `616.95` | `659.2` | `23.10%` |
| `BUF-DiffToSE-ADAU1966_BUF-DiffToSE-ADAU1966` | `126` | `516.18` | `583.2` | `35.10%` |
| `Biscay_Blueeye_sipm` | `244` | `514.61` | `545.7` | `34.60%` |
| `front-end-modules_LimeSDR_Sony` | `368` | `474.25` | `491.9` | `26.90%` |
| `SPEX_HAB_Mainboard_Hardware_Power-no-mask-text` | `199` | `427.46` | `454.0` | `28.10%` |
| `1Bitsy_1bitsy` | `138` | `215.37` | `257.3` | `26.90%` |
| **Total (10 boards)** | **`2770`** | **`8594.66`** | **`8930.4`** | **`31.02%`** |

### Length-weighted refusal taxonomy (`8594.66 mm` non-pair `off45`)

Because `octilinear_round` tries up to 2 knees and 4 fixed-fraction Zs per `(s, e)` endpoint pair (plus single-end `slides` and single-end `pad_centre` in `2f44840`), a segment stays slanted only when **every** tried way across its spans is refused. Grouping each segment into a mutually exclusive primary bottleneck class (summing to `100.00%` of `8594.66 mm`):

| Primary refusal bottleneck class (pairs excluded) | Segments | Length (`mm`) | Share (`%`) | Mechanism in `2f44840` |
| :--- | :---: | :---: | :---: | :--- |
| **1. Pad-rim depth slot + rule/dogleg** (`slot_depth`) | `829` | `2846.80` | `33.12%` | One knee hits copper/keepout (`rule`); the other knee or Z enters a round/capsule pad off-centre (or both ends lie off-centre in pads, or foreign copper occupies a diode PTH pad's centre on `Atreus`) so `depth(m) < depth(centre)/2` rejects it as `slot`, while Zs fail `dogleg` or `short_z`. |
| **2. Long-pad rim slant + rule/dogleg** (`slants_into`) | `884` | `2072.62` | `24.12%` | Track ends in a rectangular or oval SMD pad in a pin row: the axial knee hits the neighbouring pin (`rule`), and the 45-degree knee crosses the pad's long side at 45 degrees (`slants_into` < `67.5 deg`), with no 3-piece square-exit candidate tried. |
| **3. Both knees fail `rule`; fixed Zs fail `dogleg` or `short_z`** | `327` | `1204.02` | `14.01%` | Both 2-piece knees hit obstacles; `octilinear_ways` only tries `0.5 / 0.25 / 0.75` along the straight and `0.5` along the diagonal, missing clear corridors near an end (`PICA` `VCC`) or off-midpoint diagonal splits whose straight middle exceeds `1.0 mm`. |
| **4. Every tried way hits `rule` or `off_edge`** | `84` | `1116.99` | `13.00%` | Tight corridors between switch holes, keepouts, board edges, or tightly packed parallel tracks (`BUF-DiffToSE` 8-track fan) where no single-net octilinear path fits without shoving neighbours. |
| **5. Same-net clearance slot + rule/dogleg** (`slot_gap`) | `393` | `813.15` | `9.46%` | Candidate turns just outside a same-net pad, track, or via within `0 < apart < clearance` (`16.09%` `own_pad`, `7.97%` `own_track`, `5.61%` `chain_track`, `1.96%` `own_via` non-exclusively). |
| **6. Margin / airline fallthrough** (`fits_after`) | `108` | `248.44` | `2.89%` | `paths.into_iter().find(...)` committed to the first way passing `try_way(..., keep=false)` (`+2 um` margin); when `try_way(..., keep=true)` or `open(board) <= before` rejected it, all remaining ways in `paths` were abandoned. |
| **7. Pre-existing or covered graze** (`graze`) | `124` | `178.67` | `2.08%` | `grazes(board, &pieces)` rejected a way because an unchanged endpoint already met a locked escape stub via overlapping round ends (`mend_joints` mends this on write) or a pad/via completely covered the overlap. |
| **8. Interior partner join lost** (`join`) | `21` | `113.97` | `1.33%` | Moving the span detaches an interior T-junction track not met by `sides`. |
| **9. Never tried** (`span > 8` / deadline) | `0` | `0.00` | `0.00%` | Every non-pair `off45` segment on the 10 boards was reached and had `>= 1` way evaluated (`FireMC_rados` only expired in `pairs`, never in `tightening` or `squaring`). |
| **Total non-pair `off45`** | **`2770`** | **`8594.66`** | **`100.00%`** | |

Endpoint distribution of those `8594.66 mm`: `64.62%` had at least one `pad_alone` end (`33.78%` `chain_bend..pad_alone`, `30.84%` `pad_alone..pad_alone`), `5.82%` had a `pad_shared` end (daisy-chained pad centres where two same-net tracks meet), and `20.00%` were `chain_bend..chain_bend`.

### Concrete trace examples (`ULTRAROUTE_SQUARE_TRACE`)

- **`1Bitsy_1bitsy` `/PA4` on `B.Cu` `(134.62, 45.72) -> (127.605, 39.495)` (`9.39 mm`, `3.4 deg` off 45, `1.8 mm` room between `/PA5` and `/PA3`)**:
  - At `(134.62, 45.72)`, pad `X3-3` is a horizontal through-hole oval (`1.6 x 2.75 mm`). At `(127.605, 39.495)`, `/PA4` meets a locked fanout stub starting `0.070 mm` away (`(127.535, 39.495)`), joined by overlapping round ends (`mend_joints` bridges it on write).
  - In `2f44840`, the knee going West first hit `/PA3`'s pad clearance (`rule`), the knee going NW from `(134.62, 45.72)` crossed `X3-3`'s horizontal long side at `45 deg` (`slants_into` -> `slot`), and even when a way arrived horizontally at `(127.605, 39.495)` (`180 deg` into the `(127.535, 39.495)` stub), `grazes()` saw the pre-existing `0.070 mm` round-end overlap at the unchanged end and refused it as `graze`!
  - With Move 2 (unchanged-end pre-existing graze check) + Move 3 (`pad_exit_ways` stepping West along `X3-3`'s major axis past its rim before turning NW, then West into `(127.605, 39.495)`), `/PA4` squares cleanly with `0 mm` of `off45`, `0` slots, and `0` doglegs.
- **`Atreus_54percent_rev` `Net-(D11-Pad1)` on `B.Cu` `(263.171, 242.775) -> (294.680, 216.660)` (`40.92 mm`, `5.3 deg` off 45)**:
  - Both ends lay off-centre in round switch/diode pads, and `Atreus`'s diode footprints route another net's track across the pad centre on the opposite/inner geometry; single-end moves left the unmoved end shallow (`slot`), while `pad_centre` hit `rule`. Two-end moves + radial rim exit check on `Shape::Capsule` squared `60.8%` of `Atreus`'s slanted length (`2923.1 -> 1145.1 mm`) while cutting its slots `40 -> 11`.
- **`PICA_pica` `VCC` (`16.41 mm`) and `BUF-DiffToSE` `GNDA` (`14.97 mm`)**:
  - Long tracks ending at shared daisy-chain SMD pad centres where fixed `0.25 / 0.5 / 0.75` Z splits hit obstacles `4 mm` out, while a clearance-clean corridor existed `1.01 mm` past the pad or via a square pad exit from the shared centre.

---

## 2. What Was Built

### Files and flags touched

- **Modified file**: `core/crates/grid/src/cleanup.rs` only (`+849 / -49` lines, including 5 new unit tests).
- **CLI flags changed or added**: none (`0` flags added; existing squaring behaviour in `octilinear_watch` only).
- **Files outside `cleanup.rs` touched**: none.
- **Time slices**: zero `Deadline::seconds(..)` calls added; `OCTILINEAR_ROUNDS = 3` and `OCTILINEAR_SPAN = 8` are unchanged, and every new helper has a fixed small constant candidate cap per `(i, j)` span.

### Moves implemented in `core/crates/grid/src/cleanup.rs`

1. **Move 1: Two-end pad slides, two-end pad centres, and `paths` fallthrough (`two_slides`, `octilinear_round`)**:
   - Added `two_slides` (`<= 3` ways, only when `i == 0 && j == tracks.len()` and both ends lie alone in distinct pads), sliding both endpoints halfway onto a shared horizontal, vertical, or 45-degree line when each stays deep enough in its pad.
   - Added slide-at-one-end + `pad_centre`-at-other-end (`<= 6` ways) and `octilinear_ways(cs, ce)` between both pad centres when `s_pad` and `e_pad` both exist.
   - Replaced `paths.into_iter().find(...)` (which tested `try_way(..., false)` inside `.find()` and dropped all remaining candidates if `try_way(..., true)` or `open(board) <= before` failed) with a single loop that continues trying remaining candidates in `paths` when full-width rule check or net connectivity rejects one, lazily computing `open(board)` once per span.

2. **Move 2: Pre-existing unchanged-end grazes, covered grazes, and 2-point boundary dogleg window (`try_way`, `graze_covered`, `around`)**:
   - In `try_way`, when `grazes(board, &pieces)` reports a graze `g`:
     - `graze_covered` allows `g` if both rim points of the overlap interval lie completely inside a same-net `Pad`, `Via`, kept `Pour`, or an adjacent firmly joined track of the way/chain (no narrow neck exposed on the copper outline, e.g. `Biscay_Blueeye_sipm`'s `C18-2`).
     - If `g.from` is within track radius `r` of an unchanged span endpoint (`path[0] == points[i]` or `path[last] == points[j]`), `try_way` temporarily restores `removed` once (`old_gs`) and allows `g` only if the original track already grazed `g.other` at that endpoint with no greater overlap (`g.overlap + 1_000.0 >= og.overlap`). Any newly created mid-run graze is still refused.
   - Widened `around(path)` in `octilinear_round` from 1 neighbour point each side to 2 (`points[i.saturating_sub(2)..i]` and `points[j+1..(j+3)]`), because `doglegs` inspects 4-point (`3`-segment) windows and a new way's first piece can turn an existing `< 1 mm` neighbour segment at `i - 1` or `j` into the middle of a dogleg.

3. **Move 3 (`m3c`): Square pad-exit 3/4-piece ways and track/via slot guards (`pad_for_exit`, `pad_exits`, `pad_exit_ways`, `leaves_slot`, `track_leaves_slot`)**:
   - `pad_for_exit` finds a chain endpoint's pad centre `(c, moved_pad, pad)`: either `p` itself with `moved_pad = None` when `p` is already at the pad's bbox centre (preserving shared daisy-chain joints at `p` via `same_ends`), or `pad_centre` (`moved_pad = Some(pad)`) when `p` lies alone off-centre in the pad.
   - `pad_exits` tests the 8 octilinear rays from `c` not heading away from `toward`, filters out any ray that `slants_into` the pad rim, binary-searches `1..=5_000_000 nm` (`23` steps) for the shortest exit point clearing `pad` by `half_width + TAUT_MARGIN + clearance + 2 um` and `>= 2 * half_width + 2 um` from `c`, bumps past sub-width knee residuals, and keeps at most `3` nearest exits.
   - `pad_exit_ways` (run lazily in `wave == 2`, only at chain ends `i == 0` or `j == tracks.len()` after waves `0` and `1` fail) builds 3-piece single-end and 4-piece two-end ways (`<= 32` ways max), rejects collinear or acute (`< 90 deg`) bends including at chain neighbours `i - 1` and `j + 1`, and orders 135-degree bends ahead of 90-degree bends.
   - Strengthened `leaves_slot` in three ways so these multi-piece exits reduce slots rather than adding them:
     1. On `Shape::Capsule` pads/vias, an endpoint inside the capsule (deep enough or unchanged) whose segment exits radially (`!slants_into`) is accepted without requiring the segment line to pass within `depth(centre)/2` of an obstructed pad centre (`Atreus` diodes).
     2. Applied the radial/depth rim check to `Kind::Via` as well as `Kind::Pad`, preventing knees from bending at 45 degrees right on a via's circular land (`FireMC_rados` `+12V`, `1Bitsy` `/PC5`).
     3. Added `track_leaves_slot` when two same-net `Kind::Track` capsules overlap (`apart == 0.0`): refuses tangential side kisses away from unchanged ends and refuses any `< 85 deg` V-fork or X-crossing between tracks unless a same-net `Kind::Pad` covers both arms out to `crotch = (r1 + r2 + gap) / (2 * sin(theta / 2))` (`PICA` `/HE1_D`, `BUF-DiffToSE` `GNDA`).

4. **Move 4: Length-picked non-dogleg Z splits (`extra_z_ways`)**:
   - Tried lazily in `wave == 1` (after standard `octilinear_ways` in `wave == 0`, before 3/4-piece `pad_exit_ways` in `wave == 2`).
   - When the diagonal middle exceeds `DOGLEG` (`1.0 mm`), adds up to `8` symmetric length-picked and `1/8`-fraction offsets along the straight (`2*r + 2 um`, `1.01 mm`, `2.0 mm`, `st_len / 8` from either end), ordered from the ends inward so clear gaps right beside a pad are found even on `15-30 mm` runs.
   - When the straight middle exceeds `DOGLEG` (`1.0 mm`), adds up to `10` offsets along the diagonal (`1/4`, `3/4`, `1/8`, `7/8`, plus fixed distances from either end), since every diagonal split has the same `> 1.0 mm` straight middle and is guaranteed dogleg-free internally.

### Unit tests added (`core/crates/grid/src/cleanup.rs`)

Each test asserts `octilinear_watch` returns `1` (or `0` on negative cases), checks all resulting segments lie on 45-degree multiples, checks `board::check` has no violations, and checks `connectivity` has `0` airlines:
1. `a_track_with_both_ends_off_centre_in_its_pads_moves_both_ends_to_square` (both-pad-centres knee + `two_slides` single straight track).
2. `a_graze_already_at_an_unchanged_end_allows_squaring_while_a_new_graze_refuses_it` (pre-existing endpoint round-end graze acts; new mid-run pad graze returns `0`).
3. `a_slanted_track_into_a_long_pad_exits_square_to_its_side` (3-piece North-NE-East exit around adjacent row pin + round pad with obstructed centre exiting radially).
4. `a_45_degree_track_fork_is_refused_while_a_shared_pad_centre_still_exits_square` (45-degree track V-fork refused with `0` when 90-degree leg is blocked, acts with `1` when unblocked; shared daisy-chain pad centre exits square with `1`).
5. `length_picked_z_splits_thread_a_corridor_without_doglegs` (off-midpoint diagonal Z split when diagonal `< 1 mm` and straight `> 1 mm`; `1.01 mm` length-picked straight Z split on a `16 mm` run when `0.25 / 0.5 / 0.75` and both knees are blocked; asserts `doglegs == 0`).

---

## 3. Experimental Progression and A/B Results

### 3.1 Step-by-step progression on the 10-board sample

All runs route from `board.kicad_pcb --strip-all --recipe default --seed 0 --uuid-seed 0 --threads 1 --timeout 120 --cpu-clock` and are judged by KiCad 9 DRC (`refill_zones.py` + `kicad-cli pcb drc --severity-all`), `tools/dfm.py`, and `tools/tidy.py`.

| Run | Change | Clean | Lost / Won | Necks | Slots | Acute | Dogs | `Dog/m` | `Off45%` | `Off45_mm` | `WL_mm` | Vias | `CPU_s` (`time.txt`) | `Sq_s` (`log.txt`) | Bent |
| :--- | :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| `sqpad-s10-base` | `sq-2f44840` baseline | `10/10` | `0 / 0` | `1` | `304` | `39` | `78` | `2.710` | `31.02%` | `8930.4` | `28786` | `741` | `182.4` | `5.5` | `2301` |
| `sqpad-s10-m1` | + Move 1 (`two_slides`, two-end centres, fallthrough) | `10/10` | `0 / 0` | `1` | `293` | `38` | `79` | `2.721` | `28.51%` | `8278.3` | `29034` | `740` | `177.1` | `5.3` | `2410` |
| `sqpad-s10-m2` | + Move 2 (unchanged-end graze, `graze_covered`, 2-pt `around`) | `10/10` | `0 / 0` | `1` | `295` | `38` | `68` | `2.343` | `28.68%` | `8323.8` | `29026` | `741` | `179.8` | `4.2` | `2402` |
| `sqpad-s10-m3` | + Naive `pad_exit_ways` (before `track_leaves_slot` / `pad_for_exit` fix) | `10/10` | `0 / 0` | `1` | **`374`** | **`43`** | `63` | `2.127` | `16.76%` | `4965.2` | `29620` | `749` | `232.2` | `9.6` | `3145` |
| `sqpad-s10-m3b` | Tested requiring `pad_alone_at` at `c` (rejected, see Section 4) | `10/10` | `0 / 0` | `1` | `240` | `36` | `56` | `1.915` | `21.66%` | `6333.4` | `29239` | `738` | `166.4` | `6.0` | `2878` |
| `sqpad-s10-m3c` | + `pad_for_exit` at `p == c` + `track_leaves_slot` + `Kind::Via` rim check | `10/10` | `0 / 0` | `1` | `225` | `37` | `54` | `1.817` | `17.36%` | `5159.4` | `29723` | `739` | `168.9` | `7.0` | `3177` |
| `sqpad-s10-m4` | + Move 4 (`extra_z_ways` length-picked non-dogleg Z splits, **final**) | `10/10` | `0 / 0` | `1` | **`219`** | **`33`** | **`57`** | **`1.923`** | **`16.27%`** | **`4823.8`** | `29647` | `737` | `177.9` | `10.9` | `3148` |

### 3.2 Exact per-board stage CPU and metrics on the 10-board sample (`sqpad-s10-base` -> `sqpad-s10-m4` + `--report`)

Because `log.txt` prints a separate timestamp for `squaring` (`Sq_s`) while folding `tightening` into the pre-`vias` block, we also ran both binaries with `--report` on all 10 boards (`runs/reports/{base,final}/<ID>/report.json`) to measure exact CPU seconds in both `tightening` and `squaring`:

| Board | `Off45%` | `Off45_mm` | Slots | Dogs | Necks | `--report` `tightening` CPU (`s`) | `--report` `squaring` CPU (`s`) | `--report` Total CPU (`s`) | `ab` `Sq_s` (`log.txt`) | Bent |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| `1Bitsy_1bitsy` | `26.90 -> 22.00%` | `257.3 -> 215.4` | `27 -> 26` | `10 -> 6` | `0 -> 0` | `0.252 -> 0.336` | `0.099 -> 0.228` | `14.701 -> 16.312` | `0.1 -> 0.2` | `145 -> 160` |
| `Atreus_54percent_rev` | `45.50 -> 17.40%` | `2923.1 -> 1145.1` | `40 -> 11` | `1 -> 2` | `0 -> 0` | `1.451 -> 2.599` | `0.778 -> 2.135` | `23.221 -> 24.345` | `1.0 -> 1.7` | `322 -> 508` |
| `BUF-DiffToSE-ADAU1966_BUF-DiffToSE-ADAU1966` | `35.10 -> 18.40%` | `583.2 -> 320.6` | `7 -> 5` | `3 -> 1` | `0 -> 0` | `0.129 -> 0.288` | `0.067 -> 0.264` | `2.912 -> 3.486` | `0.1 -> 0.3` | `66 -> 136` |
| `Biscay_Blueeye_sipm` | `34.60 -> 21.30%` | `545.7 -> 356.2` | `28 -> 26` | `3 -> 3` | `1 -> 1` | `0.420 -> 1.011` | `0.173 -> 0.534` | `10.101 -> 11.917` | `0.2 -> 0.6` | `105 -> 157` |
| `FireMC_rados` | `26.00 -> 12.20%` | `943.3 -> 446.9` | `33 -> 25` | `10 -> 8` | `0 -> 0` | `0.691 -> 1.150` | `0.349 -> 0.838` | `38.699 -> 39.556` | `0.5 -> 0.8` | `291 -> 389` |
| `OpenAVRc_Mega_2560 core mini_full_2.0` | `21.80 -> 15.60%` | `731.5 -> 529.5` | `13 -> 8` | `26 -> 21` | `0 -> 0` | `2.839 -> 7.375` | `1.355 -> 3.688` | `23.999 -> 30.880` | `2.0 -> 3.8` | `397 -> 425` |
| `PICA_pica` | `23.10 -> 7.40%` | `659.2 -> 227.5` | `10 -> 2` | `1 -> 0` | `0 -> 0` | `0.823 -> 1.252` | `0.403 -> 1.068` | `8.172 -> 9.096` | `0.6 -> 1.1` | `202 -> 288` |
| `SPEX_HAB_Mainboard_Hardware_Power-no-mask-text` | `28.10 -> 16.00%` | `454.0 -> 271.0` | `38 -> 31` | `6 -> 4` | `0 -> 0` | `0.411 -> 0.880` | `0.205 -> 0.670` | `12.587 -> 12.179` | `0.2 -> 0.8` | `125 -> 189` |
| `front-end-modules_LimeSDR_Sony` | `26.90 -> 17.60%` | `491.9 -> 336.8` | `83 -> 63` | `12 -> 7` | `0 -> 0` | `0.652 -> 0.945` | `0.292 -> 1.000` | `17.497 -> 18.849` | `0.3 -> 0.7` | `279 -> 418` |
| `timecircuits-hardware_BTTF-TimeCircuits` | `27.40 -> 19.80%` | `1341.1 -> 974.8` | `25 -> 22` | `6 -> 5` | `0 -> 0` | `0.751 -> 1.699` | `0.496 -> 0.863` | `16.670 -> 18.472` | `0.5 -> 0.9` | `369 -> 478` |
| **Total (10 boards)** | **`31.02 -> 16.27%`** | **`8930.4 -> 4823.8`** | **`304 -> 219`** | **`78 -> 57`** | **`1 -> 1`** | **`8.419 -> 17.536`** | **`4.219 -> 11.287`** | **`168.557 -> 185.091`** | **`5.5 -> 10.9`** | **`2301 -> 3148`** |
| **Mean per board** | | | | | | **`0.842 -> 1.754`** | **`0.422 -> 1.129`** | **`16.856 -> 18.509`** | **`0.55 -> 1.09`** | |

Note on `OpenAVRc_Mega_2560 core mini_full_2.0`: it has the largest stage CPU increase (`tightening` `2.839 -> 7.375 s`, `squaring` `1.355 -> 3.688 s`), because it is a dense through-hole + SMD board (`425` tracks bent) where many spans fail waves `0` and `1` and evaluate wave `2` (`pad_exit_ways`). Even there, total CPU (`30.880 s`) stays well under its `120 s` budget (`time_limited = []`).

### 3.3 Full 28-board squaring set A/B (`sqpad-28-base` vs `sqpad-28-final`)

For historical reference on the same 28 boards, `sq-base1` (`main 0b6c76c`, before slot/dogleg checks) -> `sq-new1` (`2f44840`) had:
- `clean 28/28 -> 28/28`, `slots 1486 -> 927`, `acute 82 -> 96`, `doglegs 1153 -> 250` (`18.842 -> 4.137 /m`), `off45 19.18% -> 27.82%` (`11739.2 -> 16811.7 mm`), `wirelength 61192 -> 60424 mm`, `vias 2286 -> 2278`, `CPU 570.1 -> 558.8 s`, `Sq_s 10.6 -> 10.2 s`.
- (On the 315-board gate run `sqg-base` -> `sqg-2f44840` that landed on disk: `clean 274/315 -> 275/315`, `slots 5905 -> 3991`, `doglegs/m 11.490 -> 2.504`, `off45 19.19% -> 26.15%`.)

Running all 28 boards head-to-head against `~/Software/ultraroute-lab/bin/sq-2f44840/ultraroute` (`sqpad-28-base` -> `sqpad-28-final`):
- **Verdict**: `PASS: clean 28 -> 28, lost 0 (noise 0), won 0, boards gaining an error type 0, new router failures 0, referee failures 0`
- **Time-limited boards**: `2 -> 2` (`FireMC_rados` in `['pairs']`, `f4fc_f4fc` in `['prefanout', 'pairs']`; `0 -> 0` in `tightening` or `squaring`, `0 -> 0` at `>= timeout - 10 s`)
- **Necks**: `2 -> 1` (`f4fc_f4fc: 1 -> 0`, `Biscay_Blueeye_sipm: 1 -> 1`, `more on 0`)
- **Slots**: `928 -> 790` (`-138`, fewer on 24 boards, equal on 1, more on 3: `newer-motor-controllers_busparts` `17 -> 25`, `newer-motor-controllers_si31-3` `19 -> 27`, `4_digit_hex_display_4_digit_display` `7 -> 8`)
- **Acute**: `96 -> 85` (`-11`, more on 4: `Omega2-Berrydock_berrydock-mini` `4 -> 6`, `FireMC_rados` `5 -> 6`, `KiwiSDR_PCB_kiwi` `9 -> 10`, `OpenAVRc` `2 -> 3`)
- **Doglegs**: `251 -> 158`, `4.156 -> 2.534 /m` (`-39.0%`, tidier on 26 of 28 boards)
- **`Off45`**: `27.76% -> 16.80%` (`16765.5 -> 10477.3 mm`, `-6288.2 mm`, **tidier on all 28 of 28 boards**)
- **Other tidiness scores**: `bundled` `3.3% -> 3.3%` (tidier on 15); `bus_bundled` `6.7% -> 8.6%` (tidier on 4); `turns /10 mm` `0.822 -> 1.064` (tidier on 0); `detour` `0.980 -> 1.012` (tidier on 1)
- **Wirelength & vias**: `60390 -> 62358 mm` (`+3.26%`), `vias 2265 -> 2276` (`+11`, `+0.49%`)
- **CPU & memory**: `CPU_s` `499.7 -> 536.6 s` (`+36.9 s` total, `17.85 -> 19.16 s/board`), `Sq_s` (`log.txt`) `8.7 -> 23.2 s` (`0.311 -> 0.829 s/board`, `+0.518 s/board`), peak memory max `1034 MB` (`0` over `2 GB`).

Per-board 28-board breakdown (`sqpad-28-base` -> `sqpad-28-final`):

| Board | `Off45%` | `Off45_mm` | Slots | Dogs | Necks | `CPU_s` | `Sq_s` | Bent |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| `4_digit_hex_display_4_digit_display` | `16.00 -> 11.80%` | `207.9 -> 157.8` | `7 -> 8` | `8 -> 2` | `0 -> 0` | `10.8 -> 12.0` | `0.3 -> 1.2` | `117 -> 135` |
| `Atreus_54percent_rev` | `45.50 -> 17.40%` | `2923.1 -> 1145.1` | `40 -> 11` | `1 -> 2` | `0 -> 0` | `20.9 -> 25.9` | `0.9 -> 2.0` | `322 -> 508` |
| `BUF-DiffToSE-ADAU1966_BUF-DiffToSE-ADAU1966` | `35.10 -> 18.40%` | `583.2 -> 320.6` | `7 -> 5` | `3 -> 1` | `0 -> 0` | `2.8 -> 3.2` | `0.1 -> 0.3` | `66 -> 136` |
| `Biscay_Blueeye_sipm` | `34.60 -> 21.30%` | `545.7 -> 356.2` | `28 -> 26` | `3 -> 3` | `1 -> 1` | `9.2 -> 11.2` | `0.1 -> 0.6` | `105 -> 157` |
| `DoroidOscillo-Board_Android_Oscilloscope` | `27.80 -> 21.00%` | `420.8 -> 325.2` | `36 -> 33` | `6 -> 4` | `0 -> 0` | `12.5 -> 13.4` | `0.2 -> 0.4` | `111 -> 145` |
| `FireMC_rados` | `26.00 -> 12.20%` | `943.3 -> 446.9` | `33 -> 25` | `10 -> 8` | `0 -> 0` | `39.8 -> 41.4` | `0.5 -> 1.1` | `291 -> 389` |
| `Horticulture_Light_EQ_3-Band_3 channel led driver rev0.4` | `25.10 -> 17.30%` | `283.9 -> 199.0` | `204 -> 195` | `0 -> 0` | `0 -> 0` | `6.4 -> 7.6` | `0.2 -> 0.4` | `67 -> 89` |
| `KiwiSDR_PCB_kiwi` | `26.90 -> 22.20%` | `677.1 -> 563.1` | `48 -> 45` | `7 -> 3` | `0 -> 0` | `26.8 -> 29.3` | `0.3 -> 0.8` | `224 -> 216` |
| `MicroMaple_MicroMaple` | `20.70 -> 19.70%` | `303.9 -> 286.7` | `17 -> 13` | `10 -> 6` | `0 -> 0` | `28.5 -> 27.0` | `0.1 -> 0.5` | `127 -> 123` |
| `Omega2-Berrydock_berrydock-mini` | `37.50 -> 28.60%` | `484.1 -> 390.4` | `36 -> 31` | `5 -> 2` | `0 -> 0` | `12.3 -> 13.2` | `0.1 -> 0.3` | `90 -> 138` |
| `OpenAVRc_Mega_2560 core mini_full_2.0` | `21.80 -> 15.60%` | `731.5 -> 529.5` | `13 -> 8` | `26 -> 21` | `0 -> 0` | `24.5 -> 33.5` | `1.4 -> 4.5` | `397 -> 425` |
| `OpenHardwareExG_Shield_OpenHardwareExG_Shield` | `29.90 -> 26.30%` | `718.7 -> 639.4` | `37 -> 33` | `3 -> 2` | `0 -> 0` | `27.8 -> 27.3` | `0.3 -> 0.6` | `137 -> 168` |
| `PICA_pica` | `23.10 -> 7.40%` | `659.2 -> 227.5` | `10 -> 2` | `1 -> 0` | `0 -> 0` | `8.8 -> 10.1` | `0.5 -> 1.2` | `202 -> 288` |
| `PICA_rampzbackup` | `24.50 -> 10.70%` | `601.1 -> 274.5` | `5 -> 2` | `7 -> 4` | `0 -> 0` | `8.8 -> 11.6` | `0.4 -> 1.8` | `168 -> 209` |
| `PocketBone_pocketbone-kicad` | `26.30 -> 17.90%` | `343.3 -> 241.3` | `17 -> 15` | `30 -> 22` | `0 -> 0` | `10.7 -> 11.1` | `0.2 -> 0.3` | `129 -> 160` |
| `SPEX_HAB_Mainboard_Hardware_Power-no-mask-text` | `28.10 -> 16.00%` | `454.0 -> 271.0` | `38 -> 31` | `6 -> 4` | `0 -> 0` | `12.2 -> 13.2` | `0.2 -> 0.7` | `125 -> 189` |
| `Smart-Watch_SmartWatch` | `25.20 -> 16.80%` | `272.9 -> 195.7` | `25 -> 21` | `9 -> 6` | `0 -> 0` | `19.3 -> 19.4` | `0.2 -> 0.4` | `107 -> 140` |
| `TX5823_TX5823` | `31.30 -> 22.00%` | `361.2 -> 265.3` | `22 -> 17` | `4 -> 1` | `0 -> 0` | `18.7 -> 20.0` | `0.3 -> 0.8` | `100 -> 119` |
| `bt-tnc_bttnc2` | `26.80 -> 17.40%` | `351.9 -> 237.8` | `40 -> 35` | `5 -> 2` | `0 -> 0` | `6.5 -> 6.6` | `0.2 -> 0.3` | `87 -> 119` |
| `f4fc_f4fc` | `23.50 -> 21.10%` | `433.7 -> 401.1` | `58 -> 40` | `16 -> 11` | `1 -> 0` | `58.6 -> 59.8` | `0.3 -> 0.7` | `225 -> 238` |
| `front-end-modules_LimeSDR_Sony` | `26.90 -> 17.60%` | `491.9 -> 336.8` | `83 -> 63` | `12 -> 7` | `0 -> 0` | `19.3 -> 22.4` | `0.3 -> 0.9` | `279 -> 418` |
| `jadonk_PocketBone` | `30.00 -> 20.70%` | `390.2 -> 278.3` | `15 -> 13` | `21 -> 16` | `0 -> 0` | `10.5 -> 10.6` | `0.1 -> 0.2` | `121 -> 152` |
| `kitspace_T32_ref` | `16.80 -> 9.10%` | `301.2 -> 166.2` | `11 -> 11` | `14 -> 2` | `0 -> 0` | `24.0 -> 21.2` | `0.1 -> 0.3` | `162 -> 177` |
| `mobile-sensor-pcb_mobile-sensor-pcb` | `21.90 -> 12.00%` | `629.6 -> 358.6` | `18 -> 17` | `2 -> 0` | `0 -> 0` | `22.3 -> 22.6` | `0.4 -> 0.8` | `154 -> 207` |
| `newer-motor-controllers_busparts` | `20.00 -> 13.50%` | `455.4 -> 326.8` | `17 -> 25` | `2 -> 0` | `0 -> 0` | `14.2 -> 16.3` | `0.3 -> 0.4` | `101 -> 151` |
| `newer-motor-controllers_si31-3` | `22.40 -> 13.20%` | `505.1 -> 316.8` | `19 -> 27` | `4 -> 2` | `0 -> 0` | `16.3 -> 17.9` | `0.2 -> 0.5` | `102 -> 153` |
| `pocketbone-kicad_pocketbone-kicad` | `26.90 -> 18.20%` | `350.7 -> 244.9` | `19 -> 16` | `30 -> 22` | `0 -> 0` | `10.3 -> 10.0` | `0.1 -> 0.3` | `131 -> 163` |
| `timecircuits-hardware_BTTF-TimeCircuits` | `27.40 -> 19.80%` | `1341.1 -> 974.8` | `25 -> 22` | `6 -> 5` | `0 -> 0` | `16.9 -> 18.8` | `0.4 -> 0.9` | `369 -> 478` |

### 3.4 `--tier smoke` (48 boards) A/B (`sqpad-smoke-base` vs `sqpad-smoke-final`)

- **Verdict**: `PASS: clean 48 -> 48, lost 0 (noise 0), won 0, boards gaining an error type 0, new router failures 0, referee failures 0`
- **Time-limited boards**: `0 -> 0` (`untouched by any deadline in both: 48 boards`)
- **Necks**: `2 -> 2` (`more on 0`)
- **Slots**: `251 -> 225` (`-26`, `-10.4%`; more on 6: `ISO-port_ch340-usb-serial-isolated` `6 -> 12`, `PDB_OSD_HARDWARE_Quadcopter Power Board` `15 -> 18`, `Radar-System_fmcw_radar` `8 -> 10`, `kitspace_led_driver` `2 -> 3`, `light-painting-wand_light-wand` `7 -> 8`, `thegrid_pole_panellised` `6 -> 7`)
- **Acute**: `22 -> 21` (`-1`, more on 3 by `+1` each)
- **Doglegs**: `33 -> 24`, `1.802 -> 1.256 /m` (`-30.3%`, tidier on 19)
- **`Off45`**: `28.98% -> 17.07%` (`5307.4 -> 3262.7 mm`, `-38.5%`, **tidier on 46 of 48 boards**; the 2 small boards where `off45` nudged up slightly were `MySRaspiGW_MySRaspiGW_PA_LNA` `21.9 -> 23.4 mm` and `kitspace_PWR` `213.6 -> 224.8 mm`, caused by slightly different post-tightening via drops)
- **Other tidiness scores**: `bundled` `2.5% -> 2.4%` (tidier on 10); `bus_bundled` `3.0% -> 1.0%` (tidier on 0); `turns /10 mm` `0.724 -> 0.991` (tidier on 2); `detour` `0.969 -> 1.011` (tidier on 5)
- **Wirelength & vias**: `18314 -> 19114 mm` (`+4.37%`), `vias 462 -> 456` (`-6`)
- **CPU & memory**: `CPU_s` `73.6 -> 84.2 s` (`+10.6 s` across 48 boards, `1.533 -> 1.754 s/board`), `Sq_s` (`log.txt`) `1.4 -> 5.0 s` (`0.029 -> 0.104 s/board`, `+0.075 s/board`). On small boards where squaring had nothing new to bend (`PCB_constant_current_ac_hv` `0 -> 0` bent, `MySRaspiGW_Pimoroni` `0 -> 0` bent), `Sq_s` stayed `0.0 -> 0.0 s` (`+0.0 to +0.1 s` total CPU). Peak memory max `227 MB` (`0` over `2 GB`).

---

## 4. What Did Not Work and Why

1. **Naive `pad_exit_ways` without same-net track fork checks (`sqpad-s10-m3`)**:
   - In `sqpad-s10-m3`, `off45` plunged from `31.02%` to `16.76%`, and `doglegs` fell `78 -> 63`, *but* `slots` jumped `304 -> 374` (`+70` net: `+158` added, `-88` removed) and `acute` rose `39 -> 43`.
   - Root cause 1 (`106` of the `158` added slots): `octilinear_watch` runs first in `tightening` (`stages.rs:981`), before `vias`. In `m3`, `pad_at_end(p)` required `pad_alone_at(p)` even when `p` was already at the pad's centre `c`. During `tightening`, an off-centre track end slid to `c` (joining a second daisy-chain track already at `c`) via a 3-piece square pad exit. Next, `vias` dropped vias and `cleaning` ran `pull_tight`, collapsing that 3-piece exit into a slanted chord `(c -> e)` across the pad's long side (a slot). Then in `squaring`, `pad_alone_at(c)` returned `None` because two tracks now met at `c`, preventing `squaring` from re-squaring the exit even though `p == c` required zero endpoint movement!
   - Root cause 2 (`21` of the `158` added slots): `leaves_slot` returned `false` whenever `apart == 0.0` for `Kind::Track` and `Kind::Via`, allowing 45-degree track-to-track V-forks (`3165 um2` slots outside pads) and 45-degree bends right on circular via lands.
2. **Forbidding `pad_centre` onto occupied daisy-chain centres (`sqpad-s10-m3b`)**:
   - We tested requiring `pad_alone_at` at the destination pad centre `c` in `pad_centre`. That reduced slots (`304 -> 240`), *but* regressed `off45` by `+4.30` percentage points (`21.66%` / `6333.4 mm` vs `17.36%` / `5159.4 mm` in `m3c`) AND left more slots than `m3c` (`240` vs `225`!).
   - Why: daisy-chain nets (`OpenAVRc /Sck`, `BUF GNDA`, `PICA VCC`, `1Bitsy` header nets) legitimately meet at pad centres at 90 or 180 degrees. Keeping `pad_centre` legal while allowing `pad_for_exit` at `p == c` with `moved_pad = None` (so `same_ends` preserves all joints at `c`) and checking `< 85 deg` forks in `track_leaves_slot` recovered `1174 mm` of squaring and dropped slots further (`240 -> 225`, then `219` with Move 4).
3. **Pre-via `tightening -> vias -> pull_tight` chord slots on `newer-motor-controllers_{busparts,si31-3}`**:
   - Inspecting `newer-motor-controllers_busparts` (`slots 17 -> 25`) with `dfm.py --list` and `pcbnew` showed 6 of the 8 added slots sit on `/VBUS F.Cu` along long **un-squared slanted chords** (`(173.663, 49.714) -> (160.000, 44.491)` etc.) that `pull_tight` pulled straight across MOSFET busbar pads after `vias` dropped vias, which `octilinear_watch` then refused to bend. Because `pull_tight` does not check `leaves_slot`, pre-via `octilinear_watch` changing which vias drop can indirectly change where `pull_tight` pulls a slanted chord across same-net copper.

---

## 5. What Was Not Done or Verified

- **316-board `gate` tier and full 1182-board corpus**: we verified on the 10-board sample (`6` A/Bs + `--report`), the full 28-board `sq-new1` squaring regression set, and the 48-board `--tier smoke` set, plus `cargo clippy -p grid --all-targets -- -D warnings` (`0` warnings; `ultraroute` has 2 pre-existing `2f44840` clippy lints in `stages.rs:1324` and `lib.rs:498` left untouched outside `cleanup.rs`) and `cargo test --workspace --no-fail-fast --release`. We did not run a 316-board `gate` A/B because uncommitted worktree runs must execute locally (`--here` capped at 10 cores) under the 20-minute batch rule.
- **Multi-net / bus shove during squaring**: squaring still operates one chain at a time against fixed other-net copper.

---

## 6. What Remains Slanted and What to Try Next

Inspecting the longest remaining `off45` pieces on `sqpad-s10-m4` (`4823.8 mm` total) shows two dominant classes account for most of the gap between `~16.8%` and designers' `0.1%`:

1. **Parallel fans of taut slanted tracks (`BUF-DiffToSE-ADAU1966` `Net-(C329..C369)`, `8 x ~20 mm` tracks)**:
   - Eight single-ended tracks run side by side across a `19 mm` span at `3-20 deg`. Tried one by one, each track's knee hits its immediate neighbour (`Clearance:track`), so none can move first even across `OCTILINEAR_ROUNDS = 3`.
   - **Next move**: detect adjacent collinear/parallel slanted chains heading across the same channel and bend the outermost track first (from the free side of the bundle inward) or lay parallel stepped knees for the bundle in one transaction, which will also improve `bus_bundled`.
2. **Tight multi-obstacle chords (`Atreus_54percent_rev` `Net-(SW_12-PadSW1)` `46.1 mm`, `Net-(SW_14-PadSW1)` `32.3 mm`, `OpenAVRc` `/Sck` `29.8 mm`)**:
   - Long `chain_bend..chain_bend` chords threaded diagonally between staggered switch holes, rotated keycap keepouts, and board edges where every 2-piece knee and 3-piece Z hits an obstacle (`all_ways_rule_or_edge`), because an octilinear staircase needs `>= 4` pieces hugging intermediate obstacles.
   - **Next move**: split a long obstacle-locked chord at its closest clearance pinch point into two sub-segments before running `octilinear_ways` (`2` knees = `4` pieces hugging the intermediate pin/hole), bounded to `1` split per chord.
3. **Cut `tightening` stage CPU nearly in half**:
   - Currently `stages.rs:981` runs full `octilinear_watch` at the end of `tightening`, immediately before `vias` + `cleaning` (`pull_tight`), which tears up many of those bends anyway before `squaring` (`stages.rs:1229`) runs `octilinear_watch` again. Skipping waves `1` and `2` (`extra_z_ways` and `pad_exit_ways`) during pre-via `tightening` (running them only in the post-via `squaring` stage) should reclaim most of the `+0.91 s/board` `tightening` CPU increase while also avoiding pre-via via-drop perturbations like `newer-motor-controllers`'s `/VBUS`.

---

## For the project memory

- `octilinear_watch` (`core/crates/grid/src/cleanup.rs`) is called **twice** in the default recipe: once inside `tightening` (`stages.rs:981`, before `vias`) and once as the standalone `squaring` stage (`stages.rs:1229`, after `vias` and `cleaning`'s `pull_tight`).
  - Consequence 1 (timing): `log.txt` only prints a separate timestamp for `squaring`, folding the first `octilinear_watch` call's CPU time into the pre-`vias` block; always check `--report` (`stages` -> `tightening` and `squaring`) when measuring the full CPU cost of changes to `octilinear_watch`.
  - Consequence 2 (daisy-chain pad centres): a move in `tightening` can place a chain endpoint onto a pad's centre `c` where another same-net track already ends, and `cleaning`'s `pull_tight` can re-slant that exit across the pad's rim; in `squaring`, `pad_alone_at(c)` is then `None`, so any pad-exit helper that requires `pad_alone_at` even when `p == c` (`moved_pad = None`) will strand those tracks as slanted rim slots.
- All project memory files read at session start (`router-change-rules.md`, `completion-first-intent-scored.md`, `tidy-parallel-goal.md`, `memory-capped-runs.md`, `ultraroute-layout.md`, `pcbench-general-check.md`) matched the current codebase accurately.
