#!/bin/bash
# Rerun the copperroute configs whose leaderboard rows predate the maze outline fix (c4e5023).
set -x
cd "$(dirname "$0")/.."
R='python3 tools/route_job.py'
B=jobs/bluebuzz
$R $B --rerun --timeout 3600 --configs $B/configs.json --only cr-search-1m,cr-search-4m,cr-via20,cr-via10,cr-ripup30,cr-ripup300,cr-giveup-high,cr-no-corridor,cr-via10-search4m
$R $B --rerun --timeout 3600 --configs $B/configs-history.json --only cr-hist0.25,cr-hist0.5,cr-hist1,cr-hist2,cr-hist4,cr-hist1-search4m,cr-nofan-hist0.5,cr-nofan-hist1,cr-nofan-hist2,cr-nofan-via10,cr-nofan-via10-hist1
$R $B --rerun --timeout 3600 --configs $B/configs-e1.json --only cr-e1,cr-e1-nofan,cr-e1-nofan-via10,cr-e1-nofan-search4m,cr-e1-hist1-nofan
$R $B --rerun --timeout 3600 --configs $B/configs-region.json --only cr-region0.1,cr-region0.1-nofan,cr-region0.25,cr-region0.25-nofan,cr-region0.5,cr-region0.5-nofan,cr-region0.25-nofan-e1
$R $B --rerun --timeout 3600 --configs $B/configs-grid.json --only cr-grid,cr-grid005-nofan,cr-grid-nofan-e1-region0.25
$R $B --rerun --timeout 3600 --configs $B/configs-negotiate.json --only cr-grid005-stubs-nofan,cr-gridneg-nofan
$R $B --rerun --timeout 3600 --configs $B/configs-nudge.json --only cr-nudge-nofan,cr-nudge-nofan-e1-region0.25
$R $B --rerun --timeout 3600 --configs $B/configs-nudge-grid.json --only cr-nudge-grid005-nofan,cr-nudge-grid-nofan
E=jobs/simple_earring
$R $E --rerun --timeout 3600 --configs $E/configs-diag.json --only cr-gridneg005-nofan-lock
