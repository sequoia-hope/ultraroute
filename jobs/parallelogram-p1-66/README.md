# parallelogram-p1-66

round-pendant's lowest-cost 4-layer board (github.com/sequoia-hope/round-pendant
`hw/parallelogram/p1-66/parallelogram.*` at 4768646): a 25 x 70 mm parallelogram skewed 17.75°,
6 x 11 = 66 XL-1615RGBC-RF RGB LEDs, one AW20198QNR QFN-40 driver with a solid 3.4 mm GND pad,
an RP2350A, 148 parts, 465 connections. JLC 4-layer rules (0.1/0.1 mm track and space, 0.3/0.15
mm vias) and the project's own `.kicad_dru` (current widths, QSPI and crystal lengths, the U3
pad and ring rule areas). In1.Cu is the GND plane (`--planes In1.Cu`).

The input is the released, routed board (2604 tracks, 343 vias, none locked); the studio strips
its tracks and vias (`strip: true`) and routes it from bare pads, keeping the zones.

With the job's arguments, `--strip --planes In1.Cu --cell 0.025 --seed 0 --timeout 120`, the
core router at 24fb5d3 leaves 0 open connections (465 of 465 routed). round-pendant's own recipe
(`hw/parallelogram/p1-66/route/autoroute.json`, with `--negotiate --rip-up --tighten --prefanout
--diff-pairs --shove-plough region ...`) also reaches 0 open, in about 70 s on 4 threads.

The same board, copied unchanged, is the core's test fixture `core/fixtures/parallelogram-p1-66.*`.
