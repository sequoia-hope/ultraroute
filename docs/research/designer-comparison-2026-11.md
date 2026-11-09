# Our routing against the designers': first corpus run

Nov 5, 2026. `tools/designer.py --run full-1e935fc` on the build server: the 1009 boards that run
routed clean (KiCad DRC, 120 s), each compared with the designer's own routing of the same
board (`ultraroute-lab/pcbench/boards/<id>/board.kicad_pcb`). This is the first check of the
Examiner's "no worse than the designer" (docs/spec/verification.md). It needs no intent: what
the designer did is the evidence. Per-board results are in each run folder's `designer.json`.

## What is checked

- **width:** a net the designer widened (its usual width at least 1.5 times the board's usual
  track width) that ours routes narrower, anywhere, than 0.9 of the designer's usual width.
- **vias:** a widened net with more than 2 vias more in ours.
- **length:** a net more than twice as long in ours, and 10 mm longer.
- **decap:** a decoupling capacitor (one pad on a ground net, the other on a net an IC is on)
  whose track path to the nearest IC pin on that net is 3 mm and 1.5 times longer in ours, or
  has no track path in ours where the designer's has one.

## Results

| | Designer | Ours |
|---|---:|---:|
| Track length, 1009 boards | 1253 m | 977 m |
| Vias | 24 011 | 26 743 (+11 %) |

| Check | Findings | Boards (of 1009) |
|---|---:|---:|
| width | 2824 of 3465 widened nets (82 %) | 422 |
| decap | 270 of 6272 capacitors (4.3 %) | 153 |
| vias | 121 | 92 |
| length | 101 | 60 |

- **Width is the one large failure.** The designers widened 3465 nets, mostly power, and we
  route 82 % of them narrower; the designer's width over ours is about 2 on 1410 nets, about 3
  on 621, and 4 or more on 750. PCBench boards set no width rules, so the
  router uses each net class's width, the board's minimum. The boards are DRC-clean and would
  carry far less current than their designers meant. Example: m2r's `+BATT` is 1.5 mm in the
  designer's routing and 0.2 mm in ours.
- **Decoupling is mostly kept.** 95.7 % of capacitors' paths are within the margin. Of the 270
  findings, 202 are longer (median 2.4 times the designer's), and 68 have no track path in ours,
  which means the capacitor reaches its IC only through a pour, or not along tracks this tool
  follows (it does not follow pours).
- **Vias and length are small.** Our total track length is 22 % shorter than the designers',
  and 11 % more vias.
- Most findings: LimeSDR_Sony (119), ADC-PCM4202-SE (64), digital_clock_led_clock_v1 (55).

## Caveats

- A widened net is the designer's choice, not proof of need: some designers widen signal nets
  for looks or for etching. The width finding is "worse than the designer", not "wrong"; intent
  (current) from the recogniser will say which ones matter.
- Paths follow tracks and vias only. A track joins a pad or via where its round end's copper
  reaches it (an early version required the end inside the pad, and reported 547 false "no
  path" findings; fixed in the second commit).
- Ground is recognised by name (GND, VSS, AGND, ...); a board naming its ground otherwise has
  no decoupling capacitors found.

## What it suggests for the router

Width per net is the first thing to fix. The router can't know a net's current on these boards
without intent, but it can be given widths: from intent once the recogniser runs (current per
net, IPC-2152), and before then from net names and connections (supply nets, nets on a
regulator's output or a connector's power pins), as a soft preference with a fallback to the
minimum where the wide track does not fit.
