# earring-routed

The simple earring fully routed by the MIT core router: **0 unconnected, 0 errors, no dangling
copper** by KiCad 9's DRC after a zone refill, with the board's custom rules (`earring.kicad_dru`,
JLC's through-hole and SMD pad limits) and the schematic-parity check. Both USB pairs are routed
as differential pairs (`--diff-pairs`): USB_DP/USB_DM run side by side for 86% and 78% of their
length (0.2 mm tracks, 0.25 mm gap), USB_C_DP/USB_C_DN for 36% and 38% (the connector's
interleaved pads take the rest as legs). Stripped of all copper (the earring-kicad job), routed
with --strip-all --negotiate --rip-up --nudge --tighten --prefanout --diff-pairs --seed 1, 240 s on 1 thread (73.2 s used). No part moved; 333 vias,
1297 mm of track.

Command, on `jobs/earring-kicad/input/earring.kicad_pcb`:

    ultraroute route earring.kicad_pcb -o out.kicad_pcb --strip-all --negotiate --rip-up --nudge --tighten --prefanout --diff-pairs --seed 1 --timeout 240 --threads 1

The input here is that output after `tools/refill_zones.py`, so the pours show as KiCad fills
them. It is a finished board to look at, not a routing job: the studio loads it as it is. Lab
record: experiment `diffpairs-24`, run `S-s1`; coupling measured with
`cargo run --release -p grid --example pair_check`; see `docs/focus-trapped-escapes.md`.
