# KiCad format coverage

What the router reads from a KiCad 9 board, project and custom-rules file, what it leaves out,
and the order to close the gaps in. Audited 2026-10-30 against `core/crates/kicad-pcb` and
`core/crates/board` at d25ef91. Corpus counts are from the 649 boards and 488 projects under
`~/pcb` (316 boards are KiCad 9); the 200-board PCBench sample is converted KiCad 5 and has no
`.kicad_dru` files at all, so the benchmark has never exercised custom rules.

A gap is one of two kinds:

- **unsafe**: the router lays copper KiCad's DRC flags (a rule it does not know is stricter);
- **costly**: the router keeps further away than it must (a rule it does not know is looser),
  so it may leave connections open that would fit.

## Custom rules (`.kicad_dru`): the largest gap

`Rules::apply_custom` (`kicad-pcb/src/rules.rs`) matches seven exact condition strings
(through-hole hole clearance, through-hole hole-to-hole, SMD-to-SMD clearance, per-net or
per-class track width, edge clearance of Edge.Cuts circles or arcs, unconditional edge
clearance) and drops every other rule with a warning. Its `(layer …)` clause is ignored, so a
layer-scoped rule it does match applies on every layer (strict, safe). It takes the largest
value where KiCad takes the last matching rule, which may lower a clearance.

Of 88 rule files in `~/pcb`, 17 have rules the router skips (542 warnings). What they say:

| Rule, as written in real files | Example | Kind |
|---|---|---|
| Clearance by object type: pad-to-track, PTH-to-track, via-to-track hole clearance, hole-to-hole between nets (`A.Type == 'via' && B.Type == 'track'`, `A.isPlated()`, `A.Net != B.Net`) | JLCPCB template (dancing_forest, rp2040 motor, boost) | unsafe |
| Clearance by net class pair (`A.NetClass == 'RF' && B.NetClass == 'RF'`), class to zone | rtk_gps, picam_adapter | unsafe or costly |
| Layer limits for a net or class (`A.NetClass == 'Motor' && A.Layer != 'B.Cu'` with `disallow track via`) | bluebuzz | unsafe |
| Area rules (`A.intersectsArea('…')`) that disallow tracks or vias, or set a clearance or width | rp2350-pendant antenna keep-out; servodrive, comms fine areas at 0.12 mm | unsafe; costly |
| `length (max …)`, `via_count (max …)` per net | bluebuzz clock, QSPI, switch node | unsafe |
| `physical_hole_clearance` (NPTH to copper, via to via) | bluebuzz, rp2040 motor | unsafe |
| A global looser clearance, `(severity ignore)`, parent-footprint tests (`A.Parent == B.Parent`, `memberOfFootprint`) | led-characterizer, cube_earring, picam | costly |
| `hole_size`, `annular_width`, `via_diameter`, silk and text rules | many | none: they judge sizes the router does not choose, or silk |

The warning is printed to stderr only; `--report` does not carry it, so Studio and Boardroom
never show that a rule was dropped.

## Board and project: ranked gaps

1. **No `.kicad_pro` beside the board is a load error** (`board/src/model.rs:360`). KiCad opens
   the board with its defaults, which `Rules::parse("{}")` already supplies. Hard failure.
2. **Net class resolution is not KiCad 9's** (`rules.rs:155-164`). A net in
   `netclass_assignments` takes only the first class listed, and pattern classes are not
   combined with it; KiCad 9 combines every class a net has, by priority. A class field left out
   reads as 0 where KiCad 9 takes Default's (latent: every class in the corpus sets every field).
   Unsafe where a higher-priority class is wider. 161 projects assign, 119 use patterns.
3. **`solder_mask_to_copper_clearance` not read.** The checker keeps foreign copper 1 µm outside
   a mask opening (`MASK_MARGIN`, `board/src/check.rs:161`); 207 projects set 5 µm. Unsafe in a
   narrow case; a one-line fix.
4. **Zone pad connection modes** (`connect_pads no`, `thru_hole_only`; 31 boards) and thermal
   spokes (`thermal_gap`, `thermal_bridge_width`, `min_resolved_spokes`) are ignored. On plane
   layers and unfilled zones a pad can count as joined when KiCad will not join it, and a reroute
   can starve a thermal relief. Unsafe.
5. **KiCad 9 padstacks** (`(padstack (mode front_inner_back|custom))`) are ignored: the front
   shape stands for every layer. Unsafe where an inner pad is larger. None in the corpus yet.
6. **Pad and footprint clearance overrides only ever raise the clearance**
   (`model.rs:555`); KiCad lets them lower it, down to the board minimum (~580 boards set one).
   Costly.
7. **`remove_unused_layers` / `keep_end_layers`** on pads and vias are ignored: annuli are
   assumed on every inner layer. Costly; none in the corpus set it.
8. **Copper the reader does not see**: `gr_curve`/`fp_curve`, text boxes, dimensions and tables
   on copper layers; custom-pad `gr_curve` primitives and the stroke of a custom-pad `gr_rect`.
   Unsafe; none found in the corpus.
9. **The writer writes no arcs and no blind, buried or micro vias**: rerouted copper comes back
   as segments and through vias (`grid/src/route.rs:677` asserts it). KiCad 9 `generated`
   tuning patterns (2 boards) are left pointing at tracks the router may have removed;
   teardrops (1 board) are not regenerated.
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

1. **Done (2026-10-30): say what is dropped, and load without a project.** Put rule and board warnings in the
   `--report` JSON (`findings` of kind `rule`) so Studio and Boardroom show them; load a board
   with no `.kicad_pro` on KiCad's defaults; read `solder_mask_to_copper_clearance`. Small.
2. **Done (2026-10-30): KiCad 9 net classes.** Composite classes by priority, missing fields from Default, with
   KiCad's own resolution checked on the corpus projects (kicad-cli DRC on a probe board).
3. **A real custom-rules engine.** Parse the rule language (conditions as expressions over
   `A`/`B` properties and functions, `(layer …)`, `(severity …)`, last-match-wins) and resolve
   it, per constraint, into what the grid already understands:
   - clearance by net, net-class pair and object type (track, via, pad, PTH, NPTH, zone) into
     the per-pair clearance tables, lower values included, never below the board minimum;
   - track width by net and layer;
   - `disallow` by net, class, layer and `intersectsArea` into the region map that `--regions`
     already drives (`never` for that net on that layer);
   - area-scoped clearance and width as regions with their own clearance;
   - `hole_clearance`, `hole_to_hole`, `physical_hole_clearance`, `edge_clearance` by object
     type into the via and hole checks.
   A rule the engine cannot resolve still warns, by name, in the report. The board checker
   (`board/src/check.rs`) reads the same resolved rules, so the router judges what KiCad judges.
4. **Zone connection and thermals.** `connect_pads` modes and thermal spokes for planes and
   unfilled zones; keep `min_resolved_spokes` spokes free of new copper.
5. **Length and via-count limits** (`length`, `via_count`, `diff_pair_uncoupled`): reported by
   the checker first, then steered: a net over its length rips and reroutes with a length cost.
6. **Pad geometry**: padstacks per layer, `remove_unused_layers`, clearance overrides that lower,
   curves in custom pads and on copper.
7. **Writer**: arcs out (the tightener's corners as KiCad arcs), tuning patterns and teardrops
   dropped cleanly with a warning when their tracks are rerouted; blind and micro vias only if a
   stage ever lays them.
8. **A benchmark with rules.** A PCBench-style set of KiCad 9 boards that carry `.kicad_dru`
   files (the JLCPCB template, net layer limits, area rules), judged by kicad-cli with those
   rules, so a regression in rule handling shows the way routing regressions do.
