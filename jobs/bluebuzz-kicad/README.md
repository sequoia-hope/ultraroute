# bluebuzz-kicad

bluebuzz's own KiCad board (github.com/sequoia-hope/bluebuzz `out/` at bf235b5), for the MIT core
router, which reads `.kicad_pcb` directly. Most of its wiring is locked; strip the rest
(`--strip`) to route it. Results are judged by KiCad's DRC after `tools/refill_zones.py`.
