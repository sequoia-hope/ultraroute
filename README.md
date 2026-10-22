# ultraroute

Workspace for a fast, host-friendly KiCad autorouter and a bench for comparing routing approaches. The brief is `Fast KiCad Router Design Doc.md`. Current state is on the status page (`index.html`; `proj up ultraroute`).

The router itself lives in `copperroute/`, a clone of
[sequoia-hope/copperroute](https://github.com/sequoia-hope/copperroute) on branch `governance`
(forked from emshotton/copperroute). It is not part of this repository.

| Path | What |
|---|---|
| `tools/route_job.py` | Route one board many ways, one governed attempt at a time; keeps `leaderboard.csv` and `best/` (session, DRC, `recipe.json`) |
| `tools/slack_stats.py` | Summarize `COPPERROUTE_HEURISTIC_STATS` output |
| `tools/render_walls.py` | Render a `COPPERROUTE_OBSTACLE_GRID_DUMP` grid |
| `jobs/<board>/` | One route job per real board: `input/` snapshot, configs, leaderboard, best result |

Local checkouts, not tracked here: `copperroute-main/` (pristine upstream worktree, the `rs-head` benchmark candidate), `freerouting/`, and `PCBench/`.
