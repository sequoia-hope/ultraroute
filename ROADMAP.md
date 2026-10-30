# Roadmap

Work proceeds one phase at a time; each phase ends with its scores on the test boards
(bluebuzz, bluebuzz-1024, bluebuzz-ripped, the morph keyframes) and a commit.

## Phase 1 — Topological routing with rip-up

- [x] `copper-topo`: rubber-band routing through a triangulation of the copper, as backend `topo`.
- [x] Rip-up: an open connection may cross other connections' copper at a price that grows with
      each rip; what it crosses is ripped and rerouted, and the board joining most is kept.
- [x] The maze stage after topo runs with a bounded via optimizer (it stalled on one pass).
- [x] Latest bluebuzz with its updated rules as job `bluebuzz-1024`.
- [x] Scores (open connections, all with 0 new errors): bluebuzz-1024 topo+maze 12 (grid 10,
      maze 16); bluebuzz 12; bluebuzz-ripped topo alone 55 → 62 of 69 joined with rip-up, but after
      maze 14 without and 15 with (maze alone 9).
- [x] Keyframe reroute from bare pads, open connections (grid / topo / topo + rip-up): rectangle
      25 / 35 / 24, circle 26 / 33 / 35, diamond 21 / 29 / 33, stretched 53 / 59 / 65, rainbow
      155 / 157 / 157 (topo finds a channel for 5 of 69 there: the parts are packed shut).
- [ ] Known gaps, not blocking: legs from pads within clearance of a keepout, bends that push
      into the next obstacle, connections to traces and pours.

## Phase 2 — Physics diffusion placement

Placement migration after Ren et al. (DAC 2005): part density diffuses as heat does, and parts
ride the flow (velocity −∇d/d), so crowded regions spread while relative order is kept.

- [x] `copper-morph::diffuse`: density from part footprints per side, pin demand and optional
      routing demand (open airlines); walls at the outline; locked parts and groups.
- [x] Moves applied through `MoveOracle`, so no move adds a clearance conflict.
- [x] Backend `diffuse`: route, spread by the open connections' demand, reroute, keep the better.
- [x] Reshape: spread at keyframes, after ripping and before rerouting (`--diffuse-target`).
- [x] Unit tests: spreading lowers the peak, locked parts stay, groups move as one, the move
      limit holds, demand pushes, applied moves add no violation.
- [x] Scores: no gain yet. The backend's spread board was worse every time, so it kept the
      first route: bluebuzz-1024 10 open (29 spread; 17 at target 2.0 and 0.5 mm, 10 parts
      moved), bluebuzz-ripped 9 (70 spread). Keyframe reroute with spreading, open connections
      (vs. without): rectangle 56 (25), circle 62 (26), diamond 59 (21), stretched 83 (53),
      rainbow 104 (155); every keyframe hit the 10 min limit.
- [ ] Why: pin demand puts the peak density near fine-pitch parts above 1 (2.4–7), which no
      move can fix, so at target 0.8 nearly every part moves the full limit; and a moved board
      starts its route afresh within the same time budget. Next: normalise demand per part,
      spread only toward open airlines' congestion, and route moved parts incrementally.

## Phase 2b — Parts set the board's size; several complete layouts (later)

- [ ] Grow the outline until the board routes: start from the parts' packed area, route,
      enlarge where the open airlines crowd, repeat; the outline is an output, not an input.
- [ ] Present several complete laid-out options (placement, outline and copper) that differ
      in size, shape or layer count, each scored by the suite, for a person to choose from.

## Phase 2c — An MIT routing core (proposed)

copperroute is ~106k lines, almost all a port of freerouting (GPL); ours inside it (copper-gpu,
copper-morph, copper-topo, copper-governor) is ~7k. A core written fresh, never ported from
copperroute or freerouting, would let the studio link the router (and route in a browser tab
with WebGPU) under MIT. Estimate ~15k lines:

- [x] Board I/O from `.kicad_pcb` directly (`core/crates/kicad-pcb`, from waffle-iron's reader):
      copper layers, tracks, arcs, vias, zones and rule areas, pad layer sets and exact shapes
      (custom pads included), `.kicad_pro` rules and net classes, and a writer that replaces
      routed copper and keeps every other byte. On bluebuzz all 2290 tracks and 331 vias
      rewritten give KiCad's DRC the same result (0 violations, 0 unconnected).
- [ ] Locked flags kept when copper is written back; `.kicad_dru` custom rules applied.
- [x] Geometry in integer nanometres and a board model (`core/crates/board`): capsules and
      polygons per copper layer, a grid index, holes, keepouts and the outline; written without
      outside crates.
- [x] Connectivity as KiCad joins copper (at anchors: track ends, via and round-pad discs, pad
      centres; pours wherever they touch) and a clearance check (copper, edge, plated-hole, hole
      to hole, keepouts; KiCad's 0.5 µm epsilon). Against KiCad 9's DRC on bluebuzz and the
      earring, clean, stripped, half-stripped and shifted: identical open-connection and
      clearance counts, except 455 vs 458 open on half-stripped bluebuzz. 66 ms connectivity,
      0.5 s full check on bluebuzz.
- [x] Grid routing on the core (`core/crates/grid`, CPU): a conservative obstacle map per net
      class, our integer-cost search, legs out of crowded pads, every piece checked before it is
      kept. Pours are refilled around new copper (`tools/refill_zones.py`), so every connection is
      made in tracks. Judged by KiCad after a refill: the earring 249 → 14 open in 22 s, bluebuzz
      with its unlocked copper stripped 25 → 5 open in 13 s, 0 errors on both.
- [x] Holes as slots (oval drills), not only round (REMEDIATION Stage 9).
- [x] The GPU field solver (`--features gpu`) and negotiated congestion on the core. With
      negotiation then single routing for what is left, after a KiCad refill: the earring 10 open,
      stripped bluebuzz 3 open, 0 errors on both. Slow yet: 30 rounds that do not converge, ~3 min
      on the earring; one field at a time leaves the GPU idle (58 s vs 22 s CPU, sequential).
- [x] `ultraroute route` (`core/crates/ultraroute`): routes a `.kicad_pcb` and writes it back,
      `--place`, `--strip`, `--negotiate`, `--gpu`, `--timeout`, a `stop` line, and `--live` in
      the studio's frame format, negotiation's trial routes included. The studio runs it as the
      Core router on the `bluebuzz-kicad` and `earring-kicad` jobs.
- [x] Negotiation stops after 4 rounds without fewer collisions; pad outlines drawn outside
      their arcs so clearance to them is never overstated. Earring 8–14 open (runs vary),
      bluebuzz 3, 0 errors.
- [ ] GPU batches for negotiation rounds work but are slow: ~110 s a round on the earring's
      3.5M-node fields against ~10 s on CPU threads. The relaxation kernel needs a better
      schedule (or a coarser grid) before the GPU pays. Parked (REMEDIATION Stage 10, 2026-10-30):
      off by default (`--features gpu` builds it in, `--gpu` warns), kept for a frontier or
      windowed kernel; the browser's WebGPU copy was deleted. What runs: A* on the CPU, natively
      and in the tab's worker pool.
- [ ] Topo with rip-up, nudge and diffusion on the core.
- [x] A* search on the CPU (routes as cheap as the full field, a fraction of the work), zones
      sized per pair of net classes, legs necked down to the narrowest track out of crowded pads,
      and a rip-up finisher: a blocked connection may cross copper the router laid, which is
      ripped and rerouted, kept only if fewer connections stay open. After a KiCad refill: the
      earring 3 open in 54 s (copperroute's best is 3, nudge, 511 s; its grid recipe 4) and
      stripped bluebuzz 4, 0 errors on both.
- [x] Deterministic runs (ordered connectivity), a finisher that rips any of the router's
      own copper and retries after progress, and a pull-tight pass that straightens each chain
      of router copper between fixed points while keeping everything it joins. Earring 5 open,
      3403 → 1305 segments, 1207 → 1164 mm; stripped bluebuzz 4 (its leftovers are walled in by
      fixed copper: a job for nudging, not rip-up). 0 errors.
- [x] Courtyards read; a core nudger (`--nudge`): parts near a blocked airline stepped 0.25–1 mm
      in eight directions, kept only when legal (pads clear, no courtyard overlap) and fewer
      connections stay open. On the packed earring nearly every step is illegal and none helps.
- [x] `tools/kicad_bench.py`: both routers on the same KiCad board, judged by KiCad after a
      refill. Earring: core 5 open / 0 new errors / 304 vias / 48 s; copperroute grid 18 / 0 /
      196 / 427 s; copperroute maze 37 / 2 / 140 / 150 s. (Nudge and bluebuzz still to run.)
- [ ] Fewer vias (the core uses ~1.5× copperroute's); diffusion on the core.
- [ ] copperroute kept as the reference router across the process boundary until the core
      matches it on the leaderboards.

## Phase 2d — Our code out of the copperroute fork

Everything we wrote lives in ultraroute under MIT; the `governance` fork keeps only patches to
copperroute's own files, as the reference router across the process boundary. Our crates there
(`copper-morph`, `copper-gpu`, `copper-topo`, ~7.7k lines) are built on copperroute's GPL data
model, so they move by being re-targeted onto the core's `board` crate, not copied. `shove.rs`
leans on copperroute's push-and-shove (a freerouting port) and is rewritten fresh.

- [x] Push field (the dipole, done our way): parts and copper flow together, only locked things
      stay, copper ripped only where pushed into something that cannot move (`grid::push`,
      `--push`). Nudge steps push instead of ripping.
- [ ] copper-morph: warp field (CG), relax, reshape, move oracle and settle driver, diffusion
      placement, morph frames.
- [x] copper-topo: the topological router (world, funnel), as `topo` (`--topo`).
- [x] copper-gpu's blame (who blocks an open connection), as `grid::blame` (`--blame`); the rest was already in the core.
- [ ] ultraroute commands for settle, reshape, band, blame and frames.
- [ ] Our crates removed from the fork once each port matches it on the tests.

## Phase 3 — The test suite as a library

The route-job bench becomes a package other projects import: boards, configs, governed runs,
the referee, leaderboards and best results.

- [ ] Python package (`ultraroute`): `Job`, `Config`, `run`, `score`, `compare`; usable as a
      library, a CLI and from JSON configs; any router (copperroute backends, freerouting, an
      external command) behind one interface; KiCad import/export and checks.
- [ ] Rust side: one facade crate over copperroute for routing and placement, with a stable
      options type.
- [ ] Documentation: quick start, API reference, how to add a board, a router, a placer; served
      locally with the project.
- [ ] Tests: unit tests for the runner and referee, small fixture boards end to end, and a
      single command that runs them.

## Phase 4 — ML diffusion placement

A denoising diffusion model that generates part positions, conditioned on the netlist graph and
the board outline (in the manner of diffusion models for chip placement), judged by the Phase 3
suite.

- [ ] Data: placements from PCBench and real boards, plus synthetic boards generated at scale.
- [ ] Model and training on the local GPU; guidance toward legality and routability.
- [ ] As a placer in the suite, followed by the physics spreader and the routers; scored against
      the human placements.

## Phase 5 — Live, interactive, autorouter-first layout tool

A front end that shows the router working in real time, at a high frame rate, as it tries
routes; then lets a person guide it while it runs. The aim is a PCB layout tool built around
the GPU autorouter, and a place to explore the UX paradigms and algorithms that this kind of
collaboration needs.

Phase 5a (now) — play with what we have, on the web first, at `/studio/`:
- [x] `copperroute route --live`: frames of copper added and removed, throttled to the frame
      rate, diffed from the board itself so trial boards show as routes being tried.
- [x] `--place REF=x,y` so parts dragged in the client are honoured on the next run; the wiring
      on their pins, fixed or not, is ripped and rerouted.
- [x] `studio/`: a Rust server that runs the router as a child process and relays its frames
      over a WebSocket; a Rust core (scene, layers, picking) compiled to WASM; a Svelte + Vite
      client: pick a job and a router, watch it live, stop, drag parts, route again.
- [x] The board as designed is shown on opening, before any routing (`--backend none`).
- [x] Congestion: copper, pads and open connections against each 1 mm cell's capacity, as a map
      and a summary naming the parts nearest the fullest cells.
- [x] Locks shown on the board and listed; lock and unlock, arrow-key nudges, and "nudge it for
      me" (the nudger with every other part locked) for a selected part.
- [ ] Rubber-banding while a part is dragged: the attached wires keep their route through the
      board and are pulled taut live, falling back to airlines and a reroute when that fails.
- [ ] The grid router's negotiation rounds as live frames (today its copper appears at once).
- [ ] Steer a run without restarting it: lock or move a part, forbid a region, mid-run.
- [ ] Routers honour the job's time limit throughout (nudge fixed; grid's maze stage overran).
- [ ] GTK client on the same core (needs `libgtk-4-dev`).

- [ ] Event stream from the routers (copper added, ripped, tried; costs; congestion) with a
      compact wire format, cheap enough to emit every attempt.
- [ ] Shared Rust core for scene, layers, picking and the stream protocol.
- [ ] Two clients on that core: native GTK (gtk4-rs, GPU-drawn) and web (Svelte + Vite, the
      core compiled to WASM, WebGPU/WebGL drawing).
- [ ] Interactivity while routing: lock or drag parts, keep or forbid regions, sketch a path
      for a net, reorder or pin connections; the router replans around the edits live.
- [ ] Written new: nothing copied from copperroute's `web/` (copperroute.net) or `copper-web`.

**Licensing (decided 2026-10-24).** ultraroute is MIT. copperroute stays GPL-3.0-only (a
derivative of freerouting, with parts of KiCad's DRC), and so is everything added inside the
fork. The studio reaches the router only across a process boundary (the live stream); linking
the router into the studio, including a WASM build in the web client, would make it GPL.
