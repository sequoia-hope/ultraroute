# earring-kicad

The simple earring's KiCad board (github.com/sequoia-hope/simple-earring `out/` at bb22fb7, the WS2816C LED ring with 19 parts moved to the back to clear the CPU; leaderboard rows before 2026-10-25 are on 78920f1), for
the MIT core router. Results are judged by
KiCad's DRC after `tools/refill_zones.py`.

## Blocks

`job.json` declares the RP2350's power block (`blocks.rp2350_power`): the reference design's 1V1
(`vreg_vdd_core_3`, `_6`) and 3V3 (`vreg_3v3_1`, `_7`) pours with U1, L1, C11 and C12, applied
to the input as a KiCad group by `tools/add_block.py --job jobs/<this job>`. The router keeps
those pours as planes other nets stay clear of and holds the block's parts still. Run that
again after pulling a new input.
