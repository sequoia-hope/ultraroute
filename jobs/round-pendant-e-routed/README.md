# round-pendant-e-routed

round-pendant's 8-layer variant E as released (github.com/sequoia-hope/round-pendant
`hw/round-pendant/e/round-pendant.*` at 2fcf971): a 38 mm round pendant, 235 XL-1010RGB LEDs on
a golden-angle spiral, five IS31FL3745 WLCSP-36 drivers, an RP2350A, 342 parts, 1218
connections, 5221 tracks and 879 vias. Every connection is joined: 0 DRC errors or warnings and
0 unconnected items by KiCad 9's DRC with `--severity-all`, as round-pendant reports.

It is a finished board to look at, not a routing job: the studio loads it as it is
(`strip: false`), and a route from it has nothing left to join. round-pendant routed it with
ultraroute in stages (`scripts/bga/critpass_d.py`, `escape_d.py`, `plough_finish_d.py`, then
`clean_copper_d.py`). One run from bare pads does not finish it yet: with its recipe
(`--strip --planes In2.Cu,In5.Cu --negotiate --rip-up --tighten --prefanout --prefanout-order inner
--prefanout-mode inner --diff-pairs --shove-plough region ... --cell 0.025`, 8 threads,
`--timeout 600`) the core at 24fb5d3 leaves 47 of 1042 connections open; with only `--strip` and
120 s, 104.
