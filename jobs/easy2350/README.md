# easy2350

easy_pcb's RP2350 board (examples/easy2350, 21 x 51 mm, 4 layers), as easy_pcb hands it to the
router: placed and filled, its locked pre-routes and pad vias kept. In1.Cu is a full-board GND
plane and In2.Cu a full-board +3V3 plane, so it routes with `--planes In1.Cu,In2.Cu`: no tracks
on those layers, other nets' vias through them, and their nets joined by the fill. easy_pcb's
recipe: `--strip --negotiate --rip-up --tighten --prefanout --diff-pairs --shove-plough rip
--shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8 --threads 1 --timeout 240`.

The reference is easy_pcb's released route (easy_pcb 2d56198,
examples/easy2350/hw/easy2350/a/easy2350.kicad_pcb, Freerouting with both inner layers as
planes): 0 unconnected, 0 errors, 545 F.Cu and 274 B.Cu segments. Ours: 7 to 10 unconnected,
all signal nets, no new errors (the 20 items_not_allowed are the board's own vendor copper in
its routing keepouts); lab experiments planes-28 to planes-33.

With the region plough (`--cell 0.035 --prefanout-order round --shove-plough region
--shove-reach 0.5`, in place of `--shove-plough rip`): seed 1 leaves no signal net open, seeds
0, 2, 3 leave 2, 1, 2 (lab planes-44); the one +3V3 open is joined by easy_pcb's outer pours,
added after routing. job.json carries these flags and seed 1.

input/easy2350.kicad_dru is easy_pcb's custom rule (uncommitted there on 2026-10-26): VBUS no
narrower than 0.2 mm. With it, seeds 0-5 leave 0 1 0 2 1 2 signal nets open (lab planes-50).


Soft via-avoid areas (for easy_pcb's silk labels): rule areas named `soft-novia...`, or
`--via-avoid X0,Y0,X1,Y1` boxes in mm, make a via there cost `--via-avoid-cost` (default 3000,
in units of 10 per grid cell) but never forbid one. With the J2 label box
`--via-avoid 104.4,122.4,106.5,126.4`, seeds 0-3 leave 1 1 2 3 signal nets open (0 1 1 3
without it) and the only signal vias left in the box are U1's GPIO0-2 fan-out vias, which sit
at U1's pins (lab planes-53).
