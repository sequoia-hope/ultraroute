# ultraroute

A fast KiCad autorouter, a bench for comparing routing approaches, and **Studio**: a live,
interactive view of the router at work. The project page is published at
<https://sequoia-hope.github.io/ultraroute/>; there the studio plays back recorded runs.

The router itself is [copperroute](https://github.com/sequoia-hope/copperroute) (branch
`governance`, forked from emshotton/copperroute), checked out at `copperroute/`. It is not part
of this repository.

## Run the studio

Needs Rust (with the `wasm32-unknown-unknown` target), `wasm-pack`, Node 22 and a built router.

```sh
git clone -b governance https://github.com/sequoia-hope/copperroute
(cd copperroute && CARGO_TARGET_DIR=../target-topo cargo build --release -p copperroute)
studio/build.sh
studio/target/release/studio-server --root . --port <port>
```

Then open `http://localhost:<port>/studio/`: pick a board and a recipe, watch it route, drag or
nudge parts, lock them, and see where the board is congested. On this machine the port comes
from the project registry, and `./serve.sh` starts the server.

| Path | What |
|---|---|
| `studio/` | Server (runs the router, relays its live stream), Rust core compiled to WASM, Svelte client |
| `tools/route_job.py` | Route one board many ways, one governed attempt at a time; keeps `leaderboard.csv` and `best/` |
| `jobs/<board>/` | One route job per real board: `input/` snapshot, configs, leaderboard, best result |
| `morph-frames/` | Board-reshaping runs and their viewer |
| `ROADMAP.md` | The plan, phase by phase |

## License

ultraroute is MIT-licensed; see [LICENSE](LICENSE).

copperroute is a separate program under GPL-3.0-only: it is a derivative of
[freerouting](https://github.com/freerouting/freerouting) (GPL-3.0) with parts of KiCad's
design-rule checker (GPL-3.0-or-later). ultraroute does not include or link its code; the
studio and the tools run the `copperroute` executable as a separate process and read its
output. Keep it that way: building the router into the studio (for example as WebAssembly in
the browser) would make the studio a GPL work too.
