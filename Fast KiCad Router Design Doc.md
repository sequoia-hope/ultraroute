# Fast KiCad Router: Design Doc

> **Status, 2026-10-30:** this is the plan the project started from. The GPU routing it proposes
> was built (`grid::GpuSolver`, a Bellman-Ford field relaxed on the GPU) and measured about 10x
> slower than A* on the CPU on these boards (`REVIEW.md` §3.4), so it is parked behind
> `--features gpu`. What runs is A* on the CPU, natively and, in the studio, in a pool of
> WebAssembly workers in the browser tab; there is no WebGPU routing.

## Goal

Build an open-source, Rust-based PCB autorouter for KiCad boards that is much faster than Freerouting, runs well on a desktop GPU, and never destabilizes the host machine. Browser support (WebAssembly + WebGPU) is a nice-to-have, not a requirement. The project doubles as an experimentation suite for comparing routing approaches.

## Starting point: CopperRoute

Repo: github.com/emshotton/copperroute (site: copperroute.net). A Rust/WASM rewrite of Freerouting, split into crates:

- copper-geometry: exact-arithmetic planar geometry
- copper-board: board model, design rules, spatial indexes
- copper-dsn: .dsn/.ses/.rules/KiCad readers and writers, native .kicad\_pcb reader
- copper-settings: configuration
- copper-router: fanout, maze routing, optimizer
- copper-drc: design rule checker emitting KiCad's DRC report schema
- copper-core: routing job orchestration and result manifest

It already reads net classes from .kicad\_pro, has a 700+ board test corpus with KiCad-refereed DRC, and exposes an MCP server. Plan: fork or build alongside it, reuse board loading, DRC and corpus, and add new router back ends.

CopperRoute appears to be CPU-only: the README, crate list and PRs mention no GPU, wgpu or WebGPU. Confirm with the author. If so, the GPU back end is new work and a natural upstream contribution.

## Architecture

### Pluggable router trait

Every approach implements one Rust trait: input is the board model plus rules plus a resource budget; output is proposed copper (tracks, vias) plus metrics. Candidate back ends:

1. Baseline: CopperRoute's existing shape-based maze router
2. Negotiated congestion (PathFinder-style) on CPU, multithreaded
3. GPU grid router via wgpu, using sweep-based search in the style of GAMER
4. Hybrid: GPU coarse/global routing, then CPU exact detailed routing and repair
5. Later: learned placement or routing guidance (diffusion-guided sampling)

The suite should cover every approach discussed, so add these experimental back ends too:

- Exact formulations: ILP or SAT routing for small boards or congested regions, as a quality ceiling to compare against
- Topological routing: decide which side of each obstacle a trace passes, then embed geometry (rubber-band routing, SURF)
- Analytical placement before routing: smooth wirelength plus density penalty solved by gradient descent on GPU (ePlace/RePlAce/DREAMPlace style, Cypress for PCBs)
- Diffusion-guided placement: sample placements with rules expressed as guidance costs (Chip Placement with Diffusion Models, MacroDiff+)
- Reinforcement learning for net ordering or placement, as a lower-priority comparison

### Referee and benchmark harness

One command runs any back end across the corpus and records wall-clock time, peak CPU, peak memory, GPU time, completion rate (unrouted connections), and DRC violations from copper-drc, cross-checked against kicad-cli pcb drc. Results go to a comparable table per commit.

### Precision strategy

GPU kernels work in 32-bit integer grid units with conservative clearance inflation, so anything the GPU accepts is safe. Exact verification and final geometry stay in copper-geometry. Use wgpu feature detection: enable 64-bit atomics natively (Vulkan/Metal/DX12) where it helps, fall back to 32-bit packed cost+parent words on WebGPU. Measure before maintaining two paths.

## Resource governance (built in, not optional)

The router must never peg or crash the host. Freerouting currently saturates the machine on every new project until agents learn to throttle it.

- Default thread cap well below core count (for example, half the cores), overridable by flag and environment variable
- Memory ceiling with graceful degradation instead of OOM
- Run at low OS priority by default (nice / background priority)
- GPU work submitted in bounded batches so the desktop stays responsive
- Time budget and "good enough" stopping criteria (stop when completion and DRC targets are met, or improvement stalls)
- Clear progress reporting and resumable checkpoints
- Sensible defaults when invoked by coding agents through CLI or MCP, so no project has to rediscover limits

Also add a section to the PCB design skill telling agents to use these limits and never run the router unthrottled.

## Milestones

1. Fork CopperRoute, build, run corpus, record baseline time and quality
2. Add resource governance to the existing router (thread cap, priority, time budget)
3. Define the router trait and benchmark harness
4. Prototype wgpu grid maze search on single nets; validate with copper-drc
5. Negotiated congestion across many nets on GPU
6. Hybrid GPU global plus CPU detailed routing; compare against baseline
7. WebGPU build of the best back end for copperroute.net-style browser use

## Reading list

- Lee (1961), maze routing
- McMurchie and Ebeling, PathFinder negotiated congestion router (1995)
- Dai, Kong et al., SURF and rubber-band topological routing
- Lin and Wong, GAMER: GPU-accelerated maze routing
- FastGR and InstantGR, GPU global routing
- DREAMPlace, RePlAce, ePlace: analytical placement
- Zhang et al., Cypress: VLSI-inspired PCB placement with GPU acceleration (ISPD 2025)
- Lee, Abbeel et al., Chip Placement with Diffusion Models (ICML 2025)
- MacroDiff+, physics-guided geometric diffusion for macro placement
- fastroute (parisxmas), another Rust Freerouting port, for comparison

## Appendix: research directions for routing (added 2026-10-22)

The brief above covers diffusion only for *placement*. Upstream CopperRoute already has a 900-line survey with a verdict per technique: `copperroute/docs/plan-10-prep/routing-research-survey.md` (plus `unet-astar-assessment.md`). Its findings, and what we're doing with them:

| Technique | Upstream verdict | Our status |
|---|---|---|
| Ripup **history term** (PathFinder negotiated congestion). The router currently forgets every ripup between passes | Pursue (E3) | **Prototyped** as `COPPERROUTE_RIPUP_HISTORY`, a per-net factor that is off by default. Being measured on the corpus and bluebuzz |
| **Obstacle-aware admissible heuristic** (portal / true-distance). The current A* estimate ignores keepouts ("corridor blindness") | Pursue (E1), highest conviction | Next. Bluebuzz, with 696 keepouts, is exactly this case |
| **Windowed search-and-repair** with violation history (TritonRoute / OpenROAD drt) | Pursue (E2) | Queued |
| **Pin-access reservation** for unrouted nets | Pursue | Queued. Targets dense boards whose last connections get walled in |
| Cost-weight scheduling (arXiv 2512.03594, without the RL) | Pursue | Queued with E3 |
| Topological / rubber-band routing (SURF) | Watch | Brief item; later |
| Global routing guides (cuGR, CUGR2) | Watch | Overlaps our GPU coarse stage (M6) |
| SAT / ILP exact local repair | Decline upstream | Kept in our brief as a quality ceiling for small regions |
| Learned net ordering (RL, MCTS) | Decline | Low priority, as in the brief |
| **Learned priors / diffusion for routing** (U-Net A*; generative VLSI global routers such as HubRouter (NeurIPS 2023) and DSBRouter, a diffusion Schrödinger bridge (ICML 2025), which work on grid overflow and not PCB detailed routing) | Decline (N17–N19): no published DRC-clean wins on real boards | Revisit only as *guidance costs* for the GPU search, scored by the same referee. Upstream declined partly over its determinism and no-new-dependency constraints, which our fork relaxes |

Two cheap, research-backed habits apply to every board now:
- **Portfolio routing.** Run several cost configurations and keep the best by the referee. `tools/route_job.py` does this one governed run at a time, and saves the best session and its recipe.
- **Same-referee scoring.** Every candidate is judged by the same DRC against the bare input, so "new violations" and "unconnected" are comparable across routers.
