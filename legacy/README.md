# Legacy: the copperroute era

Before the MIT core in `core/` became the router, this project drove
[copperroute](https://github.com/sequoia-hope/copperroute) (a GPL Rust port of freerouting, run
as a separate program from a checkout at `copperroute/`). The tools and pages of that time are
kept here as a record. They are not maintained, they are not on the Pages site, and most of them
need a copperroute build to run. They were moved from the repository root and `tools/` on
2026-10-30 and still assume their old locations, so run them from a checkout before that date
if you need to reproduce something.

| Path | What it was for |
|---|---|
| `tools/route_job.py` | Route one job (`jobs/<board>/`) many ways with copperroute, one governed attempt at a time; keep the best session, its recipe and a `leaderboard.csv` |
| `tools/kicad_bench.py` | Route a KiCad board with the core and with copperroute and judge each result by KiCad's DRC after a refill |
| `tools/apply_session.py` | Write a copperroute Specctra session back into a KiCad board, honouring the copper it bent or retired |
| `tools/build_experiments.py` | Build `experiments.json` from the leaderboards, the morph runs and the benchmark reports |
| `tools/morph_outline.py` | Plan a continuous board-shape morph (outline, parts, copper preview) |
| `tools/refit_frames.py` | Fit each recorded morph frame's view to the copper the run drew |
| `tools/softbody.py` | Soft-body placement: parts held by connection and structure beams |
| `tools/render_walls.py` | Render copperroute's coarse obstacle grid and the search starts it walled in |
| `tools/slack_stats.py` | Summarise copperroute's `COPPERROUTE_HEURISTIC_STATS` lines |
| `tools/publish_phone.sh` | Freeze the studio into the old phone preview (`~/Software/ultraroute-phone`, obsolete) |
| `jobs/leaderboard.html` | Every copperroute recipe tried on every job, scored by KiCad |
| `jobs/outline-fix-rerun2.sh` | A batch of `route_job.py` reruns after the maze outline fix |
| `morph-frames/` | The morph viewer and the scripts that recorded its runs (frames are not tracked) |
| `experiments.json`, `history.html` | The experiment log and the page that showed it |

What came out of this work and lives on in the core is described in `ROADMAP.md` (Phases 1–2c).
