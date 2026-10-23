# simple_earring routing job

Input: `input/earring.dsn`, exported with pcbnew's `ExportSpecctraDSN` from
`~/pcb/simple_earring/out/earring.kicad_pcb` at commit afabd86 (LiPo rev, 2026-10-22).
That board holds only the 72 locked scripted tracks and the zones from `scripts/route.py`,
so the DSN is exactly what the project hands Freerouting. The earring repo's own
`out/earring.dsn` is from Aug 31 (coin-cell rev) and is not used.

Locked for placement tools (user's choice): every LED, the USB connector J1, buttons SW1 and SW2.
