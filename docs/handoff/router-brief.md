# Brief for whoever builds the new router

Written 2026-11-12 by the UltraRoute session, to stand alone. It assumes you have none of
this month's context. It goes with `boardroom/docs/one-system-spec.md` (where the code lives)
and `docs/handoff/2026-11-12.md` (the crates and tools that move). Longer reasoning is in
`docs/spec/from-zero.md`; model details in `docs/spec/structured-routing.md` sections 3 and 4;
the physical checks in `docs/spec/verification.md`.

Three kinds of statement are kept apart below: **decided** (the owner said so), **seen**
(measured or looked at), and **intended** (my starting design, unbuilt and unproven).

## 1. What the owner wants (decided)

- A router whose boards look as if a careful person routed them and are at least as sound
  electrically. First goal: one generic algorithm, nothing bespoke per board beyond agreed
  pre-work (locked power blocks from Composer), that across the whole corpus approximates the
  designers' style. Long-term: exceed the designers, who are not all experts, so their boards
  are scored for real physical flaws too.
- Start from zero. Do not treat existing code as the starting point; the owner knows models
  anchor on what is there and asked that this bias be resisted. Code that repairs what an
  earlier layer damaged is not a foundation.
- Completion first: a quality wish never costs a connection. Hard limits come only from
  written rules or intent.
- Every stage's result is saved and drawn, so the owner and the author can both see it and
  debug it. What exists before copper (the plan) is kept with the board and can be changed.
- Frames: polar boards, hexagons, a strip shaped like a sine wave. Reshaping (make room for a
  hole, change the outline, squeeze) as its own tool.
- Judge by eye first. The owner reviews on a phone: pages must be linked from the lab's
  review page and reachable over the network, not `localhost`.

## 2. Why the old router is not the base (seen)

It draws copper one connection at a time by a maze search on a cell grid, negotiates
congestion, then runs about 17 later stages that rip up, shove, tighten, square and mend. It
finishes boards (274 of 315 on the gate tier, 85% of the corpus) and they look chaotic:

- 17% of track length off 0/45/90 against the designers' 1%; about 16% more vias; 4.5% of
  length beside another net against 11%; bus members beside a sibling 8% against 27%.
- About 70 to 100 repairs a board at write time (mended joints, bridges, splits).
- The objective is length. Nothing above one connection knows what a bus, a plane, a
  repeated cell or a frame is. Structure added afterwards as prices or passes moved scores a
  little and the look not at all.

## 3. What looking at the boards showed (seen, 2026-11-12)

The look set is at `ultraroute-lab/planned/lookset/main-9512c88/` (`tools/lookset.py`).

- **pierre, In2.Cu.** Designer: every piece 0/45/90, tracks in groups of two to four at even
  pitch with nested corners, long runs with one 45-degree turn near each end, layer mostly
  empty. Ours: a thick pair corner to corner at about 20 degrees, a pair whose members
  drift apart, tangles round vias. The designers' bundles are small: pairs and pairs of
  pairs. Fixing angles alone will not give nested corners; those come from laying wires
  together and spacing them evenly.
- **Shift-in 32.** Designer: four straight tracks on the front, no vias on the board. Ours:
  24 vias and wandering tracks.
- **1Bitsy.** The designer's raw board has a +3V3 pour on In2 and GND pours on F, In1, B. The
  benchmark's copy has none, so we laid 188 mm of track on a layer the designer kept whole
  (they laid 19 mm). **The benchmark input hides the designers' planes**: 283 of the gate
  tier's 314 boards had pours, 40 of the 57 multi-layer boards on an inner layer.
- **motionhexa** (hand-routed, 271 addressable pixels on a hexagonal lattice, 4 layers;
  `jobs/motionhexa-unified/input`, another person's board, never commit it). In1 carries one
  pair per pixel column straight across, parallel to a hexagon edge, one kink each: 86% of
  the layer in one direction. F.Cu is one short zigzag repeated about 270 times. The
  controller corner follows the rotated MCU's own axes. So: a frame is the usual 0/45/90
  turned to match the parts; one board has several frames by region; and repetition (one cell
  stamped along a lattice) does most of the work. Our route of it left 66 open and used 31%
  more track.
- **The scores were blind.** `tidy.py` called motionhexa's designer 89% off-angle. Angle and
  layer-direction scores must be measured in each region's own frame.
- **Schematics.** PCBench ships none, but 268 of 314 gate boards have the root sheet beside
  the board on GitHub at the recorded commit, and 215 ship a `.net` netlist with part values
  and each pin's name and electrical type. 263 of the roots are legacy `.sch`: KiCad 9's CLI
  exports an empty netlist from them without an error and KiCad 10's `sch upgrade` refuses
  them. Survey and fetch: `tools/pcbench_schematics.py`; data in
  `ultraroute-lab/pcbench/schematics/`.

## 4. What the ribbon spike showed (seen)

Branch `spike/ribbons` (throwaway): route a bus as one fat wire on the old grid, derive
members as offsets, join the ends to pins.

- On an empty synthetic board: both buses (24 connections) laid by construction in 3.4 s,
  clean in KiCad, and they look designed. The spine and the lanes are sound.
- On CAL430FR and pierre: almost no ribbon survived. Pierre's DDR3 spines were found (15 and
  13 lanes) and dropped because 14 of 15 and 12 of 13 lanes could not reach their pins.
  Causes: no room planned for a row of vias; lanes joined one at a time wall each other in;
  the fan from a pin row needs empty space that is not there; grouping by pin row finds only
  13 to 16% of connections, while designers' bundles mostly share a channel mid-way.
- Lesson: order and room at the ends must be part of the same decision as the route.
  Bundles bolted onto a one-connection-at-a-time router fail. Locked ribbons laid first can
  make a board worse.

## 5. The design (intended; nothing is built or proven)

Five stages. Each writes a file and a picture.

**Understand.** Board plus intent or schematic gives: a role per net (supply, ground, clock,
pair, bus member, analogue, general: from pin electrical types, part values, net classes,
names); blocks; repeats (sets of parts related by one translation or rotation with matching
pin-to-net patterns); a frame per region (the dominant axes of the parts there); a layer plan
(which layers are planes for which net, which way each signal layer runs in its region's
frame). Composer's `intent` and `recognise` crates are the start. The same analysis on a
designer's finished board recovers their choices: which layers they kept whole, their
bundles, their directions. That is both the test of this stage and the style target.

**Plan.** Before any copper, decide for every connection which gaps it passes through, in
what order, on which layer.
- A gap is the space between two neighbouring obstacles (pad, hole, board edge, locked
  copper). A natural construction is a constrained triangulation of the obstacles per layer:
  each triangle edge is a gap with a width, and so a capacity in wires at a given pitch.
- A connection is a strand: an ordered list of gaps, with its position among the other
  strands in each gap. Two strands on one layer never cross; a crossing means a layer change,
  and a via is a plan element that needs a site with room on every layer it passes.
- All strands are decided together on this small graph. Negotiation, if needed, happens here,
  where a retry costs microseconds, not on copper.
- The plan is legal when no gap is over capacity. That is checkable before drawing, and
  the picture of it (gaps coloured by how full they are, strands threaded through) is the
  main debugging view.
- Planes are honoured by the layer plan: a plane layer gets no strands unless nothing else
  works, and then it is reported.

**Repeats.** Plan and draw one cell; stamp it by the repeat's motions; check each copy; plan
only what is left.

**Draw.** Copper is a drawing of the plan. In each gap the strands are spread evenly (at
least at clearance, wider by role where there is room). Each wire is the path with fewest
bends in its frame's directions that keeps its side of every obstacle. Ends land exactly
(via centre, inside the pad). Bundles are not detected: strands that share gaps come out
side by side with nested corners, and their order at the ends is consistent because the
plan fixed it. One function writes all copper.

**Check.** The exact checker during the work, `kicad-cli` DRC as the outside judge, then the
two scoreboards.

Open questions I had not settled:
- How to solve the whole plan: sequential insertion with plan-level rip-up, or a
  flow/negotiation formulation on the gap graph. I would start with the simple one and
  measure.
- Fine-pitch escapes (QFN, BGA): the gaps between adjacent pads hold zero or one wire, so
  the escape order is nearly forced; treat a pin row's escape as one solved pattern.
- Whether a plan that passes the capacity check always draws legally in 0/45/90. Where it
  does not, the wire keeps an any-angle piece, marked and counted, never a repair pass.
- Vias: site selection and the room a row of vias needs.

Risk: this project's earlier `topo` crate (rubber bands on a triangulation) lost to the
grid: 55 to 62 of 69 connections on one six-layer board. It still routed one connection at
a time with geometry decided as it went, so it does not settle whether a whole-board plan
works. A small prototype judged by eye does.

Reshaping falls out: a changed board keeps its plan; copper is redrawn; only strands whose
gap no longer has room are planned again.

## 6. The measure (decided in outline, intended in detail)

Two scoreboards, both computed for the designers' boards and ours, kept apart:

- **Like the designers:** track length on layers the designer poured; angle discipline in
  each region's frame; share of length beside another net, and beside a sibling; vias;
  crossings of another layer's track; regularity of repeats; track length by layer.
- **Sound:** coupled length between nets of different roles; tracks over a gap in their
  reference plane; decoupling and hot-loop areas (Composer's `verify`); width against
  current; necks, acute angles, slots. Needs roles, so needs netlists.

A number is quoted only after the pictures were looked at.

## 7. If I were starting step 4 tomorrow

In this order, each ending with a page linked from the lab's review page:

1. **Pours back.** For each corpus board, take the copper zones (outline, net, layers,
   clearance, priority) from `raw.kicad_pcb` and put them, unfilled, into the prepared board.
   Traps: most raw boards are KiCad 4 or 5 and need the pcbnew conversion the harness already
   does for `processed`; legacy zone fill settings warn on load; some footprints carry their
   own zones; duplicate footprint uuids. Check ten boards by eye against `visual.png` in the
   corpus folder. Keep prepared boards out of every repository.
2. **Netlists in.** Read the 215 shipped `.net` files first (Composer has a netlist reader).
   Join to the board by reference and pad. Record per board whether the netlist matches the
   board (same nets) and skip the ones that do not. Ask before writing a legacy `.sch`
   reader: it buys sheet structure and bus labels, which matter for blocks and bundles.
3. **Designer extraction, drawn.** From the designer's board: plane layers (poured area and
   track length per layer), frame per region (histogram of track direction, modulo 45
   degrees, per neighbourhood), bundles (runs of two or more nets side by side, with their
   membership), repeats. Overlay each on the look set. Expect to be wrong at first; the
   pictures will say how.
4. **Scoreboards in the region's frame,** then rerun the old router on the gate tier with
   pours present (it has `--auto-planes`) as the honest baseline. The numbers in section 2
   will move; the multi-layer ones cannot be trusted until this is done.
5. Only then the prototype: plan, then draw, in a new crate, on the smallest two-layer look
   boards (synthetic, ULPI Pmod, Shift-in 32, CAL430FR). Build the plan picture before the
   planner is any good. Stop rule: if plan-then-draw cannot finish those boards looking
   designed, write up why before adding any repair.

Things I got wrong today that you can skip: trusting a comparison before checking what the
input board contained; reading tables before pictures; keeping the old search "because it
works" when "works" was measured by the faulty benchmark; proposing a second data format
(`blocks` in `routing.json`) when Composer's intent already held it.

## 8. How to run and review

- Heavy jobs through `~/.claude/bin/reserve`; router runs under a memory cap.
- Compare two binaries only with `pcbench.py ab` (one pool, interleaved), same seed.
- Tiers: smoke on every change, gate (315 boards) before claiming a gain, full before a
  merge that matters. No board lost is the bar for replacing the old router, with the look
  set as the deciding review; inside the rebuild, boards may be unfinished for a while.
- New work is bounded by counts, never by time.
- Report hand-fed, measured and inferred separately. Report cost (CPU, memory, lines added
  and deleted) with every result.
- KiCad is the judge; ours agrees with it or ours is wrong.

## 9. Memories copied to Boardroom: which are stale

Still true and relevant: restart-from-fundamentals, planned-routing (its last sections),
boardroom-monorepo, completion-first-intent-scored, time-budget, router-change-rules,
pcbench-general-check, kicad-coverage, fixed-copper-and-kicad-check, kicad10-flatpak,
memory-capped-runs, the second-machine note, verification-examiner, net-widths, circuit-composer,
user-notes-2026-11, phone-review-links, lab-page, test-boards (the boards, not the recipes),
easy2350-boardroom, roadmap (as history).

Stale, about the old router or old tooling: via-reduction, tidy-parallel-goal,
remediation-progress, worktree-archive, wgpu-nvidia-backend, webkitgtk-sharedarraybuffer,
phone-preview-obsolete, agy-gemini-agents, freerouting-parity (the result stands, the method
is the old router's), standing-focus-trapped-escapes (the earring is still a target; the
approach listed there is not), ultraroute-layout (describes this repository, not Boardroom),
pcb-skill-readonly (recheck in Boardroom). In planned-routing, everything before the
2026-11-12 entries describes plans that were dropped.
