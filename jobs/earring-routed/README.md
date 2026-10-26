# earring-routed

The simple earring fully routed by the MIT core router: **0 unconnected, 0 errors, no dangling
copper** by KiCad 9's DRC after a zone refill, with the board's custom rules (`earring.kicad_dru`,
JLC's through-hole and SMD pad limits) and the schematic-parity check. Both USB pairs are routed
as length-matched differential pairs (`--diff-pairs`): USB_DP/USB_DM (0.2 mm tracks, 0.25 mm
gap) are 0.00 mm apart in length end to end, 49% and 59% of their length side by side, the rest
being the legs and the bumps that matched them (6.8 mm of skew before); USB_C_DP/USB_C_DN are
0.16 mm apart, 30% side by side (the connector's interleaved pads take the rest as legs).
Stripped of all copper (the earring-kicad job), routed with --strip-all --negotiate --rip-up --nudge --tighten --prefanout --diff-pairs --seed 5, 240 s
on 1 thread (71.1 s used). No part moved; 315 vias, 1329 mm of track.

Command, on `jobs/earring-kicad/input/earring.kicad_pcb`:

    ultraroute route earring.kicad_pcb -o out.kicad_pcb --strip-all --negotiate --rip-up --nudge --tighten --prefanout --diff-pairs --seed 5 --timeout 240 --threads 1

The input here is that output after `tools/refill_zones.py`, so the pours show as KiCad fills
them. It is a finished board to look at, not a routing job: the studio loads it as it is. Lab
record: experiment `diffpairs-26`, run `S-s5`; coupling and skew measured with
`cargo run --release -p grid --example pair_check`; see `docs/focus-trapped-escapes.md`.
