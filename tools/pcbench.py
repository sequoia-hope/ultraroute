#!/usr/bin/env python3
"""PCBench, routed from bare pads by the core router and judged by KiCad, against the published
baselines (FreeRouting, PcbRouter, RL-MCTS; PCBench/Baselines/Results_all.csv).

    pcbench.py prepare [--only ID,...]       convert each board to KiCad 9 once, strip it bare,
                                             and record KiCad's verdict on the bare board
    pcbench.py run RUN [--recipe "..."] [--timeout S] [--only ID,...] [--sample N] [--seed N]
                                             route every prepared board (wrap in reserve: it
                                             runs $RESERVED_CORES routes at once)
    pcbench.py report RUN [RUN ...]          success, wirelength and vias against the baselines

A board succeeds when KiCad finds nothing unconnected after a zone refill and no DRC error of a
type the bare board did not have as often: routed copper that breaks the board's own rules is a
failure, not a success. Wirelength is the routed tracks' length and vias their count, as the
baselines report them. Everything lives in ~/Software/ultraroute-lab/pcbench/: boards/ID/ holds
board.kicad_pcb (+ .kicad_pro), bare.kicad_pcb and bare.json, runs/RUN/ID/ each run's output.
"""

import argparse
import concurrent.futures
import csv
import json
import math
import os
import random
import re
import shutil
import subprocess
import sys
import time
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BENCH = ROOT / "PCBench"
HOME = Path(os.environ.get("ULTRAROUTE_LAB", Path.home() / "Software" / "ultraroute-lab")) / "pcbench"
ROUTER = Path(os.environ.get("ULTRAROUTE_BIN", ROOT / "core/target/release/ultraroute"))
RECIPE = "--negotiate --rip-up --tighten --prefanout --diff-pairs --shove-plough region --shove-reach 0.5 --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8"
MASK = {"solder_mask_bridge"}
CAP = ["systemd-run", "--user", "--scope", "-q", "-p", "MemoryMax=8G", "-p", "MemorySwapMax=0"]


def baselines():
    """Each board's baseline results: {router: (success, wirelength, vias, seconds)}."""
    rows = list(csv.reader(open(BENCH / "Baselines/Results_all.csv")))[2:]
    out = {}
    for r in rows:
        def num(v):
            # Some runtimes are written as sums, "31743 (RL)+1029 (MCTS)".
            parts = re.findall(r"[\d.]+", v)
            return sum(float(x) for x in parts) if parts and v != "N/A" else None

        out[r[0]] = {name: (r[1 + 4 * k] == "Yes", num(r[2 + 4 * k]), num(r[3 + 4 * k]), num(r[4 + 4 * k])) for k, name in enumerate(["FreeRouting", "PcbRouter", "RL-MCTS", "Original"])}
    return out


def board_ids(only=None):
    ids = [b for b in baselines() if (BENCH / "PCBs" / b / "processed.kicad_pcb").exists()]
    if only:
        wanted = set(only.split(","))
        ids = [b for b in ids if b in wanted]
    return ids


def drc(pcb, out_dir, stem):
    """Refill the zones and ask KiCad: (unconnected, {error type: count}, {warning type: count})."""
    filled, report = out_dir / f"{stem}.filled.kicad_pcb", out_dir / f"{stem}.drc.json"
    for suffix in (".kicad_pro", ".kicad_dru"):
        if pcb.with_suffix(suffix).exists():
            shutil.copy(pcb.with_suffix(suffix), filled.with_suffix(suffix))
    subprocess.run([sys.executable, ROOT / "tools/refill_zones.py", pcb, filled], capture_output=True, timeout=900)
    subprocess.run(["kicad-cli", "pcb", "drc", "--format", "json", "--severity-all", "-o", report, filled], capture_output=True, timeout=900)
    if not report.exists():
        return None
    d = json.loads(report.read_text())
    errors, warnings = {}, {}
    for v in d.get("violations", []):
        into = errors if v["severity"] == "error" else warnings
        into[v["type"]] = into.get(v["type"], 0) + 1
    return len(d.get("unconnected_items", [])), errors, warnings


def convert(src, dst):
    """The KiCad 5 board saved by KiCad 9, each uuid that repeats (KiCad 5 boards number some
    footprints 0, 1, ...) given a fresh one after its first use."""
    code = f"""
import pcbnew
pcbnew.SaveBoard({str(dst)!r}, pcbnew.LoadBoard({str(src)!r}))
"""
    r = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, timeout=600)
    if not dst.exists():
        return r.stderr[-500:]
    seen = set()

    def fresh(m):
        if m[1] in seen:
            return f'(uuid "{uuid.uuid4()}")'
        seen.add(m[1])
        return m[0]

    dst.write_text(re.sub(r'\(uuid "([^"]*)"\)', fresh, dst.read_text()))
    return None


def prepare_one(b):
    d = HOME / "boards" / b
    if (d / "bare.json").exists():
        return b, json.loads((d / "bare.json").read_text())
    d.mkdir(parents=True, exist_ok=True)
    board = d / "board.kicad_pcb"
    info = {"id": b}
    err = convert(BENCH / "PCBs" / b / "processed.kicad_pcb", board)
    if err is not None:
        info["error"] = "convert: " + err
    else:
        bare = d / "bare.kicad_pcb"
        if board.with_suffix(".kicad_pro").exists():
            shutil.copy(board.with_suffix(".kicad_pro"), bare.with_suffix(".kicad_pro"))
        r = subprocess.run([ROUTER, "route", board, "-o", bare, "--strip-all", "--no-route"], capture_output=True, text=True, timeout=600)
        if r.returncode != 0 or not bare.exists():
            info["error"] = "load: " + (r.stderr.strip().splitlines() or ["?"])[-1][:300]
        else:
            v = drc(bare, d, "bare")
            if v is None:
                info["error"] = "drc failed on the bare board"
            else:
                info["unconnected"], info["errors"], info["warnings"] = v
    (d / "bare.json").write_text(json.dumps(info, indent=1))
    return b, info


def lengths(pcb):
    """Track length in millimetres and via count of a board file."""
    text = pcb.read_text()
    total = 0.0
    for m in re.finditer(r"\((segment|arc)\s+\(start ([-\d.]+) ([-\d.]+)\)\s+(?:\(mid ([-\d.]+) ([-\d.]+)\)\s+)?\(end ([-\d.]+) ([-\d.]+)\)", text):
        x0, y0, x1, y1 = float(m[2]), float(m[3]), float(m[6]), float(m[7])
        if m[4] is not None:
            xm, ym = float(m[4]), float(m[5])
            total += math.hypot(xm - x0, ym - y0) + math.hypot(x1 - xm, y1 - ym)
        else:
            total += math.hypot(x1 - x0, y1 - y0)
    return total, len(re.findall(r"^\s*\(via\b", text, re.M))


def route_one(b, run_dir, recipe, timeout, seed):
    d = run_dir / b
    if (d / "result.json").exists():
        return json.loads((d / "result.json").read_text())
    d.mkdir(parents=True, exist_ok=True)
    src = HOME / "boards" / b
    out = d / "out.kicad_pcb"
    res = {"id": b}
    args = [str(ROUTER), "route", str(src / "board.kicad_pcb"), "-o", str(out), "--strip-all", *recipe.split(), "--seed", str(seed), "--threads", "1", "--timeout", str(timeout)]
    started = time.time()
    try:
        r = subprocess.run(CAP + args, capture_output=True, text=True, timeout=timeout * 3 + 300)
        (d / "log.txt").write_text(r.stderr[-200_000:])
        res["code"] = r.returncode
    except subprocess.TimeoutExpired:
        res["code"] = "hung"
    res["seconds"] = round(time.time() - started, 1)
    if out.exists():
        for suffix in (".kicad_pro", ".kicad_dru"):
            if (src / "board").with_suffix(suffix).exists():
                shutil.copy((src / "board").with_suffix(suffix), out.with_suffix(suffix))
        res["wirelength"], res["vias"] = lengths(out)
        v = drc(out, d, "out")
        if v is not None:
            bare = json.loads((src / "bare.json").read_text())
            res["unconnected"], errors, warnings = v
            res["new_errors"] = {t: n - bare.get("errors", {}).get(t, 0) for t, n in errors.items() if n > bare.get("errors", {}).get(t, 0)}
            # Bare boards judged before warnings were kept have none recorded: then all count.
            res["new_warnings"] = {t: n - bare.get("warnings", {}).get(t, 0) for t, n in warnings.items() if n > bare.get("warnings", {}).get(t, 0)}
            res["success"] = res["unconnected"] == 0 and not res["new_errors"]
    (d / "result.json").write_text(json.dumps(res, indent=1))
    return res


def prepare(a):
    ids = board_ids(a.only)
    workers = int(os.environ.get("RESERVED_CORES", "4"))
    done = 0
    with concurrent.futures.ThreadPoolExecutor(workers) as pool:
        for b, info in pool.map(prepare_one, ids):
            done += 1
            if "error" in info:
                print(f"[{done}/{len(ids)}] {b}: {info['error']}", flush=True)
    bad = sum("error" in json.loads((HOME / "boards" / b / "bare.json").read_text()) for b in ids)
    print(f"{len(ids)} boards, {len(ids) - bad} prepared, {bad} not")


def run(a):
    ids = [b for b in board_ids(a.only) if "error" not in json.loads((HOME / "boards" / b / "bare.json").read_text())] if (HOME / "boards").exists() else []
    if a.sample:
        random.Random(a.sample_seed).shuffle(ids)
        ids = sorted(ids[: a.sample])
    run_dir = HOME / "runs" / a.run
    run_dir.mkdir(parents=True, exist_ok=True)
    meta = run_dir / "run.json"
    head = subprocess.run(["git", "-C", ROOT, "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
    dirty = bool(subprocess.run(["git", "-C", ROOT, "status", "--porcelain", "core"], capture_output=True, text=True).stdout.strip())
    meta.write_text(json.dumps({"recipe": a.recipe, "timeout": a.timeout, "seed": a.seed, "boards": ids, "commit": head, "dirty": dirty, "started": time.strftime("%Y-%m-%dT%H:%M:%S")}, indent=1))
    workers = int(os.environ.get("RESERVED_CORES", "4"))
    ok = 0
    with concurrent.futures.ThreadPoolExecutor(workers) as pool:
        futures = [pool.submit(route_one, b, run_dir, a.recipe, a.timeout, a.seed) for b in ids]
        for k, f in enumerate(concurrent.futures.as_completed(futures), 1):
            r = f.result()
            ok += bool(r.get("success"))
            print(f"[{k}/{len(ids)}] {r['id']}: {'ok' if r.get('success') else 'FAIL'} open {r.get('unconnected')} new {r.get('new_errors')} {r['seconds']} s", flush=True)
    print(f"{ok} of {len(ids)} succeeded")


def report(a):
    base = baselines()
    for name in a.runs:
        run_dir = HOME / "runs" / name
        results = {}
        for f in run_dir.glob("*/result.json"):
            r = json.loads(f.read_text())
            results[r["id"]] = r
        ids = sorted(results)
        n = len(ids)
        ours = [b for b in ids if results[b].get("success")]
        # Routed: nothing open and no new copper error, solder mask bridges aside; the
        # baselines' "success" likely asked no more than that.
        routed = [b for b in ids if results[b].get("unconnected") == 0 and not set(results[b].get("new_errors") or {}) - MASK]
        print(f"== {name}: {n} boards, ours {len(ours)} clean ({100 * len(ours) / max(n, 1):.1f}%), {len(routed)} routed ({100 * len(routed) / max(n, 1):.1f}%)")
        for router in ["FreeRouting", "PcbRouter", "RL-MCTS"]:
            theirs = [b for b in ids if base[b][router][0]]
            both = [b for b in ours if base[b][router][0]]
            only_us = [b for b in ours if not base[b][router][0]]
            only_them = [b for b in theirs if not results[b].get("success")]
            line = f"   {router:12} {len(theirs):4} ({100 * len(theirs) / max(n, 1):.1f}%); both {len(both)}, only us {len(only_us)}, only them {len(only_them)}"
            if both:
                wl = sorted(results[b]["wirelength"] / base[b][router][1] for b in both if base[b][router][1])
                vias = sum(results[b]["vias"] for b in both), sum(base[b][router][2] or 0 for b in both)
                secs = sum(results[b]["seconds"] for b in both), sum(base[b][router][3] or 0 for b in both)
                line += f"; on both: median wirelength {wl[len(wl) // 2]:.3f}x theirs, vias {vias[0]} vs {vias[1]:.0f}, seconds {secs[0]:.0f} vs {secs[1]:.0f}"
            print(line)
        kinds = {}
        for b in ids:
            r = results[b]
            if r.get("success"):
                continue
            k = "router failed" if r.get("code") not in (0, None) or "unconnected" not in r else ("open and errors" if r["unconnected"] and r.get("new_errors") else "open" if r["unconnected"] else "new errors")
            kinds[k] = kinds.get(k, 0) + 1
        errs = {}
        for b in ids:
            for t, c in (results[b].get("new_errors") or {}).items():
                errs[t] = errs.get(t, 0) + 1
        print(f"   failures: {kinds}; boards with new errors by type: {dict(sorted(errs.items(), key=lambda e: -e[1]))}")
        warns = {}
        for b in ids:
            for t in results[b].get("new_warnings") or {}:
                warns[t] = warns.get(t, 0) + 1
        dangling = sum(1 for b in ours if set(results[b].get("new_warnings") or {}) & {"via_dangling", "track_dangling"})
        print(f"   boards with new warnings by type: {dict(sorted(warns.items(), key=lambda e: -e[1]))}; clean boards with dangling copper: {dangling}")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("prepare")
    s.add_argument("--only")
    s = sub.add_parser("run")
    s.add_argument("run")
    s.add_argument("--recipe", default=RECIPE)
    s.add_argument("--timeout", type=float, default=120)
    s.add_argument("--seed", type=int, default=0)
    s.add_argument("--only")
    s.add_argument("--sample", type=int, help="route this many boards, drawn at random")
    s.add_argument("--sample-seed", type=int, default=1)
    s = sub.add_parser("report")
    s.add_argument("runs", nargs="+")
    a = p.parse_args()
    {"prepare": prepare, "run": run, "report": report}[a.cmd](a)


if __name__ == "__main__":
    main()
