# PCBench

PCBench (PCBench/, 1,182 KiCad boards with published results for FreeRouting, PcbRouter and
RL-MCTS in PCBench/Baselines/Results_all.csv) routed by the core from bare pads and judged by
KiCad. `tools/pcbench.py` does it:

    python3 tools/pcbench.py prepare                  # once: KiCad 5 -> 9, strip, judge the bare board
    ~/.claude/bin/reserve -c 16 -l pcbench -- python3 tools/pcbench.py run RUN [--timeout 120]
    python3 tools/pcbench.py report RUN

Boards and runs live in ~/Software/ultraroute-lab/pcbench/. `ULTRAROUTE_BIN` picks another
router build. Conversion gives each repeated uuid a fresh one (KiCad 5 numbers footprints 0, 1,
...).

A board is **clean** when KiCad finds nothing unconnected after a refill and no DRC error of a
type the bare board had fewer of; **routed** allows new solder mask bridges, closer to what the
baselines probably counted. Wirelength and vias are counted as the baselines report them.

## Base line (574caac, 120 s, one thread, seed 0; run base-1)

| | boards | % |
|---|---|---|
| ours, clean | 518 | 43.8 |
| ours, routed | 607 | 51.4 |
| FreeRouting | 857 | 72.5 |
| PcbRouter | 514 | 43.5 |

On the 461 boards both we and FreeRouting succeed on: median wirelength 0.81x theirs, 5.7x their
vias (13,320 against 2,325), a fifth of their time. Boards with new errors by type: solder mask
bridge 285, clearance 150, shorts 101, board edge 78, hole clearance 55, annular ring 38.

## Electrical validity

What the router now sees that it did not, each a source of those errors:

- Solder mask openings: a pad's opening (its own `solder_mask_margin`, its footprint's, or the
  board's `pad_to_mask_clearance`) on the mask layers it lists; a via's where the board leaves
  vias bare (`(tenting ...)`); and openings drawn on F.Mask/B.Mask, which admit only the net
  whose pads they bare. Other nets' tracks and vias keep out of them (`Board::mask_openings`,
  `via_openings`, `mask_areas`; `board::mask_gap`).
- Copper drawn rather than routed: lines, arcs, shapes, polygons and visible text on copper
  layers, the board's and footprints' (`Copper::graphics`, kept pours of zone `DRAWN`).
- A pad's copper moved off its hole by `(drill ... (offset x y))`.
- A pad's or footprint's own `(clearance X)` (`Board::pad_clearances`).
- Every Edge.Cuts and Margin primitive as board edge, closed into the outline or not.
- Unplated slots (oval drills) as keepouts the hole clearance round the whole slot.
- A via grows to keep `min_via_annular_width` round the class's drill.
- Per-net minimum track widths from `.kicad_dru` `track_width (min X)` rules.

On the 429 boards that had new errors in base-1 (run valid-2, before the last three items):
169 now clean; boards with shorts 101 -> 8, clearance 150 -> 8, annular ring 38 -> 0, board edge
78 -> 27, hole clearance 55 -> 17, mask bridges 285 -> 55. Of 150 boards clean in base-1, 5 now
leave a connection or two open (valid-3): mostly boards whose 0.2 mm pad mask expansion the old
routes happened to miss, now a constraint.
