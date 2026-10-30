#!/bin/bash
# Record one animation per morph operator on bluebuzz's best routed result.
# Usage: run-demos.sh [name ...]   (default: all settle demos; "nudge" routes, so run it alone)
set -u
cd "$(dirname "$0")"
B=../copperroute/target-frames/release/copperroute
J=../jobs/bluebuzz
LOCK=J1,U7,U20,U21,U22,U23
# R4 must drop 0.065 mm to open 3V3_SYS (copperroute blame on the best session).
DIPOLE=102.3,-104.9,0,-0.3,2
BIG=102.3,-104.9,0,-0.6,3

r60() {
    grep -oE '\(place R60 [^ ]+ [^ ]+' "$J/best/best.ses" | awk '{printf "%.3f,%.3f", $3/10000, $4/10000}'
}

settle() {
    local name=$1; shift
    rm -rf "$name" "$name.ses" "$name.superseded.json" "$name.report.json"
    echo "== $name: settle $*"
    nice -n 15 timeout 1800 "$B" --log-level warn settle "$J/input/bluebuzz-rm2.dsn" \
        --ses "$J/best/best.ses" --kicad-project "$J/input/bluebuzz-rm2.kicad_pro" \
        --lock "$LOCK" --frames "$name" -o "$name.ses" "$@" > "$name.report.json"
    echo "   exit $? frames $(wc -l < "$name/frames.jsonl")"
    python3 - "$name" "$*" <<'EOF'
import json, sys
name, args = sys.argv[1], sys.argv[2]
runs = json.load(open("runs.json")) if __import__("os").path.exists("runs.json") else {}
runs[name] = {"args": args}
json.dump(runs, open("runs.json", "w"), indent=1, sort_keys=True)
EOF
}

all=(dipole-all dipole-warp dipole-shove dipole-push dipole-group dipole-big-push dipole-big-group dipole-soft dipole-r60 bend-r4 bend-r60 dipole-bend dipole-retire)
for name in "${@:-${all[@]}}"; do
    case $name in
        dipole-all)       settle $name --dipole $DIPOLE ;;
        dipole-warp)      settle $name --dipole $DIPOLE --only warp ;;
        dipole-shove)     settle $name --dipole $DIPOLE --only shove ;;
        dipole-push)      settle $name --dipole $DIPOLE --only push ;;
        dipole-group)     settle $name --dipole $DIPOLE --only group ;;
        dipole-big-push)  settle $name --dipole $BIG --only push ;;
        dipole-big-group) settle $name --dipole $BIG --only group ;;
        dipole-soft)      settle $name --dipole $BIG --soft ;;
        dipole-r60)       settle $name --dipole $(r60),0.2,0,1.5 ;;
        # R4 and R60 are pinned by route.py's fixed copper: they move only when it may bend.
        # blame (routed copper priced) wants R60 +0.0175 mm x for 3V3_SYS and 3V3_SENS.
        bend-r4)          echo '{"R4": [0, -0.0645]}' > bend-r4.targets.json
                          settle $name --targets bend-r4.targets.json --bend-fixed ;;
        bend-r60)         echo '{"R60": [0.0175, 0]}' > bend-r60.targets.json
                          settle $name --targets bend-r60.targets.json --bend-fixed ;;
        dipole-bend)      settle $name --dipole $DIPOLE --bend-fixed ;;
        dipole-retire)    settle $name --dipole $DIPOLE --retire-fixed all ;;
        nudge)
            rm -rf nudge nudge.ses
            echo "== nudge: route with the best recipe"
            COPPERROUTE_NUDGE_BASE=grid COPPERROUTE_GRID_NEGOTIATE=1 COPPERROUTE_GRID_CELL_MM=0.05 \
            systemd-run --user --scope --quiet -p MemoryMax=6G -p CPUQuota=1200% nice -n 15 timeout 3600 \
                "$B" --log-level info route "$J/input/bluebuzz-rm2.dsn" -o nudge.ses \
                --kicad-project "$J/input/bluebuzz-rm2.kicad_pro" --backend nudge \
                --set router.fanout.enabled=false --lock "$LOCK" --morph-frames nudge > nudge.log 2>&1
            echo "   exit $? frames $(wc -l < nudge/frames.jsonl)"
            python3 -c "import json,os;r=json.load(open('runs.json')) if os.path.exists('runs.json') else {};r['nudge']={'args':'route --backend nudge (best recipe)'};json.dump(r,open('runs.json','w'),indent=1,sort_keys=True)" ;;
        *) echo "unknown demo $name"; exit 2 ;;
    esac
done
python3 check-frames.py
