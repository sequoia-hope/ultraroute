#!/usr/bin/env bash
# publish_phone.sh — freeze the studio, routing in the tab, into the phone preview.
#
#   tools/publish_phone.sh [PREVIEW_DIR]     (default: ~/Software/ultraroute-phone)
#
# The preview is its own registered project (`proj up ultraroute-phone`) serving a static
# snapshot: the studio built to route in the tab (the MIT core compiled to WebAssembly, run in
# a worker), the studio's boards, and a jobs listing in the shape the studio server gives. It
# changes only when this script runs, so development never disturbs it.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${1:-$HOME/Software/ultraroute-phone}"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
[ -d "$DEST" ] || { echo "no preview at $DEST: register one with proj add" >&2; exit 1; }

cd "$ROOT/core"
nice -n 15 wasm-pack build crates/web --release --target web \
  --out-dir ../../../studio/web/src/router --out-name router --no-pack
cd "$ROOT/studio/web"
[ -f src/core/studio_core.js ] || { echo "build the studio first: studio/build.sh" >&2; exit 1; }
VITE_LOCAL=1 nice -n 15 npx vite build --outDir "$STAGE/studio" --emptyOutDir

python3 - "$ROOT" "$STAGE" <<'EOF'
import json, shutil, sys
from pathlib import Path
root, stage = Path(sys.argv[1]), Path(sys.argv[2])
jobs = []
for d in sorted((root / "jobs").iterdir()):
    settings = d / "job.json"
    if not settings.is_file():
        continue
    s = json.loads(settings.read_text())
    pcb = sorted((d / "input").glob("*.kicad_pcb"))
    if not s.get("studio") or not pcb:
        continue
    pro = sorted((d / "input").glob("*.kicad_pro"))
    out = stage / "jobs" / d.name / "input"
    out.mkdir(parents=True)
    for f in pcb[:1] + pro[:1]:
        shutil.copy(f, out / f.name)
    jobs.append({"name": d.name, "kicad_pcb": pcb[0].name, "kicad_project": pro[0].name if pro else None,
                 "studio": True, "order": s.get("order"), "strip": s.get("strip", False), "presets": []})
(stage / "api").mkdir()
(stage / "api" / "jobs").write_text(json.dumps(jobs))
print(f"{len(jobs)} boards: {', '.join(j['name'] for j in jobs)}")
EOF

cat > "$STAGE/index.html" <<EOF
<!doctype html><meta charset="utf-8"><meta http-equiv="refresh" content="0; url=studio/">
<title>Ultraroute Studio (phone preview)</title>
<p>Published $(date '+%Y-%m-%d %H:%M') from $(git -C "$ROOT" rev-parse --short HEAD)$(git -C "$ROOT" diff --quiet || echo ' with uncommitted changes'). <a href="studio/">Open the studio</a>.</p>
EOF
# Synced into place, not swapped: the preview's server serves from its working directory.
rsync -a --delete --exclude serve.sh "$STAGE/" "$DEST/"
echo "published to $DEST"
