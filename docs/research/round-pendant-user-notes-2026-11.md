# UltraRoute User Experience & Engineering Notes (`round-pendant`, Nov 2026)

Compiled from intensive real-world usage of `ultraroute` (`vendor/ultraroute` @ `a3ba419`) across:
- **`round-pendant` Variants D & E (8-layer, 2 mm pitch sunflower matrix):** `235` RGB LEDs, `5× IS31FL3745` WLCSP-36, `RP2350A`, `879` vias, **`0` open, `0` DRC errors/warnings**.
- **`parallelogram` 4-layer low-cost density-limit family (`p1-66` .. `p4-264`, `25 × 70 mm`, `17.75°` skew, `1–4× AW20198QNR` QFN-40 `0.4 mm` pitch with solid `3.4 × 3.4 mm` GND EPs):** `475` to `1,329` connections on JLCPCB 4-layer standard rules (`0.10/0.10 mm` trace/space, `0.30/0.15 mm` via, `--planes In1.Cu`, `--cell 0.025`).

Repro inputs and raw post-router outputs are archived at `~/Software/ultraroute-lab/repro/round-pendant-parallelogram/`, and a copy of this report is placed in `~/Software/ultraroute/docs/research/round-pendant-user-notes-2026-11.md`.

---

## Executive Summary (User Experience)

- **Below ~85% physical channel capacity (`p1-66` on 4L, `round-pendant` D/E on 8L):** Connectivity is close to set-and-forget (`0` opens in ~3–4 minutes).
- **Near the routing knee (`85–99.8%` capacity, e.g., `p2-132` on 4L) and for strict KiCad 9 `--severity-all` DRC sign-off:** `ultraroute` is vastly faster and stronger than Freerouting DSN/SES, **but** shipping a 0-DRC production board currently required us to maintain two external wrapper scripts in `round-pendant` (which per `LESSONS.md` belong upstream in `ultraroute`):
  1. `scripts/parallelogram/route_variant.py` (3-stage orchestration: unobstructed critical-net pre-pass → negotiated seed search → `--adopt --shove-plough region` finisher).
  2. `scripts/parallelogram/clean_copper_4l.py` (post-route copper hygiene for KiCad `connection_width`, `copper_sliver`, off-center `.025`-grid via grazes, unmerged `25 µm` collinear sub-segments, and 1-layer dangling vias).

### What Worked Exceptionally Well
1. **`--blame` (`Outcome` stage, `stages.rs:848`):** Pinpointing exact blocking nets and cell counts (`open CS2_18: U2-12 to D122-4: routed copper of CS2_16, SW2_5, BTN (7 cells)` vs `fixed or end parts U1` vs `move (-0.095, +0.015) mm`) immediately separates placement/keepout impossibilities from congestion.
2. **`--adopt --shove-plough region` (`stages.rs:1047`):** Region-ploughing consistently broke fine-pitch QFN-40 escape logjams after negotiated rip-up plateaued (`7 → 2` open on `p2-132` XL-1010, `50 → 28` on `p3-162`, `74 → 56` on `p3-198`, `179 → 139` on `p4-264`).
3. **Native `.kicad_dru` named rule-area enforcement + `--planes` + `--prefanout-mode inner`:** Protecting solid QFN thermal pads (`A.intersectsArea('U1 solid EP') && A.NetName != 'GND'`, `disallow track via`) and `RP2350` `+1V1`/`+3V3` rings natively with zero DSN/SES round-trip loss.

### Empirical Results Summary (`ultraroute a3ba419` on `round-pendant` & `parallelogram`)

| Board / Variant | Stackup | LEDs & Package | Drivers & EPs | Total Conns | Stage 2 Open (`autoroute.py`) | Stage 3 Open (`--adopt` plough) | Vias (Final) | Post-`clean_copper` Strict DRC (`--severity-all`) |
|---|---|---|---|---:|---:|---:|---:|---|
| `round-pendant-e` | 8L VIP | `235× XL-1010RGB` (`1.0×1.0`) | `5× IS31FL3745` WLCSP (`0` driver EP) | `~1,020` | `0` | **`0` (`100.0%`)** | `879` | **`0` err, `0` warn, `0` `copper_audit`, `0` `pad_joints`** |
| `parallelogram p1-66` (old) | 4L | `66× XL-1010RGB` (`0.35 mm` gap) | `1× AW20198` (`3.4²` solid EP) | `475` | `0` | **`0` (`100.0%`)** | `467` | **`0` err, `0` warn** |
| **`parallelogram p1-66` (new)** | **4L** | **`66× XL-1615RGBC` (`0.80 mm` gap)** | `1× AW20198` (`3.4²` solid EP) | `475` | `1` | **`0` (`100.0%`, `2.5 s`)** | **`343` (`-26.6%`)** | **`0` err, `0` warn, `0` `copper_audit`, `0` `pad_joints`** |
| `parallelogram p2-132` (old) | 4L | `132× XL-1010RGB` (`0.35 mm` gap) | `2× AW20198` (`3.4²` solid EP) | `761` | `7` | `2` (`99.74%`) | `859` | `2` unconn (`U2.10, U2.12`) |
| **`parallelogram p2-132` (new)** | **4L** | **`132× XL-1615RGBC` (`0.80 mm` gap)** | `2× AW20198` (`3.4²` solid EP) | `761` | `1` (plane) | **`0` (`100.0%`, `6.2 s`)** | **`670` (`-22.0%`)** | **`0` err, `0` warn, `0` `copper_audit`, `0` `pad_joints`** |
| `parallelogram p3-162` | 4L | `162× XL-1010RGB` (`2.55×3.15` pitch) | `3× AW20198` (`3.4²` solid EP) | `901` | `53` | `31` (`96.56%`) | `793` | Column pitch `< 3.4 mm` solid EP shadows `F.Cu` |
| `parallelogram p3-198` | 4L | `198× XL-1010RGB` (`2.55×2.60` pitch) | `3× AW20198` (`3.4²` solid EP) | `1,045` | `74` | `56` (`94.64%`) | `955` | `4` opens left with `"nothing on the grid"` after deadline |
| `parallelogram p4-264` | 4L | `264× XL-1010RGB` (`2.02×2.60` pitch) | `4× AW20198` (`3.4²` solid EP) | `1,329` | `179` | `139` (`89.54%`) | `1,041` | Single `In2.Cu` signal layer saturated |

---

## High-Leverage Engineering Findings & Root Causes (Verified Against `a3ba419`)

### 1. Post-Deadline Stage Starvation: `Tightening`, `Vias`, and `Refinish` Bail Out on Expired `ctx.deadline()` (`stages.rs:679, 708, 792`)

**Observed symptoms across `p2-132`, `p3-162`, `p3-198`, `p4-264`:**
1. Whenever `RipUp` or `Shove` consumes `--timeout`, `Tightening` prints `pulled tight: 9328 -> 0 segments` and writes raw `25 µm` stair-stepped segments (`2,063` unmerged collinear segments on `p2-132`), and `Vias` drops `0` redundant vias.
2. Worse, on `p3-198`, `Outcome --blame` reported **4 open connections** with:
   > `nothing on the grid: a clear path exists`

**Exact root cause in `core/crates/ultraroute/src/stages.rs` (interaction with `REMEDIATION Stage 3` single deadline, `ROADMAP.md:22, 242`):**
- `REMEDIATION Stage 3` rightly introduced one wall-clock `Deadline` across the run so `--timeout 120` never runs for `648 s`. However, `Negotiate`, `RipUp`, and `Shove` are allowed to consume `100%` of `ctx.deadline()`, leaving `0 ms` for post-search finishing:
- `Tightening::run` (`stages.rs:679–681`) and `Vias::run` (`stages.rs:708–712`) pass `ctx.deadline()` — already expired when search hit `--timeout`.
- Immediately afterward, `Cleaning::run` (`stages.rs:731–768`) runs **unconditionally without a deadline** (`"whatever the time: what it leaves out would be written as it stands"`), pruning dangling copper and freeing blocked corridors.
- Then `Refinish::run` (`stages.rs:774–805`, specifically written to reconnect airlines freed by `Vias`/`Tightening`/`Cleaning`) passes `deadline: ctx.deadline()` at **`stages.rs:792`**! Because `ctx.deadline()` is already expired, `grid::route_open` aborts immediately without routing the newly clear airlines, and `Outcome` (`stages.rs:848`) then diagnoses them as `"nothing on the grid: a clear path exists"`.

**Proposed fix (while keeping `REMEDIATION Stage 3`'s hard wall-clock bound):**
- Partition `--timeout` into e.g. `search_deadline = start + 0.90 * timeout` (or `timeout - min(15s, 0.15 * timeout)`) for `Negotiate`/`RipUp`/`Shove`, reserving the final `10–15%` inside `ctx.deadline()` for `Tightening`, `Vias`, and post-`Cleaning` `Refinish` so no board is ever written with unmerged `25 µm` stair-steps or `"nothing on the grid: a clear path exists"`.

---

### 2. Twin Parallel Stems & Sibling Grazing Bypass `mend_joints` Because of `!firmly_linked(...)` (`cleanup.rs:325`)

**Observed symptom on `0-open` boards (`p1-66` & `round-pendant-d`):**
- Raw `ultraroute` output reported `0` narrow joints of its own, yet `kicad-cli pcb drc --severity-all` (`min_connection = 0.09 mm`, `copper_sliver = error`) flagged **`15–22` `connection_width` and `copper_sliver` errors**.

**Exact root cause in `core/crates/grid/src/cleanup.rs`:**
- At `cleanup.rs:325` (`narrow_joints`, and the corresponding logic in `mend_joints` / `mend_grazes`), narrow round-cap overlaps are skipped whenever `firmly_linked(board, id, other)` returns `true` (`cleanup.rs:334–336`: *"then a narrow overlap between them is a tip grazing copper already joined, not the joint that carries the net"*).
- **KiCad 9 DRC disagrees geometrically:** when two `0.10 mm` tracks exit the same SMD pad (or diverge at `< 45°` from a shared junction) with centre-lines `0.088–0.125 mm` apart, they ARE `firmly_linked` through the pad, so `ultraroute` leaves them alone — but KiCad 9's polygon-union checker measures a `0.055–0.070 mm` overlap lens (`< 0.09 mm` → `connection_width` error) or a `< 45°` acid-trap throat (`copper_sliver` error) between the two sibling branches!

**Other copper-hygiene artifacts we had to clean in `clean_copper_4l.py`:**
1. **`mend_pad_joints` aiming only at pad centre & missing roundrect corner-arc grazes (`"the track ends outside Q2-1 / SW1-1"` + `D97-4` `0.021 mm` neck):**
   - When a `--cell 0.025` track ends just outside a long rectangular, rotated, or rounded-rectangle (`roundrect_rratio = 0.25`) pad (`Q2-1` at `-17.75°`, `SW1-1`, `U8-4`, or `XL-1615RGBC` `0.45 × 0.65 mm` pad `D97-4` where `(98.9875, 122.2847)` sits inside the pad's axis-aligned bounding box `(98.55..99.00, 121.65..122.30)` yet `0.027 mm` *outside* the `0.1125 mm` rounded corner arc!), either a straight chord to the pad's *geometric centre* is blocked by an adjacent pin clearance, or the endpoint is treated as inside the pad box while its `0.050 mm` round cap only grazes the corner arc by `0.021 mm` (`< 0.090 mm` → KiCad `connection_width` error).
   - Aiming the short bridge at the **nearest point on the true pad polygon eroded by `w / 2`** (`pad_poly.buffer(-w/2)`) whenever `!pad_poly.buffer(-w/2).contains(end_pt)` mends 100% of both rotated-pad edge grazes and `roundrect` corner-arc grazes without colliding with neighbour pads!
2. **Narrow `Via ↔ Pad`, `Via ↔ Track`, and `Pad ↔ Track` lateral corner kisses (`0.021–0.054 mm < 0.090 mm`):** When a via lands diagonally off the corner of a same-net `0402` pad (`Via [VSYS] ↔ C32-1` on `B.Cu`, `0.054 mm`) or a loop-linked track grazes a via disc or pad corner laterally (`< 0.090 mm`), KiCad 9's net-polygon erosion flags `connection_width` even when another branch already carries the net (`firmly_linked == true`). Adding a short `max(min_width, 0.10 mm)` bridge whenever the 2D union overlap is `< min_connection` eliminates every KiCad `connection_width` error.
3. **Off-center `.025`-grid track-to-via grazes (`off_via`):** Prefanout/escape vias sit at `.00/.20/.25 mm` coordinates while `--cell 0.025` tracks pass `0.075–0.145 mm` from the via centre — legally overlapping the `0.15 mm` via radius in KiCad, but eccentric in copper instead of snapping to the via centre.
4. **Collinear `25 µm` sub-segments:** When `Tightening` is skipped or hits `fixed` junction predicates (`cleanup.rs:58`), long straight runs remain fractured into hundreds of `25 µm` capsule segments (`2,063` merged on `p2-132`).
5. **1-layer dangling vias (`via_dangling`):** Vias that end up connected on only a single copper layer (e.g., `B.Cu` stub only, after an inner/top branch was ripped or replaced) survive `prune_dangling` if they touch a pad/pour on that one layer, triggering KiCad `--severity-all` `via_dangling`.

**Proposed fix:**
- In `Write` (`stages.rs:909–934`), bridge or collapse `firmly_linked` sibling stems whose 2D capsule union forms a neck `< board.min_joint_of(net)` or an acute V-throat (`< 45°`), snap track endpoints/mid-grazes within `r_via` to the via centre where DRC-legal, merge exact collinear same-width/same-layer/same-net segments unconditionally (O(N), needs no deadline), and drop unlocked vias that only connect copper on `≤ 1` layer.

---

### 3. Priority Net Staging & Search-Time `length (max ...)` Steering (`custom.rs:20–23, 196–198, 271`)

**Observed symptom:**
- Routing `66–264` LED matrix nets simultaneously with `12 MHz` crystal (`XIN`/`XOUT` `≤ 11 mm`, `0` vias), `USB_D±`, and `75 MHz` `QSPI` (`≤ 25 mm`) lets early LED matrix congestion crowd the MCU pins and push `QSPI` / `XIN` into long detours. We worked around this with Stage 1 of `route_variant.py` (`two_pass.py`: strip matrix nets, route critical nets unobstructed, lock crystal copper).
- Also, two subtle points in `core/crates/board/src/custom.rs`:
  1. `custom_warnings` (`custom.rs:270–272`) emits `"checked after routing, not routed to"` for all three `REPORTED = ["length", "via_count", "connection_width"]` (`custom.rs:23`), **even though `via_count (max 0)` IS enforced during routing** via `Board::no_vias` (`custom.rs:169–171`)! Splitting `via_count (max 0)` out of that warning avoids misleading users into thinking `via_count (max 0)` was ignored.
  2. `Board::vias_cost_length` (`custom.rs:196–198`) returns `false` on a 4-layer board with `--planes In1.Cu` (since 3 layers `F.Cu`, `In2.Cu`, `B.Cu` are non-plane: `count() == 3 > 2`), even though every through-via grazed on both `F.Cu` and `B.Cu` still adds `via_height` (`1.586 mm`) in `Board::net_length` (`custom.rs:187`) and in KiCad 9 DRC! And `length (max ...)` currently only guards via-dropping rather than bounding A* path length or prioritizing length-capped `.kicad_dru` nets into `ledger.first` (today `stages.rs:135–136` only populates `ledger.first` from `--regions` JSON, not from `.kicad_dru` `length (max ...)` rules!).

**Proposed fix:**
1. Automatically populate `ledger.first` (`stages.rs:136`) with nets that have `.kicad_dru` `length (max ...)` or `via_count (max 0)` rules (in addition to `--regions`), or add `--priority-classes CLOCK,DIFF_HS,SE_HS` to route critical netclasses before bulk matrix/signal nets in one invocation.
2. Clarify `custom_warnings` (`custom.rs:271`) when `via_count` has `max == 0` (since `no_vias` enforces it).

---

### 4. `autoroute.py` Multi-Seed Budget Allocation vs `--adopt --shove-plough region`

**Observed behavior:**
- `.claude/skills/pcb-design/scripts/autoroute.py` passes `--negotiate --rip-up --tighten ... --shove-plough region` in a single `--timeout` budget per seed. On dense 4L boards (`p2-132` .. `p4-264`), `Negotiate + RipUp` consumes 80–100% of a 5–13 minute timeout per seed, starving `MergeEscapes` and `Shove (Plough::Region)` — whereas running a fast negotiated pass first and then running `--adopt --shove-plough region --shove-ties` on the winning seed cut opens by **25–71%** (`7 → 2` on `p2-132`, `50 → 28` on `p3-162`, `74 → 56` on `p3-198`, `179 → 139` on `p4-264`).

**Proposed fix:**
- Either cap `Negotiate + RipUp`'s share of `--timeout` inside `ultraroute` when `--shove-plough region` is enabled so `Shove` + `Tightening` + `Refinish` always get air time, or let `autoroute.py` run quick seeds (`120–180 s`) to pick the best topology and spend a dedicated `--adopt --shove-plough region` finisher budget on the single winning seed.

---

### 5. Coordinate Sign Mismatch & Silent Zero-Op on `--rip-region` (`lib.rs:1310` vs `stages.rs:142`) + `physical_hole_clearance` Discoverability

1. **Y-coordinate sign mismatch:**
   - `--place REF X Y` (`lib.rs:1310`) negates `Y`: `Point::new((x * 1e6).round() as i64, (-y * 1e6).round() as i64)` (Gerber/cartesian y-up).
   - `--rip-region X,Y,R` (`stages.rs:142`) and `--via-avoid` (`stages.rs:138`) do **not** negate `Y`: `Point::new((x * 1e6).round() as i64, (y * 1e6).round() as i64)` (KiCad `.kicad_pcb` y-down), and `--rip-region` logs nothing on stderr. Passing `--place`-convention coordinates (`Y = -91.8`) silently rips `0` items!
   - **Fix:** Document/unify the Y-sign convention across `--place`, `--rip-region`, and `--via-avoid`, and log `rip-region (X, Y, r=R): ripped N items` in `SetUp::run` (`stages.rs:144`).
2. **Same-net Via-in-SMD-Pad shorthand (`--no-via-in-smd-pad`):**
   - `0b0f336` (`fix/via-in-pad`) already enforces `.kicad_dru` `(constraint physical_hole_clearance (min 0.2mm))` on same-net SMD pads (`easy2350.kicad_dru:84–86`), which is awesome — whereas standard `hole_clearance` skips same-net pads (`90–150` vias in `0402`/LED pads on `p1-66`/`p2-132` when `physical_hole_clearance` isn't written). Documenting that `.kicad_dru` snippet in `ultraroute route --help` / `docs/kicad-coverage.md` (or offering `--no-via-in-smd-pad [MM]` that auto-exempts multi-via exposed thermal pads) would prevent 2L/4L `POFV = No` users from missing it.

---

### 6. Small Reference Copper Zones (`< 5 mm²`) Sliced Into Islands Unless Mirrored as `.kicad_dru` Rule Areas

**Observed symptom on both `round-pendant` Variant E and `parallelogram` `p1-66` (`RP2350` reference regulator layout):**
- Following `LESSONS.md` ("copy the reference layout's power **zones** and let the router join them"), we placed the vendor `RP2350` `B.Cu` copper zones (`vendor +1V1` `COUT` pour, `vendor GND` `PGND`, `vendor VREG_LX`, `vendor +3V3`, plus `0.24 mm` `IOVDD`/`FB` finger pours).
- Because `ultraroute` treats un-protected copper zones as passive pours that `ZONE_FILLER` will refill around new copper, `ultraroute` routed foreign tracks and vias (`+3V3`, `SW1_8`, `VBAT_SENSE`, `QSPI_SD3`) straight across those `< 3 mm²` `B.Cu` zones. After KiCad zone refill, **12 small zones split into disconnected islands (`islands=2`) or lost their Kelvin sense path**, while KiCad DRC stayed silent because inner planes/rings still kept the nets electrically joined!
- Replacing `0.2 mm` strip zones with locked tracks and duplicating all four `vendor *` copper zones as `B.Cu` `.kicad_dru` `intersectsArea` rule areas (`disallow track via` for `A.NetName != '<zone_net>'`) eliminated all 12 intrusions immediately.

**Proposed fix:**
- For small non-plane copper zones (`outline area < 5 mm²`, e.g. regulator `LX` / `COUT` / `PGND` pours), either treat the zone polygon on its layer as a foreign-net track/via obstacle by default (unless `--allow-zone-cut` is passed), or check post-refill island count / `min_thickness` pinch in `ultraroute`'s outcome report so silently severed reference pours are caught without bespoke `copper_audit.py` checks.
