# One system: Boardroom with Composer and UltraRoute as crates

Status: draft 2026-11-12 for the user's review. Nothing here is done. It is drafted in the
ultraroute repository and moves to Boardroom at cutover. It replaces
`boardroom/docs/monorepo-spec.md` (2026-11-08) in approach; that spec's inventory of what
exists stays useful. It goes with `docs/spec/from-zero.md`: the router is being rebuilt from
zero, and this says where it is built.

## Decisions already made by the user (2026-11-12)

- One system with several crates, in the Boardroom repository. No directory shaped like an
  old repository (`composer/`, `ultraroute/`).
- Three modes of one app: **Boardroom** is conceptually the file manager, **Composer** the
  schematic and floorplanning, **UltraRoute** the part that lays all the open copper.
- No git history is imported. Code appears in Boardroom as new files.
- No byte-identical-output requirement.
- Claude does the migration; no Antigravity agents. The stage-1 import script on Boardroom's
  `origin/main` (worktree `monorepo-stage-1`) is not used.
- The current router is frozen as a baseline (`from-zero.md`).

## What the surveys found (2026-11-12, read-only)

- Composer already reads boards through ultraroute's `kicad-pcb` crate (a git dependency at
  0e581c4), so there is one board reader today. Its `pcb` crate is a second in-memory board
  model (copper per net) on top of it.
- Composer's `intent` crate is the schema of `intent.json`: per net a class, voltage, current,
  noise as aggressor and victim, bandwidth, impedance, match group, maximum length, each
  with a strength and where it came from; and blocks with roles (`role -> REF.PIN`), domains,
  regions, protection paths. `recognise` drafts it from a netlist or a board's pads (rails,
  clocks, pairs, buses, analogue, switch nodes, blocks), each guess with a confidence.
  `verify` measures buck and half-bridge loops, Kelvin sense, connector protection, return
  path and decoupling distance on a board.
- Composer has no schematic reader, only a netlist reader that shells out to `kicad-cli`,
  and nearly all its board writing is Python through pcbnew (`apply_layout.py`,
  `apply_placement.py`, `prepare_route.py`).
- Composer and the router meet through a compiled sidecar: `compile` turns `intent.json`
  into `routing.json` and `.kicad_dru` for the router.
- Boardroom links none of the others. It proxies Composer's and Studio's own servers
  (`app/server/src/apps.rs`), runs `ccomp` and `ultraroute` as subprocesses, vendors
  ultraroute and pcbview as submodules, and clones ultraroute a second time at another
  revision to build WASM. It has two crates, no workspace and no toolchain pin.
- Board repositories (round-pendant, simple-earring, robot-link, and every `new-board.sh`
  output) pin `vendor/ultraroute` and `vendor/pcbview` by commit.

## The structure

One Cargo workspace at Boardroom's root, one npm workspace, one toolchain pin. Crates are
named for what they do. A crate may depend only on crates in the groups above it.

```
boardroom/
  crates/
    # Foundation: files and the board
    kicad/        the only place KiCad files are parsed and written: board, project rules,
                  .kicad_dru, netlists; schematics (current and legacy) when added
                  (from ultraroute kicad-pcb, intent's netlist reader)
    board/        the one in-memory board: items, nets, layers, rules, geometry, copper per net
                  (ultraroute board and Composer pcb, merged)
    check/        the exact rule checker that agrees with KiCad (from ultraroute board::check)
    shape/        the board outline document (Composer shape)

    # Understanding: shared by Composer and UltraRoute
    intent/       intent.json: nets, blocks and their roles, domains, regions; extended with
                  the layer plan, frames by region and repeats (Composer intent)
    recognise/    drafts intent from a netlist, a schematic or a board; and from a finished
                  board recovers what its designer chose (Composer recognise, extended)
    library/ interfaces/     cited claims, typed interfaces (Composer)

    # Composer: schematic and floorplan
    compose/ pinsolve/ floorplan/ blockgen/     (Composer)

    # UltraRoute: the open copper (all new, from-zero.md)
    plan/         the whole-board plan: which gaps each connection passes, in what order
    draw/         wires, frames, the wire fit, one writer
    route/        plan, repeats, draw, check, as one call

    # Judging
    examine/      the two scoreboards: like the designers, and sound (Composer verify, the
                  style features of tools/tidy.py, new physical measures)
  app/
    server/       one server; links the crates; no proxies, no subprocesses for our own tools
    core/         one WASM crate for the browser
    web/          one web app: Projects, Compose, Route
    gtk/          the window
  cli/            one binary, `boardroom`, with `compose`, `route`, `check`, `examine` commands
  bench/          the corpus harness, the look set, the scoreboard (ultraroute tools/)
  packages/ library/      Composer's data
  gpl/kicad-text/         GPL text outlines: its own binary, run as a process, never linked
  docs/ scripts/ template/ examples/ .claude/skills/
```

What changes in kind, not only in place:

- **The seam between Composer and the router is `intent.json` and the board, nothing else.**
  The router reads intent itself. `compile`, `routing.json` and the `--regions` flag go away.
  Block roles for hot loops are already in intent (`Block.roles`), so the `blocks` list
  proposed to Circuit Composer for `routing.json` is not needed.
- **Recognition is one crate used three ways:** to draft intent for a new design, to give the
  router roles on a board that has none, and to read a designer's finished board for the
  scoreboard.
- **Board writing moves to Rust** (`kicad`), so applying a block, a placement or a route is
  one call, not a pcbnew script. `kicad-cli` stays as the outside judge for DRC.
- **The server calls functions.** The proxies to Composer's and Studio's servers, the `ccomp`
  subprocess and the second ultraroute clone for WASM are removed.

What does not move:

- The current router (`core/crates/grid`, `ultraroute`, `morph`, `topo`, `studio`) stays in
  the ultraroute repository as a pinned binary the benchmark runs for comparison. The Route
  mode keeps showing today's Studio through the existing proxy until the new router has its
  own view, then the proxy and the submodule go.
- The lab (`ultraroute-lab`) stays a directory beside the repository.
- Board repositories keep their pins on the old ultraroute and pcbview repositories, which
  stay as they are, until the new router is ready; then `new-board.sh` and the scripts point
  at Boardroom.

## Order

Each step leaves Boardroom building and its tests passing.

1. **Preconditions.** Composer's 36 uncommitted files and Boardroom's 5 are committed or set
   aside by their sessions; both sessions are told. The user answers the questions below.
2. **Workspace.** Root `Cargo.toml`, toolchain pin, `crates/`; Boardroom's server and gtk
   become members.
3. **Foundation.** `kicad`, `board`, `check` come in from ultraroute with their tests;
   Composer's `pcb` is merged into `board`.
4. **Understanding.** `intent`, `recognise`, `library`, `interfaces`, and `verify` as the
   start of `examine`. From here the ground-truth work of `from-zero.md` (pours restored,
   schematics, designer extraction, scoreboards) is done in Boardroom, with `bench/`.
5. **Composer.** `compose`, `pinsolve`, `floorplan`, `blockgen`, `shape`; `ccomp`'s commands
   become `boardroom compose ...`; its web app becomes the Compose mode; its Python board
   writers are replaced as `kicad` learns to write.
6. **UltraRoute.** `plan`, `draw`, `route` are written here from zero; the Route mode.
7. **Retire.** The proxies, `vendor/ultraroute`, the WASM clone, `compile`; the
   circuit-composer repository is archived.

Steps 4 and 5 can overlap; step 6 starts as soon as step 4's scoreboards exist.

## Answers from the user (2026-11-12)

1. One binary `boardroom` with `compose`, `route`, `check` commands: yes.
2. Board repositories stay pinned to the old repositories until the new router is ready: yes.
3. pcbview is brought in too (not kept outside): it lives in this repository, as the `view`
   command of the one binary; its Python stays as it is at first.
4. Composer's eight boards become Boardroom projects, not source: yes. How projects are
   stored is the next section.
5. UltraRoute's public replay site stays on the old repository as a record: yes.

## Projects: where boards live (proposal, not yet agreed)

What the user wants: a board, or several, lives in its own repository; every project
carries at least its board view; Boardroom on a machine knows its projects from a config
file in the home directory that points at folders; sample boards may live in the Boardroom
repository, and other boards are linked somehow.

Proposed:

- **A project is a folder with a `boardroom.toml` at its root.** It can be a whole
  repository or a folder inside one, and it can hold several boards
  (`hw/<board>/<variant>/`, as today). The file names the boards and the Boardroom version
  the project was last built with. It replaces `pcbview.toml` and the scattered per-tool
  settings over time.
- **The machine's list is `~/.config/boardroom/projects.toml`:** paths to project folders,
  managed by `boardroom add`, `remove` and `list` (these commands exist already). Boardroom
  never requires a project to be inside its own repository.
- **A project holds data, not tools.** Board repositories stop vendoring the router and the
  viewer as submodules and stop carrying copies of the scripts. They use the installed
  `boardroom` binary and record its version in `boardroom.toml`. This is what ends the
  problem of old pins and mirrors.
- **Every project carries its view as output:** the static `viewer/` folder that
  `boardroom view` builds is committed in the project, so the board can be looked at on
  GitHub or Pages with nothing installed. That is the "at least the board view" each
  project has, without the viewer's source in it.
- **Each mode writes only its own folder,** as now: KiCad files in `hw/`, Composer's in
  `compose/`, the router's in `route/`, the shared outline in `interface/`, the design
  record in `design/`. KiCad files remain the shared truth.
- **Sample boards:** a few small ones live in Boardroom's `examples/` because Boardroom's
  own tests and demos need them. Real boards that are also used as samples are listed in
  `examples.toml` by repository URL and revision and cloned on demand into a cache
  (the file exists today, listing simple-earring by repository). No git submodules: a
  listed URL with a revision gives the same pin without detached checkouts inside the
  Boardroom tree, and Boardroom does not have to be updated when a board moves on.
- **Composer's eight boards:** the ones that are real designs (earrings, flight controller,
  motor controller) become projects in their own repositories; any that exist only as
  fixtures are cut down and kept under `examples/` or `bench/`.
