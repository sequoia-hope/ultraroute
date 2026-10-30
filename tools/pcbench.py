#!/usr/bin/env python3
"""PCBench, routed from bare pads by the core router and judged by KiCad, against the published
baselines (FreeRouting, PcbRouter, RL-MCTS; PCBench/Baselines/Results_all.csv).

    pcbench.py prepare [--only ID,...]       convert each board to KiCad 9 once, strip it bare,
                                             and record KiCad's verdict on the bare board
    pcbench.py run RUN [--recipe "..."] [--timeout S] [--only ID,...] [--sample N] [--seed N]
                                             route every prepared board (wrap in reserve: it
                                             runs $RESERVED_CORES routes at once)
    pcbench.py report RUN [RUN ...]          success, wirelength and vias against the baselines
    pcbench.py rescore RUN [RUN ...]         judge saved reports again (after the rules change)
    pcbench.py compare A B [C ...] [--noise N|auto] [--noise-from A B] [--json OUT] [--exact]
                                             each later run against A, board by board: boards
                                             lost and won, error types gained, vias, wirelength,
                                             time limit, referee failures, peak memory; exits 1
                                             when one loses more than N boards (auto: the boards
                                             whose verdict differs between remed-0-base-s0 and
                                             remed-0-base-s0b, the same binary and seed twice: 3)
                                             or a board gains an error type; with --exact, also
                                             when a board no deadline touched differs at all
    pcbench.py note RUN TEXT                 add a dated note to the run's run.json

A board succeeds when KiCad finds nothing unconnected after a zone refill and no DRC error that
involves routed copper (a track, arc or via) of a type the bare board did not have as often: routed copper that breaks the board's own rules is a
failure, not a success. When KiCad itself fails (the zone refill or the DRC), the board gets
`referee_error` and no verdict: the referee failed, not the router. Wirelength is the routed tracks' length and vias their count, as the
baselines report them. Everything lives in ~/Software/ultraroute-lab/pcbench/: boards/ID/ holds
board.kicad_pcb (+ .kicad_pro), bare.kicad_pcb and bare.json, runs/RUN/ID/ each run's output.
runs/RUN/run.json records where the run came from (commit, binary and its sha256, kicad-cli,
Python, host, workers) when it starts; resuming a run with another commit or binary is refused
unless --resume-anyway is given. Each board's peak memory (`peak_mb`) comes from /usr/bin/time.
"""

import argparse
import concurrent.futures
import csv
import hashlib
import json
import math
import os
import platform
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
# The two runs of the Stage 0 baseline made with the same binary and seed: the boards whose verdict
# differs between them are the benchmark's noise floor (`compare --noise auto`).
NOISE_RUNS = ("remed-0-base-s0", "remed-0-base-s0b")


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


class RefereeError(Exception):
    """KiCad (the zone refill or the DRC) failed on a board: no verdict, and not the router's fault."""


def referee(name, cmd):
    """Runs one of the referee's steps, raising RefereeError with the end of its stderr if it fails."""
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=900)
    except subprocess.TimeoutExpired:
        raise RefereeError(f"{name}: timed out after 900 s")
    except OSError as e:
        raise RefereeError(f"{name}: {e}")
    if r.returncode != 0:
        raise RefereeError(f"{name}: exit {r.returncode}: {r.stderr[-300:]}")


def drc(pcb, out_dir, stem):
    """Refill the zones and ask KiCad: (unconnected, {error type: count}, {warning type: count}).
    Raises RefereeError when the refill or the DRC fails."""
    filled, report = out_dir / f"{stem}.filled.kicad_pcb", out_dir / f"{stem}.drc.json"
    for suffix in (".kicad_pro", ".kicad_dru"):
        if pcb.with_suffix(suffix).exists():
            shutil.copy(pcb.with_suffix(suffix), filled.with_suffix(suffix))
    report.unlink(missing_ok=True)
    referee("refill", [sys.executable, ROOT / "tools/refill_zones.py", pcb, filled])
    referee("kicad-cli drc", ["kicad-cli", "pcb", "drc", "--format", "json", "--severity-all", "-o", report, filled])
    if not report.exists():
        raise RefereeError("kicad-cli drc: wrote no report")
    return tally(json.loads(report.read_text()))


def routed(v):
    """Whether a violation involves routed copper (a track, arc or via), not only the board's own
    parts, edges and drawings: a pad too near the edge is the designer's, whatever KiCad reports."""
    return any(re.match(r"(Track|Via|Arc) \[", i.get("description", "")) for i in v.get("items", []))


def tally(d):
    """(unconnected, {error type: count}, {warning type: count}) of a KiCad report, counting only
    violations that involve routed copper."""
    errors, warnings = {}, {}
    for v in d.get("violations", []):
        if not routed(v):
            continue
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
            try:
                info["unconnected"], info["errors"], info["warnings"] = drc(bare, d, "bare")
            except RefereeError as e:
                info["error"] = f"drc failed on the bare board: {e}"
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
    timing = d / "time.txt"
    args = ["/usr/bin/time", "-f", "%e %M", "-o", str(timing), str(ROUTER), "route", str(src / "board.kicad_pcb"), "-o", str(out), "--strip-all", *recipe.split(), "--seed", str(seed), "--uuid-seed", str(seed), "--threads", "1", "--timeout", str(timeout)]
    started = time.time()
    try:
        r = subprocess.run(CAP + args, capture_output=True, text=True, timeout=timeout * 3 + 300)
        (d / "log.txt").write_text(r.stderr[-200_000:])
        res["code"] = r.returncode
        res["time_limited"], res["hard_deadline"] = deadlines(r.stderr)
    except subprocess.TimeoutExpired:
        res["code"] = "hung"
    res["seconds"] = round(time.time() - started, 1)
    try:
        # /usr/bin/time's last line; a router killed by a signal gets a "Command terminated" line first.
        res["peak_mb"] = round(int(timing.read_text().split()[-1]) / 1024)
    except (OSError, ValueError, IndexError):
        pass
    if out.exists():
        for suffix in (".kicad_pro", ".kicad_dru"):
            if (src / "board").with_suffix(suffix).exists():
                shutil.copy((src / "board").with_suffix(suffix), out.with_suffix(suffix))
        res["wirelength"], res["vias"] = lengths(out)
        try:
            drc(out, d, "out")
            score(res, run_dir, b)
        except RefereeError as e:
            res["referee_error"] = str(e)
    (d / "result.json").write_text(json.dumps(res, indent=1))
    return res


def deadlines(log):
    """The stages whose time ran out, and whether the run's hard deadline passed, as the router
    reports them: a result the clock cut short may differ from run to run."""
    limited = [m for line in log.splitlines() if line.startswith("deadline: time ran out in ") for m in line.split(" in ", 1)[1].split(", ")]
    return limited, any(line.startswith("deadline: the run's hard limit passed") for line in log.splitlines())


def score(res, run_dir, b):
    """Fills a result's verdict from the run's KiCad report, against the bare board's."""
    d = run_dir / b
    src = HOME / "boards" / b
    report = d / "out.drc.json"
    if not report.exists() or "referee_error" in res:
        return res
    res["unconnected"], errors, warnings = tally(json.loads(report.read_text()))
    _, bare_errors, bare_warnings = tally(json.loads((src / "bare.drc.json").read_text()))
    res["new_errors"] = {t: n - bare_errors.get(t, 0) for t, n in errors.items() if n > bare_errors.get(t, 0)}
    res["new_warnings"] = {t: n - bare_warnings.get(t, 0) for t, n in warnings.items() if n > bare_warnings.get(t, 0)}
    res["success"] = res["unconnected"] == 0 and not res["new_errors"]
    return res


def rescore(a):
    """Judges a run's results again from their saved reports, as `score` judges now."""
    for name in a.runs:
        run_dir = HOME / "runs" / name
        for f in run_dir.glob("*/result.json"):
            res = score(json.loads(f.read_text()), run_dir, f.parent.name)
            f.write_text(json.dumps(res, indent=1))


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
    workers = int(os.environ.get("RESERVED_CORES", "4"))
    now = provenance(workers)
    info = {"recipe": a.recipe, "timeout": a.timeout, "seed": a.seed, "boards": ids, **now, "started": time.strftime("%Y-%m-%dT%H:%M:%S"), "notes": [a.note] if a.note else []}
    if meta.exists():
        info = json.loads(meta.read_text())
        differ = {k: (info.get(k), now[k]) for k in ("commit", "binary_sha256") if info.get(k) != now[k]}
        if differ:
            for k, (was, new) in differ.items():
                print(f"{a.run} was started with {k} {was}, now {new}", file=sys.stderr)
            if not a.resume_anyway:
                sys.exit(f"refusing to resume {a.run} with another router; pass --resume-anyway to mix them")
            info.setdefault("resumed", []).append({**now, "at": time.strftime("%Y-%m-%dT%H:%M:%S")})
            meta.write_text(json.dumps(info, indent=1))
    else:
        meta.write_text(json.dumps(info, indent=1))
    ok = 0
    with concurrent.futures.ThreadPoolExecutor(workers) as pool:
        futures = [pool.submit(route_one, b, run_dir, a.recipe, a.timeout, a.seed) for b in ids]
        for k, f in enumerate(concurrent.futures.as_completed(futures), 1):
            r = f.result()
            ok += bool(r.get("success"))
            verdict = "ok" if r.get("success") else "REFEREE FAILED" if "referee_error" in r else "FAIL"
            print(f"[{k}/{len(ids)}] {r['id']}: {verdict} open {r.get('unconnected')} new {r.get('new_errors')} {r['seconds']} s", flush=True)
    print(f"{ok} of {len(ids)} succeeded")


def provenance(workers):
    """What a run is made with: the commit, the router binary and its hash, the referee's version,
    Python, the host and the number of routes at once."""
    def out(cmd):
        try:
            return subprocess.run(cmd, capture_output=True, text=True).stdout.strip()
        except OSError:
            return None
    binary = ROUTER.resolve()
    return {
        "commit": out(["git", "-C", ROOT, "rev-parse", "--short", "HEAD"]),
        "dirty": bool(out(["git", "-C", ROOT, "status", "--porcelain", "core"])),
        "binary": str(binary),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest() if binary.exists() else None,
        "kicad_cli_version": out(["kicad-cli", "--version"]),
        "python_version": platform.python_version(),
        "hostname": platform.node(),
        "workers": workers,
    }


def note(a):
    meta = HOME / "runs" / a.run / "run.json"
    info = json.loads(meta.read_text())
    info.setdefault("notes", []).append(f"{time.strftime('%Y-%m-%d')}: {a.text}")
    meta.write_text(json.dumps(info, indent=1))


def load(name):
    """A run's results by board, and its run.json (empty if it has none)."""
    run_dir = HOME / "runs" / name
    if not run_dir.is_dir():
        sys.exit(f"no run {name} in {HOME / 'runs'}")
    results = {}
    for f in run_dir.glob("*/result.json"):
        r = json.loads(f.read_text())
        results[r["id"]] = r
    meta = run_dir / "run.json"
    return results, json.loads(meta.read_text()) if meta.exists() else {}


def noise(a_name, b_name):
    """The number of boards both runs routed whose clean verdict differs."""
    ra, rb = load(a_name)[0], load(b_name)[0]
    return sum(bool(ra[i].get("success")) != bool(rb[i].get("success")) for i in set(ra) & set(rb))


def compare(a):
    if a.noise == "auto":
        floor = noise(*(a.noise_from or NOISE_RUNS))
        print(f"noise floor {floor}: boards whose verdict differs between {' and '.join(a.noise_from or NOISE_RUNS)}")
    else:
        floor = int(a.noise)
    base, base_meta = load(a.runs[0])
    table = {"noise": floor, "runs": a.runs, "boards": {}}
    passed = True
    for name in a.runs[1:]:
        new, meta = load(name)
        common = sorted(set(base) & set(new))
        print(f"== {a.runs[0]} -> {name}: {len(common)} boards in both" + (f" ({len(base) - len(common)} only in {a.runs[0]}, {len(new) - len(common)} only in {name})" if len(common) != max(len(base), len(new)) else ""))
        clean = lambda rs: [i for i in common if rs[i].get("success")]
        print(f"   clean {len(clean(base))} -> {len(clean(new))}")
        lost = [i for i in common if base[i].get("success") and not new[i].get("success")]
        won = [i for i in common if new[i].get("success") and not base[i].get("success")]

        def why(r):
            if "referee_error" in r:
                return "referee failed"
            if "unconnected" not in r:
                return f"router failed (code {r.get('code')})"
            return f"open {r['unconnected']}" + (f", new {sorted(r['new_errors'])}" if r.get("new_errors") else "")
        for title, ids, rs, was in (("lost", lost, new, ""), ("won", won, base, "was ")):
            print(f"   {title} {len(ids)}" + "".join(f"\n      {i}: {was}{why(rs[i])}" for i in ids))
        gained = {i: sorted(set(new[i].get("new_errors") or {}) - set(base[i].get("new_errors") or {})) for i in common}
        gained = {i: t for i, t in gained.items() if t}
        changed = [i for i in common if set(new[i].get("new_errors") or {}) != set(base[i].get("new_errors") or {})]
        print(f"   error types differ on {len(changed)} boards, gained on {len(gained)}" + "".join(f"\n      {i}: {sorted(base[i].get('new_errors') or {})} -> {sorted(new[i].get('new_errors') or {})}" for i in changed))
        both = [i for i in common if base[i].get("success") and new[i].get("success")]
        vias = [sum(rs[i].get("vias") or 0 for i in both) for rs in (base, new)]
        wl = [sum(rs[i].get("wirelength") or 0 for i in both) for rs in (base, new)]
        print(f"   on the {len(both)} boards clean in both: vias {vias[0]} -> {vias[1]}, wirelength {wl[0]:.0f} -> {wl[1]:.0f} mm")
        limits = [m.get("timeout", 120) - 10 for m in (base_meta, meta)]
        print(f"   at the time limit (>= timeout - 10 s): {sum(base[i]['seconds'] >= limits[0] for i in common)} -> {sum(new[i]['seconds'] >= limits[1] for i in common)}")
        refs = [[i for i in common if "referee_error" in rs[i]] for rs in (base, new)]
        print(f"   referee failures: {len(refs[0])} -> {len(refs[1])}" + "".join(f"\n      {i}: {new[i]['referee_error'][:120]!r}" for i in refs[1]))
        peaks = {i: new[i]["peak_mb"] for i in common if "peak_mb" in new[i]}
        if peaks:
            top = max(peaks, key=peaks.get)
            big = sorted(i for i, m in peaks.items() if m > 2048)
            print(f"   peak memory: max {peaks[top]} MB ({top}); over 2 GB: {len(big)}" + (f" {big}" if big else ""))
        else:
            print("   peak memory: not recorded")
        # Boards no deadline touched in either run route the same every time: any difference there is
        # the change's, not the clock's.
        same = lambda r: (r.get("success"), r.get("unconnected"), sorted(r.get("new_errors") or {}), r.get("vias"), round(r.get("wirelength") or 0, 3))
        untouched = [i for i in common if all("time_limited" in rs[i] and not rs[i]["time_limited"] and not rs[i]["hard_deadline"] for rs in (base, new))]
        moved = [i for i in untouched if same(base[i]) != same(new[i])]
        if any("time_limited" in rs[i] for rs in (base, new) for i in common):
            print(f"   untouched by any deadline in both: {len(untouched)} boards, {len(moved)} differ" + "".join(f"\n      {i}: vias {base[i].get('vias')} -> {new[i].get('vias')}, open {base[i].get('unconnected')} -> {new[i].get('unconnected')}" for i in moved))
        ok = len(lost) <= floor and not gained and not (a.exact and moved)
        passed &= ok
        print(f"   verdict: {'PASS' if ok else 'FAIL'}: lost {len(lost)} (noise {floor}), won {len(won)}, boards gaining an error type {len(gained)}")
        keys = ("success", "unconnected", "new_errors", "vias", "wirelength", "seconds", "code", "peak_mb", "referee_error")
        for i in common:
            row = table["boards"].setdefault(i, {a.runs[0]: {k: base[i].get(k) for k in keys}})
            row[name] = {k: new[i].get(k) for k in keys}
    if a.json:
        Path(a.json).write_text(json.dumps(table, indent=1))
    sys.exit(0 if passed else 1)


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
            k = "referee failed" if "referee_error" in r else "router failed" if r.get("code") not in (0, None) or "unconnected" not in r else ("open and errors" if r["unconnected"] and r.get("new_errors") else "open" if r["unconnected"] else "new errors")
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
    s.add_argument("--resume-anyway", action="store_true", help="resume a run started with another commit or binary")
    s.add_argument("--note", help="a note for run.json")
    s = sub.add_parser("report")
    s.add_argument("runs", nargs="+")
    s = sub.add_parser("rescore")
    s.add_argument("runs", nargs="+")
    s = sub.add_parser("compare")
    s.add_argument("runs", nargs="+", help="the reference run, then the runs to judge against it")
    s.add_argument("--noise", default="0", help="boards a run may lose and still pass, or auto")
    s.add_argument("--noise-from", nargs=2, metavar=("A", "B"), help="the two runs --noise auto measures")
    s.add_argument("--json", help="write the per-board table here")
    s.add_argument("--exact", action="store_true", help="fail when a board no deadline touched differs at all (same binary twice)")
    s = sub.add_parser("note")
    s.add_argument("run")
    s.add_argument("text")
    a = p.parse_args()
    if a.cmd == "compare" and len(a.runs) < 2:
        p.error("compare needs at least two runs")
    {"prepare": prepare, "run": run, "report": report, "rescore": rescore, "compare": compare, "note": note}[a.cmd](a)


if __name__ == "__main__":
    main()
