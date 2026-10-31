# KiCad format coverage

What the router reads from a KiCad 9 board, project and custom-rules file, what it leaves out,
and the order to close the gaps in. Audited 2026-10-30 against `core/crates/kicad-pcb` and
`core/crates/board` at d25ef91, and updated as the roadmap below was worked through. Corpus counts are from the 649 boards and 488 projects under
`~/pcb` (316 boards are KiCad 9); the 200-board PCBench sample is converted KiCad 5 and has no
`.kicad_dru` files at all, so the benchmark has never exercised custom rules.

A gap is one of two kinds:

- **unsafe**: the router lays copper KiCad's DRC flags (a rule it does not know is stricter);
- **costly**: the router keeps further away than it must (a rule it does not know is looser),
  so it may leave connections open that would fit.

## Custom rules (`.kicad_dru`)

Until 2026-10-30 the router matched seven exact condition strings and dropped every other rule.
It now reads the rule language (`kicad_pcb::dru`) and asks it as KiCad 9.0.8's DRC does, each
point below checked with kicad-cli on a probe board:

- the last rule in the file that holds wins, over the net classes and over the board's own
  minimums, lower or higher; a rule between two objects is tried both ways round;
- a pad's own clearance (or its footprint's) outranks the classes and the rules, lower or
  higher, and only the board's minimum holds under it (the router keeps the wider of the two);
- a `disallow` holds if any rule naming the object's kind holds, not only the last;
- a through via's `A.Layer` equals no layer and differs from none: only a `(layer …)` clause or
  `existsOnLayer` reaches it; `A.NetClass == 'X'` holds for any of a net's classes, `Default`
  included where it gave a field, or the whole combined name.

The checker (`board/src/check.rs`) and the grid (`grid/src/grid.rs`) ask the same rules
(`board::custom`): clearance, hole clearance, hole-to-hole and physical hole clearance between
each pair, edge clearance per edge primitive, track widths, the router's via sizes (`hole_size`,
`via_diameter`, `annular_width`), and `disallow` by net, class, layer and named area (zones and
rule areas by name, `intersectsArea`, `insideArea`, `enclosedByArea`). `length` and `via_count`
maximums are checked after routing and reported as findings of kind `length`. A rule whose
constraints the router does not keep is a warning naming them, and every warning is a finding
of kind `rule` or `board` in `--report`.

Where the router cannot answer a condition for copper it has yet to lay (which named area a
track will cross, a courtyard, a fabrication property) the rule may hold, and the strictest
outcome is taken: a rule that tightens is kept everywhere it might apply; one that loosens is
not used. So servodrive's 0.12 mm tracks and clearance inside one inner-layer area stay 0.15 mm
there (costly, not unsafe). A layer-scoped width rule is taken at its widest on every layer.

On ten `~/pcb` boards with rule files, routed from bare pads and judged by kicad-cli with their
own rules (`tools/rulebench.py`), every rule violation the old router left is gone: rp2040 motor 44 hole
clearance errors, servodrive 178 track width errors, the pendant's 45 antenna keep-out errors and
bluebuzz's 11 layer-limit errors, all to none. Open connections rose by one or two on four boards
where a rule now closes space the old router used.

## Board and project: ranked gaps

1. **Done.** A board with no `.kicad_pro` loads on KiCad's defaults.
2. **Done.** Net classes resolve as KiCad 9 does: every class a net is assigned or matches,
   by priority then the project's order, each field from the first that sets it.
3. **Done.** `solder_mask_to_copper_clearance` is read.
4. **Done.** A pad whose zone connection is none (its own `zone_connect 0`, its
   footprint's, or the zone's `connect_pads no`) is no longer joined by its net's pour, plane or
   kept block pour. A pad a pour joins by thermal reliefs keeps its spokes (`board::spokes`):
   every spoke KiCad would draw that fits (inside the zone, clear of fixed copper, not landing
   in another pad's relief) is room other nets' new copper may cut only while the pad keeps
   `min_resolved_spokes` (the project's, else a custom rule's, else 2), its own net's tracks
   out of it counting as spokes, as KiCad counts them. Gap and width come from the pad, the
   rules (`thermal_relief_gap`, `thermal_spoke_width`), the zone, KiCad's defaults. The grid
   walls off the spokes a pad cannot spare and prices the rest. Checked with kicad-cli 9.0.8
   probes (a track kills a spoke when its clearance leaves the pour past the gap less than
   `min_thickness` deep; round pads at 45°) and KiCad's DRC source (spokes counted where the
   fill crosses the pad grown by half the gap; a pad no spoke reaches is skipped). Rulebench:
   8 starved pads to 1, the one a pad the designer's own board starves too. Left: a pad short
   of room could be given a track of its net into the pour, as designers do; a spoke's land
   is not proven to join the rest of the pour.
5. **KiCad 9 padstacks** are taken as the front shape on every layer, now with a warning.
   Unsafe where an inner pad is larger. None in the corpus yet.
6. **Judged, not used.** `Board::clearance_between` gives KiCad's answer (a pad's or footprint's
   own clearance stands in for the classes and rules, lower or higher, the board minimum under
   it), but the router keeps the wider of that and the classes' and rules' (`clearance_kept`):
   using the narrower lost two PCBench boards to pads boxed in by other nets and won one.
7. **`remove_unused_layers`** on pads and vias: rings are assumed on every layer, now with a
   warning. Costly; none in the corpus set it.
8. **Copper the reader does not see**: curves, text boxes, dimensions and tables on copper
   layers, now with a warning; custom-pad `gr_curve` primitives and the stroke of a custom-pad
   `gr_rect` still not. Unsafe; none found in the corpus.
9. **The writer writes no blind, buried or micro vias**: rerouted copper comes back as segments,
   through vias and, with `--arcs MM`, arcs at its corners (`grid/src/route.rs` asserts the vias
   go through). A tuning pattern's tracks
   are now held as they are (locked), so the router never reroutes tuned copper; teardrop zones
   are kept, with a warning that new tracks get none.
10. **`rule_severities`** is ignored: the router's checker enforces every rule it knows. Safe.

Read and honoured: thickness and stackup, mask expansion and minimum web, KiCad 9 via tenting,
copper layer order in both numberings, every pad shape (chamfered as its rectangle, custom by
convex hull), drill offsets and slots, NPTH clearance, mask margins, net ties, tracks, arcs as
chords, existing blind and micro vias as spans, zones (outlines, cutouts, fills, priority,
clearance, wildcard layers, `(net 0)`), every keepout type including footprint keepouts and
cutouts, copper graphics and text, groups as locked blocks, Edge.Cuts with cutouts, footprint
outlines, Béziers and the Margin layer, the project's design rules with KiCad 9's defaults, and
per-class clearance, width, via and pair sizes. The writer splices: every byte it does not
replace survives, KiCad 9 and 10 tokens included.

## Roadmap

Each step lands with a fixture board and a KiCad DRC check of the router's output on it, the
way Stage 9 did; steps 1-3 also add the `~/pcb` rule files above as parser tests.

1. **Done (2026-10-30): say what is dropped, and load without a project.** Put rule and board
   warnings in the `--report` JSON (`findings` of kind `rule`) so Studio and Boardroom show them;
   load a board with no `.kicad_pro` on KiCad's defaults; read `solder_mask_to_copper_clearance`.
   Small.
2. **Done (2026-10-30): KiCad 9 net classes.** Composite classes by priority, missing fields from
   Default, with KiCad's own resolution checked on the corpus projects (kicad-cli DRC on a probe
   board).
3. **Done (2026-10-30): a custom-rules engine**, as described above. Left: area-exact
   clearances and widths for copper not yet laid (stamping each named area with its own rule
   instead of the strictest), and per-layer track widths.
4. **Done (2026-10-31): zone connection and thermals.** No-connection pads stay unjoined;
   relief pads keep `min_resolved_spokes` spokes free of new copper (above). Left: a track
   into the pour for a pad short of room.
5. **Length and via-count limits.** Done: reported after routing. Left: steering (a net over
   its length rips and reroutes with a length cost), and `diff_pair_uncoupled`.
6. **Pad geometry.** Done: clearance overrides judged as KiCad does; warnings for the rest. Left:
   padstacks per layer, `remove_unused_layers`, curves in custom pads and on copper.
7. **Writer.** Done: tuned tracks held, teardrops warned of; `--arcs MM` rounds the router's
   corners with KiCad arcs (`grid::round_corners`, at write time: each corner between two of its
   unlocked tracks gets the widest arc up to MM the checker passes, halving down to the track's
   width, judged as chords grown by their sag as arcs read from a file are). Left: blind and
   micro vias only if a stage ever lays them.
8. **Done (2026-10-30): a benchmark with rules**, `tools/rulebench.py`, ten boards to start. A PCBench-style set of KiCad 9 boards that carry `.kicad_dru`
   files (the JLCPCB template, net layer limits, area rules), judged by kicad-cli with those
   rules, so a regression in rule handling shows the way routing regressions do.
