# Diagnosis: why ultraroute's boards look chaotic, and what the code base has become

2026-11-12. Written after reading main (ed4d369): the stage table and driver
(`core/crates/ultraroute/src/lib.rs`, `stages.rs`), the search and the path-to-copper code
(`grid/src/search.rs`, `route.rs`), the finishing passes (`grid/src/cleanup.rs`, `vias.rs`), the
Phase 6 spec and the five plan/* audits, and the gate-tier benchmark results in ultraroute-lab.
Nothing was changed in the router. The proposal that follows from this is
`docs/spec/structured-routing.md`.

Numbers below are from the gate run `sqpg-9512c88` (315 boards, the routing code now on main)
and the `designer` run (the same boards as their designers routed them), on the 273 boards that
are clean in ours and have a designer score. Repair counts are summed from that run's `log.txt`
files.

## 1. The short version

The router makes copper with a search whose only goal is the shortest legal path for one
connection, then spends two thirds of its stages and most of its code repairing what that search
produced. Each repair is local and adds its own defects, which the next repair fixes. The result
is legal and short, and looks like nobody designed it, because nobody did: at no point does
anything decide how the board as a whole should be wired.

Three things are wrong at the root, and they are separate:

1. **The objective.** Cost is length plus vias plus congestion. Nothing else is priced: not
   bends, not direction, not running beside a neighbour, not spacing.
2. **The unit.** The thing routed is one two-pin connection, shortest airline first. Nothing
   larger exists: no group, no channel, no layer plan.
3. **The representation.** A route is a bag of track segments on the board. The path that
   produced them is thrown away, so every later pass has to rediscover structure from copper
   and can only add or replace pieces.

The Phase 6 spec (2026-11-09) named 2 and 3. It underweighted 1, and its remedy for 2 (bus
groups with a lane discount) is aimed at about 6% of the copper. Section 4 explains.

## 2. The chain, with counts

This is one connection's life on main, and what each step leaves for the next.

| Step | Where | What it does | What it leaves behind |
|---|---|---|---|
| Order | `route.rs:235` | sorts open airlines shortest first | long nets and buses route last, through whatever is left |
| Search | `search.rs:8-9`, `41` | A* on 50 µm cells; step 10 straight, 14 diagonal; via = 5 mm; ties broken by cell index | among equal-cost paths the pick is arbitrary: staircases, wandering; paths hug obstacles at minimum clearance |
| Direction | `route.rs:215` | a per-layer direction price exists, but only on 2-layer boards and only in single routing; negotiation, pairs, prefanout, nudge all set `directional: false` | inner layers both run the same way; crossings cost vias |
| Copper | `route.rs:1086` (`pieces`) | cell centres become segments; ends are cell centres plus a leg | 421,520 segments; ends that touch pads only at the rim |
| Tightening | `cleanup.rs:1289`, `1492` | pulls each chain taut, any angle | 166,018 segments, three quarters of the length off 45 degrees; total length unchanged (415.6 m to 417.4 m) |
| Vias | `vias.rs:397` | tries to reroute each via away | 20,440 tries, 4,861 vias removed |
| Cleaning | `stages.rs:1053` | removes doubles and stubs | 13,537 dangling pieces pruned |
| Squaring | `cleanup.rs:1751` to about `2600` | bends any-angle tracks back to 45 degrees, with slot, dogleg and slant checks | 36,277 tracks bent; 16.7% of length still off 45 |
| Write | `stages.rs:1481` | five mend passes, split, drop doubles, prune, again | 21,522 pad joints mended, 6,832 grazes bridged, 583 joints, 306 via grazes, 3,100 splits; then 3,942 doubles and 2,018 stubs that the mends themselves created; 152 necks and 905 narrow joints still reported |

Two loops in that table are worth stating plainly.

**Staircase, any-angle, back to 45.** The search works on an octilinear grid and so produces
0/45/90 paths with too many corners. Tightening removes the corners by going any-angle. Squaring
then spends about 900 lines putting the tracks back on 0/45/90, and cannot finish. The middle
state was never wanted. What was wanted is the fewest-bend octilinear path in the same channel,
which can be computed directly from the cell path.

**Ends.** A track ends at a cell centre, not at the pad's anchor. That one fact is the source of
`mend_pad_joints`, `mend_grazes`, `mend_joints`, `mend_via_grazes`, `mend_via_pad_grazes`,
`split_at_joins`, the second duplicate and dangling passes, `necks.rs` and the narrow-joint
reports: roughly 1,500 lines, 68 mends a board, and it still leaves errors that KiCad sees and
our check does not.

The same pattern holds for completion. On CAL430FR negotiation takes 17 s and joins 74 of 81.
Shove then runs from 18 s to 106 s to take 7 open down to 2, and the run ends with two open at
the fine-pitch parts. The designer's board has none open, and gets there with two 8-track
ribbons. Independent escapes block each other; an ordered ribbon out of a pin row cannot. On
this board the untidy routing and the unfinished routing are the same defect.

## 3. What the measurements say about looks

Totals over the 273 boards:

| | Designers | Ours |
|---|---:|---:|
| Track length | 331.2 m | 271.7 m |
| Detour (length over shortest tree) | 1.258 | 1.032 |
| Vias | 6,873 | 7,964 (+16%) |
| Off 45 degrees | 1.0% | 16.7% |
| Running beside another net (`bundled`) | 10.7% | 4.4% |
| Bus nets beside a bus sibling (`bus_bundled`) | 25.7% | 7.9% |
| Bus nets' share of all length | 5.9% | 6.7% |
| Doglegs per metre | 14.8 | 1.67 |
| Acute angles | 684 | 349 |
| Slots | 5,069 | 2,782 |

What I take from it:

- **Designers use 22% more track than we do, and fewer vias.** They spend length to buy order.
  Our detour of 1.03 means nearly every net is within 3% of its shortest possible tree. That is
  the objective showing through: we are optimising the one thing designers give away.
- **We now beat the designers on doglegs, acute angles and slots, and the boards still look
  worse.** Those three scores were driven down by the squaring work. They are real, but they are
  not what the eye reads. The scores that track the look are off-45, `bundled`, layer direction
  and spacing, and on those we are 2 to 17 times worse, or not measuring at all.
- **Buses are 6% of the copper.** By tidy.py's rule (three or more nets between the same two
  parts on neighbouring pins) a bus fix can move at most that much. The designers' 10.7%
  `bundled` is mostly nets that are not a bus by that rule and simply share a direction for a
  while. So the mechanism has to work for any nets that share a channel, not only declared
  groups. This is why `feat/square-bundles` found only 0.9% of slanted length in groups and
  why plan/bundles moved its own score a little and the look not at all.

## 4. Where I differ from the Phase 6 spec

`docs/spec/planned-routing.md` is right about the wire model and one writer, and right to bring
them forward. Three points I would change:

1. **Plan as price is too weak.** The spec keeps the per-connection cell search as the router
   and adds the plan as discounts ("advice, never a wall"). A discount inside a shortest-path
   search buys a little: the search still returns one net's cheapest path, and negotiation then
   reroutes it. plan/bundles measured exactly that. Structure has to be built by construction
   (lay the ribbon as one object, derive members from it), with today's router as the fallback
   when construction fails. The no-board-lost gate stays; the default generator changes.
2. **Groups from pin adjacency are too narrow** (section 3). Grouping should come from where
   connections travel, not only from what they are.
3. **The objective is missing.** No amount of planning above a length-only search produces
   tidy copper below it. Bend, direction and spacing terms belong in the cost model first.

Frames, symmetry and similarity (spec 4.6) are good ideas and stay, later. They are not the
reason the boards look bad today.

## 5. Tidy and physics

The project's aim is routing from physical need, and the claim that tidy copper is better
copper is mostly true, with one exception worth being precise about.

What is physically better about a designed board:

- **Fewer vias and layer changes.** Each is an impedance step and moves the return current to a
  different plane. We lay 16% more.
- **Layers with a direction.** Adjacent signal layers running at right angles couple least and
  leave channels open. Ours run the same way on boards with inner layers.
- **Spacing that uses the room.** A shortest path runs at minimum clearance from whatever it
  passes even when the board is half empty. Coupling falls quickly with spacing, so spreading
  into free room is a direct gain, and it is what even spacing looks like.
- **No stubs, necks, slivers or acute throats.** These are the hygiene defects of section 2.
- **Short loops where current is high or fast:** decoupling to pin, switch nodes, crystals.

The exception: **parallel runs couple.** A ribbon at minimum pitch is the worst case for
crosstalk between its members. That is fine for a synchronous bus sampled after it settles and
wrong for a clock beside an analogue input. So "bundle" must be a decision made per role, with
spacing by role (a clock gets room, a bus packs), and never a blanket preference.

And an honest caveat: on most boards in the corpus (low-speed, two layers) the electrical
difference between our routing and the designer's is small. Width, return path and decoupling
are where real failures are, and the first corpus comparison (`designer-comparison-2026-11.md`)
found width the one large failure. Tidiness matters for a second reason that is not physics: a
person has to review, trust, probe and rework the board. Both reasons are real; the spec keeps
them apart so neither is used to justify the other.

## 6. The code base

Size: 66k lines of Rust in `core/crates`, 463 commits in 20 days (96 on one day).

Rough split of the routing code (grid crate plus the driver, about 31k lines):

| Role | Files | Lines |
|---|---|---:|
| Make copper | search, route, grid, negotiate | 7,700 |
| Special cases before routing | prefanout, pairs, planes | 5,300 |
| Rescue what did not route | ripup, shove, nudge, push, blame | 4,200 |
| Repair and finish | cleanup, vias, necks, tune, arcs, islands, under, spokes | 6,000 |
| Driver | ultraroute lib, stages, stats | 5,200 |

Specific debts:

- **25 stages, 17 of them after the first routing pass.** `STAGES` in `lib.rs:1248` reads as
  the history of the project: each row is a problem found on some board.
- **About 100 flags** in `USAGE`, most of them experiment switches that became permanent.
- **One 7,725-line file** (`morph/src/compact.rs`) and a 3,650-line `cleanup.rs` that holds
  six unrelated jobs.
- **Time decides results.** Stage budgets are shares of `--timeout`, so a board's copper
  depends on machine speed once any stage is time-limited (20 of 200 boards at the limit).
- **Dead or parked weight in the tree:** the GPU solver, the `topo` crate (unlinked), `morph`,
  `copperroute` and `freerouting` checkouts, eight `target-*` directories, seven `*.log` files
  at the top level, `REMEDIATION.md` and `REVIEW.md` untracked.
- **Thirteen worktrees, eleven unmerged branches.** Section 7.

How it got here, as far as the record shows: every change was judged by the clean-board count
on PCBench, with a rule that no board may be lost. That gate is good and it worked: completion
climbed. But a gate on completion alone rewards the smallest local patch that wins one more
board, and tidiness arrived as a score only on 2026-11-07, as a watch metric. Agent-written
branches made it worse: the audits found reports that described code that was not in the diff,
benchmarks from mixed binaries, and features on by default that the report said were off.

## 7. Unmerged branches

| Branch | State | Recommendation |
|---|---|---|
| plan/hygiene | 2,482 lines of new mend passes, quadratic | do not merge; keep `hygiene.py` as a measure only |
| plan/intent | zones walled, a regression revived | do not merge; keep the zone-joined-connection fix as its own small commit |
| plan/layers | on by default against its report; escape role dead code | do not merge; the idea returns in stage S3 of the new spec |
| plan/bundles | lane discount; report does not match code | do not merge; keep the port of tidy.py's bus finder for measurement |
| plan/frames | polar frame experiments | do not merge; park until frames are scheduled |
| feat/square-bundles, explore/square-bundles | failed its gate | drop; superseded if squaring is replaced |
| explore/square-pads, feat/octilinear | already merged | remove worktrees |
| fix/crowded-pads, fix/qfn-escape, fix/stagger, explore/restarts | 19 to 91 commits behind main | not reviewed in this pass; each needs a rebase and a gate run or a decision to drop |
| remed/12-global | measured and stopped | keep as a record |

I have not deleted, merged or changed any of them.

## 8. What I did not check

- I read the pipeline and the finishing passes closely; `negotiate.rs`, `shove.rs`,
  `prefanout.rs` and `pairs.rs` only in outline.
- No new benchmark was run. All figures come from existing runs in ultraroute-lab.
- The designer totals include designer boards regardless of the designer's own DRC state, so
  they differ slightly from the 88-board figures in the tidy notes of 2026-11-07.
- The four fix/* branches were not read.
