# Porting the board-morphing work to Rust

Source: `~/pcb/rp2350-motor-controller/hardware/tools` (branch `rev-b-fixes`) and `~/pcb/rp2350-motor-controller-lean-jiggle/hardware/tools` (branch `lean-jiggle`). Surveyed 2026-10-22, read-only.

**Goal:** when a part's pads have no escape route, nudge the part (and the copper attached to it) just enough that the autorouter can finish, verified by DRC.

## Lineage

| Package | Status | Notes |
|---|---|---|
| `jiggle2/solve.py`, `drcloop.py` | superseded | PBD relaxation; morph of the shipped board went 7 → 209 violations |
| `jiggle2/creep.py` | oracle of record | feasibility-projected stepping; 18/18 checkpoints gate-clean; 68 reroutes; 40 parts stalled |
| `settle/` (lean-jiggle only) | **most complete** | creep + ghosts (grow a part in place), part pressure, length springs, groups, 5-check gate |
| `rubber/` | done, separate | per-layer harmonic warp field; new violations 306–612 → 22–49 |
| `pcbmorph/` (`ab25ee8`) | newest rewrite | settle oracle + rubber field + optional CUDA; dropped ghosts and group stepping |
| `diffusion/` | counterpoint | no routable samples; leave behind |
| `modules/place_scan.py` | useful | brute-force pose scan scored by other-net tracks crossed |

## Port order

1. **Constraint oracle**: clearance pairs between moving copper and everything else, the grandfathered floor `min(rule + guard, d0)` (pre-existing tight pairs may not get worse), pad attachment, same-net zone containment, courtyard pairs, net-length caps `max(1.25·L0, L0 + 4 mm)`, bisection line search. Build on copper-board items, search trees and copper-drc rules instead of the Python edge kinds and grid hashes.
2. **Motion**: part step with pressure on blocking free copper; copper relaxation (pressure, overstretch, Laplacian spring); segment splitting; group stepping of mutually wedged parts (union-find).
3. **Escape-driven targets (new)**: from the router's failed fanouts and unrouted connections, choose candidate directions and sites (as `place_scan` and settle's ghost `search` do), run step 2, and let the router plus copper-drc referee.
4. **Blocker reroute**: when stepping stalls, rip the blocking chain and re-plan it with the router's own maze search, with transaction and rollback (creep `rr_apply` / `rr_rollback`). Optional: keep the homotopy class (jiggle2 `reroute.py`).
5. **Warp field (optional, last)**: smooths corridors and isn't the feasibility mechanism. Use rubber's eliminated-Dirichlet form (SPD) with faer sparse Cholesky, factored once and re-solved per update. Rasterize real pad polygons; two-pass via coupling.

## Leave behind

`diffusion/` (ML stack, not routable), the CUDA accel (suspected uninitialised-output bug: `pcbmorph/data/dipole_fan_push` shows 19 898 violations and worst edge 6.5e-310; suspicion `accel.py:287` `np.empty`; unverified), `jiggle2/solve.py`, rubber's whole-board dilation, and the KiCad emit/refill pipeline (keep the fingerprint diff idea for regression tests).

## Known failure modes to design around

- Courtyard walls stop most moves.
- pcbnew courtyard polygons are about 45 µm smaller than what DRC tests.
- Uniform position scaling is illegal at any k > 1.
- Net-length caps need an absolute floor.
