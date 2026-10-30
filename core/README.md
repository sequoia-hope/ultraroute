# The MIT routing core

A KiCad router written from the file formats and our own algorithms, under the MIT license.
It reads and writes `.kicad_pcb` boards directly.

| Crate | What |
|---|---|
| `kicad-pcb` | `.kicad_pcb` reader (from waffle-iron's, same author) and writer: copper layers, tracks, vias, zones, keepouts, pads with their exact shapes, courtyards, and `.kicad_pro`/`.kicad_dru` rules and net classes. The writer replaces routed copper and moved parts and keeps every other byte. |
| `board` | The board in integer nanometres: shapes per layer in a grid index, connectivity as KiCad joins copper, and the clearance, edge, hole and keepout checks, pinned by tests to KiCad 9's DRC. |
| `grid` | Routing: a conservative grid per net class, A* on the CPU (a GPU field solver, experimental, behind `--features gpu`), pre-fanout, differential pairs, negotiated congestion, a rip-up finisher, the plough, part nudging, via reduction and pulling copper tight. |
| `morph` | Moving copper smoothly: a harmonic warp field, copper carried along with moving parts, and finding the fine-pitch parts whose pads are hard to escape. |
| `topo` | Topological routing: a Delaunay triangulation of the copper per layer, a search for a channel wide enough, and the taut rubber band through it. An experiment: the command does not link it; it is built and tested with the workspace. |
| `ultraroute` | The command, and the same pipeline as a library (`parse`, `route`, `run_with_solver`). |
| `web` | `ultraroute` compiled to WebAssembly for the studio's router in the browser tab. |
| `lab-films` | `feature_film`: records one stage of a run as a film for `lab/features.html`. |

`docs/library.md` explains the crates for a program of your own.

## Build and route a board

Needs Rust; `rust-toolchain.toml` pins 1.95.0, which `rustup` installs on first use. From `core/`:

```sh
cargo build --release -p ultraroute
mkdir -p /tmp/earring
target/release/ultraroute route fixtures/earring.kicad_pcb -o /tmp/earring/routed.kicad_pcb --timeout 120
```

The `.kicad_pro` (and `.kicad_dru`) beside the input supply the rules. The run prints a line per
stage to standard error; near the end, `routed 0 more, 1 failed, 0 open, 20.5 s` counts the
connections left open (that is the earring on loaf, 2026-10-30: 21 s, 227 MB peak). Pours are
left as they were, for KiCad to refill. To judge the result as the benchmark does, refill the
zones and run KiCad's DRC with the project file beside the board:

```sh
cp fixtures/earring.kicad_pro /tmp/earring/routed.kicad_pro
python3 ../tools/refill_zones.py /tmp/earring/routed.kicad_pcb /tmp/earring/filled.kicad_pcb
cp fixtures/earring.kicad_pro /tmp/earring/filled.kicad_pro
kicad-cli pcb drc --severity-error /tmp/earring/filled.kicad_pcb -o /tmp/earring/drc.rpt
```

For the earring that reports `Found 0 violations` and `Found 0 unconnected items`. Without
`--severity-error` KiCad also lists the board's own warnings (silkscreen, footprint library),
which the router does not touch.

`refill_zones.py` needs KiCad's `pcbnew` Python module (KiCad 9). `ultraroute route --help`
prints every flag.

Run the tests with `cargo test --workspace` (about 240 tests), and `cargo clippy --workspace
--all-targets -- -D warnings` before a commit. `--features gpu` adds the GPU solver and its test.

## The default recipe

A route command that names no stage runs the default recipe, `ultraroute::RECIPE`:

    --negotiate --rip-up --tighten --prefanout --diff-pairs --shove-plough region --shove-reach 0.5
    --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8

It is the benchmark's recipe (`docs/pcbench.md`), measured as the best the router has.
`--recipe default` (or `bench`, the same) puts it first and lets later flags override it.
Naming stages yourself runs those alone, and `--recipe minimal` runs none but the grid router
and the clean-up. A via costs 5 mm of straight track in negotiation (`ultraroute::VIA_COST_MM`,
measured at 1.0, 2.5 and 5.0 mm), 1.2 times that when routing one connection at a time and 0.8
times it in rip-up; on boards with planes, 1.0, 0.4 and 0.4 mm.

## The stages

A run is the stages of `ultraroute::STAGES` in this order (`core/crates/ultraroute/src/lib.rs`;
each stage is in `stages.rs`). The table is the only place the order and the time budgets live.
A stage runs only when its flag or the board asks for it.

| Stage | Runs when | Time it gets |
|---|---|---|
| `set-up` | always: locks, `--place`, strip, planes, `--adopt`, `--regions`, prices, `--rip-region` | no limit |
| `prefanout` | `--prefanout` | 15% of what is left of the run |
| `planes` | a layer is a plane (`--planes`, `--auto-planes`) | 5% of what is left |
| `pairs` | `--diff-pairs` | until a quarter of `--timeout` has gone |
| `pair-escapes` | `--prefanout`, after `pairs` | 15% of what is left |
| `routing` | always; holds the next six | |
| `first` | `--regions` gives nets a length limit | a quarter of what is left of the routing time |
| `negotiation` | `--negotiate` | half of `--timeout`; after `first`, half of what is left of the routing time |
| `finishing` | always: one connection at a time | the routing time |
| `prune-escapes` | there are escapes | no limit |
| `rip-up` | `--rip-up` | the routing time |
| `merge-escapes` | there are escapes | no limit |
| `shove` | `--shove` or `--shove-plough` | the routing time |
| `nudge` | `--nudge` | the routing time |
| `tightening` | `--tighten` | the run's time |
| `vias` | the router laid copper | the run's time; it keeps 10% of `--timeout` back from the routing time |
| `cleaning` | always, whatever the time | no limit |
| `length matching` | `--diff-pairs` | the run's time |
| `result` | always: what is left open, with `--blame` what blocks it | no limit |
| `write` | always | no limit |

The run's time is `--timeout` less a reserve for cleaning and writing (2% of it, at least 2 s).
The routing time is the run's time less what `vias` keeps back. With no `--timeout` nothing is
limited. Once the run's time has gone, no timed stage starts.

`--stats FILE` (or `--report FILE`) writes the run as JSON: per stage its wall time, work counts
and open connections before and after; the input's hash, the router's version and commit, the
settings, peak memory, whether the deadline fired and in which stage, every connection given up
with its stage and reason, and findings: connections left open, forced necks, regions,
lengths, the deadline, and, of kind `rule`, every project or custom rule left out or not applied
(the same lines the run prints as warnings). `--verbose` prints the same as a table.

## Every flag

| Flag | |
|---|---|
| `-o`, `--output OUT` | where to write the routed board |
| `--recipe default\|bench\|minimal` | the default stages (see above), or none; later flags override it |
| `--strip` | take out every unlocked track and via first |
| `--strip-all` | take out every track and via first, locked ones too (the benchmark uses this) |
| `--adopt` | treat the board's unlocked tracks and vias as the router's own: free to rip, move and rewrite |
| `--no-route` | only load, strip and place, then write |
| `--prefanout` | escape every fine-pitch part's pads before routing |
| `--prefanout-mode stub\|dogbone\|grid\|inner\|same` | how the escapes are laid; default `inner` (a fine-grid route across layers). `same` first tries an escape without a via for a pad whose net meets no other layer; measured as no better than the default (`docs/pcbench.md`), kept as an option |
| `--prefanout-order round\|inner\|trapped` | the order a part's pads claim escapes; default `inner` |
| `--prefanout-ring MM` | how far past the part's pads the escapes end (0.2 to 5 mm; default 1.1) |
| `--diff-pairs` | route differential pairs coupled, before the rest, and match their lengths at the end |
| `--negotiate` | negotiated congestion: every connection routed at once, round after round, until none share |
| `--negotiate-rounds N` | most rounds (default 30) |
| `--negotiate-patience N` | rounds without fewer collisions before it stops (default 4) |
| `--negotiate-growth G` | how fast the congestion price grows per round (default 1.6; at least 1) |
| `--negotiate-repasses N` | passes after the first over what it left open (default 2) |
| `--negotiate-jacobi` | each round routes against the last on `--threads` threads, instead of one connection after another; slower to converge on the fixtures (Stage 4 notes) |
| `--negotiate-reroute-all` | every round reroutes every connection, not only those that collided (the old way; slower) |
| `--rip-up` | let blocked connections cross the router's own copper, rerouting what they rip |
| `--shove` | plough a way out for connections trapped at fine-pitch pads |
| `--shove-plough rip\|region` | how the plough clears the copper in its way (implies `--shove`); `region` rips round the pinch and negotiates it again |
| `--shove-reach MM` | how far the plough clears (default 2) |
| `--shove-repair S` | seconds of rip-up repair after each try (default 10) |
| `--shove-passes N` | plough passes, each in another order (default 1) |
| `--shove-patience N` | passes in a row that free nothing before it stops (default 1) |
| `--shove-ties` | keep a try that frees its connection without losing ground elsewhere |
| `--shove-cascade N` | connections a try knocked loose that are ploughed in turn (default 0) |
| `--nudge` | move unlocked parts aside for connections nothing else could route; not in the default (it moves the designer's parts) |
| `--tighten` | straighten the routed copper |
| `--via-cost MM[,OPEN_MM]` | a via's price in mm of straight track (0.05 to 50): negotiation's, and with OPEN_MM that of routing one connection at a time and of rip-up (else 1.2 and 0.8 times MM) |
| `--via-avoid X0,Y0,X1,Y1` | a box (mm, KiCad's frame) where vias cost dearly but are allowed; repeatable |
| `--via-avoid-cost N` | that price, in search units (10 a straight cell; default 3000) |
| `--pour-cost W` | crossing another net's pour costs W times the length inside (default 0) |
| `--pours-block` | treat pours as walls, and count connections through them as made |
| `--planes LAYER,...` | layers given whole to a plane: no tracks on them, vias through them |
| `--auto-planes` | take inner layers one net's pour all but fills as planes |
| `--regions FILE` | a planner's routing regions and length limits (circuit-composer's `routing.json`) |
| `--rip-region X,Y,R` | rip every unlocked track and via in this disc (mm, KiCad's frame) first; repeatable |
| `--lock REF,...` | parts that may not move |
| `--place REF=X,Y` | put a part at X,Y mm before routing; Y points up (KiCad's y negated); repeatable |
| `--cell MM` | the grid cell for every stage (0.005 to 1; default 0.05) |
| `--timeout SECONDS` | the run's time; the board routed so far is written when it runs out |
| `--seed N` | reorders connections of nearly equal length, for a search over runs (default 0) |
| `--uuid-seed N` | draw the new copper's uuids from this seed, so the output is the same every time |
| `--threads N` | threads for pre-fanout and Jacobi negotiation (default half the machine's) |
| `--max-memory MB` | the most memory the run may take; default the cgroup's `memory.max`, else 60% of available memory. Over it, the run first gives up Jacobi threads, then routes on a coarser cell, up to 0.1 mm; a board that still does not fit is refused (exit 1) and the input is written as the output |
| `--estimate` | print what the run's grids will take and the limit, then stop |
| `--gpu` | solve cost fields on the GPU: experimental, about 10x slower than A* on the CPU, and only in a router built with `--features gpu` |
| `--live PATH\|-` | stream the board as the studio's JSON frames; with `-`, to standard output, and a `stop` line on standard input stops the run |
| `--live-fps N` | the stream's frame rate (default 30) |
| `--live-step MS` | least time between the stream's step frames |
| `--blame` | list what stands in the way of every connection left open |
| `--stats FILE`, `--report FILE` | write the run's stats and findings as JSON |
| `--verbose`, `-v` | everything the router does, on standard error, and the stats table |
| `-h`, `--help` | print the flags |

A test (`tests/cli.rs`) checks that this table names every flag in `ultraroute::USAGE`.
