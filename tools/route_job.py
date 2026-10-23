#!/usr/bin/env python3
"""Route one board many ways, one governed attempt at a time, and keep the best.

A job directory holds:
  input/<board>.dsn       the routing problem (copied from the source project, never edited)
  input/<board>.kicad_pro optional; its design rules go to both the router and the referee
  configs.json            [{"name": ..., "router": "copperroute"|"freerouting", "args": [...], "env": {...}}]
Results go to runs/<name>.*, leaderboard.csv, and best/ (session + recipe.json + drc.json).

Every attempt is scored by the same referee, `copperroute drc`, against the bare input:
clean attempts (no errors added) before the rest, then unconnected items, then errors added,
then vias, then copper length. Violations the board's rules rate as warnings (a dangling stub)
are counted in new_warnings and do not rank.
"""
import argparse
import collections
import csv
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
COPPERROUTE = ROOT / "copperroute" / "target" / "release" / "copperroute"
FREEROUTING = ROOT / "freerouting" / "build" / "libs" / "freerouting-current-executable.jar"
FIELDS = ["name", "router", "unconnected", "new_violations", "new_warnings", "vias", "length_mm", "wall_s",
          "peak_rss_mb", "exit", "args", "env", "finished_at"]


def governed(cmd, timeout_s, memory="6G"):
    """One autorouter at a time, niced, half the cores, a memory scope and a wall-clock cap."""
    quota = max(1, (os.cpu_count() or 2) // 2) * 100
    return ["systemd-run", "--user", "--scope", "--quiet", "-p", f"MemoryMax={memory}",
            "-p", f"CPUQuota={quota}%", "nice", "-n", "15", "timeout", str(timeout_s)] + cmd


def drc(dsn, ses, out, project):
    cmd = [str(COPPERROUTE), "--log-level", "off", "drc", str(dsn), "-o", str(out)]
    if ses:
        cmd[5:5] = ["--ses", str(ses)]
    if project:
        cmd += ["--kicad-project", str(project)]
    out.unlink(missing_ok=True)
    proc = subprocess.run(cmd)
    if not out.exists():
        raise SystemExit(f"drc failed (exit {proc.returncode}) and wrote no report: {' '.join(cmd)}")
    return json.loads(out.read_text())


def violation_key(v):
    """What a violation is between, not where: a moved part carries its old violations along."""
    return (v["type"], tuple(sorted(i["description"] for i in v["items"])))


def new_violations(report, bare_keys, severity):
    after = collections.Counter(violation_key(v) for v in report["violations"]
                                if v.get("severity", "error") == severity)
    return sum(max(0, n - bare_keys[k]) for k, n in after.items())


def session_stats(ses):
    """Vias and wire length straight from the Specctra session, so both routers are measured alike."""
    text = ses.read_text()
    vias = text.count("(via ")
    length = 0.0
    resolution = 1.0
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("(resolution"):
            resolution = float(line.rstrip(")").split()[-1])
    tokens = text.replace("(", " ( ").replace(")", " ) ").split()
    i = 0
    while i < len(tokens):
        if tokens[i] == "path":
            j, nums = i + 3, []
            while tokens[j] != ")":
                nums.append(float(tokens[j]))
                j += 1
            pts = list(zip(nums[0::2], nums[1::2]))
            length += sum(((x1 - x0) ** 2 + (y1 - y0) ** 2) ** 0.5
                          for (x0, y0), (x1, y1) in zip(pts, pts[1:]))
            i = j
        i += 1
    return vias, length / resolution / 1000.0


def rank(row):
    return (int(row["new_violations"]) > 0, int(row["unconnected"]), int(row["new_violations"]),
            int(row["vias"]), float(row["length_mm"]))


def run_one(job, dsn, project, cfg, timeout_s, bare_keys):
    name = cfg["name"]
    runs = job / "runs"
    ses = runs / f"{name}.ses"
    env = {**os.environ, **cfg.get("env", {})}
    if cfg["router"] == "copperroute":
        cmd = [cfg.get("binary", str(COPPERROUTE)), "--log-level", cfg.get("log_level", "warn"), "route", str(dsn), "-o", str(ses),
               "--timeout", str(timeout_s - 30), "--result-json", str(runs / f"{name}.result.json")]
        if project:
            cmd += ["--kicad-project", str(project)]
    elif cfg["router"] == "freerouting":
        cmd = ["java", "-Xmx4g", "-jar", cfg.get("jar", str(FREEROUTING)), "-de", str(dsn), "-do", str(ses),
               "--gui.enabled=false"]
    else:
        raise SystemExit(f"{name}: unknown router {cfg['router']}")
    cmd += cfg.get("args", [])
    timing = runs / f"{name}.time"
    timed = ["/usr/bin/time", "-f", "%e %M", "-o", str(timing)] + governed(cmd, timeout_s)
    started = time.time()
    with open(runs / f"{name}.log", "w") as log:
        proc = subprocess.run(timed, env=env, stdout=log, stderr=subprocess.STDOUT, text=True)
    wall, rss_kb = time.time() - started, 0
    tail = timing.read_text().strip().splitlines() if timing.exists() else []
    if tail:
        try:
            wall, rss_kb = float(tail[-1].split()[0]), int(tail[-1].split()[1])
        except (ValueError, IndexError):
            pass
    row = {"name": name, "router": cfg["router"], "exit": proc.returncode,
           "wall_s": f"{wall:.1f}", "peak_rss_mb": rss_kb // 1024,
           "args": " ".join(cfg.get("args", [])), "env": json.dumps(cfg.get("env", {})),
           "finished_at": time.strftime("%Y-%m-%dT%H:%M:%S")}
    if not ses.exists():
        row.update(unconnected=10**6, new_violations=10**6, new_warnings=0, vias=0, length_mm=0)
        return row, None
    score(row, dsn, project, ses, runs / f"{name}.drc.json", bare_keys)
    return row, ses


def score(row, dsn, project, ses, out, bare_keys):
    report = drc(dsn, ses, out, project)
    vias, length = session_stats(ses)
    row.update(unconnected=len(report["unconnected_items"]),
               new_violations=new_violations(report, bare_keys, "error"),
               new_warnings=new_violations(report, bare_keys, "warning"),
               vias=vias, length_mm=f"{length:.1f}")


def write_board(board, rows):
    with board.open("w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=FIELDS)
        w.writeheader()
        w.writerows(sorted(rows, key=rank))


def keep_best(job, dsn, cfg, row, ses):
    best = {"config": cfg, "score": row, "input_dsn": dsn.name,
            "copperroute": str(COPPERROUTE), "freerouting": str(FREEROUTING)}
    shutil.copy(ses, job / "best" / "best.ses")
    shutil.copy(job / "runs" / f"{cfg['name']}.drc.json", job / "best" / "drc.json")
    (job / "best" / "recipe.json").write_text(json.dumps(best, indent=2) + "\n")
    return best


def rescore(job, dsn, project, rows, bare_keys, best):
    """Scores every kept session again, as the referee now judges, and re-picks the best."""
    configs = {c["name"]: c for f in sorted(job.glob("configs*.json")) for c in json.loads(f.read_text())}
    if best:
        configs.setdefault(best["config"]["name"], best["config"])
    for row in rows:
        ses = job / "runs" / f"{row['name']}.ses"
        if ses.exists():
            score(row, dsn, project, ses, job / "runs" / f"{row['name']}.drc.json", bare_keys)
            print(f"  {row['name']}: unconnected={row['unconnected']} "
                  f"new_errors={row['new_violations']} new_warnings={row['new_warnings']}", flush=True)
    write_board(job / "leaderboard.csv", rows)
    kept = [r for r in rows if (job / "runs" / f"{r['name']}.ses").exists() and r["name"] in configs]
    if kept:
        top = min(kept, key=rank)
        keep_best(job, dsn, configs[top["name"]], top, job / "runs" / f"{top['name']}.ses")
        print(f"  ★ best: {top['name']}", flush=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("job", type=Path)
    ap.add_argument("--configs", type=Path, help="config file (default: <job>/configs.json)")
    ap.add_argument("--only", help="comma-separated config names")
    ap.add_argument("--timeout", type=int, default=600, help="seconds per attempt")
    ap.add_argument("--rerun", action="store_true", help="rerun configs already on the leaderboard")
    ap.add_argument("--rescore", action="store_true",
                    help="score the kept sessions again instead of routing, and re-pick the best")
    args = ap.parse_args()

    job = args.job.resolve()
    (job / "runs").mkdir(exist_ok=True)
    (job / "best").mkdir(exist_ok=True)
    dsn = next((job / "input").glob("*.dsn"))
    project = next((job / "input").glob("*.kicad_pro"), None)
    configs = json.loads((args.configs or job / "configs.json").read_text())
    if args.only:
        wanted = set(args.only.split(","))
        configs = [c for c in configs if c["name"] in wanted]

    bare = drc(dsn, None, job / "runs" / "bare.drc.json", project)
    bare_keys = collections.Counter(violation_key(v) for v in bare["violations"])
    print(f"bare input: {len(bare['unconnected_items'])} unconnected, "
          f"{len(bare['violations'])} violations already present")

    board = job / "leaderboard.csv"
    rows = list(csv.DictReader(board.open())) if board.exists() else []
    done = {r["name"] for r in rows}
    best_file = job / "best" / "recipe.json"
    best = json.loads(best_file.read_text()) if best_file.exists() else None
    if args.rescore:
        rescore(job, dsn, project, rows, bare_keys, best)
        return

    for cfg in configs:
        if cfg["name"] in done and not args.rerun:
            continue
        print(f"→ {cfg['name']} ({cfg['router']} {' '.join(cfg.get('args', []))})", flush=True)
        row, ses = run_one(job, dsn, project, cfg, args.timeout, bare_keys)
        print(f"  unconnected={row['unconnected']} new_errors={row['new_violations']} "
              f"new_warnings={row['new_warnings']} "
              f"vias={row['vias']} length={row['length_mm']}mm wall={row['wall_s']}s", flush=True)
        rows = [r for r in rows if r["name"] != cfg["name"]] + [row]
        write_board(board, rows)
        if ses and (best is None or rank(row) < rank(best["score"])):
            best = keep_best(job, dsn, cfg, row, ses)
            print(f"  ★ new best", flush=True)


if __name__ == "__main__":
    sys.exit(main())
