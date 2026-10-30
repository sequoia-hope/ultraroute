#!/usr/bin/env python3
"""Boards that carry custom rules (.kicad_dru), routed from bare pads by the core router and
judged by KiCad with their own rules: the benchmark for the router's handling of KiCad's rules,
as PCBench is for routing (PCBench's boards have no rule files).

    rulebench.py add NAME PATH/BOARD.kicad_pcb   copy a board, its project and rule file into the
                                                 lab, strip it bare and record KiCad's verdict
    rulebench.py run RUN [--timeout S]           route every board (wrap in reserve)
    rulebench.py compare A B                     board by board: open connections, vias, and new
                                                 KiCad errors (those the bare board did not have)

Everything lives in ~/Software/ultraroute-lab/rulebench/: boards/NAME/ holds board.kicad_pcb,
.kicad_pro and .kicad_dru, bare.kicad_pcb and bare.json; runs/RUN/NAME/ each run's output. The
boards are the designer's own and stay out of this repository. Judging is PCBench's: a zone
refill, then kicad-cli DRC, counting violations that involve routed copper.

Several of these boards run to the 120 s limit, so what they route depends on how busy the
machine is: compare two binaries in runs made side by side, under the same load (on 2026-10-30
dancing_forest left 8, 20 and 70 connections open with one binary as loaf's load changed).
"""

import argparse
import concurrent.futures
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import pcbench  # noqa: E402

HOME = Path(os.environ.get("ULTRAROUTE_LAB", Path.home() / "Software" / "ultraroute-lab")) / "rulebench"


def add(a):
    src = Path(a.path)
    d = HOME / "boards" / a.name
    d.mkdir(parents=True, exist_ok=True)
    for suffix in (".kicad_pcb", ".kicad_pro", ".kicad_dru"):
        if src.with_suffix(suffix).exists():
            shutil.copy(src.with_suffix(suffix), d / f"board{suffix}")
            shutil.copy(src.with_suffix(suffix), d / f"bare{suffix}")
    if not (d / "board.kicad_dru").exists():
        sys.exit(f"{src}: no .kicad_dru beside it")
    bare = d / "bare.kicad_pcb"
    r = subprocess.run([pcbench.ROUTER, "route", d / "board.kicad_pcb", "-o", bare, "--strip-all", "--no-route"], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(r.stderr[-500:])
    u, e, w = pcbench.drc(bare, d, "bare")
    (d / "bare.json").write_text(json.dumps({"source": str(src), "unconnected": u, "errors": e, "warnings": w}, indent=1))
    print(f"{a.name}: bare board has {u} unconnected, errors {e}")


def route_one(name, run_dir, timeout):
    d = run_dir / name
    if (d / "result.json").exists():
        return json.loads((d / "result.json").read_text())
    d.mkdir(parents=True, exist_ok=True)
    src = HOME / "boards" / name
    out = d / "out.kicad_pcb"
    for suffix in (".kicad_pro", ".kicad_dru"):
        if (src / f"board{suffix}").exists():
            shutil.copy(src / f"board{suffix}", d / f"out{suffix}")
    args = [pcbench.ROUTER, "route", src / "board.kicad_pcb", "-o", out, "--strip-all", "--seed", "0", "--uuid-seed", "0", "--threads", "1", "--timeout", str(timeout), "--report", d / "report.json"]
    r = subprocess.run(pcbench.CAP + [str(x) for x in args], capture_output=True, text=True)
    (d / "log.txt").write_text(r.stderr[-200_000:])
    res = {"board": name, "code": r.returncode}
    if out.exists():
        res["wirelength"], res["vias"] = pcbench.lengths(out)
        try:
            u, e, w = pcbench.drc(out, d, "out")
            bare = json.loads((src / "bare.json").read_text())
            res["unconnected"] = u
            res["new_errors"] = {t: n - bare["errors"].get(t, 0) for t, n in e.items() if n > bare["errors"].get(t, 0)}
            res["new_warnings"] = {t: n - bare["warnings"].get(t, 0) for t, n in w.items() if n > bare["warnings"].get(t, 0)}
        except pcbench.RefereeError as ex:
            res["referee_error"] = str(ex)
    try:
        res["seconds"] = json.loads((d / "report.json").read_text())["summary"]["seconds"]
    except (OSError, KeyError, ValueError):
        pass
    (d / "result.json").write_text(json.dumps(res, indent=1))
    return res


def run(a):
    run_dir = HOME / "runs" / a.run
    run_dir.mkdir(parents=True, exist_ok=True)
    names = sorted(p.name for p in (HOME / "boards").iterdir() if (p / "bare.json").exists())
    workers = int(os.environ.get("RESERVED_CORES", "4"))
    with concurrent.futures.ThreadPoolExecutor(workers) as pool:
        for res in pool.map(lambda n: route_one(n, run_dir, a.timeout), names):
            print(f"{res['board']:20} open {res.get('unconnected')!s:>4} vias {res.get('vias')!s:>5} new errors {res.get('new_errors', res.get('referee_error'))}", flush=True)


def compare(a):
    load = lambda r: {p.parent.name: json.loads(p.read_text()) for p in (HOME / "runs" / r).glob("*/result.json")}
    x, y = load(a.a), load(a.b)
    for name in sorted(set(x) & set(y)):
        p, q = x[name], y[name]
        errors = lambda r: sum(r.get("new_errors", {}).values())
        print(f"{name:20} open {p.get('unconnected')!s:>3} -> {q.get('unconnected')!s:<3} vias {p.get('vias')!s:>4} -> {q.get('vias')!s:<4} "
              f"new errors {errors(p):>3} -> {errors(q):<3} {q.get('new_errors') or ''}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("add")
    p.add_argument("name")
    p.add_argument("path")
    p = sub.add_parser("run")
    p.add_argument("run")
    p.add_argument("--timeout", type=int, default=120)
    p = sub.add_parser("compare")
    p.add_argument("a")
    p.add_argument("b")
    a = ap.parse_args()
    {"add": add, "run": run, "compare": compare}[a.cmd](a)


if __name__ == "__main__":
    main()
