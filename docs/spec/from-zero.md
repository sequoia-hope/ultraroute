# Routing from zero: direction agreed 2026-11-12

Status: direction approved by the user on 2026-11-12 ("all approved"). Nothing here is built.
This supersedes the order of work in `docs/spec/structured-routing.md` (written the same
morning): that spec improved the generator inside the current pipeline; this one starts again
from what a designer does. Its model sections still hold where this document says so.

## Why start again

One day of looking at the designers' boards beside ours (the look set) showed gaps that were
not about the search algorithm:

- **The judge was wrong.** PCBench's input boards have the designers' pours removed (283 of
  the gate tier's 314 boards had pours, 40 of the 57 multi-layer boards on an inner layer).
  For weeks "we match the designers" was measured on boards the designers never drew.
- **The scores were blind.** They know only the board's x and y: motionhexa's hand routing,
  a lattice turned to the hexagon, reads as 89% off-angle.
- **The router is told almost nothing:** no planes, no schematic, no idea which net is a
  supply or a clock, no idea that 270 pixels are one cell repeated.
- **Structure added afterwards fails.** The ribbon spike laid good bundles and could not
  join them to their pins: order and room at the ends were never part of the decision.

The current router starts by drawing copper one connection at a time and recovers structure
through 25 stages of repair. Code that repairs what an earlier layer damaged is not a
foundation. There are no users; nothing of the current router has to survive. The user's
standing instruction: do not treat the code that exists as the starting point, and look at
pictures before trusting a table.

## What a designer does, and so what the system does

1. **Understand.** From the board and its schematic, as explicit data that can be drawn and
   checked: each net's role (supply, ground, clock, pair, bus member, analogue, general, from
   pin types, part values and classes), blocks (power sections, sheets), repeats (cells
   related by a translation or rotation), frames by region (how the parts sit), a layer plan
   (which layers are planes, which way each signal layer runs). The same analysis run on the
   designer's finished board recovers what they chose: the test of this stage and the style
   target.
2. **Plan the whole board before any copper.** For every connection, which gaps between
   obstacles it passes through and in what order, on which layer, with the capacity of each
   gap checked. All nets are decided together on a small graph; negotiation, if any, happens
   here.
3. **Solve patterns once.** A repeated cell is planned and drawn once and stamped by the
   repeat's motions, each copy checked.
4. **Draw by construction.** Wires that share a gap are laid evenly at pitch, in the local
   frame's directions, ending exactly. Bundles are not detected: nets that share a way come
   out side by side, and their order at the ends is already consistent. The wire record, one
   writer, the wire fit and frames of `structured-routing.md` (3.1, 3.2, 3.3, 3.6) are this
   stage.
5. **Check.** The exact checker, then the physical measures.

Kept from today's code: the board model and file reading, the checker that agrees with
KiCad, the benchmark harness, the lab and the look set. The cell search survives at most as a
helper for leftovers. Power sections come from upstream as locked blocks
(`structured-routing.md`, S6).

Risk, stated plainly: planning a whole board topologically is hard, and this project's
earlier `topo` crate lost to the grid. That attempt still routed one connection at a time, so
it does not settle the question; a small prototype judged by eye does.

## The measure

Two scoreboards, kept apart, both computed for the designers' boards and ours:

- **Like the designers.** The same structural features extracted from both: planes kept
  (track length on a layer the designer poured), direction per region in that region's frame,
  bundling, vias, regularity of repeats. First goal: one generic algorithm, nothing bespoke
  per board beyond agreed pre-work such as power blocks, that across the corpus approximates
  the designers' style.
- **Sound.** Physics that does not depend on the designer: coupled length between nets of
  different roles, return paths and plane gaps, decoupling loops, width for current,
  manufacturability. The designers are scored too: they are not all experts, their flaws
  should be found, and the long-term aim is to exceed them. Needs roles, so needs schematics.

Looking at the look set, designer beside ours, is a required step before any number is
quoted, for the author as much as for the user.

## Order of work

1. **Ground truth. No router work.**
   - Benchmark boards prepared with the raw board's pour outlines restored, unfilled.
   - Schematics and netlists pulled in (`tools/pcbench_schematics.py`: gate tier 268 of 314
     have the root sheet beside the board on GitHub; 215 ship a `.net` with part values and
     pin types; 263 roots are legacy `.sch`, which KiCad 9 and 10's command lines cannot
     convert). Read the shipped netlists first. Ask Circuit Composer what it already derives
     before writing a reader for the legacy format.
   - A designer-extraction tool: layer roles, frames by region, bundles, repeats, drawn as
     overlays on the look set and counted over the corpus.
   - The two scoreboards, frame-aware, on the designers' boards and on main as the baseline.
2. **Prototype from zero, in a new crate,** using only the board model and the checker: plan
   first, then geometry, on the smallest two-layer look-set boards (synthetic, ULPI, Shift-in,
   CAL430FR). Judged against the designer by eye and on both scoreboards. Stop rule: if the
   plan-then-draw prototype cannot finish those boards looking designed, write up why before
   adding any repair.
3. **The current router is frozen as a baseline.** No further stages are built into its
   pipeline; the wire record and wire fit are built in the new crate, not adapted into the
   old one. `spike/ribbons` and the plan/* branches stay as records.
4. Then, in the new system: multi-layer and planes, repeats (motionhexa's cell), frames
   (turned, polar, spine), roles and the physical measures, larger boards.

## Records from 2026-11-12

- Look set: `tools/lookset.py`, `tools/lookset.json`; pages under
  `ultraroute-lab/planned/` (index there, linked from the lab's front page).
- Scoreboard: `tools/scoreboard.py`. Its layer-coherence figures are not to be relied on
  until the boards have their pours back.
- Ribbon spike: branch `spike/ribbons`; result in `structured-routing.md`, S4.
- Motionhexa's hand routing as the frame reference: `structured-routing.md`, 3.6.
- Circuit Composer: told of the power-blocks decision; a `blocks` list in `routing.json`
  for hot-loop roles proposed, not agreed.
