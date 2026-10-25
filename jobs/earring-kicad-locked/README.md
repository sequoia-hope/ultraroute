# earring-kicad-locked

The same board as `earring-kicad` (simple-earring `out/` at 78920f1, the WS2816C ring), but
routed the way the board's own lock policy asks: locked copper stays (`"strip": true` takes off
only unlocked tracks and vias), and nudging may move only the parts the commit leaves free.
Locked there: LED1-45, J1, SW2, BT1, Q1, U1, U2, U5, R14, L1, C11, C12, C13, R13, C22.
