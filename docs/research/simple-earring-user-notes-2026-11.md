# simple_earring: notes from using ultraroute (Nov 2026)

These notes come from the agent working in `~/pcb/simple_earring`. That project uses ultraroute as its only router,
pinned at `vendor/ultraroute` **99bb7ca**. The user asked what it is like to use, what we work around, and what would
make it set-and-forget. These notes answer that. They describe how the router is to use, not specific bugs; earlier
bug reports are in `~/pcb/simple_earring/design/ultraroute-requests.md`.

## The board and the runs behind these notes

- **Board:** the earring, a 30.5 mm teardrop on 4 layers.
  - F.Cu carries the LEDs and the RP2350A. In1 is a solid GND plane.
  - In2 holds a 3V3 disc (r < 8.95 mm); the rest of In2 is free.
  - B.Cu holds most parts, under a LiPo cell.
- **Variant A:** WS2816 daisy chain. It routes, DRC 0/0 (seed 2, `route/earring/recipe.json`).
- **Variants C/D** (new, Nov 5–6): 45 plain RGB LEDs on an AW20198QNR matrix, 9 SW × 15 CS. That's 180 LED pins
  more than A.
  - Outer ring: 37 LEDs at r 13.25 mm, 2.44 mm pitch. Inner ring: 8 LEDs at r ≈ 9.3 mm.
  - Recipe: `--negotiate --rip-up --nudge --tighten --prefanout --diff-pairs --shove-plough region --shove-reach 0.5
    --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8 --strip --planes In1.Cu --pour-cost 3
    --threads 1 --timeout 900`, seeds 1–6.
- **Judging:** every result was judged by KiCad 9 DRC (project rules) after a pcbnew refill. I also measured the In1
  GND fill area.

| Run | Open (KiCad) | DRC err | In1 GND fill |
|---|---|---|---|
| C, recipe, 6 seeds, 900 s | 37–45 | 0–2 | 610 mm² (intact) |
| C, `--seed 1` only (defaults, no `--planes`), ~57 min | **5** | 2 | **213 mm²** |
| C, recipe without `--planes`, `--pour-cost 3` / `10` | 8 / 15 | 1 | 244 / 219 mm² |
| D (driver moved to the only LED-free pocket), 6 seeds | 41–50 | 1–4 | ~610 |
| D + arc remap (each driver side serves its own half of the ring), 6 seeds | 43–50 | 1–4 | ~610 |
| D + 0.10/0.10 mm netclass for SW?/CS?/CS??, 6 seeds | (running: negotiation 268/320 vs 253–256 before) | | |

Repro inputs (route.py's pre-routed boards) are in `~/Software/ultraroute-lab/repro/earring-matrix-c/` and
`earring-matrix-d/` (the latter has the 0.1 mm `matrix` class). Before routing, run `tools/add_block.py <pcb>
rp2350_power vreg_vdd_core_3,vreg_vdd_core_6,vreg_3v3_1,vreg_3v3_7 --parts U1`. My KiCad judge is `judge.py` in the
C folder.

## What works well

- **Speed and reproducibility.** A 6-seed sweep is ~15 min wall clock on 6 cores, and the same seed with
  `--threads 1` gives the same board. That is what made a placement study possible at all.
- **Its open count matches KiCad's.** "37 open" in the log was 37 unconnected in KiCad DRC, so the number can be
  trusted.
- **`--planes` does what it says.** With it, In1 stays a whole plane (610 mm²).
- **`--diff-pairs`, KiCad netclass and `.kicad_dru` support.** A new netclass in the `.kicad_pro` was picked up with no
  extra flags ("4 classes" in the memory line).
- **The log hint about planes:** "In1.Cu looks like GND's plane ...; --planes or --auto-planes keeps signals off it".
  It was the right warning (see 1).

## What cost time, in order of hours lost

### 1. The defaults destroyed the GND plane quietly, and the result looked like a breakthrough

My recipe-arg parsing broke once, so a sweep ran with `--seed N` and nothing else. It got to **5 open** where the
recipe gave 37–45. It took a separate measurement to find why: it had routed 566 mm of signal across 43 nets on In1,
leaving 213 of 610 mm² of the GND plane.

- The only warning was the one log line about planes, which is easy to miss among ~40 lines.
- The end-of-run summary said nothing about the plane.
- **`--pour-cost` didn't help.** At 10 (vs 3) the plane still ended at ~219 mm², and the route just got worse (11–15
  open).
  - I couldn't tell from `--help` what `--pour-cost` is meant to do: a cost per cell of pour crossed, or per cut?
  - I couldn't tell whether any value would keep a plane mostly whole.

**Asks:**
- Make plane protection the default whenever a layer is ≥ ~80 % one net's pour (i.e. `--auto-planes` on by default),
  with `--allow-plane-routing` to opt out.
- Report, in the final summary and `--report`: each pour's area before → after, and its piece count.
  - Example: `In1 GND: 610 → 213 mm², 1 → 9 pieces`.
  - A result that halves a plane should look bad in the summary, not good.
- Document `--pour-cost`'s units, with an example of what values do.

### 2. Is this board routable at all? Answering that took ~4 hours of sweeps

The question I needed answered was a placement question: is the driver's position the problem, or is 4 layers just
too few? Answering it took three full sweeps (C, D, D-remapped), and all three ended in the same band (37–50 open). The
router knew much sooner that it was capacity-bound: rip-up kept reporting "lost 22–36 with no way through". It didn't
tell me where, or by how much.

**Asks:**
- **A capacity / congestion report** (`--estimate` already exists for memory): demand against supply across the
  natural cuts.
  - Example: "the annulus r 10–12.4 mm must carry ~26 tracks; with In1 reserved and B.Cu parts in the way it has ~18".
  - Better still, a heat-map PNG or SVG of the overflow after negotiation.
  - With either, the answer "4 layers can't do it as placed; you need ~8 more tracks of capacity here" comes in one
    run, not three.
- **`--blame` in the default report.** I only learned it exists from round-pendant's notes. The question "is the
  blocker a fixed part, routed copper or a plane?" is exactly the placement question.
- **Suggestions when stuck.** For example, "narrowing class matrix to 0.10/0.10 would add N tracks across the worst
  cut", or "M open connections end on U11; its fanout escaped 77 of 83".

### 3. "Time ran out" doesn't say whether more time would help

Every 900 s run ended with `deadline: time ran out in shove` while it was still improving. One run without
`--timeout` took ~57 min, and I don't know what default deadline applies when none is given. A background job of mine
was killed at my own 90 min limit while two seeds were still dropping (7 → 6 open).

**Asks:**
- **A plateau stop:** stop when K passes in a row make no progress (`--shove-patience` exists per stage; make it the
  global stop) and treat `--timeout` as a safety cap only.
- **Say in the summary why it stopped,** for example "deadline hit while improving: last 3 passes 9 → 7 open". Then I
  know whether to buy time or change the board.
- **Make the work budget independent of machine load.** On this shared 24-core machine, the same seed and args gave
  different boards under different load, because the stages are wall-clock bounded. A deterministic budget (in
  iterations, not seconds) would make "seed 2 is the good one" a fact rather than a timing accident.

### 4. Everything around a route I had to script myself

For each experiment I wrote or rewrote:
- **A seed-sweep script:** copy the board, `.kicad_pro` and `.kicad_dru` into one folder per seed, run `add_block.py`,
  route, judge.
- **A judge:** load in pcbnew, set route.py's refill widths, refill, save, `kicad-cli pcb drc --format json`, count
  opens by net prefix, measure plane area.
  - Gotcha I lost a run to: `pcbnew.SaveBoard` writes default rules over a `.kicad_pro` beside it. My first judge
    copied the rules before saving and so reported ~1,200 false errors.
- **A per-board `add_block.py` call:** the U1 power-zones block has to be added by an external tool before every run.

**Asks:**
- **`ultraroute sweep`:** `--seeds 1-6 --jobs 6 --out DIR`. Run the seeds in parallel. Rank them by open, then DRC,
  then plane damage, then length. Write `best.kicad_pcb` and a table.
- **A built-in KiCad judge:** with `--report`, refill like KiCad and run KiCad's checks. Or ship the judge script,
  tested, in `tools/`.
- **Declare blocks in the board, not via a pre-step:** a named rule area or `.kicad_dru` rule (e.g. `(rule
  "block rp2350_power" ...)`), so the route is one command.

### 5. `--help` is one 1,500-character usage line

`ultraroute route --help` prints every flag on one line with no descriptions, defaults or groups. I didn't know
`--blame`, `--adopt`, `--auto-planes`, `--report`, `--recipe` or `--cell` were relevant until I read another project's
notes.

**Asks:**
- Grouped `--help`: stages, budget, planes/pours, output/diagnostics. One line per flag, with its default.
- A short "recipes for common boards" section. Round-pendant's 3-stage flow (pre-pass, seed search, then `--adopt
  --shove-plough region` finisher) should be a recipe, not a wrapper script in every project.

### 6. Logs name counts, not the parts that failed

`prefanout: ... 77 of 83 escaped (6 failed)`, `lost 33 with no way through`, `13 to copper it may not rip`. Which
pins? Which nets? To find out, I parsed KiCad's DRC JSON and plotted positions myself.

**Ask:** name them, the 6 pins that failed to escape and the first ~10 nets with no way through (with coordinates), in
`--report` if not the log.

### 7. Fills and copper hygiene

- Every routed board had 1–4 `connection_width` errors. These are KiCad fill slivers after refill, not routed copper.
  Our route.py works round them: a 0.2 mm min fill width on large planes and a no-pour area at one pinch.
- The log also mends a lot of its own geometry, which reads as alarming even if it's fine:
  - "mended 274 pad joints where a track's end only reached the pad's edge"
  - "bridged 118 places where a track only grazed its net's copper"

**Asks:**
- Predict fill slivers and nudge tracks to avoid them, or report where KiCad's refill will leave one.
- Say whether the mends are expected or a sign of trouble.

### 8. Upgrading is risky, so we stay pinned

We are pinned at 99bb7ca because fe903ff routed this board worse (requests #4). Newer fixes (3de3d83: length rules,
`--place` frame) are therefore not used here. A regression corpus that includes this board would let projects follow
main safely. `jobs/earring-*` exist; the two matrix repros above are good hard cases near the 4-layer limit.

## If I could ask for one thing

Make the end of a run explain itself: **"N open: these nets, blocked here, by this; stopped because X; plane areas
before → after; capacity shortfall at this cut ≈ Y tracks"**. With that, most of this session's ~6 hours of sweeps
would have been one or two runs, and the placement decision would have come from the router instead of from
trial and error.
