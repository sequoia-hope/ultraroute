# Standing focus: trapped escapes

Set by the user on 2026-10-25, to stand until they say otherwise: explore, as comprehensively as
the time allows, every way of freeing connections trapped at a fine-pitch part's pads, and their
permutations, until the simple earring routes. When the earring gets stuck, route the other boards
(bluebuzz first) for what they show. Record every experiment in the lab (`tools/lab.py`); the
judge is KiCad's DRC after a refill: unconnected items and errors.

## The problem

On the earring the core router leaves about 10 connections open, nearly all at U1 (RP2350,
QFN-60, 0.4 mm pitch): CHRG, LED_EN_L, LIGHT, QSPI_SD2, SWCLK, SWDIO, USB_DM, USB_DP, VBUS,
VSYS. Each is trapped: the copper of its neighbours closes every way out of its pad, and routing
it later, one connection at a time, finds no path.

## The space

Approaches, each alone and combined, and each in several places in the pipeline (before
negotiation, after rip-up, before or after nudging) and with several time budgets:

1. **Comb** (`--escape`): pull each net's copper onto straight lanes in a fixed 1 mm zone. Made
   the earring worse (10 → 18 after rerouting).
2. **Lane clearing** (`--clear-lanes`): slide other copper out of one pad's lane at a time.
3. **Plough** (`--shove`, `grid::shove`): the cheapest path out with other copper passable at a
   price, then that copper moved off the path: a straight push, or a warp field held still on
   pads and vias; later rounds lay the path outright, ripping exactly what it hits. Variants:
   warp, linear, rip-only.
4. **Dipole**: a repulsive source at the trapped end that moves copper away a little at a time,
   checking legality each step, and rips only when moving stops helping.
5. **Rubber band**: the topological router (`topo`) for the trapped connections, which can
   slide copper around rather than through it.
6. **Priority restart**: route again from scratch with the connections that ended open routed
   first, before negotiation lays the rest.
7. **Pre-fanout**: before anything else, escape every signal pad of a fine-pitch part a short way
   out of the package (a stub, or a stub and a via), and route from the escapes; generalise to
   other parts later.

Also open: how a try is judged (fewer open only, or ties when the trapped connection is out),
what knocked-loose connections are rerouted with (plain reroute, rip-up), cascading, and the
second pass over undone tries.

## Tools

- `core/crates/ultraroute/examples/fanout_trace.rs` traces the escape passes on a routed board;
  the lab's `fanout.html` draws them, with "Plough (--shove)" showing every try round by round.
- `tools/lab.py run EXP --title ... NAME=BOARD::ARGS ...` runs the router, refills, runs KiCad's
  DRC and records it all for http://localhost:8065/ (`proj url ultraroute-lab`).

## Log

- 2026-10-25: comb 10 → 18; plough (warp) on the routed earring 10 → 7 alone, and in the studio
  recipe 10 → 9 by KiCad; with later rounds laid outright every trapped connection routes out,
  but knocked-loose ones decide the score: rip-only with a rip-up repair 10 → 7, warp plough
  with the same repair 10 → 9.
- 2026-10-25, lab `sweep-plough-1` (earring-kicad, 120 s, 2 threads, 13 runs at once; KiCad
  unconnected): base 11; warp 6; gentle after nudging 7; rip with a 20 s repair 8; gentle 9,
  linear 9; rip 10, dipole 10, rip with ties and passes 10, dipole after nudging 10; rip after
  nudging 11, warp after nudging 11 (the plough got no time: nudging took it all); gentle with
  ties and passes 12. One run each and timing-bound under load, so noisy: the next sweeps repeat
  over seeds (`--seed`).
- 2026-10-25, lab `sweep-plough-2` (four seeds each, 120 s): base 9/8/8/8 (mean 8.25); warp
  12/9/11/9; warp after nudging 12/9/11/8; gentle after nudging 11/8/11/9; rip with repair
  11/9/11/8. Inside a fixed 120 s the plough costs the other stages more than it wins, and the
  seed (connection order) alone moves the base from 11 to 8: order is a strong lever. Next: a
  seed portfolio, and the plough as its own pass on the best routed boards.
- Gemini (Argon) agents in worktrees: explore-prefanout, explore-priority, explore-placement,
  explore-features (frame-by-frame debug films of every feature), explore-massage (raindrop
  attractors and repulsors; inflate the placement and shrink it back).
- 2026-10-25, lab `sweep-seeds-3` (plain recipe, seeds 5-20, 120 s): 7 7 8 8 8 9 9 9 9 10 10 10
  11 12 12 13 (best seeds 6 and 12). USB_DM is open in all 16, LIGHT in 14, LED_DATA_R in 13,
  SWDIO in 12. `--blame` says a clear path exists for USB_DM, yet the grid router finds none:
  `grid/examples/why_blocked.rs` shows U1-51 walled in on F.Cu by routed copper, a VDD_CORE via
  0.156 mm off (from the next pad, heading for the VDD_CORE block pour 1.5 mm below) and USB_DP's
  own track, with nothing reachable beyond the pad; the inner layers below are open but no via
  fits. The block pours do not cover the way out; it is a narrow channel below the bottom row
  that USB_DM, USB_DP and VDD_CORE share.
- 2026-10-25, lab `postpass-plough-4`: the plough as its own 300 s pass (`--adopt`, 20 s repair,
  up to 3 passes) on the routed boards of seeds 6, 12, 7, 13 (7, 7, 8, 8 open): rip 7/5/5/6, warp
  7/5/5/6, gentle 7/12/5/6. Best so far: 5 unconnected, 0 errors (KiCad). The gentle 12 was a bug:
  the router joined two track pieces whose round ends only touch, KiCad does not, so the cleanup
  pruned every other 0.075 mm piece the plough had cut and the file lost connections the router
  thought it had. Fixed in board::joined (strict overlap; test `track_ends_join_where_they_overlap_
  not_where_they_only_touch`); the KiCad agreement tests still pass. Rerun as `postpass-plough-5`.
- 2026-10-25, `postpass-plough-5` (after the join fix, with --tighten): seed 7 and 12 boards
  reach 5 by KiCad with any plough; router and KiCad now agree within one.
- Traces of the plough on the s7 board: every trapped connection routes out, but the try's
  repair (rip-up) then ripped the freed route itself: `grid::rip_up` chose what to rip by the
  soft list and ignored `locked`, so holding the route did nothing. Fixed (rip-up leaves locked
  copper alone). After that each try frees its own connection and costs one or two others
  (USB_DM against VDD_CORE in the channel below U1): a zero-sum exchange, so ties are worth
  keeping as moves of a search. The plough is now a seeded multi-pass search (`--shove-ties
  --shove-passes N --shove-patience N --seed N`, each pass in another order); on the s7 board
  the router reports 4 open for three of four seeds. KiCad check: lab `plough-search-6`.
- 2026-10-25, lab `plough-search-6`: the seeded plough search on the 5-open boards reaches **3
  unconnected, 0 errors (KiCad)** in 6 of 16 runs (s7 board seeds 2-6: QSPI_SD1 or SD3, SWDIO,
  USB_DM; s12 board seed 1: LED_DATA_R, SWCLK, USB_DM). USB_DM is open in every run.
- Insight: a kept (block) pour was never a connection's end, since only pads, vias and track ends
  have anchors, so U1's power pins were joined to their net's copper outside the package, through
  the channels the signals need, though the block pour lies just inside the pad ring. Kept pours
  now have anchors every 0.25 mm along their edges (KiCad joins a track ending in the zone on
  refill). Test: lab `sweep-pour-anchors-7`.
- 2026-10-25, lab `sweep-pour-anchors-7` (same seeds, kept pours as ends): 8 8 6 11 10 11 11 8,
  mean 9.1 against 9.0 before: no clear effect. USB_DM still open everywhere.
- USB_DM's pinch, from the board: the block's VDD_CORE pad `vreg_vdd_core_6` starts 1 mm below
  pins 50-51 and the 3V3 strip `vreg_3v3_7` hugs the left of pin 50's way down (x <= 98.75), so a
  straight drop from VDD_CORE pin 50 breaks clearance to the strip and must jog right, under pin
  51 (USB_DM, x 99.2). The router's VDD_CORE goes pin 50 -> via to In1 -> via back -> zone at
  x 99.08, closing USB_DM in. A question for the user: how the RP2350 block is meant to let
  USB_DM out past VDD_CORE's link to its pad below.
- 2026-10-25, lab `plough-search-8` (search on the pour-anchor boards): best 3 (two runs); one
  board routes USB_DM and loses VDD_CORE instead (s12-q12), the pinch being a straight trade.
  Lab `plough-search-9` (a second round, fresh seeds, on the 3-open boards): **2 unconnected,
  0 errors (KiCad)** in 5 of 12 runs: SWDIO + USB_DM, or SWCLK + USB_DM. Rounds compound: an
  iterated local search, each round's best boards searched again with new seeds.
- 2026-10-25, lab `plough-search-10` (third round, seeds 31-33, 600 s, on the 2-open boards): all
  15 stay at 2 (SWDIO or SWCLK, and USB_DM). The iterated search has reached a local optimum on
  these boards.
- Gemini placement agent, lab `placement-wave1-120s`: its wave-1 placement routes to 3 (SWDIO,
  USB_DM, VBUS_SENSE) with the plain recipe at 120 s, the stock placement 10 in the same run.
  Audit: no locked part moved; 109 of the movable parts moved, some far (R18 10.7 mm, R3 9.5 mm,
  R32 8.5 mm, R12 7.5 mm), decoupling capacitors up to 4 mm, many rotated; 0 KiCad errors. Legal,
  but a heavy reshuffle for the user to judge. Stacking the plough search on it: lab
  `placed-w1-seeds-11`.
- 2026-10-25, lab `placed-w1-seeds-11`: the placement agent's wave-1 placement over 8 seeds gives
  5 7 7 8 8 8 9 9 (mean 7.6; the stock placement about 9.4 alike): worth about 2, not the 7 a
  single lucky run suggested. `tools/search.py` (new: iterated search through the lab, keeping
  the best boards by KiCad each round) on it: 5 -> 3 in round 1, then a plateau at 3. The stock
  board's 2 stays best.
- To leave the local optimum: `--rip-region X,Y,R` (repeatable) rips every unlocked track and via
  in a disc before routing, a large-neighbourhood move; search `lns-u1` rips 5 mm round U1 on the
  2-open boards, then negotiates, rips up and ploughs, seeded.
- 2026-10-25, search `lns-u1` (rip 5 mm round U1, rebuild, plough): 3-5, never back to 2. Too
  destructive as a kick.
- **The designer already solved USB_DM's pinch.** simple_earring `scripts/route.py` commits locked
  wiring around U1: the VDD_CORE link out of pin 50, jogging right at y 103.66 to x 98.98 and down
  into the VDD_CORE pad (98.98 rather than 99.00 to give "the USB_DM escape diagonal its 0.277
  corner clearance"), USB_DM and USB_DP assist vias at (99.39, 105.02) and (99.74, 104.6), and
  more. The job `earring-kicad-locked` carries it (`"strip": true`: only unlocked copper goes),
  but every "locked" run so far passed `--strip-all` (the lab's default recipe has it), which
  removed it. Lab `locked-kept-12` routes that job as meant, with `--strip`.
- 2026-10-25, lab `locked-kept-12` (locked job with its committed copper, seeds 1-8, 120 s): 6 7
  8 9 9 9 10 10 (mean 8.5, about as stripped). USB_DM routes in two runs (USB_DP open there
  instead); PWR_EN and LEDC_D6 join the failures, the locked copper narrowing other ways.
  Iterated search on the best three: `locked-search`.
- Gemini features agent done (worktree explore-features, REPORT.md there): `feature_film.rs`
  records any feature frame by frame, `lab/features.html` plays the films (31 on the earring, in
  the lab's films/). Lab pages merged into main; its Rust watch hooks to merge with the others.
  Its findings: blame-guided nudge never acts (blame prices routed copper so cheaply, 6 per
  cell, that paths cross tracks rather than near movable parts, so nothing is blamed); the push
  field's Cauchy tail moves parts 12 mm away; per-step net put-back after re-cutting tracks can
  leave hairline gaps (seen at 10 -> 320 in its guarded dipole).
- 2026-10-25, search `locked-search` on the locked job with its committed copper: 6 -> **2
  unconnected, 0 errors (KiCad)** in round 1 and four runs of round 2: **CHRG and LED_DATA_R**;
  USB_DM and USB_DP both route, through the designer's committed VDD_CORE jog and assist vias.
  On those boards: CHRG is blocked only by parts (blame: C1 -0.027,-0.314 mm and C17 +0.067 mm,
  both movable); LED_DATA_R (U1-28) is walled in by LED_DATA_C's via 0.127 mm off. Lab
  `locked-nudge-13`: guided nudge, alone and before the plough search.
- Gemini priority agent done (worktree explore-priority, REPORT.md; 15 lab experiments, 63 runs,
  all 0 KiCad errors; two headline numbers spot-checked against the lab and exact). Built
  `--first` (nets, `@REF`, `@auto`), `--first-negotiate`, `--first-weight W` (priority nets stay
  in negotiation with their congestion costs scaled by W), `--restart N` (re-route from the start
  with the nets left open promoted, pour nets excluded), `--topo-first`, `--topo-trapped`,
  `--topo-trapped-rip` (rubber bands for the trapped connections after the grid, ripping soft
  clusters), and a necked topo exit for fine pitch. Single runs each: best 5 (stripped, 240 s,
  `--restart 2 --first-weight 0.15 --shove --topo-trapped-rip`) and 6 (locked, 240 s,
  `--topo-trapped-rip`). Hard priority (`--first`) fails: nets laid first without the others'
  congestion wall in their neighbours; topo first likewise. To merge, then use as starting
  boards for the iterated search.
- Lab `locked-nudge-13`: guided nudge did nothing (0 steps): nudge asked blame with routed copper
  priced, which blames no part; `--blame` prints the walled query too, where C1 and C17 show.
  Fixed: nudge falls back to the walled query. Lab `locked-place-14`: moving C1 and C17 by
  blame's vectors with `--place` (no legality check) gave 2 KiCad clearance errors and left
  CHRG open; blame's vector alone is not enough there.
- Merging the finished agents' work (features, priority) into worktree `integrate`, by a Claude
  subagent; the source worktrees started from an older snapshot and every agent changed shove.rs.
- **2026-10-25: the stripped earring routes completely.** Gemini pre-fanout agent (worktree
  explore-prefanout, REPORT.md): `--prefanout` (grid/src/prefanout.rs) escapes every signal pad
  of each fine-pitch part before negotiation: power pads bridged inward to the block pours, the
  rest by a search on a 10 um local grid out to a ring 1.1 mm past the pad rows with a via
  ("inner" mode), traces necked to the board minimum (0.127 mm) so a lane passes a neighbouring
  0.4 mm via, vias staggered across two depths, lanes kept radial and each earlier escape's
  hand-off to the global grid protected, the pinched pads (U1-50 beside the 3V3 strip) first.
  Lab `prefanout-final-earring`: **0 unconnected, 0 errors (KiCad) in 30.9 s**, eight runs at 0
  across flag settings; locked job 1 (VBSENSE). Audited: judge path, rules and boards identical
  to main; my own refill and KiCad DRC (with schematic parity) on its output: 0 unconnected
  items, 0 errors (warnings only: library, silk, 3 dangling tracks, 1 dangling via); same 167
  footprints, 573 pads, 104 nets, 16 zones; no part moved. Robustness: lab `audit-prefanout-15*`.
- Lab `audit-prefanout-15/15b/15c` (the pre-fanout binary at 2, 3, 6 threads and 60 s):
  stripped job **0, 0, 0** (21-30 s); locked job 1, 1, 1 (VBSENSE each time). Search on the
  locked boards for the last one: `locked-pf-search`.
- Integration (Claude subagent, worktree `integrate`): features and priority merged; 128 tests
  pass; at default options the merged router gives the same board as before (uuids aside); one
  default change kept on purpose: topo's necked pad exits. Pre-fanout merge next. Noted: the
  cleanup now prunes about 1500 dangling pieces a run (300-740 before); to look into.
- **2026-10-25: the locked job routes completely too.** `tools/search.py locked-pf-search`: the
  iterated plough search (main's router) on the locked pre-fanout boards (1 open, VBSENSE) gives
  **0 unconnected, 0 errors (KiCad)** in every run of rounds 1 and 2. Audited one
  (locked-pf-search-r1/L-t2~r1s101): my own refill and DRC with schematic parity: 0
  unconnected, 0 errors (warnings: library, silk, 5 dangling vias, 4 dangling tracks); all 45
  locked tracks and vias of the designer's committed copper kept; 573 pads and 16 zones as
  input; one part moved: R16 (unlocked) 1.0 mm in y by the recipe's nudge. Recipe: pre-fanout
  (`--strip --negotiate --rip-up --nudge --tighten --prefanout`, about 75 s), then the plough
  search (`--adopt --shove-plough rip --shove-repair 20 --shove-ties --shove-passes 60
  --shove-patience 8 --tighten`, one round of five seeds).
- Loose ends: dangling vias and tracks left on finished boards (cosmetic warnings); the cleanup's
  1500 pruned pieces a run; merge the pre-fanout code into `integrate` and then main, so one
  router does all of it; then the other boards (bluebuzz).
- Pre-fanout merged into `integrate` (135 tests pass). The integrator found the merged router's
  default seed ending at 0 open but 4 KiCad clearance errors (the In2 3V3 pour against tracks
  between U1's thermal vias, after the refill; seeds 1 and 2 clean), and bisected it to the
  kept-pour anchors added earlier: they change which connections exist before negotiation. They
  had shown no gain (sweep 7), so they are removed, in main and in integrate. Lab
  `integrated-16`: the integrated router, one command per run, both jobs, six seeds.
- Lab `integrated-16` (merged router, one command per run, 12 runs at once at 2 threads):
  locked job (pre-fanout + plough search) 0 0 0 0 1 1 unconnected, 0 errors throughout (75-111
  s); stripped job (pre-fanout) 0* 0 0 0 2 1, where * is 0 unconnected with 4 KiCad clearance
  errors (the In2 3V3 pour against copper between U1's thermal vias, after refill; still there
  with the pour anchors gone, so the bisection's explanation does not cover this run). A seed
  portfolio of 4-6 gives a clean full route of either job today. Next: keep tracks out of the
  gaps between U1's thermal vias on refilled-pour layers; let the plough search finish seeds
  left at 1 open.
- 2026-10-26, bluebuzz (agy, worktree `agy-bluebuzz`, audited): `bluebuzz-rm2` routes with
  `--prefanout`, from bare pads and with its locked copper, 0 unconnected and 0 KiCad errors on
  seeds 0-3 each (lab `bluebuzz-check-2`; its .kicad_pro makes dangling copper an error).
  Without pre-fanout it ends at 9 open. agy's cleanup fix (a via no longer counts as covering a
  track, overhanging stubs past a T-joint are trimmed) is merged; on the earring it removes nearly
  all dangling warnings and leaves the open count before and after cleanup unchanged (lab
  `cleanup-agy-18`, one thread per run: stripped 0 0 1 0 3 2, locked 1 0 0 1 1 0).
- Bluebuzz on main (lab `bluebuzz-main-19`, 2 threads): stripped and locked both 0 unconnected
  and 0 KiCad violations on seeds 0-3. On one thread (as in the browser) seed 0 left a 0.11 mm
  XOUT piece on In1.Cu running from the route's end into the side of a fan-out stub, which KiCad
  calls dangling (an error under bluebuzz's rules). The cleanup now drops a track whose both
  ends lie inside other tracks of its net when nothing needs it, unless it meets a via
  (taking those stranded vias in a first try). Lab `redundant-20`, one thread: bluebuzz 8 of 8
  clean; earring unchanged. Best bluebuzz kept as job `bluebuzz-routed` (seed 3, 232 vias against the hand-routed board's 331, 1348 mm).
- 2026-10-26, differential pairs (`--diff-pairs`, grid/src/pairs.rs): pairs found by name (one P
  against N/M, or + against -), ends as pin pairs, each run between them routed as one centreline
  wide enough for both tracks and the gap on a single layer, then laid as two offset tracks; the
  pad legs are left to the router. Routed before the escapes, the runs blocked U1 pins
  (LED_DATA_L, TAC_CS2 open on every seed, lab `diffpairs-21`); after them, nothing is blocked.
  The cleanup now cuts a track loose at both ends back to its outermost joints (pair runs
  crossing their legs' vias) and cuts a tail back to a joint that already lies inside it. Lab
  `diffpairs-24`, one thread: earring stripped 0 0 0 0 2 3 unconnected (was 0 0 1 0 4 2), no
  pair copper dangling; locked 0 1 1 1 0 0; bluebuzz 0 on all 6. Coupling (`pair_check`):
  earring USB_DP/DM 78-92%, USB_C 17-38%; bluebuzz USB_DP/DM 72-83% (hand-routed 31%), its 4
  mm USB_D*_C pair uncoupled. Jobs earring-routed, earring-locked-routed and bluebuzz-routed
  re-solved with pairs.
- 2026-10-26, length matching and the short pair. Pins of a pair whose ends share a layer are
  left out of the fan-out; each run's layer is chosen by cost (length, 2 mm per via its ends
  need, the start ring's radius), each layer from its nearest start ring that lays; pair runs
  may cross pours, kept block planes included, to be refilled round them (bluebuzz's 1V1 block
  pour had cut USB_D*_C off F.Cu). After the cleanup, `grid::match_lengths` measures each net
  end to end along its copper and bumps the shorter one, one bump at a time, tallest that lays,
  uncoupled tracks first, undoing any bump that does not reduce the skew. Lab `diffpairs-26`,
  one thread: earring stripped 1 0 0 0 0 0 unconnected, locked 0 0 0 0 1 0, bluebuzz 0 0 0 0
  0 1; skew after tuning mostly 0.00-0.16 mm (earring seed 2, a 19.8 mm detour, ends at 6.5
  mm); bluebuzz's short pair now 54-60% coupled on F.Cu with no via, 0.08-0.25 mm apart (too
  short for a bump). The earring's USB_DP/DM side-by-side share falls to 35-59%: about as much
  is coupled as before, but the matching bumps add length. Jobs re-solved; job.json `args`
  now carries the seed the studio routes each job with (earring-kicad 5, bluebuzz-kicad 2).
