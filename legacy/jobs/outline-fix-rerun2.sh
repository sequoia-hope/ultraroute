#!/bin/bash
# Rerun the copperroute configs whose leaderboard rows predate the maze outline fix (c4e5023).
set -x
cd "$(dirname "$0")/.."
R='python3 tools/route_job.py'
E=jobs/simple_earring
$R $E --rerun --timeout 3600 --configs $E/configs-diag.json --only cr-gridneg005-nofan-lock
