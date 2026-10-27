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

## Planes

In1.Cu is the board's GND reference plane. Routed freely, 250-340 mm of signal ran on it, the USB
pair included, and cut it into pieces; with `--planes In1.Cu` (now in `job.json`) nothing routes
there and seeds 1 and 2 still route completely (lab `earring-planes-in1`, from the simple-earring
session, whose board is that P-s1 route). `--auto-planes` finds such layers by their pours.

Input refreshed 2026-10-27 from the simple-earring session (route.py fix): the 8 free-standing
assist vias it had left (their tracks unlocked, so stripped, so the vias dangled), USB_C_DP's
B.Cu stub past B6, the VBUS trunk's end and R14's 3V3 via-in-pad are gone or fixed; pre-route
KiCad DRC 0 errors. Their seed 3 with this job's recipe routes clean (lab earring-noassist).

`--pour-cost 3` (in `job.json`): crossing another net's pour on In2 (VLED_L/C/R, 3V3) costs three
times the length inside, so routes keep the pours whole where they can: In2's poured area
425-433 mm2 against 361-395 without, VLED_C in 2-4 pieces against 5-8, all seeds 0-2 still
complete, about 10% more track (lab pours-1).

