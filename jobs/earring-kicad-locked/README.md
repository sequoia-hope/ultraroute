# earring-kicad-locked

The same board as `earring-kicad` (simple-earring `out/` at bb22fb7, the WS2816C ring; leaderboard rows before 2026-10-25 are on 78920f1), but
routed the way the board's own lock policy asks: locked copper stays (`"strip": true` takes off
only unlocked tracks and vias), and nudging may move only the parts the commit leaves free.
Locked there: LED1-45, J1, SW2, BT1, Q1, U1, U2, U5, R14, L1, C11, C12, C13, R13, C22.

## Blocks

`job.json` declares the RP2350's power block (`blocks.rp2350_power`): the reference design's 1V1
(`vreg_vdd_core_3`, `_6`) and 3V3 (`vreg_3v3_1`, `_7`) pours with U1, L1, C11 and C12, applied
to the input as a KiCad group by `tools/add_block.py --job jobs/<this job>`. The router keeps
those pours as planes other nets stay clear of and holds the block's parts still. Run that
again after pulling a new input.
