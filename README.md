# ultraroute

A KiCad autorouter under the MIT license, a benchmark that judges it by KiCad's own checks, and
**Studio**: a live, interactive view of the router at work. The project page is published at
<https://sequoia-hope.github.io/ultraroute/>; there the studio plays back recorded runs.

The router is the core in `core/`: Rust crates that read a `.kicad_pcb`, route it and write it
back, keeping every byte they did not change. It runs as a command, `ultraroute route`, and the
same code compiled to WebAssembly routes in the studio's browser tab. On the PCBench sample it
benchmarks on (200 boards, 120 s each), 164 boards come out clean by KiCad's DRC (see
`ROADMAP.md`, "State at 2026-10-30").

## Build and route a board

Needs Rust (`core/rust-toolchain.toml` pins 1.95.0). From `core/`:

```sh
cargo build --release -p ultraroute
target/release/ultraroute route fixtures/earring.kicad_pcb -o /tmp/routed.kicad_pcb --timeout 120
```

With no stage named it runs the default recipe, the benchmark's. The rules come from the
`.kicad_pro` and `.kicad_dru` beside the input; pours are left for KiCad to refill.
`core/README.md` lists the crates, the stages in their order, and every flag, and shows how to
judge the result with KiCad; `ultraroute route --help` prints the flags.
`docs/library.md` is for programs of your own built on the crates.

## Benchmark

`tools/pcbench.py` routes boards of [PCBench](https://github.com/emshotton/PCBench) from bare
pads and judges each by KiCad 9 (zone refill, then DRC). `docs/pcbench.md` says how to prepare
the boards, run a sample, and compare two runs against the noise floor.

## Run the studio

Needs Rust (with the `wasm32-unknown-unknown` target), `wasm-pack` and Node 22.

```sh
studio/build.sh
studio/target/release/studio-server --root . --port <port>
```

`build.sh` builds the server, the core's `ultraroute`, and the core as WebAssembly for the tab.
Then open `http://localhost:<port>/studio/`: pick a board, route it with the default recipe or
stages of your choice, watch it route, drag or nudge parts, lock them, and see where the board
is congested. On this machine the port comes from the project registry, and `./serve.sh` starts
the server.

A Boardroom project opens by its path: `/?project=<absolute path>&board=<name>&variant=<v>`
(board and variant may be left out when the project has one board). The studio routes
`hw/<board>/<variant>/<board>.kicad_pcb` with the `.kicad_pro`, `.kicad_dru` and `routing.json`
beside it; **Save to project** writes the routed board back over that file, unless it changed on
disk since it was loaded, and records the route and the router's report under
`apps.studio.<board>` in `design/status.json`.

| Path | What |
|---|---|
| `core/` | The router: crates, the `ultraroute` command, test fixtures |
| `studio/` | Server (runs the router, relays its live stream), Rust scene core compiled to WASM, Svelte client |
| `tools/pcbench.py`, `tools/lab.py` | The benchmark, and lab runs recorded under `~/Software/ultraroute-lab` |
| `docs/` | The library guide, the benchmark, the studio's design, research notes |
| `lab/` | Pages of recorded stage films and fan-out experiments |
| `ROADMAP.md` | The plan, phase by phase |

## Legacy: copperroute

Before the core, the router was [copperroute](https://github.com/sequoia-hope/copperroute)
(branch `governance`, forked from emshotton/copperroute), a Rust port of freerouting, run as a
separate program from a checkout at `copperroute/`. It is kept as a reference router. The studio
can still run it on the jobs that have a Specctra `.dsn`, and `studio-server` still looks for its
binary at `target-topo/release/copperroute` (or `--router PATH`) when it starts. The tools of that
era stay in this repository: `tools/route_job.py` (route a job many ways, keep a leaderboard),
`tools/kicad_bench.py` (the core and copperroute on one board, judged by KiCad), `jobs/<board>/`,
`morph-frames/`, `experiments.json` and `history.html`. Whether they move to a `legacy/`
directory is not yet decided. copperroute is GPL-3.0-only (a derivative of freerouting, with
parts of KiCad's design-rule checker); ultraroute does not include or link its code and runs it
only as a separate process.

## License

ultraroute is MIT-licensed; see [LICENSE](LICENSE). The core is our own code, written from the
file formats, so its WebAssembly build in the studio is MIT too. Code from GPL routers must stay
out of `core/` and `studio/`: linking it in, natively or as WebAssembly, would make the studio a
GPL work.
