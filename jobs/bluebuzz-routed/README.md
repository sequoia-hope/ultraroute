# bluebuzz-routed

bluebuzz fully routed by the MIT core router: **0 unconnected, 0 errors, 0 warnings about
dangling copper** by KiCad 9's DRC after a zone refill, under the board's own project rules
(which make dangling copper an error). Stripped of all copper (the bluebuzz-kicad job), routed
with --strip-all --negotiate --rip-up --nudge --tighten --prefanout --seed 3, 240 s on 2
threads (87.6 s used). No part moved. 232 vias and 1348 mm of track; the hand-finished board
has 331 vias.

Command, on `jobs/bluebuzz-kicad/input/bluebuzz-rm2.kicad_pcb`:

    ultraroute route bluebuzz-rm2.kicad_pcb -o out.kicad_pcb --strip-all --negotiate --rip-up --nudge --tighten --prefanout --seed 3 --timeout 240 --threads 2

The input here is that output after `tools/refill_zones.py`, so the pours show as KiCad fills
them. It is a finished board to look at, not a routing job: the studio loads it as it is. Lab
record: experiment `bluebuzz-main-19`, run `S-s3`; see `docs/focus-trapped-escapes.md`.
