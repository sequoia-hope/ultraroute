# earring-locked-routed

The simple earring fully routed by the MIT core router: **0 unconnected, 0 errors** by KiCad
9's DRC after a zone refill, with the board's custom rules (`earring.kicad_dru`, JLC's
through-hole and SMD pad limits) and the schematic-parity check. With the designer's 45 locked
tracks and vias kept (the earring-kicad-locked job); some of that locked copper is left
dangling once the rest is stripped (warnings). Both USB pairs are routed as differential pairs
and matched to 0.00 mm end to end; USB_DP/USB_DM run side by side for 53% and 41% of their
length. Routed with --strip --negotiate --rip-up --nudge --tighten --prefanout --diff-pairs and the plough search (--shove-plough rip --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8) --seed 1, 240 s on 1 thread (71.6
s used). No part moved; 356 vias.

Command, on `jobs/earring-kicad-locked/input/earring.kicad_pcb`:

    ultraroute route earring.kicad_pcb -o out.kicad_pcb --strip --negotiate --rip-up --nudge --tighten --prefanout --diff-pairs --shove-plough rip --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8 --seed 1 --timeout 240 --threads 1

The input here is that output after `tools/refill_zones.py`, so the pours show as KiCad fills
them. It is a finished board to look at, not a routing job: the studio loads it as it is. Lab
record: experiment `diffpairs-26`, run `L-s1`; coupling and skew measured with
`cargo run --release -p grid --example pair_check`; see `docs/focus-trapped-escapes.md`.
