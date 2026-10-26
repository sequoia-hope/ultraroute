# bluebuzz-routed

bluebuzz fully routed by the MIT core router: **0 unconnected, 0 errors, no dangling copper** by
KiCad 9's DRC after a zone refill, under the board's own project rules (which make dangling
copper an error). Both USB pairs are routed as length-matched differential pairs (0.15 mm
tracks, 0.15 mm gap): USB_DP/USB_DM from the connector to the series resistors run side by side
for 76% and 72% of their length, 0.00 mm apart end to end (the hand-routed board: 31% side by
side); USB_DP_C/USB_DM_C, the 3.7 mm on from the resistors to the RP2350, run side by side on
F.Cu with no via for 58% and 60%, 0.13 mm apart (their pins are left out of the fan-out so the
pair reaches them straight). Stripped of all copper (the bluebuzz-kicad job), routed with
--strip-all --negotiate --rip-up --nudge --tighten --prefanout --diff-pairs --seed 2, 240 s on 1 thread (105.2 s used). No part moved. 265 vias and 1358
mm of track; the hand-finished board has 331 vias.

Command, on `jobs/bluebuzz-kicad/input/bluebuzz-rm2.kicad_pcb`:

    ultraroute route bluebuzz-rm2.kicad_pcb -o out.kicad_pcb --strip-all --negotiate --rip-up --nudge --tighten --prefanout --diff-pairs --seed 2 --timeout 240 --threads 1

The input here is that output after `tools/refill_zones.py`, so the pours show as KiCad fills
them. It is a finished board to look at, not a routing job: the studio loads it as it is. Lab
record: experiment `diffpairs-26`, run `BS-s2`; coupling and skew measured with
`cargo run --release -p grid --example pair_check`; see `docs/focus-trapped-escapes.md`.
