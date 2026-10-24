#!/bin/bash
# Reshape bluebuzz along outline-plan into morph-frames/NAME, then refresh the run summaries.
#   [SES=session] [PLAN=plan.json] [BIN=target-dir] run-reshape.sh NAME DESCRIPTION [reshape options...]
set -euo pipefail
cd "$(dirname "$0")/.."
name=$1; description=$2; shift 2
rm -rf "morph-frames/$name"
nice -n 15 copperroute/${BIN:-target-frames}/release/copperroute --log-level ${LOG_LEVEL:-info} reshape \
  jobs/bluebuzz/input/bluebuzz-rm2.dsn --ses "${SES:-jobs/bluebuzz/best/best.ses}" \
  --kicad-project jobs/bluebuzz/input/bluebuzz-rm2.kicad_pro \
  --plan "${PLAN:-morph-frames/outline-plan/plan.json}" --frames "morph-frames/$name" \
  -o "morph-frames/$name.ses" "$@" > "morph-frames/$name.log" 2>&1
python3 tools/refit_frames.py "morph-frames/$name"
python3 - "$name" "$description" <<'PY'
import json, sys
path = 'morph-frames/runs.json'
runs = json.load(open(path))
runs[sys.argv[1]] = {'args': sys.argv[2]}
json.dump(runs, open(path, 'w'), indent=1, sort_keys=True)
PY
python3 morph-frames/check-frames.py | grep "$name"
python3 tools/build_experiments.py
