# Structured routing: specification and roadmap

Status: agreed by the user 2026-11-12 with the decisions in section 7. Nothing here is built. It is the
replanning of Phase 6 that `docs/spec/planned-routing.md` was put on hold for, and would replace
that spec's sections 4.2 to 4.5 and its order of work. Its sections 4.1 (intent) and 4.6
(frames, symmetry) stand and are scheduled last here. The reasons are in
`docs/research/diagnosis-2026-11.md`.

## 1. Aim

A board routed by ultraroute should look as if a careful person routed it, and be at least as
sound electrically, without losing any board the current router finishes.

Stated as measures on the gate tier, against the designers' own routing of the same boards
(today's figures in brackets, designers then ours):

| Measure | Target | Today |
|---|---|---|
| Boards clean | no board lost at any stage | 274 of 315 |
| Off 45 degrees | under 2% | 1.0%, 16.7% |
| Vias | no more than the designers' | 6,873, 7,964 |
| Length beside another net (`bundled`) | 10% or more | 10.7%, 4.4% |
| Bus members beside a sibling | 25% or more | 25.7%, 7.9% |
| Layer coherence, boards with 3+ signal layers | 60% or more along the layer's axis | about 63%, about 53% both layers the same way (pierre) |
| Repairs at write (mends, bridges, splits) | 0 | 68 a board |
| Necks and narrow joints reported | 0 | 1,057 over the tier |
| Router code | smaller at the end than at the start | 31k lines in grid and driver |

Wirelength is allowed to rise toward the designers' (they use 22% more than we do). It is no
longer a goal except for nets whose role makes it one.

## 2. Principles

1. **Structure is built, not repaired.** The default generator lays wires that are already
   octilinear, already end exactly, and already travel together where they share a way. A
   pass that fixes a defect class is a sign the generator is wrong.
2. **One router, no old path kept beside it.** (User decision 2026-11-12, replacing "today's
   router is the fallback".) A connection that cannot join a ribbon is routed alone by the
   same system: search, then wire fit, on the wire record. The current pipeline is not kept as
   a second path behind a flag or a failure branch. What this means for the no-board-lost
   gate while the new system is being built is open (section 7).
3. **The grid decides topology, not shape.** The cell search stays as the tool that finds which
   side of each obstacle a connection passes and whether it fits. The copper's shape comes from
   fitting a wire to that answer.
4. **One record, one writer.** A wire is the only record of a route. Copper is derived from
   wires in one place.
5. **Every cost has a physical or human reason, written down** (section 4). No term exists
   only to move a score.
6. **Each stage deletes what it replaces.** A stage is not done while the code it supersedes is
   still reachable by a flag.

## 3. The model

### 3.1 Wire

One per connection, kept from the moment it is routed until the board is written:

- its two ends, each an item (pad, via, track point, zone) and an anchor on it;
- a list of vertices with a layer per piece and a width per piece;
- its vias, as vertices where the layer changes;
- its net, its role (3.5) and the ribbon it belongs to, if any (3.3).

Invariants the type enforces, so no pass can break them:

- every piece runs in one of its frame's directions (3.6; for the cartesian frame 0, 45 or 90
  degrees), unless the wire is marked any-angle by a written reason (a pad exit that has no
  legal way in the frame's directions);
- an end lies at its anchor: a via's centre, a point inside the pad at least half the track
  width from its edge, a vertex of the wire it joins;
- no zero-length piece, no two collinear neighbours, no vertex visited twice.

Every pass that changes a route edits vertices of a wire. No pass inserts a board item.

### 3.2 Writer

One function turns all wires into tracks and vias at the end of the run, and the same function
gives the checker its view during the run. With the invariants above it has nothing to mend. The
existing check (`board::check`) is then extended to judge necks on a net's merged copper, as
KiCad does, so the router and KiCad agree on what clean means.

Replaces: `mend_joints`, `mend_pad_joints`, `mend_grazes`, `mend_via_grazes`,
`mend_via_pad_grazes`, `split_at_joins`, the post-mend duplicate and dangling passes, most of
`necks.rs` and the narrow-joint reports.

### 3.3 Wire fit

Given a connection's cell path from the search, produce the wire: the octilinear polyline with
the fewest bends, then the least length, that stays inside the free cells around the path
(same side of every obstacle) and meets both anchors with a legal pad exit.

- Method: a search over (cell, heading) restricted to the corridor of the cell path, cost =
  length + a bend price. The corridor is a few thousand cells, so the eightfold state is cheap
  and the whole-board search keeps its 9 bytes a node.
- Pad exits are part of the fit: leave a pad along its axis or at 45 degrees through its
  centre, never across its rim at a slant.
- Bounded by work (corridor cells), not time. If no octilinear wire fits, the connection keeps
  an any-angle wire, marked, and is counted.

Replaces: `pull_tight`, `pull_taut`, `smooth_staircases`, `straighten_turn_backs`,
`octilinear_watch` and its slot, dogleg and slant checks, and the `Squaring` stage: about 2,300
lines and two stages.

### 3.4 Ribbon

A ribbon is a set of wires that travel side by side at a fixed pitch for part of their way. It
is built as one object:

1. **Find candidates** from geometry, not only from names: connections whose two ends lie in
   the same pair of part neighbourhoods and whose airlines are close in direction and position.
   Buses by pin adjacency are the easy case; any connections sharing a way qualify.
2. **Route the spine** as one fat wire: the existing search with the half-width set to
   n x pitch / 2. The grid already routes by net-class half-width, so this needs no new search.
   Layer changes of the spine become a row of vias, all members changing together.
3. **Derive members** as offsets of the spine, in the order that makes the ends planar. Where
   the order at the two ends disagrees, the ribbon is split into the largest sub-ribbons that
   agree, and the rest route alone.
4. **Join the ends** to their pins in order. For a pin row this is river routing, which is
   planar and solvable exactly; `prefanout`'s escapes become ordered escapes per row.
5. **Fall back by halving.** A spine that does not fit is tried as two ribbons of half the
   members, down to single wires (search and fit, the same system). So a ribbon costs a bounded number
   of extra searches and can never leave a connection worse placed than routing it alone
   after its siblings.

A ribbon stays an object afterwards: rip-up, shove and spacing move it as one, or split it and
say so.

Whether a set of nets may be ribboned at all is a role decision (3.5): a clock or an analogue
input never joins one.

### 3.5 Role

Per net, from written data first (net class, `.kicad_dru`, `routing.json`), then recognised:
power, ground, clock or crystal, high-speed pair, bus member, analogue, general. A role sets,
and only sets: route order, whether the net may join a ribbon, its spacing to other roles, its
via price, and whether its length is a goal. Hard limits still come only from written rules
(completion first, intent scored).

### 3.6 Frame

Asked for by the user (2026-11-09 and 2026-11-12): polar routing on round boards, 60-degree
sectors on hexagons, and in general boards whose "along" and "across" are curves, such as a
thick strip shaped like a sine wave. So no part of the wire model speaks of x and y. It speaks
of a frame, and the frame is in the interface from the first stage, with cartesian as the
only implementation at first.

A frame is a map between board space and a flat canonical space (u, v), plus the set of
directions a wire may run in there:

- *Cartesian*: u, v = x, y; directions every 45 degrees.
- *Polar* about a centre: u along the ring, v along the spoke. A run along u is written as an
  arc, along v as a spoke.
- *Sector*: one wedge of an n-fold board with its own axes; a hexagon's sectors use
  directions every 60 degrees (and the 30-degree set between them) in place of every 45.
- *Spine*: u is distance along a centre curve, v the distance from it along its normal. Lines
  of constant v are parallel curves of the spine, so spacing across the strip stays true. A
  sine-wave strip is a spine frame; so is any curved flex tail. It holds only where the
  spine's radius of curvature exceeds the strip's half-width. A ribbon (3.4) is the same
  object at a smaller scale: a spine and offsets.
- A board may carry several frames by region. The frame comes from where the parts sit and
  how they are turned, or from written intent, not from the outline alone: plan/frames found
  that a polar frame on a round board whose parts sit on an x/y grid costs vias and doglegs.

How it is used: the wire fit (3.3) maps its corridor into (u, v), fits there with the frame's
directions, and maps the result back; the writer draws each piece as the line, arc or
polyline its frame makes of it. The whole-board search, the obstacle maps and the checker stay
cartesian and exact; the search takes the frame only as the local axis for its direction
price. Spacing in (u, v) is not spacing on the board where the map stretches (rings widen
outward), so pitch and clearance are always judged on the board.

Symmetry and similarity (route one sector or one repeated cell, copy by the group's motions,
check each copy) are as in `planned-routing.md` 4.6 and work on wires.

## 4. The cost model

One table, used by the whole-board search, the wire fit and the spacing pass, so they agree.

| Term | Reason | Where it acts |
|---|---|---|
| Length | resistance, delay, room | everywhere; weight by role, low for general nets |
| Via | impedance step; return current changes plane; drill cost | search; price by role, higher where no return via or plane is near |
| Bend | each is a discontinuity and reads as noise | wire fit (exact); search (approximate, by heading of the parent step) |
| Against the layer's direction | adjacent layers at right angles couple least and keep channels open | search, in every stage including negotiation; direction chosen per board and per layer, none on a layer where it costs vias |
| Beside a neighbour at pitch | shares a channel, leaves room for others | ribbon construction; a discount in single routing only for nets whose roles allow it |
| Closer than room requires | coupling falls with spacing; minimum clearance is a limit, not a target | spacing pass (stage S5) |
| Coupled length to a different role | crosstalk from an aggressor to a victim | spacing by role; reported as a score |
| Crossing a gap in the reference plane | the return path breaks | search penalty where planes are known; reported |
| Congestion | negotiation | as today |

Prices that lower a step below its base cost break the search's heuristic. Where a discount
is wanted, the base rises and the favoured step keeps the old cost, so A* stays exact.

## 5. Roadmap

Each stage is one branch, one author, measured on the gate tier against main, and reviewed by
eye on the look set (S0) before it merges. A stage that replaces passes deletes them in the
same merge. Stages are listed in order; S3 can run beside S2.

### S0. Ground: stop, measure, tidy the repo

No router behaviour changes.

- Decide every unmerged branch (table in the diagnosis, section 7); remove merged worktrees.
- Repo hygiene: the top-level logs, the eight `target-*` directories, `REMEDIATION.md` and
  `REVIEW.md` tracked or archived; `cleanup.rs` split by job with no logic change.
- **The look set:** about twelve boards (CAL430FR, pierre, fmcw-radar-mcu, easy2350, a matrix
  board, the synthetic bus board, the earring, round-pendant, plus four from the gate tier
  chosen for variety), each rendered designer beside ours on one lab page, per layer. This is
  the review every later stage gets, and what the friend who tried it would be shown.
- **Scores added to pcbench:** layer coherence, crossings, repairs at write (from the log),
  any-angle share by cause, share of length at minimum clearance where more room exists. With
  the existing ones these become a scoreboard with the designers' figures beside ours.
- Done when: the page and the scoreboard exist for main, and the branch list is empty or
  decided.

### S1. Wire record and one writer (3.1, 3.2)

Built together with S2 as one stage (section 7); the lists below are kept apart only for
reading. "Existing passes are adapted" applies only to passes that survive S2.

- The search returns a wire; every stage that now holds `Vec<ItemId>` (`Ledger.laid`, `fanout`,
  `pairs`) holds wires. Existing passes are adapted to edit wires. Exact ends by construction.
- Gate: no board lost; repairs at write 0; necks and narrow joints at or below main; KiCad and
  our check agree on the tier.
- Deletes: the mend passes and their second cleanup (about 1,500 lines).
- Risk: this touches every stage. It is the stage most likely to take longer than planned and
  the one everything else stands on. It cannot be byte-identical to main, so the gate tier is
  its only proof; expect to run it several times.

### S2. Wire fit (3.3)

- Gate: no board lost; off 45 under 2% with the any-angle remainder listed by cause; doglegs,
  acute and slots no worse than main; CPU no higher.
- Deletes: tightening, squaring and their helpers (about 2,300 lines, two stages).
- Kill rule: if off 45 cannot get under 5% without losing boards, stop and report why before
  adding any exception.

### S2b. A second frame (3.6)

- Straight after S1+S2, not last: the polar frame through the wire fit and the writer, on the
  earring, the round pendant and the synthetic round board. Its purpose is to prove the frame
  interface is real while the code that uses it is still small. plan/frames' arc writer and
  round-outline detection are read for what can be reused, with its audit's corrections.
- Gate: cartesian output unchanged by the interface; on the three boards no connection lost
  against cartesian, arcs pass KiCad DRC, and the look judged by eye.

### S3. Objective (section 4)

- Layer direction on every board through every stage, chosen per board; bend price in the
  search; via price by role.
- Gate: no board lost; vias at or under the designers' total; layer coherence at target; CPU
  within 10%.
- Measured 2026-11-12 (`tools/scoreboard.py`, 57 gate boards with tracks on 3+ layers, main
  9512c88): along the layer's axis designers 65.7%, ours 63.9%; neighbouring layers on the
  same axis designers 20.3%, ours 23.4%. The gap pierre shows is not general, so the layer
  direction term has little to win on this tier and should be judged by vias and crossings
  (designers 7.7, ours 9.9 per 100 mm), not by coherence.
- Kill rule per term: a term that loses a board is made a tie-break (applied only between
  equal-cost paths) or dropped. plan/layers' sweep is not reused; it was not run on its code.

### S4. Ribbons (3.4)

- **Spike first, one week, throwaway code:** fat-wire spine and offsets on the synthetic bus
  board, CAL430FR and pierre's DDR3 bus, judged by eye and by whether CAL430FR's two open
  connections and pierre's DDR3 close. This is the least proven idea here. If the spike does
  not produce ribbons that look right on those three boards, do not build it; write up why.
- Then: candidates from geometry, ordered escapes, halving fallback, ribbons as objects in
  rip-up and shove.
- Gate: no board lost; `bundled` 10% or more; bus members beside a sibling 25% or more; shove
  and rip-up time down on boards with ribbons.

### S5. Spacing

- An elastic pass on wires: spread wires in a channel evenly up to a role's wanted spacing,
  keep ribbons at pitch, keep every piece octilinear, keep topology. Bounded by iterations.
- Gate: no board lost; share at minimum clearance down; coupled length to other roles down.

### S6. Physical intent

- Roles recognised and read (3.5); return-path and plane-gap terms; decoupling loop length;
  width by need. Scored for ours and the designers' through the Examiner
  (`docs/spec/verification.md`).
- This is where "routed from physical need" becomes checkable instead of implied.
- **Power sections are not invented here** (user decision 2026-11-12). Known-good layouts for
  power ICs (the regulator, its capacitors and the copper between them) come from upstream
  (Circuit Composer) as locked blocks, which ultraroute already accepts as KiCad groups and
  routes around. ultraroute's part is to score the hot loops (buck: input capacitor to switch
  and back through ground; boost: output capacitor to output) on ours and the designers', and
  to let a block be declared in the board so no pre-step such as `add_block.py` is needed.

### S7. Frames, symmetry, similarity

The frame interface and the polar frame come earlier (3.6, S2b). This stage adds the rest:
sector frames with 60-degree directions (motionhexa), the spine frame (a synthetic sine-wave
strip as its test board), frames by region, and symmetry and similarity as
`planned-routing.md` 4.6.

### Simplification that falls out

After S2 the stage table loses Tightening, Squaring and half of Write. After S4, PairEscapes,
PruneEscapes and MergeEscapes fold into ordered escapes, and shove's fine-pitch plough has less
to do. A reasonable end state is about ten stages: set up, intent, escapes, ribbons, routing
(negotiation), rescue (rip-up and shove), fit, vias, spacing, write.

## 6. How the work is run

- **One author at a time in the router core**, and no agent-written architecture without a
  line-by-line audit. The plan/* round cost more to audit than it saved.
- **A/B rules as in the router change rules** (work caps, cost reported, exactness), plus:
  the look-set page is part of every report, and a stage's report states the lines added and
  deleted.
- **Flags are temporary.** A flag exists while its A/B runs and is removed, with the losing
  side's code, when the stage merges. The standing flag list is cut during S0 to those a user
  would set.
- **Time does not decide copper.** New work is bounded by counts. The existing time shares are
  left alone until S2 removes two of the stages that need them.

## 7. Open questions for the user

All answered by the user on 2026-11-12. Kept here as the record of what was decided.

- **No fallback** (principle 2): the current system is not preserved beside the new one.
- **Bar to replace main:** the work lives on one long-lived branch (`structured`). Main is
  left as it is. The branch merges when it matches main's clean count on the gate tier, with
  the look set as the deciding review. Boards may be lost on the branch in between; each
  stage's report lists which and why. The per-stage "no board lost" gates in section 5 are
  therefore gates for the merge to main, and a count to report at each stage.
- **Search core:** the cell search and negotiation are kept, behind an interface that does
  not assume cells: the search hands the wire fit a topological answer (which side of each
  obstacle, which layer), not a cell path it may lean on. Rip-up and shove are rewritten on
  wires, not adapted. A triangulation engine (the `topo` crate's idea) may be tried behind
  the same interface after S1+S2, first on the earring's trapped escapes.
- **S1 and S2 are one stage:** wire record, one writer and wire fit land together, and the
  mend passes, tightening and squaring are deleted in that merge. No old pass is adapted to
  wires only to be deleted.
- **fix/crowded-pads, fix/qfn-escape, fix/stagger, explore/restarts:** dropped without a gate
  run; tips in `ultraroute-lab/deleted-branches-2026-11-12.txt` and still on origin.
- **Ribbon spike first:** yes, started 2026-11-12 on `spike/ribbons`.
- Byte-identity with main is given up and wirelength may rise (questions 1 and 2 below).

The questions as asked:

1. Is losing byte-identity with main for S1 acceptable, with the gate tier as the proof?
2. Wirelength will rise, perhaps by 10 to 20%, as nets stop taking the shortest path. Agreed
   as intended?
3. The four fix/* branches and explore/restarts: rebase and gate each, or drop?
4. Should the ribbon spike (S4) run first, before S1, to learn early whether the idea holds?
   It would be throwaway code either way. My recommendation is yes: it is one week, it is the
   largest uncertainty, and its answer does not depend on S1.
