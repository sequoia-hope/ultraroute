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
