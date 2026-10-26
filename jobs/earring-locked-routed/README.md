# earring-locked-routed

The simple earring fully routed by the MIT core router: **0 unconnected, 0 errors** by KiCad
9's DRC after a zone refill, with the board's custom rules (`earring.kicad_dru`, JLC's
through-hole and SMD pad limits) and the schematic-parity check. With the designer's 45 locked tracks and vias kept (the earring-kicad-locked job), routed with --strip --negotiate --rip-up --nudge --tighten --prefanout and the plough search (--shove-plough rip --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8) --seed 3, 240 s on 2 threads (80.5 s used). The nudge moved R16, unlocked, 0.5 mm in y.

Command, on `jobs/earring-kicad-locked/input/earring.kicad_pcb`:

    ultraroute route earring.kicad_pcb -o out.kicad_pcb --strip --negotiate --rip-up --nudge --tighten --prefanout --shove-plough rip --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8 --seed 3 --timeout 240 --threads 2

The input here is that output after `tools/refill_zones.py`, so the pours show as KiCad fills
them. It is a finished board to look at, not a routing job: the studio loads it as it is. Lab
record: experiment `full-rules-17`, run `L-s3`; see `docs/focus-trapped-escapes.md`.
