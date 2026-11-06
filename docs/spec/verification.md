# Verification spec: the Examiner

Nov 5, 2026 · draft 1

## Why

A route that KiCad's DRC passes is legal by the board's own rules. It is not therefore a good
board. KiCad will pass a 0.2 mm track carrying 3 A, a USB pair routed over a split in its
reference plane, a crystal trace running beside a switch node, a decoupling capacitor 15 mm from
its pin, so long as the board's rules do not forbid them, and most boards' rules do not.

ultraroute's goal is to go further than the routers that exist: to treat a circuit board as what
it is, a collection of electromagnetic systems sharing copper, and to lay each one out as its
physics needs. That goal needs a judge that knows the physics. This spec defines one: the
**Examiner**, a program whose only job is to find what is wrong with a layout.

## Principles

1. **The Examiner challenges; it never approves.** It reports findings, each a measured value
   against a limit with a reason. "No findings" means it found nothing, at the depth it looked,
   and says how deep that was.
2. **Independent of the router.** It does not share the router's geometry, costs or search. It
   reads the routed KiCad file, as anyone else would. Where it needs a parser or a connectivity
   model, it cross-checks itself against KiCad (pcbnew's connectivity, kicad-cli DRC) on the
   corpus, so a shared bug cannot make both agree.
3. **The router may aim at what the Examiner measures, but not be graded only by checks it was
   built to satisfy.** Some checks are held out of the router's cost model on purpose, so that
   a score that rises means the boards got better, not that the router learned the test.
4. **Every limit has a provenance**, in Circuit Composer's sense: a datasheet claim, a library
   claim, a standard (IPC-2152, IPC-2221B), a derivation from physics, or the board's own rules.
   A limit derived from physics records its derivation (e.g. rise time 1 ns and FR-4 give a
   critical length of 25 mm).
5. **The Examiner is itself verified** (see "Verifying the Examiner"). A checker nobody has
   tried to fool is not a checker.

## What a board is, to the Examiner

The unit of electromagnetic need is not the net. It is the **current loop**: a forward path and
the path its current returns by. A signal net's return flows in a ground or power net; a buck's
hot loop runs through four nets and two parts. So the Examiner works on a hierarchy:

| Level | What it is | Example needs |
|---|---|---|
| Pin | A driver or receiver, with its electrical character | edge rate, drive impedance, input capacitance, I/O standard |
| Net | Copper joining pins | current, voltage, width, length, topology |
| Loop | Forward path + return path | loop area, inductance, reference continuity, impedance |
| Block | Parts that work together | hot-loop area, sense routing, protection order |
| Domain | Supplies and grounds and their barriers | isolation, creepage, where grounds meet |
| Board | Stack-up, fab, outline | fab limits, edge clearance, emission |

Each net and loop is characterised by a small set of physical quantities, from which every check
follows:

- **Spectrum.** Rise time t_r gives the knee frequency (about 0.5 / t_r) and the critical length
  beyond which a trace behaves as a transmission line (about a sixth of the distance a signal
  travels in t_r: on FR-4, roughly 6 to 7 ps per mm). A 1 ns edge is electrically long past
  ~25 mm; a 10 kHz I2C bus never is.
- **Aggression.** How it disturbs others, split by mechanism: dv/dt (capacitive coupling, a
  high-voltage switch node) and di/dt (inductive coupling, a gate-drive or hot loop).
- **Sensitivity.** How much disturbance it tolerates, and through which mechanism: a high-
  impedance node (feedback divider, crystal, TIA input) is a victim of capacitive coupling; a
  low-impedance loop with a long area (sense lines, a remote ground) of magnetic coupling. With a
  noise budget in volts or amps over a bandwidth where known.
- **Power.** Continuous, peak and fault current; working voltage.
- **Impedance.** Target and tolerance, single-ended or differential, when it is a transmission
  line.

Aggressor and victim are then not labels but numbers that can be multiplied: a coupling between
two loops is estimated from their mechanisms, the geometry between them, and the overlap of the
aggressor's spectrum with the victim's bandwidth.

## Where intent comes from

The Examiner needs each net's needs. In order of trust:

1. **Explicit intent**: a Circuit Composer intent file for the board.
2. **Package intent**: the packages of the parts on the board, mapped by pin (a USB connector's
   D+/D- pins are a 90 Ω pair; an RP2350's VREG_LX is a switch node).
3. **Recognised intent**: inferred from the netlist, parts, values and footprints by Circuit
   Composer's recogniser, each with a confidence. A net joining an inductor to a regulator's SW
   pin is a switch node; a net between a crystal and an MCU's XIN is a clock victim.
4. **Defaults**, stated as such.

Recognised intent may also use the designer's own routing as evidence (a 1.5 mm track is likely
a power net). But such intent may not be used to judge that same designer's routing: that would
grade the designer against themselves. The Examiner records each requirement's source so this
can be enforced.

## Checks

Grouped into gates. A board reaches a gate when it passes every check up to and including it.

### G0 Connected

Every connection made; KiCad's ratsnest is empty after the zones are refilled. (Today's
PCBench `unconnected == 0`.)

### G1 Legal

KiCad's DRC under the board's own rules, after refill, with no error the bare board did not have.
(Today's PCBench judge.) Mastering KiCad means passing this on everything it can be passed on,
including custom rules (`.kicad_dru`), net classes and keepouts. Strengthen the judge: today an
error of one type fixed and another of the same type added leaves the count unchanged and passes;
match errors by location and items, not by count.

### G2 Buildable

The board's rules can be laxer than any fab will build. Check against a fab profile (JLCPCB
standard, OSH Park 2-layer and 4-layer, as data files): minimum track, gap, drill, annular ring,
via-in-pad, copper to edge, silk over pads, mask slivers; plus the DFM checks we already run
(acute angles, acid slots, necks).

### G3 Fit for purpose: intent met

One check per kind of requirement, each with its formula and source:

- **Current.** Track width and copper weight against IPC-2152 for each net's current and an
  allowed temperature rise; vias in a current path counted against their capacity; fault current
  against fusing. Narrowest section counts, not the average.
- **Voltage.** Clearance and creepage between nets by working voltage (IPC-2221B, IEC 62368 where
  the board asks for safety), including across slots and under parts.
- **Impedance.** Width and spacing against the target for the stack-up, by a 2D field solver,
  not a formula, for every transmission-line net; impedance changes along the route (necking at
  pads, layer changes) measured and limited.
- **Length and match.** Max length, match groups, pair skew, uncoupled pair length.
- **Topology.** Star, daisy chain, fly-by: the routed tree's shape against the requirement; stub
  lengths against the critical length.
- **Protection order.** Energy reaches the TVS or fuse pad before any branch, with no stub
  (Circuit Composer's protection path).

### G4 Electromagnetically sound: physics proxies

Fast estimates (milliseconds to seconds per board) of what a field simulation would find:

- **Return-path continuity.** For every fast net (electrically long or with a fast edge), walk
  the trace and find its reference plane underneath, layer by layer. Findings: a gap, slot or
  split crossed (with the length of the detour the return must make); a trace over a plane of
  another net with no nearby stitching capacitor; a trace near the board edge or a plane's edge.
- **Layer changes.** A signal via between layers referenced to different planes needs a return
  via (same net planes) or a capacitor (different nets) within a distance set by its rise time.
- **Loop area and inductance.** Per loop: the area between forward and return, and partial
  inductance estimates. The buck hot loop; each decoupling capacitor's mounting loop (pad to via
  to plane to the chip's pin); gate-drive loops; sense lines (Kelvin connection, pair kept
  together).
- **Coupling.** For each aggressor/victim pair within a few track heights of each other: coupled
  length, separation over height above the plane, and the resulting coupling coefficient
  (microstrip roughly 1 / (1 + (s/h)²)); estimated near- and far-end crosstalk against the
  victim's budget. Includes coupling across layers (broadside) and through plane voids.
- **Plane integrity.** How much of each reference plane survives antipads, slots and tracks;
  islands; necked necks between plane regions carrying return current.
- **Radiators.** Fast nets on outer layers longer than a fraction of a wavelength at their knee
  frequency, unshielded; stubs that resonate; edge-of-board routing.
- **Domains.** Ground domains meet where intended and nowhere else; isolation barriers have no
  copper across them; analog and digital currents do not share a return they should not.
- **Thermal and IR drop.** DC current density and voltage drop through power copper, with the
  pours as they fill.

### G5 Signed off: simulation

For the regions G4 flags, or on request: field-solver impedance and S-parameters of a channel,
PDN impedance against target over frequency, openEMS (or another solver, to be chosen) of a
region to settle a disputed proxy, thermal simulation. Slow; run on samples and on boards headed
to fabrication.

### Relative: no worse than the designer

Every PCBench board came with its designer's own routing, which we strip before routing. The
Examiner measures both, the designer's and ours, on every check above, and reports where ours is
worse: per-net width against theirs for power nets, decoupling loop against theirs, pair skew,
via count in current paths, coupling between their critical pairs. Not because hobby designers
are always right, but because a router that is worse than the person, on what that person
evidently cared about, is the first thing to fix, and this needs no intent at all.

## Findings

One format, shared with Circuit Composer's verifier:

```json
{"check": "return_path.gap", "gate": "G4", "severity": "error",
 "nets": ["USB_D+"], "loop": "USB_D+/GND", "at": [[x, y, "F.Cu"], ...],
 "measured": {"detour_mm": 7.4}, "limit": {"detour_mm": 1.0},
 "source": {"kind": "derived", "from": "rise 0.5 ns, FR-4", "requirement": "intent:recognised:0.82"},
 "fix_stage": "route", "held_out": false}
```

`fix_stage` (intent, floorplan, block, route) says where to go back to, as Circuit Composer's
spec does. `held_out` marks checks the router is not told about. A board's report lists the
gate it reached, every finding, and what was not checked and why (no intent for a net, no
stack-up, no solver).

## Verifying the Examiner

1. **Seeded defects (recall).** Take boards that pass a check and break them in the one way the
   check exists to catch: neck a power track, cut a slot under a USB pair, move a decoupling cap
   away, run a clock beside a switch node, drop a return via. The Examiner must find each one.
   Kept as a test corpus that grows with every check; a check with no seeded defects is not
   finished.
2. **Good designs (precision).** Vendor evaluation boards and reference designs are the best
   public layouts there are. Findings on them are false alarms until shown otherwise; each one
   is either a real fault (recorded, with evidence) or a check to fix.
3. **Cross-check with KiCad.** Connectivity, clearance and DRC-equivalent checks agree with KiCad
   on every corpus board, as PCBWorld's evaluator was cross-checked against ours (677 of 677).
4. **Proxies against simulation.** Each G4 proxy is calibrated on small cases simulated at G5
   (a trace over a slot, two coupled microstrips, a decoupling loop) and its error recorded. A
   proxy whose ranking disagrees with simulation is demoted to a warning.
5. **Against the bench, eventually.** Fabricated boards and coupons: impedance by TDR, coupling
   by near-field probe, emissions by pre-compliance scan. Few, chosen to settle the claims the
   proxies are least sure of.

## Corpus plan

1. **Recognise the corpus.** Circuit Composer's recogniser runs over the 1182 PCBench boards and
   writes an intent file per board, every attribute marked recognised with a confidence. A
   person spot-checks a sample of 50 boards to measure its precision per class (power, switch
   node, clock, USB, analog input, ...).
2. **Examine the designers.** G2 to G4 on the designers' own routing, with intent recognised from
   the netlist only: what real boards look like, and where the Examiner's false alarms are.
3. **Examine ours.** Same checks on full-run outputs, and the relative report against the
   designer. A new column in `pcbench.py compare`: boards reaching each gate, base and new.
4. **Route to it.** Requirements the router can act on become router inputs through the same
   regions and rules Circuit Composer's compiler already emits (current widths, clear_under,
   max length, pairs), and new ones as they are needed (return-via placement, reference-plane
   preference, aggressor/victim spacing). Held-out checks stay out.

## Milestones

1. **Judge by location (G1).** Match DRC errors by location and items, not count. Small, and it
   closes a known hole.
2. **Fab profiles (G2)** and the designer comparison for power widths, via count and decoupling
   distance: no intent needed, runs on the corpus at once.
3. **Recogniser v0 on the corpus** (Circuit Composer's), with the 50-board precision check.
4. **G3 current, voltage, length and match** from recognised intent.
5. **G4 return path and layer changes**, with seeded defects and a calibration against a
   solver on a handful of cases.
6. **G4 coupling and loops.**
7. **G5** for flagged regions; choice of solver.
8. **First fabricated test board** to calibrate the proxies.

## Open decisions

- **Where the Examiner lives.** Circuit Composer already has a `verify` crate (buck, half-bridge,
  protection, return path, decoupling checks) and owns the intent schema. Recommendation: one
  Examiner, grown from that crate, used by both projects, with ultraroute's PCBench harness as
  its corpus runner; not two verifiers that drift. Both projects are MIT, so either home works.
- **Stack-up.** Most PCBench boards do not state their stack-up; G3 impedance and every G4
  height needs one. Assume a standard per layer count and say so in each finding, or skip?
- **Gate for merging router changes.** Today: G1 count does not fall. Proposed: G1 by location,
  and no rise in G2 to G4 findings on the tiers, once those checks exist.
