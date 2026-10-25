#!/usr/bin/env python3
"""Route a KiCad board with several routers and judge every result the same way, by KiCad.

    kicad_bench.py JOB [--configs configs-kicad.json] [--timeout 900] [--only NAME]

A job directory holds input/<board>.kicad_pcb and its .kicad_pro, and a configs file:
  [{"name": ..., "router": "ultraroute"|"copperroute", "args": [...], "env": {...}}]
plus optionally {"strip": true} in job.json, to take out every unlocked track and via first
({"strip": "all"}: every track and via, locked or not).

Every router starts from the same board (runs/start.kicad_pcb). The core routes it directly;
copperroute routes the DSN KiCad exports from it, and its session is imported back with
tools/apply_session.py. Each result has its zones refilled by KiCad and is judged by
`kicad-cli pcb drc`: open connections, errors that the start board does not have, vias, length.
Results go to runs/ and leaderboard-kicad.csv.
"""
import argparse
import csv
import json
import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# ULTRAROUTE_BIN points at another build, so experiments need not rebuild the one a job is using.
ULTRAROUTE = Path(os.environ.get("ULTRAROUTE_BIN", ROOT / "core" / "target" / "release" / "ultraroute"))
COPPERROUTE = ROOT / "target-topo" / "release" / "copperroute"
FIELDS = ["name", "router", "unconnected", "new_errors", "vias", "length_mm", "wall_s", "exit", "args", "env", "finished_at"]
# Violations that are not about routing, or that KiCad rates as warnings on these boards.
NOT_ROUTING = {"silk_overlap", "silk_over_copper", "silk_edge_clearance", "text_height", "text_thickness",
               "lib_footprint_issues", "lib_footprint_mismatch", "courtyards_overlap", "solder_mask_bridge",
               "track_dangling", "via_dangling", "isolated_copper", "starved_thermal"}


def governed(cmd, timeout_s, memory="6G"):
    """One router at a time, niced, half the cores, a memory scope and a wall-clock cap."""
    quota = max(1, (os.cpu_count() or 2) // 2) * 100
    return ["systemd-run", "--user", "--scope", "--quiet", "-p", f"MemoryMax={memory}",
            "-p", f"CPUQuota={quota}%", "nice", "-n", "15", "timeout", str(timeout_s)] + cmd


def project_beside(board, project):
    target = board.with_suffix(".kicad_pro")
    if project and not target.exists():
        target.write_bytes(project.read_bytes())


def refill(source, target, project):
    project_beside(target, project)
    subprocess.run(["nice", "-n", "15", sys.executable, str(ROOT / "tools" / "refill_zones.py"), str(source), str(target)], check=True)


def kicad_drc(board, out, project):
    project_beside(board, project)
    out.unlink(missing_ok=True)
    subprocess.run(["nice", "-n", "15", "kicad-cli", "pcb", "drc", "--format", "json", "-o", str(out), str(board)],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if not out.exists():
        raise SystemExit(f"kicad-cli wrote no report for {board}")
    return json.loads(out.read_text())


def error_keys(report):
    return {(v["type"], tuple(sorted(i["description"] for i in v["items"])))
            for v in report["violations"] if v["severity"] == "error" and v["type"] not in NOT_ROUTING}


def copper_stats(board):
    import pcbnew
    b = pcbnew.LoadBoard(str(board))
    vias = sum(1 for t in b.GetTracks() if t.GetClass() == "PCB_VIA")
    length = sum(t.GetLength() for t in b.GetTracks() if t.GetClass() != "PCB_VIA") / 1e6
    return vias, round(length, 1)


def start_board(job, input_pcb, project, strip):
    runs = job / "runs"
    runs.mkdir(exist_ok=True)
    start = runs / "start.kicad_pcb"
    if not start.exists():
        project_beside(start, project)
        cmd = [str(ULTRAROUTE), "route", str(input_pcb), "-o", str(start), "--no-route"] + (["--strip-all"] if strip == "all" else ["--strip"] if strip else [])
        subprocess.run(cmd, check=True)
    return start


def run_one(job, start, project, cfg, timeout_s):
    runs = job / "runs"
    name = cfg["name"]
    routed = runs / f"{name}.kicad_pcb"
    project_beside(routed, project)
    env = dict(os.environ, **cfg.get("env", {}))
    began = time.time()
    if cfg["router"] == "ultraroute":
        cmd = [str(ULTRAROUTE), "route", str(start), "-o", str(routed)] + cfg.get("args", [])
        code = subprocess.run(governed(cmd, timeout_s), env=env).returncode
    elif cfg["router"] == "copperroute":
        dsn = runs / "start.dsn"
        if not dsn.exists():
            import pcbnew
            pcbnew.ExportSpecctraDSN(pcbnew.LoadBoard(str(start)), str(dsn))
        ses = runs / f"{name}.ses"
        cmd = [str(COPPERROUTE), "route", str(dsn), "-o", str(ses)] + (["--kicad-project", str(project)] if project else []) + cfg.get("args", [])
        code = subprocess.run(governed(cmd, timeout_s), env=env).returncode
        if ses.exists():
            subprocess.run([sys.executable, str(ROOT / "tools" / "apply_session.py"), str(start), str(ses), str(routed), "--dsn", str(dsn)], check=True)
    else:
        raise SystemExit(f"unknown router {cfg['router']}")
    wall = round(time.time() - began, 1)
    row = {"name": name, "router": cfg["router"], "wall_s": wall, "exit": code, "args": " ".join(cfg.get("args", [])),
           "env": json.dumps(cfg.get("env", {})), "finished_at": time.strftime("%Y-%m-%dT%H:%M:%S")}
    if not routed.exists():
        row.update(unconnected="", new_errors="", vias="", length_mm="")
        return row
    filled = runs / f"{name}-filled.kicad_pcb"
    refill(routed, filled, project)
    report = kicad_drc(filled, runs / f"{name}.drc.json", project)
    vias, length = copper_stats(filled)
    row.update(unconnected=len(report["unconnected_items"]), new_errors=len(error_keys(report) - BARE[0]), vias=vias, length_mm=length)
    return row


BARE = [set()]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("job", type=Path)
    ap.add_argument("--configs", type=Path)
    ap.add_argument("--timeout", type=int, default=900)
    ap.add_argument("--only")
    args = ap.parse_args()
    job = args.job.resolve()
    inputs = sorted((job / "input").glob("*.kicad_pcb"))
    if not inputs:
        raise SystemExit(f"{job}/input has no .kicad_pcb")
    project = next(iter(sorted((job / "input").glob("*.kicad_pro"))), None)
    settings = json.loads((job / "job.json").read_text()) if (job / "job.json").exists() else {}
    configs = json.loads((args.configs or job / "configs-kicad.json").read_text())
    if args.only:
        configs = [c for c in configs if c["name"] == args.only]

    start = start_board(job, inputs[0], project, settings.get("strip", False))
    filled_start = job / "runs" / "start-filled.kicad_pcb"
    refill(start, filled_start, project)
    bare = kicad_drc(filled_start, job / "runs" / "start.drc.json", project)
    BARE[0] = error_keys(bare)
    print(f"start: {len(bare['unconnected_items'])} unconnected, {len(BARE[0])} errors")

    board_csv = job / "leaderboard-kicad.csv"
    rows = {}
    if board_csv.exists():
        with board_csv.open() as f:
            rows = {r["name"]: r for r in csv.DictReader(f)}
    for cfg in configs:
        print(f"-> {cfg['name']} ({cfg['router']} {' '.join(cfg.get('args', []))})", flush=True)
        row = run_one(job, start, project, cfg, args.timeout)
        print(f"   {row['unconnected']} unconnected, {row['new_errors']} new errors, {row['vias']} vias, {row['length_mm']} mm, {row['wall_s']} s", flush=True)
        rows[row["name"]] = row
        ranked = sorted(rows.values(), key=lambda r: (r["new_errors"] in ("", None) or int(r["new_errors"]) > 0,
                                                      int(r["unconnected"]) if str(r["unconnected"]).isdigit() else 10**9,
                                                      int(r["vias"]) if str(r["vias"]).isdigit() else 10**9))
        with board_csv.open("w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=FIELDS)
            w.writeheader()
            for r in ranked:
                w.writerow({k: r.get(k, "") for k in FIELDS})


if __name__ == "__main__":
    main()
