#!/usr/bin/env python3
"""Summarise every experiment into experiments.json for the project page.

Route jobs come from jobs/*/leaderboard.csv (ranked best first by route_job.py) and best/recipe.json;
morph runs from morph-frames/runs.json (written by check-frames.py); PCBench corpus experiments
from the reports listed in CORPUS, whose verdicts are written by hand when each report is read.
"""
import csv
import json
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REPORTS = "copperroute/benchmark/reports"
# The maze outline fix landed in the release build at this time; older rows predate it.
OUTLINE_FIX = "2026-10-23T04:29"

JOBS = {
    "bluebuzz": "The priority board: route.py's tamed DSN, 3 routable layers, J1, U7 and U20–U23 locked.",
    "simple_earring": "LED earring, latest rev; every LED, J1, SW1 and SW2 locked.",
    "bluebuzz-ripped": "Bluebuzz with route.py's scripted copper removed.",
    "simple_earring-ripped": "The earring with route.py's scripted copper removed.",
}

CORPUS = [
    ("Baseline", "baseline-20261022.html",
     "42 KiCad-refereed PCBench boards. Freerouting 15/42 clean, CopperRoute 23/42 (81 unrouted, 122 violations); governed matches main."),
    ("Ripup history", "hist-20261022.html",
     "Negative: per-net history detours with more vias and violations (80–88 unrouted, 144–189 violations). Off."),
    ("Obstacle-aware heuristic (E1)", "e1-20261022.html",
     "Neutral and 2.2× slower: 87 unrouted, 119 violations, fewest vias. Opt-in."),
    ("Region history", "region-20261022.html",
     "Mixed, promising: ×0.1 cuts violations to 107 at no cost; ×0.25 reaches 73 unrouted but loses a clean board."),
    ("Grid → maze, first version", "grid-20261022.html",
     "Lost 609 connections to timeouts (19/42 clean); fixed by cached obstacle maps and a 40% time share."),
    ("Grid → maze (GPU grid first)", "grid2-20261022.html",
     "Neutral on the corpus (21/42, 83 unrouted, 117 violations), a win on dense boards; 3.3× the time."),
]


def route_jobs():
    out = []
    for board in sorted((ROOT / "jobs").glob("*/leaderboard.csv")):
        job = board.parent.name
        rows = list(csv.DictReader(board.open()))
        if not rows:
            continue
        best = rows[0]
        stale = sum(1 for r in rows if r["router"] == "copperroute" and r["finished_at"] < OUTLINE_FIX)
        recipe = board.parent / "best" / "recipe.json"
        links = [{"label": "leaderboard", "href": f"jobs/leaderboard.html?job={job}"}]
        if recipe.exists():
            links.append({"label": "best recipe", "href": f"jobs/{job}/best/recipe.json"})
        if (board.parent / "README.md").exists():
            links.append({"label": "notes", "href": f"jobs/{job}/README.md"})
        out.append({
            "when": max(r["finished_at"] for r in rows)[:16],
            "title": job,
            "about": JOBS.get(job, ""),
            "result": f"best {best['unconnected']} open, {best['new_violations']} new errors, "
                      f"{best['vias']} vias ({best['name']})",
            "detail": f"{len(rows)} attempts" + (f", {stale} predate the outline fix" if stale else ""),
            "links": links,
        })
    return out


def corpus():
    return [{"when": when(ROOT / REPORTS / report), "title": title, "result": verdict,
             "links": [{"label": "report", "href": f"{REPORTS}/{report}"}]}
            for title, report, verdict in CORPUS if (ROOT / REPORTS / report).exists()]


def when(path):
    return datetime.fromtimestamp(path.stat().st_mtime).isoformat(timespec="minutes")


def morph_runs():
    path = ROOT / "morph-frames" / "runs.json"
    if not path.exists():
        return []
    out = []
    for name, run in sorted(json.loads(path.read_text()).items()):
        s = run.get("summary")
        if not s:
            continue
        ops = ", ".join(f"{op.split('/')[-1]} {n}" for op, n in s["operators"].items())
        change = "no connection opened" if not s["opened"] else f"{len(s['opened'])} connection(s) opened"
        out.append({
            "when": run.get("when") or when(ROOT / "morph-frames" / name / "frames.jsonl"),
            "title": name,
            "about": run["args"],
            "result": f"{s['moves']} moves of {s['parts']} parts ({ops}); {change}",
            "detail": f"open {s['open_start']} → {s['open_end']}",
            "links": [{"label": "watch", "href": f"morph-frames/?run={name}"}],
        })
    return out


def main():
    groups = [
        {"title": "Route jobs", "items": route_jobs()},
        {"title": "Morph runs on bluebuzz", "items": morph_runs(),
         "link": {"label": "viewer", "href": "morph-frames/"}},
        {"title": "PCBench corpus", "items": corpus()},
    ]
    for group in groups:
        group["items"].sort(key=lambda i: i["when"], reverse=True)
    groups.sort(key=lambda g: max((i["when"] for i in g["items"]), default=""), reverse=True)
    data = {"generated": datetime.now().isoformat(timespec="minutes"), "groups": groups}
    (ROOT / "experiments.json").write_text(json.dumps(data, indent=1))
    print(f"experiments.json: {sum(len(g['items']) for g in data['groups'])} experiments")


if __name__ == "__main__":
    main()
