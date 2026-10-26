# bluebuzz-routed

bluebuzz fully routed by the MIT core router: **0 unconnected, 0 errors, no dangling copper** by
KiCad 9's DRC after a zone refill, under the board's own project rules (which make dangling
copper an error). The USB pair from the connector to the series resistors (USB_DP/USB_DM, 0.15
mm tracks, 0.15 mm gap) is routed as a differential pair, side by side for 81% and 80% of its
length with 0.18 mm of skew (the hand-routed board: 31%); the 4 mm pair on from the resistors
to the RP2350 is legs only. Stripped of all copper (the bluebuzz-kicad job), routed with
--strip-all --negotiate --rip-up --nudge --tighten --prefanout --diff-pairs --seed 2, 240 s on 1 thread (105.5 s used). No part moved. 272 vias and 1331
mm of track; the hand-finished board has 331 vias.

Command, on `jobs/bluebuzz-kicad/input/bluebuzz-rm2.kicad_pcb`:

    ultraroute route bluebuzz-rm2.kicad_pcb -o out.kicad_pcb --strip-all --negotiate --rip-up --nudge --tighten --prefanout --diff-pairs --seed 2 --timeout 240 --threads 1

The input here is that output after `tools/refill_zones.py`, so the pours show as KiCad fills
them. It is a finished board to look at, not a routing job: the studio loads it as it is. Lab
record: experiment `diffpairs-24`, run `BS-s2`; see `docs/focus-trapped-escapes.md`.
