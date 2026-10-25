# bluebuzz-kicad

bluebuzz's own KiCad board (github.com/sequoia-hope/bluebuzz `out/` at bf235b5), for the MIT core
router, which reads `.kicad_pcb` directly. Most of its wiring is locked; strip the rest
(`--strip`) to route it. Results are judged by KiCad's DRC after `tools/refill_zones.py`.

## Blocks

`job.json` declares the RP2350's power block (`blocks.rp2350_power`): the reference design's 1V1
(`rp2350_1v1_3`, `_6`) and 3V3_SYS (`rp2350_3v3_sys_1`, `_7`) pours with U1, L1, C80, C83 and C97, applied
to the input as a KiCad group by `tools/add_block.py --job jobs/<this job>`. The router keeps
those pours as planes other nets stay clear of and holds the block's parts still. Run that
again after pulling a new input.
