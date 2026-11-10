# Planned routing: intent, bundles, lanes, frames and one copper writer

Status: proposed 2026-11-09. Phase 6 in `ROADMAP.md`. This spec answers two things together:
the requests in the round-pendant notes (`docs/research/round-pendant-user-notes-2026-11.md`,
a copy of the round-pendant project's own requests file) and why our boards look
worse than a designer's when both are DRC-clean. The two have the same root.

## 1. The root

ultraroute routes one connection at a time on a fine cell grid (shortest airline first, then
negotiation, rip-up, shove), and then improves what it drew with passes that each fix one kind of
defect: tightening, via removal, cleaning, refinishing, squaring, widening, joint mending. Nothing
above the single connection is ever modelled, and nothing below the cell path is either:

- **Above:** no net roles (clock, bus, matrix, power), no groups of nets travelling together, no
  channels between regions of the board, no plan of which layer runs which way. A bus of eight
  nets is eight unrelated searches; each finds its own cheapest path through what the earlier
  ones left.
- **Below:** no wire model. A route is cells, turned into many short segments, then edited in
  place as copper. Each repair adds copper next to what it repairs, because the copper is the only
  record of the route.

Everything the user sees as quality is therefore patched after the fact, and every patch is local.
That is why the round-pendant project needed two wrapper scripts (staging and 20 cleanup passes),
and why our boards read as spaghetti even at 0 open, 0 DRC.

## 2. What the requests say, by root

Sections 1 to 6 of the notes were read against `a3ba419`, 7 and 8 against `3de3d83`; main is 76
commits past that. Spot checks on main (1277a33): section 5's y-sign is fixed (`--place`,
`--rip-region` and `--via-avoid` all take KiCad's y-down frame, and `--rip-region` prints what it
took out); stage budgets now include `KeepBack` and `Rescue` (lib.rs `STAGES`), which address
section 1 at least in part. The rest are open until checked (task A below checks every one).

| Request | Symptom | Root (level in section 4 that owns it) |
|---|---|---|
| 1, 4: stage starvation, seed budgets | finishing passes run out of time; users orchestrate seeds and a plough finisher outside | no run plan: stages share one deadline greedily (Plan) |
| 2, 7.1-7.5: necks, slivers, off-centre ends, 25 µm fragments, dangling vias, repair triangles, 156 loops | KiCad errors on boards the router calls clean; cleanup script needed | no wire model; repairs add copper; necks judged per pair, not on merged copper (Geometry, Copper) |
| 3: critical nets crowded by matrix nets; `via_count (max 0)` warning; length rules not routed to | wrapper pre-routes clock/QSPI/USB | no net roles; `ledger.first` only from `--regions` (Intent) |
| 6, 7.6, 8: small zones cut into islands; pour landings on the edge; zone-joined vias seen as open | rule areas mirrored by hand; ring tie script | zones are passive copper, not owned areas or connections (Intent, Copper) |

## 3. What designers do that we don't

Seen side by side (CAL430FR and Own-Mailbox pierre, designer vs ours on main; renders made with
`kicad-cli pcb export svg`) and measured:

- **Buses travel as ribbons.** On CAL430FR the designer runs two 8-track ribbons, one around the
  bottom on F.Cu and one across the top on B.Cu, each at constant pitch, splitting off a few
  tracks where they reach their pins. Ours routes the same nets as separate paths that cross each
  other, interleave layers and change layer often (104 vias vs 72). tidy.py on the gate tier:
  bus_bundled 18.5% (designers) vs 6.0% (ours) before squaring.
- **Layers have roles.** On pierre the designer's inner layers are 64% vertical (In1) and 62%
  horizontal (In2). Ours: In1 52% horizontal, 3% vertical; In2 54% horizontal: both inner layers
  run the same way, so crossings cost vias or detours. In the code, the layer preference
  (`step_cost`, 10 along vs 12 across) applies only to two-layer boards (`route.rs:215`) and only
  in single routing: negotiation sets `directional = false` (`grid.rs:1431`).
- **Angles are 0/45/90.** Designers' off-45 share is 0.1%; ours was 72.7% before squaring and is
  16.7% after it (gate, 13947cf). Squaring is a post-pass that cannot move a bus as one.
- **Long diagonals across regions are rare.** Ours has single tracks crossing a whole board region
  at an angle (pierre, bottom right; inner layers 20-26% any-angle).
- **Power is wide** (separate spec: net widths, `--power-width`).

Two-layer boards are less clear-cut: designers there do not split strictly by direction
(CAL430FR designer: both layers about 45% horizontal); they use one layer as the main layer and
the other for crossings. Layer roles must be chosen per board, not imposed.

## 4. The model: five levels

A run builds each level from the one above and never skips one. Lower levels may report back up
(a bundle that will not fit splits; a lane that cannot be laid falls back to the cell search),
but the result is always written from the wire model, never from patched copper.

### 4.1 Intent

What each net is, from the board's own data first (net classes, `.kicad_dru`, `--regions` /
`routing.json` from Circuit Composer), then recognised:

- `NetRole`: power, ground, clock/crystal, high-speed (USB, QSPI, diff pairs), bus member,
  matrix row/column, general.
- **Groups:** nets that should travel together. A bus is three or more nets between the same two
  parts on neighbouring pins at both ends (tidy.py's rule). A matrix group is the rows or columns
  of an LED/key matrix. A group has an order at each end (the pin order).
- **Priority:** length-limited and `via_count (max 0)` nets, crystals, clocks and diff pairs
  route first (fixes request 3: `ledger.first` filled from rules as well as `--regions`).
- **Owned areas:** a small non-plane zone (under ~5 mm²) on its layer belongs to its net; other
  nets treat it as an obstacle unless allowed (request 6). A zone that joins items counts as a
  connection before anything is called open (request 8).

Intent never makes a routable board fail: roles change order and price, hard limits come only
from written rules (memory: completion first, intent scored).

### 4.2 Plan

A coarse, board-level decision, made once before detailed search, cheap to redo:

- **Regions and channels.** Parts cluster into regions (connected courtyards plus a margin); the
  free space between them is a graph of channels with capacity per layer (tracks that fit across).
  The `topo` crate's triangulation of the copper is the natural base: a channel is a run of
  triangle edges between obstacles, its capacity the edge length over track pitch.
- **Bundles.** Connections are grouped by the pair of regions they join (and by group from 4.1).
  A bundle is routed through the channel graph as one flow with width n x pitch. Where members
  leave for different regions the bundle splits: a bundle is a tree, which is the "little
  subgroups that split off" of a designed board. Force-directed edge bundling (section 5) is one
  way to find these trees from the airlines.
- **Layer roles.** Per board: for each signal layer, a preferred direction (horizontal, vertical,
  or none) and a role (main, crossing, escape-only, plane). Default for 4+ layers: alternate H/V
  over the non-plane signal layers; for 2 layers: the layer with more pads is main, the other
  crosses, with direction preference only where it lowers vias on that board.
- **Layer assignment per bundle, after the 2-D plan.** A bundle's horizontal legs go to an H
  layer, vertical legs to a V layer, with its vias lined up as a row at the turn, all members
  changing layer together. This is the step Stage 12 lacked (it assigned layers per GCell step,
  per net, and laid as many vias as the whole base result).
- **Ordering.** Within a bundle, the member order along each channel is fixed so members do not
  cross inside it: the order is read from the pin order at the ends. Where the two ends' orders
  disagree, the plan places the crossings deliberately (a via pair per crossing, or a swap at a
  layer change) instead of letting them fall where the search puts them.
- **Run budget.** The plan also splits the time: search stages get the run's time minus a reserve
  that tightening, vias, cleaning, refinish and the writer always keep (request 1), and a portfolio
  of seeds / a plough finisher is a run mode, not a wrapper script (request 4).

### 4.3 Topology

Each connection becomes a path through the triangulation (which side of each obstacle it passes),
following its bundle's corridor and order. The detailed router of today (A* on cells, negotiation,
rip-up, shove) still runs, now with the plan as prices: inside the bundle's corridor and on the
layer the plan gave cost less; a sibling's track at exact pitch alongside costs least (a "lane"
discount). Anything the plan cannot fit is routed as today, unplanned: the plan is advice, never
a wall. A connection the plan would lose is routed free of it.

### 4.4 Geometry

Paths become octilinear wires: a bundle's members are offsets of one spine at pitch, so their
bends line up (the knees squaring tries to make after the fact); single wires are 0/45/90 with
the fewest bends that keep clearance. Relaxation (section 5) evens out spacing within channels and
aligns parallel runs while keeping the topology.

The wire model holds, per connection: its end items, its vertices, its layer per piece and its
vias. Every later edit (tightening, via removal, squaring, mending) is an edit of this model,
never an added piece of copper.

### 4.5 Copper

One writer turns wires into tracks and vias, with exact endings, and one checker judges the result
as KiCad does:

- A wire ends at a via's centre, or at the nearest point of its pad shrunk by half the track
  width (true pad outline: rotated rectangles, rounded corners, custom shapes); a same-net via it
  crosses is a vertex of the wire (requests 2, 7.2).
- Collinear same-net, same-width, same-layer pieces merge on write; zero-length pieces never
  exist (7.5).
- Necks and acute throats are judged on the merged copper of the net, not per pair with a
  `firmly_linked` exemption (7.3); a neck is mended by moving the wire, not by adding a bridge.
- A repair replaces what it repairs; after repairs the whole board, adopted copper included, is
  pruned of cycles, stubs and vias joining fewer than two layers (7.1, 7.5).
- A pour landing ends inside the pour by its minimum thickness (7.6).

### 4.6 Frames, symmetry and similarity

"Horizontal", "vertical" and "45 degrees" are one choice of frame. A round board reads better
routed in rings and spokes, with arcs; a hexagonal board as six identical triangular sectors; a
pentagon with five-fold symmetry; a matrix of LED cells as one cell routed and repeated. So the
Plan and Geometry levels never speak of x and y directly. They speak of a **frame**, and a board
may carry a **symmetry**.

**Frame.** A map from board space to a canonical space, giving at every point the local axes that
"straight" and "turn" mean:

- *Cartesian* (today): axes x and y; octilinear = multiples of 45 degrees.
- *Polar* about a centre: axes radial and tangential. A tangential run is an arc of constant
  radius; a radial run is a spoke. Layer roles become "ring layer" and "spoke layer" instead of H
  and V; bundles travel as concentric ribbons; "octilinear" becomes spoke, ring, or the 45-degree
  spiral between them.
- *Sector-local*: inside a sector of a symmetric board, the sector's own axes (for a hexagon, the
  sector's edge directions at 60 degrees), so the same rules apply rotated in each sector.
- *Per-region frames*: a board can mix them (a round LED field around a cartesian MCU area); each
  region of the plan carries its own frame, and channels between regions blend.

Everything frame-dependent goes through the frame: the layer-direction price in the search (the
angle between a step and the local axis, not `di`/`dj`), what squaring calls square, what
tidy.py calls off-axis, and how the writer draws a run (a tangential run in a polar frame is
written as an arc; the writer already has `--arcs`). The cell search, the obstacle maps and the
checker stay cartesian: they are exact and fast there, and a frame only changes prices and the
final geometry. That keeps the change deep (it reaches the search's step cost) without forking the
router per board shape.

**Symmetry.** A group of rigid motions (rotations by 360/n degrees about a centre, mirrors,
translations) under which the placement maps to itself: each part to a part with the same
footprint and each net to a net with the same role, found by matching parts and pin-to-net
patterns, or declared (`routing.json`). With a symmetry:

1. The plan picks a fundamental domain (one sector of n) and the nets that live wholly inside it.
2. Those nets are routed once, inside the domain, with the seams (sector edges) as soft walls.
3. The result is copied to every other sector by the group's motions, each copy checked; a copy
   that breaks a rule (a part that is not quite symmetric, a mounting hole in one sector only) is
   routed on its own from the copy as a starting point.
4. Nets that cross sectors (shared buses, power rings, the controller's connections) are routed
   afterwards, symmetrically where their ends are (a ring is one net shaped by the polar frame),
   otherwise as today.

**Similarity.** The same with a partial match: repeated sub-circuits (channels, LED cells, driver
blocks) related by a motion, though the board as a whole is not symmetric. Route one instance,
copy to the others, as KiCad's multichannel tools do by hand.

Symmetry and similarity are plan-level advice like everything else: a board must route at least as
well with them as without, and a copy is kept only where it is legal. Score: the share of routed
length that maps onto same-role copper under the group's motions (ours and the designer's), arc
share for polar frames. Test boards: the round pendant (a sunflower: polar frame, but golden-angle
spacing has no n-fold symmetry, so similarity, not symmetry), the earring (polar), motionhexa
(`jobs/motionhexa-unified`, hexagonal), and synthetic round, hexagonal and pentagonal boards made
by rotating one sector.

## 5. Springs: what the force-directed idea is good for

The suggestion: "force-directed layout, like treat the traces as gentle springs". Two well-known
methods match it, each at a different level, and one place it does not fit.

**Plan level: edge bundling.** Force-directed edge bundling (Holten and van Wijk, 2009) makes
neat bundles out of a tangle of straight edges: each edge is subdivided, its points attract the
matching points of compatible edges (similar direction, length, position) and springs along the
edge keep it smooth. Run on the airlines with obstacles as repellers, the result is a set of
bundle trees: where airlines merge and where they split. That is exactly the bundle and split
structure in 4.2, found without a channel graph. It is cheap (hundreds of airlines, a few hundred
iterations) and its output is advice, so it cannot cost a connection.

**Geometry level: elastic bands.** Once topology is fixed, each wire is a chain of points with
springs along it (tension: shorten), repulsion from obstacles and other nets' copper (spread
evenly in a channel instead of hugging one wall), attraction to same-bundle neighbours at pitch
(parallel lanes), and an angle term pulling pieces to 0/45/90 and to the layer's direction. Moves
that would cross an obstacle are not allowed (the band stays on its side of everything), so
topology and connections are kept by construction; each move is checked and a step that breaks a
rule is undone. This is rubber-band routing (the `topo` crate already does the tension part) plus
wire spreading as used in IC layout for yield.

**Not as the router.** Forces do not know clearance as a hard rule and do not decide which side
of an obstacle to pass, so a spring layout cannot be the router of record: it would trade
connections for looks, which completion-first forbids. Springs relax a routed board and shape
the plan; they never route.

Both spring stages must keep the router's change rules: work caps (iterations, points), not time
slices; scope (they act on bundles and channels that exist); scored.

## 6. Why this is not Stage 12 again

Stage 12 (REMEDIATION, `remed-12-notes.md`) planned every net on 1 mm GCells with per-net soft
corridors and stopped with vias +15.5% and 8 fewer clean boards. Its two mechanisms and what this
spec does instead:

- **GCell layer assignment laid as many vias as the base result.** Here layers are assigned per
  bundle, after a 2-D plan, with vias as rows at bundle turns, priced against a one-layer detour.
- **Corridors slowed negotiation 2-3x** (regions turn off grid patching; the octile heuristic
  ignores corridor prices). Here the plan prices only planned nets, with discounts inside rather
  than penalties outside (the heuristic stays admissible), and nets the plan does not cover route
  as today.

And the stop rule stays: each step is opt-in, measured against main, and stopped if it costs
connections.

## 7. Measures

Tidiness and hygiene become scores in pcbench next to clean count, for ours and the designer's:

- Existing (tools/tidy.py, tools/dfm.py): off45, doglegs, bundled, bus_bundled, turns, detour,
  slots, acute.
- New, tidy.py: **layer coherence** (per layer, the share of length along its dominant axis; per
  board, the mean over signal layers, weighted by length), **crossings** (pairs of different
  nets' wires that cross in plan view per connection: bundles reduce them), **bundle continuity**
  (for each bus, the share of its length where all members share the same layer and run at
  constant pitch), **vias per bus member**.
- New, hygiene audit (the round-pendant cleanup script's checks, in the router's report and in
  pcbench): merged-copper necks under min connection, acute throats, collinear fragments, segments
  under 10 µm, same-net loops, dangling vias, ends off via centre, pad grazes.

## 8. Order of work

Each step lands alone, opt-in until its gate passes, A/B on the gate tier against main. A
quality step passes when it loses no board, costs under 10% CPU, and moves its own measure.

- **P0. Copper writer and hygiene (4.5) and the request audit.** Every ask in the notes checked
  against main and either fixed or marked fixed with the commit. Goal: the round-pendant cleanup
  script has nothing left to do on p1-66, p2-132-usb6 and round-pendant-e.
- **P1. Intent (4.1).** Roles, groups, priority from rules, owned small zones, zone connections.
- **P2. Layer roles (4.2).** Per-layer direction on every board with 3+ signal layers, kept
  through negotiation; score: layer coherence and vias.
- **P3. Bundles with lanes (4.2, 4.3).** Groups route consecutively in pin order with a lane
  discount alongside the previous member; score: bus_bundled, crossings, bundle continuity.
- **P4. Relaxation (5, geometry).** Elastic-band spacing and alignment after squaring.
- **P5. The plan proper.** Edge bundling over airlines and regions/channels from `topo`; bundle
  trees with layer assignment per bundle; the run budget as part of the plan.
- **P6. Wire model end to end (4.4).** Tightening, squaring and repairs as edits of wires; the
  copper writer as the only place copper is made.
- **P7. Frames (4.6).** The frame abstraction behind layer direction, squaring and tidy.py;
  a polar frame with arcs written for tangential runs. First step, landable early: the search's
  direction price and squaring read a frame, cartesian by default (byte-identical), polar by flag.
- **P8. Symmetry and similarity (4.6).** Detect or read the group; route a fundamental domain and
  copy; repeated sub-circuits routed once.

P0, P2, P3, P4 and P7's first step are independent enough to run at once; P5 builds on P1 and P3;
P6 is the refactor that makes P0's fixes structural rather than a final pass; P8 builds on P5 and
P7.
