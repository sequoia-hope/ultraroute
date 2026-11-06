# Review: Circuit Composer — System Spec (Oct 27, 2026)

Nov 5, 2026. Read against what circuit-composer has built (its README, `crates/intent`,
`crates/verify`, the library) and against ultraroute's needs, in particular the verification
spec in `docs/spec/verification.md`.

## What is right and should stay

- **Attributes, not categories.** Categories as presets over attributes is what lets a LiDAR
  board or a TENS unit fit without a schema change.
- **Provenance and strength on every attribute, and contradictions kept.** This is rarer than
  it should be and it is the basis for trusting any automated rule.
- **Critical blocks generated, not routed.** A hot loop is a small search with a sharp
  objective; the router should get it locked.
- **`fix_stage` on findings.** It makes the verifier a loop, not a report.
- **Milestones end to end.** The buck, the RP2350 package and the battery board each went all
  the way through to a routed, checked board. Keep this.

## Where it falls short of the goal

The goal, as the user put it on Nov 5: classify the electromagnetic needs of every part of every
circuit, and lay the board out to them. Measured against that:

### 1. The recogniser is the hinge, and it is not built

Everything Circuit Composer does today starts from intent written by hand or composed from
packages. That covers boards made with it: six board folders so far. It does not cover a board someone
brings, and it does not cover the 1182 PCBench boards, which are our only way to measure
anything at scale. The spec lists the recogniser second in the toolset; no crate implements it.

Recommendation: make it the next milestone. Input: a KiCad netlist with parts, values,
footprints and, where present, part numbers. Output: an intent file with every attribute marked
`recognised` with a confidence, and the evidence (which pins, which parts) that led to it. Start
with the classes that are cheap to recognise and matter most: ground and power rails (and their
current, from what they feed), switch nodes, crystals and clocks, USB and other named pairs,
feedback and sense nodes, analog inputs, high-voltage nets. Measure it on a hand-checked sample.

### 2. The unit of electromagnetic need is the loop, not the net

The schema's attributes hang on nets. But a net's return current flows in another net, and the
things that go wrong (a slot under a fast trace, a hot loop, a decoupling loop, a sense line
picking up flux) are properties of a loop: forward path plus return. Blocks partly cover this
(hot-loop area is a block goal), but an ordinary signal has no way to say "my return is GND on
In1.Cu, and it must not detour".

Recommendation: add a reference to each signal net's intent: the net (or nets) its return flows
in, and the layers it may reference. Add loops as named objects in blocks (hot loop, gate loop,
decoupling loop, sense loop) with their member pins in order. The verifier then checks loops;
the router can be told which plane a trace must stay over.

### 3. Aggressor and victim are scalars; they need a mechanism and a spectrum

`aggressor: strong` and `victim: strong` cannot be combined into a prediction. Whether a switch
node hurts a feedback trace depends on dv/dt (capacitive, through the gap between them) versus
di/dt (inductive, through loop overlap), and on whether the aggressor's spectrum reaches into
the victim's bandwidth.

Recommendation: record the physical quantities and derive the labels:
- for aggressors: edge rate (rise time), voltage swing, current step;
- for victims: node impedance level (high-Z nodes are capacitive victims, low-Z loops magnetic
  ones), bandwidth, and a noise budget where known.
Presets can still fill these from `strong`/`weak` when nothing better is known. With numbers,
coupling between any pair can be estimated, and the router's spacing between them can be
derived instead of guessed.

### 4. Pin-level electrical data should come from ingest

The quantities above are properties of driver and receiver pins: rise time, drive strength,
input capacitance, I/O standard. Packages hold pin tables already. Recommendation: ingest
should record each pin's electrical character, from the datasheet's AC tables and, where the
vendor publishes one, the IBIS model, which gives edge rates and driver impedance directly.
This is what lets the recogniser classify nets on chips it has a package for, without a person.

### 5. Requirements and geometric rules should be two levels

Today a package rule like `unreferenced_length_mm <= 0` or a `max_length` is already geometric.
Geometry is how the router is told; physics is why. When the stack-up, the edge rate or the part
changes, the geometric number should change with it.

Recommendation: let intent state the physical requirement where it is known (loop inductance
under X nH, crosstalk under Y mV, return detour under Z mm at a rise time), have the constraint
compiler derive the geometric rule from it, and keep the derivation as the rule's provenance.
Hand-written geometric rules remain allowed, marked as such.

### 6. No stack-up or fab in the schema

Impedance, plane heights and every coupling estimate need the stack-up (layer order, dielectric
thickness, Dk, copper weight). `prepare_route.py` sets a 4-layer stack-up in code. Recommendation:
board-level intent carries the stack-up and a fab profile, with provenance (stated by the fab,
assumed default).

### 7. One verifier, not two

Circuit Composer's `verify` crate (buck, half-bridge, protection, ideal diode, return path,
decoupling) is the start of the Examiner in ultraroute's verification spec. Recommendation:
grow one Examiner from it, used by both projects, with ultraroute's PCBench harness as its corpus
runner, rather than two checkers that drift apart. Keep it independent of the block generators
too: today the same repository generates a buck and judges it by the same model of hot-loop
area, so a wrong model would pass its own output. Seeded-defect tests and comparison against
simulation (the spec's slow tier) are the guard.

### 8. Hand-annotated boards will not tell us whether it generalises

The intent schema has been exercised on easy2350, rp2350-battery, the bolt earring and the motor
controller: four related boards. Running the recogniser and the fast-tier checks over the 1182
PCBench boards would show which attributes are ever recognisable, which checks fire on real
designs, and how often designers' layouts fail them. It would also give priors (how wide do
designers make a 2 A rail?) as evidence, not as rules.

### 9. Spec drift

- The router handoff still describes cost grids per cell per layer. What was agreed and built
  is polygons (`--regions routing.json`) with `never`/`must`/`avoid`/`prefer`, `clear_under` and
  `max_length`. Update the spec.
- Open question "intent file format" is answered in practice (JSON with a generated schema).
  Close it.
- "How the router represents locked copper and cost grids internally" is answered by the
  regions work (merged in ultraroute f179fcb). Close it.

## Suggested order

1. Recogniser v0 and the corpus run (this unblocks ultraroute's verification work).
2. Signal references and loops in the schema (2), and physical aggressor/victim fields (3).
3. Stack-up and fab profile in board intent (6).
4. Merge the verifiers into one Examiner (7).
5. Pin electrical data and IBIS at ingest (4), then derived rules (5).
