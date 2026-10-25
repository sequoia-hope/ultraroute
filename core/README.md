# The MIT routing core

A KiCad router written from the file formats and our own algorithms, under the MIT license.
It reads and writes `.kicad_pcb` boards directly; nothing here is ported from copperroute or
freerouting.

| Crate | What |
|---|---|
| `kicad-pcb` | `.kicad_pcb` reader (from waffle-iron's, same author) and writer: copper layers, tracks, vias, zones, keepouts, pads with their exact shapes, courtyards, and `.kicad_pro` rules and net classes. The writer replaces routed copper and moved parts and keeps every other byte. |
| `board` | The board in integer nanometres: shapes per layer in a grid index, connectivity as KiCad joins copper, and the clearance, edge, hole and keepout checks, pinned by tests to KiCad 9's DRC. |
| `grid` | Routing: a conservative grid per net class, A* (CPU) or a GPU field solver, negotiated congestion, a rip-up finisher, part nudging, and pulling copper tight. |
| `topo` | Topological routing: a Delaunay triangulation of the copper per layer, a search for a channel wide enough, and the taut rubber band through it, with rip-up of its own copper. Written by us first on copperroute's model and re-targeted onto `board`; none of copperroute's code comes with it. |
| `ultraroute` | The command. |

## Route a board

```sh
cargo build --release -p ultraroute
target/release/ultraroute route board.kicad_pcb -o routed.kicad_pcb --negotiate --rip-up --tighten
```

The project's `.kicad_pro` beside the board supplies the rules. Pours are left for KiCad to
refill: `python3 ../tools/refill_zones.py routed.kicad_pcb filled.kicad_pcb`, then judge it with
`kicad-cli pcb drc filled.kicad_pcb`.

| Option | |
|---|---|
| `--strip` | take out every unlocked track and via first |
| `--topo` | route as rubber bands through a triangulation of the copper first; the grid finishes what is left |
| `--negotiate` | negotiated congestion before routing one connection at a time |
| `--rip-up` | let blocked connections cross the router's own copper, rerouting what they rip |
| `--nudge` | move unlocked parts aside for connections nothing else could route |
| `--tighten` | straighten the routed copper |
| `--lock REF,...` | parts that may not move |
| `--place REF=X,Y` | put a part at X,Y mm (studio coordinates: Y up) before routing |
| `--timeout S` | stop routing after S seconds; the result so far is written |
| `--gpu` | solve cost fields on the GPU (built by default; slower than A* on the CPU for now) |
| `--live PATH\|-` | stream the board as the studio's JSON frames; with `-`, a `stop` line on standard input stops the run |
| `--no-route` | only load, strip and place, then write |

`tools/kicad_bench.py` routes a job with this and with copperroute and judges both by KiCad.
