# earring-routed

The simple earring fully routed by the MIT core router: **0 unconnected, 0 errors** by KiCad
9's DRC after a zone refill, with the board's custom rules (`earring.kicad_dru`, JLC's
through-hole and SMD pad limits) and the schematic-parity check. Stripped of all copper (the earring-kicad job), routed with --strip-all --negotiate --rip-up --nudge --tighten --prefanout --seed 1, 240 s on 2 threads (92.8 s used). No part moved.

Command, on `jobs/earring-kicad/input/earring.kicad_pcb`:

    ultraroute route earring.kicad_pcb -o out.kicad_pcb --strip-all --negotiate --rip-up --nudge --tighten --prefanout --seed 1 --timeout 240 --threads 2

The input here is that output after `tools/refill_zones.py`, so the pours show as KiCad fills
them. It is a finished board to look at, not a routing job: the studio loads it as it is. Lab
record: experiment `full-rules-17`, run `S-s1`; see `docs/focus-trapped-escapes.md`.
