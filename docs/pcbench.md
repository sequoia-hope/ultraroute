# PCBench

PCBench (1,182 KiCad boards with published results for FreeRouting, PcbRouter and RL-MCTS in
`PCBench/Baselines/Results_all.csv`) routed by the core from bare pads and judged by KiCad.
`tools/pcbench.py` does it. It needs KiCad 9 (`kicad-cli` and its `pcbnew` Python module), a
release build of the router, and a clone of <https://github.com/emshotton/PCBench> at `PCBench/`
in the checkout. `PCBench/` is not tracked, so a git worktree has none: run the tool from the
checkout that holds it, and point `ULTRAROUTE_BIN` at another build when needed.

    git clone https://github.com/emshotton/PCBench
    (cd core && cargo build --release -p ultraroute)
    python3 tools/pcbench.py prepare                  # once: KiCad 5 -> 9, strip, judge the bare board
    ~/.claude/bin/reserve -c 16 -l pcbench -- python3 tools/pcbench.py run RUN --sample 200 --sample-seed 1
    python3 tools/pcbench.py report RUN

`--sample 200 --sample-seed 1` is the 200-board subset every remediation stage was measured on
(about 16 minutes on 16 workers). Leave it out to route all 1,182 boards; `--only ID,ID` routes
the boards named (the directory names under `boards/`). Each board gets `--strip-all`, the
recipe (`--recipe default` unless `--recipe "..."` is given), `--seed`, `--uuid-seed`,
`--threads 1` and `--timeout` (120 s by default), under an 8 GB memory cap; the run takes as
many workers as `reserve` booked (`RESERVED_CORES`, 4 without it). Boards and runs live in
`~/Software/ultraroute-lab/pcbench/` (`ULTRAROUTE_LAB` moves it). Conversion gives each repeated
uuid a fresh one (KiCad 5 numbers footprints 0, 1, ...).

A two-board check of the command line (2026-10-30, loaf, the Stage 11 build):

    ~/.claude/bin/reserve -c 2 -l pcbench -- python3 tools/pcbench.py run remed-11-walkthrough --only 1-Wire-Wing-pcb_1-Wire_Wing,1Bitsy_1bitsy
    [1/2] 1-Wire-Wing-pcb_1-Wire_Wing: ok open 0 new {} 1.5 s
    [2/2] 1Bitsy_1bitsy: ok open 0 new {} 14.3 s
    2 of 2 succeeded

## Comparing runs

    python3 tools/pcbench.py compare A B [C ...] [--noise N|auto] [--noise-from A B] [--json OUT]
    python3 tools/pcbench.py note RUN "text"

`compare` judges each later run against A on the boards both routed: clean count, boards lost and
won (with what was open and which error types were new), boards whose new error types changed,
vias and wirelength over boards clean in both, boards at the time limit (within 10 s of the run's
timeout), referee failures and peak memory. It exits 1 when a run loses more boards than the
noise allows or any board gains an error type. `--json` writes the per-board table.

**Noise floor.** Until the router is deterministic under a wall-clock budget, the same binary and
seed disagree with themselves on a few boards. `--noise auto` counts the boards whose verdict
differs between `remed-0-base-s0` and `remed-0-base-s0b` (the 200-board subset, `--sample 200
--sample-seed 1`, binary 184f640 run twice on seed 0): **3**. Across seeds 0, 1 and 2 it is 7
(s0 vs s1) and 5 (s0 vs s2). No board gained an error type in any of those pairs.

## The default recipe

`ultraroute route IN -o OUT` naming no stage runs `ultraroute::RECIPE`, the recipe these runs use
(`--recipe default`; `--recipe minimal` runs only the grid router). Two candidates were measured
against it on the 200-board sample (sequoia-server, 2026-10-30, against `srv-46e1f7d`), and neither
joined it:

| added to the default | clean | lost / won | vias (clean in both) | at the limit | decision |
|---|---|---|---|---|---|
| `--nudge` | 162 -> 163 | 0 / 1 | 4649 -> 4629 | 23 -> 26 | no: within noise (3), slower, and it moves the designer's parts; the studio offers it as a choice |
| `--prefanout-mode same` | 162 -> 163 | 1 / 2 | 4549 -> 4516 | 23 -> 21 | no: within noise; kept as an option |

## Provenance and referee failures

`run` writes `runs/RUN/run.json` once, when the run starts: recipe, timeout, seed, boards,
commit (and whether `core/` was dirty), the router binary's path and sha256, `kicad-cli
--version`, Python, host and workers. Resuming a run whose commit or binary hash differs is
refused unless `--resume-anyway` is given, which records the new provenance under `resumed`.
`--note` (or the `note` command) adds to its `notes`.

Each route runs under `/usr/bin/time`, and `result.json` keeps its peak resident memory as
`peak_mb`. When the zone refill or KiCad's DRC fails, the board gets `referee_error` (the end of
the tool's stderr) and no verdict; `report` counts it as "referee failed", not "router failed".

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
