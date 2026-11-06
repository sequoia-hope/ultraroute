#!/usr/bin/env python3
"""PCBench, routed from bare pads by the core router and judged by KiCad, against the published
baselines (FreeRouting, PcbRouter, RL-MCTS; PCBench/Baselines/Results_all.csv).

    pcbench.py prepare [--only ID,...]       convert each board to KiCad 9 once, strip it bare,
                                             and record KiCad's verdict on the bare board
    pcbench.py run RUN [--recipe "..."] [--timeout S] [--only ID,...] [--sample N] [--seed N] [--no-board-timeouts] [--wall-clock]
                                             route every prepared board (wrap in reserve: it
                                             runs $RESERVED_CORES routes at once); the boards in
                                             tools/pcbench_timeouts.json get their own time
                                             --escalate S,S...: then each board that failed for want
                                             of time again at each longer limit, in RUN@S (the
                                             gate is clean within 30 minutes; 120 s is the fast pass);
                                             then each board still open by at most --retry-open (3)
                                             with the router's --attempt 1..--attempts (4), each
                                             its own run RUN@SaK at the longest limit
    pcbench.py ab BASE NEW --base-bin BIN [--tier T] [run's options]
                                             route the same boards with BIN and this router in
                                             one pool, each board's two routes together, then
                                             compare them
    pcbench.py tiers RUN [RUN ...]           sort the boards into tiers (smoke, focus, guard,
                                             noisy, gate) by their history in these full runs;
                                             `run --tier T` routes one (docs/pcbench.md)
    pcbench.py report RUN [RUN ...]          success, wirelength and vias against the baselines
    pcbench.py rescore RUN [RUN ...]         judge saved reports again (after the rules change)
    pcbench.py rejudge RUN [RUN ...]         ask KiCad again of saved boards and the bare ones,
                                             when the judge asks more than it did (JUDGE)
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
failure, not a success. Apart from the verdict, the judge counts necks: copper narrower than
NECK times the board's minimum track width (KiCad's connection width check at warning severity),
the joints a router leaves where tracks meet only where their round ends overlap; KiCad's own DRC
passes them, as PCBench's boards set no connection width. It counts acid traps the same way
(tools/dfm.py): tracks meeting at an acute angle, and slots where a net's copper comes within the
board's clearance of itself without joining. When KiCad itself fails (the zone refill or the DRC), the board gets
`referee_error` and no verdict: the referee failed, not the router. When the router fails (a
crash, which writes the input board back, a refusal or a hang), its file is not judged: the
board fails, and `compare` counts it apart and never as noise. Wirelength is the routed tracks' length and vias their count, as the
baselines report them. Everything lives in ~/Software/ultraroute-lab/pcbench/: boards/ID/ holds
board.kicad_pcb (+ .kicad_pro), bare.kicad_pcb and bare.json, runs/RUN/ID/ each run's output.
runs/RUN/run.json records where the run came from (commit, binary and its sha256, the kicad-text
helper the router strokes text with and its sha256, kicad-cli, Python, host, workers) when it starts,
and each board's result its `text` line (stroked by the helper, or boxes and why); resuming a run with another commit or binary is refused
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
# The router's own default (ultraroute::RECIPE), named rather than spelt out here.
RECIPE = "--recipe default"
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


# What the judge asks of a board beyond its own rules; reports judged by an older one are judged
# again by `rejudge`. 2: necks counted (`neck_rule`); 3: acid traps too (`acid_traps`).
JUDGE = 3
# A neck is copper narrower than this share of the board's minimum track width: under KiCad's
# reading of a track drawn at exactly the minimum (about 0.88 of it), over the necks a router's
# overlapping round ends leave (the earring's 0.05 mm on 0.15 mm track, a third).
NECK = 0.8
NECK_RULE = "pcbench neck"


def neck_rule(pro):
    """A KiCad rule reporting copper narrower than NECK times the board's minimum track width, as
    a warning, so it counts necks without touching the verdict. None when the board sets no
    minimum."""
    try:
        width = json.loads(pro.read_text())["board"]["design_settings"]["rules"]["min_track_width"]
    except (OSError, ValueError, KeyError, TypeError):
        return None
    if not width:
        return None
    return f'\n(rule "{NECK_RULE}"\n  (severity warning)\n  (constraint connection_width (min {NECK * width:.6f}mm)))\n'


def acid_traps(pcb):
    """{"acute": N, "slots": N} for a routed board (tools/dfm.py), or {} when the check fails:
    a manufacturing count, never a verdict, so its failure fails nothing."""
    r = subprocess.run([sys.executable, str(ROOT / "tools/dfm.py"), str(pcb), "--json"], capture_output=True, text=True, timeout=600)
    try:
        return json.loads(r.stdout.strip().splitlines()[-1])
    except (ValueError, IndexError):
        return {}


def drc(pcb, out_dir, stem):
    """Refill the zones and ask KiCad, with the board's rules and [`neck_rule`]: (unconnected,
    {error type: count}, {warning type: count}), necks counted as warnings of type "neck". Raises
    RefereeError when the refill or the DRC fails."""
    filled, report = out_dir / f"{stem}.filled.kicad_pcb", out_dir / f"{stem}.drc.json"
    for suffix in (".kicad_pro", ".kicad_dru"):
        if pcb.with_suffix(suffix).exists():
            shutil.copy(pcb.with_suffix(suffix), filled.with_suffix(suffix))
    rule = neck_rule(pcb.with_suffix(".kicad_pro"))
    if rule:
        dru = filled.with_suffix(".kicad_dru")
        dru.write_text((dru.read_text() if dru.exists() else "(version 1)\n") + rule)
    report.unlink(missing_ok=True)
    referee("refill", [sys.executable, ROOT / "tools/refill_zones.py", pcb, filled])
    referee("kicad-cli drc", ["kicad-cli", "pcb", "drc", "--format", "json", "--severity-all", "-o", report, filled])
    if not report.exists():
        raise RefereeError("kicad-cli drc: wrote no report")
    return tally(json.loads(report.read_text()))


# Violations of the pour refilled round the routes that name no routed copper: a pad whose
# thermal spokes the new copper cut off names only the zone and the pad.
POUR_AFTER_ROUTING = {"starved_thermal"}


def routed(v):
    """Whether a violation involves routed copper (a track, arc or via), not only the board's own
    parts, edges and drawings: a pad too near the edge is the designer's, whatever KiCad reports.
    A pour starved by the routes counts too; what the bare board has already is taken off."""
    return v.get("type") in POUR_AFTER_ROUTING or any(re.match(r"(Track|Via|Arc) \[", i.get("description", "")) for i in v.get("items", []))


def tally(d):
    """(unconnected, {error type: count}, {warning type: count}) of a KiCad report, counting only
    violations that involve routed copper."""
    errors, warnings = {}, {}
    for v in d.get("violations", []):
        if not routed(v):
            continue
        into = errors if v["severity"] == "error" else warnings
        kind = "neck" if v["type"] == "connection_width" and NECK_RULE in v.get("description", "") else v["type"]
        into[kind] = into.get(kind, 0) + 1
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
                info["judge"] = JUDGE
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


def route_one(b, run_dir, recipe, timeout, seed, router=ROUTER, clock="cpu"):
    d = run_dir / b
    if (d / "result.json").exists():
        return json.loads((d / "result.json").read_text())
    d.mkdir(parents=True, exist_ok=True)
    src = HOME / "boards" / b
    out = d / "out.kicad_pcb"
    res = {"id": b}
    timing = d / "time.txt"
    # /usr/bin/time's user and system seconds are the CPU time `--cpu-clock` counts the timeout in.
    args = ["/usr/bin/time", "-f", "%e %U %S %M", "-o", str(timing), str(router), "route", str(src / "board.kicad_pcb"), "-o", str(out), "--strip-all", *recipe.split(), "--seed", str(seed), "--uuid-seed", str(seed), "--threads", "1", "--timeout", str(timeout), *(["--cpu-clock"] if clock == "cpu" else [])]
    started = time.time()
    try:
        r = subprocess.run(CAP + args, capture_output=True, text=True, timeout=timeout * 3 + 300)
        (d / "log.txt").write_text(r.stderr[-200_000:])
        res["code"] = r.returncode
        res["time_limited"], res["hard_deadline"] = deadlines(r.stderr)
        # What shaped the board's copper and mask text: the kicad-text helper, or why boxes.
        if m := re.search(r"^text: (.*)$", r.stderr, re.M):
            res["text"] = m[1]
        # The pours' copper the routing kept, as the router fills them before and after: a score, not a verdict.
        if m := re.search(r"^pours: (\d+)% of (\d+) mm² kept", r.stderr, re.M):
            res["pours"] = {"kept": int(m[1]) / 100, "mm2": int(m[2])}
        # Nets with an optimum width (--power-width, a rule's opt): their track, and how much of it at that width.
        if m := re.search(r"^widths: \d+% of the track of (\d+) nets with an optimum width at it \(([\d.]+) of ([\d.]+) mm\)", r.stderr, re.M):
            res["opt_widths"] = {"nets": int(m[1]), "at_mm": float(m[2]), "of_mm": float(m[3])}
    except subprocess.TimeoutExpired:
        res["code"] = "hung"
    res["seconds"] = round(time.time() - started, 1)
    try:
        # /usr/bin/time's last line; a router killed by a signal gets a "Command terminated" line first.
        times = timing.read_text().split()
        res["peak_mb"] = round(int(times[-1]) / 1024)
        if len(times) >= 4:
            res["cpu_seconds"] = round(float(times[-3]) + float(times[-2]), 1)
    except (OSError, ValueError, IndexError):
        pass
    # A router that failed wrote the input board, the designer's copper and all, or nothing: what
    # KiCad would judge there is not the router's route.
    if router_failed(res):
        res["success"] = False
    elif out.exists():
        for suffix in (".kicad_pro", ".kicad_dru"):
            if (src / "board").with_suffix(suffix).exists():
                shutil.copy((src / "board").with_suffix(suffix), out.with_suffix(suffix))
        res["wirelength"], res["vias"] = lengths(out)
        try:
            drc(out, d, "out")
            score(res, run_dir, b)
            res.update(acid_traps(out))
            res["judge"] = JUDGE
        except RefereeError as e:
            res["referee_error"] = str(e)
    (d / "result.json").write_text(json.dumps(res, indent=1))
    return res


def deadlines(log):
    """The stages whose time ran out, and whether the run's hard deadline passed, as the router
    reports them: a result the clock cut short may differ from run to run."""
    limited = [m for line in log.splitlines() if line.startswith("deadline: time ran out in ") for m in line.split(" in ", 1)[1].split(", ")]
    return limited, any(line.startswith("deadline: the run's hard limit passed") for line in log.splitlines())


def router_failed(res):
    """Whether the router itself failed (crashed, refused, hung): its file is then the input board
    or nothing, not a route, and is not judged."""
    return res.get("code") not in (0, None)


def score(res, run_dir, b):
    """Fills a result's verdict from the run's KiCad report, against the bare board's."""
    if router_failed(res):
        for k in ("unconnected", "new_errors", "new_warnings", "wirelength", "vias"):
            res.pop(k, None)
        res["success"] = False
        return res
    d = run_dir / b
    src = HOME / "boards" / b
    report = d / "out.drc.json"
    if not report.exists() or "referee_error" in res:
        return res
    res["unconnected"], errors, warnings = tally(json.loads(report.read_text()))
    _, bare_errors, bare_warnings = tally(json.loads((src / "bare.drc.json").read_text()))
    res["new_errors"] = {t: n - bare_errors.get(t, 0) for t, n in errors.items() if n > bare_errors.get(t, 0)}
    res["new_warnings"] = {t: n - bare_warnings.get(t, 0) for t, n in warnings.items() if n > bare_warnings.get(t, 0)}
    res["necks"] = res["new_warnings"].get("neck", 0)
    res["success"] = res["unconnected"] == 0 and not res["new_errors"]
    return res


def rescore(a):
    """Judges a run's results again from their saved reports, as `score` judges now."""
    for name in a.runs:
        run_dir = HOME / "runs" / name
        for f in run_dir.glob("*/result.json"):
            res = score(json.loads(f.read_text()), run_dir, f.parent.name)
            f.write_text(json.dumps(res, indent=1))


def rejudge_bare(b):
    """The bare board judged again when an older judge saw it."""
    d = HOME / "boards" / b
    info = json.loads((d / "bare.json").read_text())
    # A bare board has no tracks, so no acid traps: judged for necks, it is judged enough.
    if "error" in info or info.get("judge", 1) >= 2:
        return b, info
    try:
        info["unconnected"], info["errors"], info["warnings"] = drc(d / "bare.kicad_pcb", d, "bare")
        info["judge"] = JUDGE
    except RefereeError as e:
        info["error"] = f"drc failed on the bare board: {e}"
    (d / "bare.json").write_text(json.dumps(info, indent=1))
    return b, info


def rejudge_one(run_dir, b):
    f = run_dir / b / "result.json"
    res = json.loads(f.read_text())
    out = run_dir / b / "out.kicad_pcb"
    if router_failed(res) or res.get("judge", 1) >= JUDGE or not out.exists():
        return res
    res.pop("referee_error", None)
    try:
        # A run judged for necks lacks only the acid traps; an older one asks KiCad again.
        if res.get("judge", 1) < 2:
            drc(out, run_dir / b, "out")
            score(res, run_dir, b)
        res.update(acid_traps(out))
        res["judge"] = JUDGE
    except RefereeError as e:
        res["referee_error"] = str(e)
    f.write_text(json.dumps(res, indent=1))
    return res


def rejudge(a):
    """Asks KiCad again of each run's saved boards, and of the bare boards, what the judge asks now
    ([`JUDGE`]), without routing them again."""
    workers = int(os.environ.get("RESERVED_CORES", "4"))
    with concurrent.futures.ThreadPoolExecutor(workers) as pool:
        for name in a.runs:
            run_dir = HOME / "runs" / name
            ids = sorted(f.parent.name for f in run_dir.glob("*/result.json"))
            list(pool.map(rejudge_bare, ids))
            results = list(pool.map(lambda b: rejudge_one(run_dir, b), ids))
            meta = run_dir / "run.json"
            info = json.loads(meta.read_text())
            info["judge"] = JUDGE
            info.setdefault("notes", []).append(f"{time.strftime('%Y-%m-%d')}: judged again by judge {JUDGE}")
            meta.write_text(json.dumps(info, indent=1))
            necks = [r.get("necks", 0) for r in results]
            acute, slots = sum(r.get("acute", 0) for r in results), sum(r.get("slots", 0) for r in results)
            print(f"{name}: {sum(bool(r.get('success')) for r in results)} of {len(results)} succeeded; {sum(necks)} necks on {sum(n > 0 for n in necks)} boards; acid traps: {acute} acute, {slots} slots")


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


# Boards given more than the run's --timeout, each the time it has routed clean in
# (tools/pcbench_timeouts.json): those that fail at the default only for want of time.
BOARD_TIMEOUTS = Path(__file__).with_name("pcbench_timeouts.json")


def board_timeouts(a):
    """Each listed board's own time limit, never under the run's; none with --no-board-timeouts."""
    if a.no_board_timeouts or not BOARD_TIMEOUTS.exists():
        return {}
    return {b: max(float(t), a.timeout) for b, t in json.loads(BOARD_TIMEOUTS.read_text())["timeouts"].items()}


# Boards grouped by what a run of them tells, so most changes need not route all 1182
# (`tiers` writes it from past runs; `run --tier` and `ab --tier` read it).
TIERS = Path(__file__).with_name("pcbench_tiers.json")


def features(b):
    """What a board is made of, for spreading a tier across kinds of board: copper layers,
    whether it has zones, and its size in pads."""
    t = (HOME / "boards" / b / "board.kicad_pcb").read_text()
    m = re.search(r"\(layers\s*(.*?)\n\s*\)\n", t, re.S)
    cu = len(re.findall(r'\(\d+\s+"[^"]*\.Cu"\s+(?:signal|power|mixed|jumper)', m.group(1))) if m else 0
    pads = len(re.findall(r"\(pad\s", t))
    return {"cu": min(cu, 4), "zones": "(zone" in t, "pads": 0 if pads < 100 else 1 if pads < 400 else 2}


def spread(ids, n, rng):
    """n of the boards, taken in turn from each kind of board (`features`), so a small tier still
    holds four-layer boards, boards with zones and big boards."""
    kinds = {}
    for b in sorted(ids):
        kinds.setdefault(tuple(features(b).values()), []).append(b)
    for k in kinds.values():
        rng.shuffle(k)
    out = []
    while len(out) < min(n, len(ids)):
        for k in sorted(kinds):
            if kinds[k] and len(out) < n:
                out.append(kinds[k].pop())
    return sorted(out)


def tiers(a):
    """Sorts the boards by their history over the given runs (same seed and recipe, newest last)
    and writes the tiers:

    smoke  boards clean in every run in under SMOKE_S s with no deadline touching them, spread
           across kinds of board, and a few quick failures; a minute or two. Any difference on
           them is the change's, not the clock's (`compare --exact` holds there).
    focus  boards never clean: those the clock did not stop in the newest run (the router gave
           up with time to spare: algorithm gaps) and those it did that were within NEAR open.
           Where a change should win.
    guard  where a change loses boards: a spread sample of the boards always clean with no
           deadline touching them (they route the same every time, so any loss there is the
           change's) and a spread sample of those clean but touched by a deadline or slower than
           SLOW_S s (the first to lose when a change slows the router).
    noisy  boards clean in some runs and not others, nearly all at the time limit: they mostly
           measure the clock, so only the full corpus routes them.
    gate   focus and guard: the run to make before a change goes to the full corpus.
    """
    hist = {}
    for name in a.runs:
        for b, r in load(name)[0].items():
            hist.setdefault(b, []).append(r)
    ids = sorted(b for b, rs in hist.items() if len(rs) == len(a.runs))
    rng = random.Random(a.seed)
    secs = {b: sorted(r["seconds"] for r in hist[b])[len(hist[b]) // 2] for b in ids}
    always = [b for b in ids if all(r.get("success") for r in hist[b])]
    never = [b for b in ids if not any(r.get("success") for r in hist[b])]
    flaky = [b for b in ids if b not in always and b not in never]
    touched = lambda b: any(r.get("time_limited") or r.get("hard_deadline") for r in hist[b])
    last = {b: hist[b][-1] for b in ids}
    quick = [b for b in always if secs[b] < a.smoke_s and not touched(b)]
    quick_fail = sorted((b for b in never if not touched(b) and secs[b] < a.smoke_s * 2), key=lambda b: (last[b].get("unconnected") or 99, secs[b]))
    smoke = spread(quick, a.smoke, rng) + quick_fail[: a.smoke // 5]
    gaps = [b for b in never if not last[b].get("time_limited")]
    near = [b for b in never if last[b].get("time_limited") and (last[b].get("unconnected") or 99) <= a.near]
    marginal = [b for b in always if touched(b) or secs[b] > a.slow_s]
    steady = [b for b in always if b not in marginal]
    picked = spread(marginal, a.marginal, rng)
    guard = sorted(spread(steady, a.steady, rng) + picked)
    why = lambda bs: f"{len(bs)} boards, about {sum(secs[b] for b in bs) / 3600:.1f} core-hours"
    made = {
        "smoke": {"boards": sorted(smoke), "why": f"clean in every run under {a.smoke_s:g} s untouched by a deadline, spread by kind; and the {len(smoke) - len(spread(quick, a.smoke, random.Random(a.seed)))} quickest failures the clock never stopped"},
        "focus": {"boards": sorted(gaps + near), "why": f"never clean: {len(gaps)} the clock did not stop (algorithm gaps), {len(near)} it did within {a.near} open"},
        "guard": {"boards": guard, "why": f"{len(guard) - len(picked)} of the {len(steady)} always clean untouched by a deadline, {len(picked)} of the {len(marginal)} clean but touched by one or over {a.slow_s:g} s"},
        "noisy": {"boards": sorted(flaky), "why": "clean in some runs, not in others"},
        "gate": ["focus", "guard"],
    }
    TIERS.write_text(json.dumps({"made": time.strftime("%Y-%m-%d"), "from": a.runs, "tiers": made}, indent=1) + "\n")
    for name, t in made.items():
        bs = t["boards"] if isinstance(t, dict) else sorted({b for p in t for b in made[p]["boards"]})
        print(f"{name:6} {why(bs)}" + (f": {t['why']}" if isinstance(t, dict) else f": {' + '.join(t)}"))
    print(f"all    {why(ids)}; history: {len(always)} always clean, {len(never)} never, {len(flaky)} sometimes")


def tier_ids(name):
    """The boards of a tier in tools/pcbench_tiers.json (`tiers` writes it); a tier made of
    others ("gate": ["focus", "guard"]) is their union."""
    tiers = json.loads(TIERS.read_text())["tiers"]
    if name not in tiers:
        sys.exit(f"no tier {name} in {TIERS}: {', '.join(tiers)}")
    t = tiers[name]
    return sorted({b for part in t for b in tier_ids(part)}) if isinstance(t, list) and all(p in tiers for p in t) else t["boards"]


def chosen(a):
    """The prepared boards a run routes: --only, --tier and --sample narrow them."""
    ids = [b for b in board_ids(a.only) if "error" not in json.loads((HOME / "boards" / b / "bare.json").read_text())] if (HOME / "boards").exists() else []
    if a.tier:
        wanted = set(tier_ids(a.tier))
        ids = [b for b in ids if b in wanted]
    if a.sample:
        random.Random(a.sample_seed).shuffle(ids)
        ids = sorted(ids[: a.sample])
    return ids


# What `--timeout` counts. The CPU's (the router's `--cpu-clock`) unless `--wall-clock`: on the wall
# clock a board's verdict depended on the machine's load (12 of 34 near misses failed on
# sequoia-server running 15 routes and routed clean on an idle loaf, 2026-11-02).
def clock_of(a, router):
    if a.wall_clock:
        return "wall"
    try:
        usage = subprocess.run([str(router), "route", "--help"], capture_output=True, text=True, timeout=30)
    except OSError as e:
        sys.exit(f"{router}: {e}")
    if "--cpu-clock" not in usage.stdout + usage.stderr:
        sys.exit(f"{router} has no --cpu-clock (built before it): pass --wall-clock to time it on the wall clock")
    return "cpu"


def start(name, ids, a, router, workers, commit=True):
    """A run's directory and its run.json, written when it starts and checked when it resumes."""
    run_dir = HOME / "runs" / name
    run_dir.mkdir(parents=True, exist_ok=True)
    meta = run_dir / "run.json"
    now = provenance(workers, router, commit)
    own = {b: t for b, t in board_timeouts(a).items() if b in ids}
    info = {"recipe": a.recipe, "timeout": a.timeout, "clock": clock_of(a, router), "board_timeouts": own, "seed": a.seed, "boards": ids, "tier": a.tier, **now, "started": time.strftime("%Y-%m-%dT%H:%M:%S"), "notes": [a.note] if a.note else []}
    if meta.exists():
        info = json.loads(meta.read_text())
        differ = {k: (info.get(k), now[k]) for k in ("commit", "binary_sha256") if info.get(k) != now[k]}
        if differ:
            for k, (was, new) in differ.items():
                print(f"{name} was started with {k} {was}, now {new}", file=sys.stderr)
            if not a.resume_anyway:
                sys.exit(f"refusing to resume {name} with another router; pass --resume-anyway to mix them")
            info.setdefault("resumed", []).append({**now, "at": time.strftime("%Y-%m-%dT%H:%M:%S")})
            meta.write_text(json.dumps(info, indent=1))
    else:
        meta.write_text(json.dumps(info, indent=1))
    return run_dir, info


def route_all(jobs, a, workers):
    """Routes (run name, run dir, run.json, router, board) jobs, `workers` at once, and prints each verdict."""
    ok = {}
    with concurrent.futures.ThreadPoolExecutor(workers) as pool:
        futures = {pool.submit(route_one, b, run_dir, info.get("recipe", a.recipe), info.get("board_timeouts", {}).get(b, info.get("timeout", a.timeout)), a.seed, router, info.get("clock", "wall")): name for name, run_dir, info, router, b in jobs}
        for k, f in enumerate(concurrent.futures.as_completed(futures), 1):
            r, name = f.result(), futures[f]
            ok[name] = ok.get(name, 0) + bool(r.get("success"))
            verdict = "ok" if r.get("success") else "REFEREE FAILED" if "referee_error" in r else f"ROUTER FAILED (code {r.get('code')})" if router_failed(r) else "FAIL"
            print(f"[{k}/{len(jobs)}] {name} {r['id']}: {verdict} open {r.get('unconnected')} new {r.get('new_errors')} {r['seconds']} s", flush=True)
    return ok


def run(a):
    ids = chosen(a)
    workers = int(os.environ.get("RESERVED_CORES", "4"))
    need_text(a, ((a.run, ROUTER),))
    run_dir, info = start(a.run, ids, a, ROUTER, workers)
    ok = route_all([(a.run, run_dir, info, ROUTER, b) for b in ids], a, workers)
    print(f"{ok.get(a.run, 0)} of {len(ids)} succeeded")
    escalate(a, [(a.run, ROUTER)], workers)


def out_of_time(r):
    """Whether a board failed for want of time: not clean, and the clock cut a stage or the run."""
    return not r.get("success") and not router_failed(r) and bool(r.get("time_limited") or r.get("hard_deadline"))


def escalate(a, runs, workers):
    """Each board a run failed for want of time, routed again at each longer `--escalate` limit
    into RUN@SECONDS, until it is clean or the clock no longer cuts it short: 120 s is the fast
    pass, and a board clean within the longest limit routes (the user's gate is 30 minutes).
    Two runs (ab) escalate the boards either one ran out of time on, so both see them."""
    limits = sorted(float(t) for t in (a.escalate or "").split(",") if t)
    if not limits:
        return
    last = {name: load(name)[0] for name, _ in runs}
    for limit in limits:
        ids = sorted({b for results in last.values() for b, r in results.items() if out_of_time(r)})
        if not ids:
            break
        print(f"== escalating {len(ids)} boards to {limit:g} s", flush=True)
        a.timeout, saved = limit, a.no_board_timeouts
        a.no_board_timeouts = True
        jobs = []
        for name, router in runs:
            sub = f"{name}@{limit:g}"
            d, info = start(sub, ids, a, router, workers, commit=router == ROUTER)
            jobs += [(sub, d, info, router, b) for b in ids]
        a.no_board_timeouts = saved
        # Interleaved, as in ab, so a board's routes start together.
        jobs.sort(key=lambda j: (j[4], j[0]))
        ok = route_all(jobs, a, workers)
        print(" ".join(f"{name}@{limit:g}: {ok.get(f'{name}@{limit:g}', 0)} of {len(ids)} clean" for name, _ in runs))
        for name, _ in runs:
            later = load(f"{name}@{limit:g}")[0]
            last[name] = {b: r for b, r in later.items()}
    attempt(a, runs, workers, last, limits[-1])


def few_open(r, most):
    """Whether a board failed by at most `most` open connections and nothing else."""
    return not r.get("success") and not router_failed(r) and not r.get("new_errors") and 0 < (r.get("unconnected") or 0) <= most


def attempt(a, runs, workers, last, limit):
    """Each board still open by at most --retry-open connections after the escalations, routed
    again at `limit` with each of the router's `--attempt K` changes in turn (another seed, no
    fan-out, dearer vias), each its own run RUN@SECONDSaK with the whole limit, until one is
    clean. The router's own `--retries` share one run's time, which a first attempt that runs to
    the deadline leaves none of (FreeSmartEEG at 1800 s)."""
    runs = [(name, router) for name, router in runs if "--attempt" in usage_of(router)]
    for k in range(1, a.attempts + 1):
        ids = sorted({b for name, _ in runs for b, r in last[name].items() if few_open(r, a.retry_open)})
        if not ids:
            break
        print(f"== attempt {k}: {len(ids)} boards open by at most {a.retry_open}, at {limit:g} s", flush=True)
        saved = (a.timeout, a.no_board_timeouts, a.recipe)
        a.timeout, a.no_board_timeouts, a.recipe = limit, True, f"{a.recipe} --attempt {k}"
        jobs = []
        for name, router in runs:
            sub = f"{name}@{limit:g}a{k}"
            d, info = start(sub, ids, a, router, workers, commit=router == ROUTER)
            jobs += [(sub, d, info, router, b) for b in ids]
        a.timeout, a.no_board_timeouts, a.recipe = saved
        jobs.sort(key=lambda j: (j[4], j[0]))
        ok = route_all(jobs, a, workers)
        print(" ".join(f"{name}@{limit:g}a{k}: {ok.get(f'{name}@{limit:g}a{k}', 0)} of {len(ids)} clean" for name, _ in runs))
        for name, _ in runs:
            for b, r in load(f"{name}@{limit:g}a{k}")[0].items():
                if r.get("success") or (r.get("unconnected") or 0) < (last[name].get(b, {}).get("unconnected") or 0):
                    last[name][b] = r


def usage_of(router):
    try:
        u = subprocess.run([str(router), "route", "--help"], capture_output=True, text=True)
        return u.stdout + u.stderr
    except OSError:
        return ""


def stage_of(d):
    """(limit, attempt) of an escalation's run directory RUN@SECONDS or RUN@SECONDSaK."""
    tail = d.name.split("@")[1]
    limit, _, k = tail.partition("a")
    return float(limit), int(k or 0)


def escalated(name):
    """A run's verdicts with its escalations folded in: each board's last attempt, and the limit
    it was clean within (None if never)."""
    results, meta = load(name)
    first = lambda b: meta.get("board_timeouts", {}).get(b, meta.get("timeout"))
    final = {b: dict(r, clean_within=first(b) if r.get("success") else None) for b, r in results.items()}
    for d in sorted((HOME / "runs").glob(f"{name}@*"), key=stage_of):
        limit = stage_of(d)[0]
        for b, r in load(d.name)[0].items():
            if b in final and not final[b].get("success"):
                final[b] = dict(r, clean_within=limit if r.get("success") else None)
    return final


def text_helper(binary):
    """The text helper `binary` would run (`ultraroute text-helper`, since feat/text-strokes);
    None if it would box the text or is older."""
    try:
        helper = subprocess.run([str(binary), "text-helper"], capture_output=True, text=True).stdout.strip()
    except OSError:
        return None
    return Path(helper) if helper and Path(helper).is_file() else None


def need_text(a, routers):
    """Stops unless every router shapes its text with the kicad-text helper, or --boxed-text
    asks for boxes: a build with `-p ultraroute` alone, or a binary copied away from its helper,
    boxes copper and mask text, which buries pads under it and reads as a routing change."""
    missing = [(name, router) for name, router in routers if not text_helper(router)]
    for name, router in missing:
        print(f"{name}: no kicad-text helper beside {router} (cargo build --release -p ultraroute -p kicad-text, and copy both)", file=sys.stderr)
    if missing and not a.boxed_text:
        sys.exit("not routing: text would be boxed (--boxed-text to route anyway)")


def ab(a):
    """Routes the same boards with two routers in one pool, each board's two routes side by side,
    then compares them. Two runs started at once each with half the workers are not equal: on
    sequoia-server the side with 8 workers ran about 1.6x slower than the side with 7, which cost
    it about 7 time-limited boards. Here both see the same load at the same moment."""
    ids = chosen(a)
    workers = int(os.environ.get("RESERVED_CORES", "4"))
    base = Path(a.base_bin).resolve()
    if not base.exists():
        sys.exit(f"no router at {base}")
    need_text(a, ((a.base, base), (a.new, ROUTER)))
    runs = [(a.base, *start(a.base, ids, a, base, workers, commit=False), base), (a.new, *start(a.new, ids, a, ROUTER, workers), ROUTER)]
    # Interleaved so each board's two routes start together.
    ok = route_all([(name, d, info, router, b) for b in ids for name, d, info, router in runs], a, workers)
    print(" ".join(f"{name}: {ok.get(name, 0)} of {len(ids)} succeeded" for name, *_ in runs))
    escalate(a, [(a.base, base), (a.new, ROUTER)], workers)
    a.runs, a.noise_from, a.json, a.exact = [a.base, a.new], None, None, False
    compare(a)


def provenance(workers, router=ROUTER, commit=True):
    """What a run is made with: the commit (unless the router is not this checkout's build), the
    router binary and its hash, the referee's version, Python, the host and the number of routes at once."""
    def out(cmd):
        try:
            return subprocess.run(cmd, capture_output=True, text=True).stdout.strip()
        except OSError:
            return None
    binary = Path(router).resolve()
    helper = text_helper(binary)
    return {
        "commit": out(["git", "-C", ROOT, "rev-parse", "--short", "HEAD"]) if commit else None,
        "dirty": bool(out(["git", "-C", ROOT, "status", "--porcelain", "core"])),
        "binary": str(binary),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest() if binary.exists() else None,
        "text_helper": str(helper) if helper else None,
        "text_helper_sha256": hashlib.sha256(helper.read_bytes()).hexdigest() if helper else None,
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
        if any((HOME / "runs").glob(f"{name}@*")) or any((HOME / "runs").glob(f"{a.runs[0]}@*")):
            eb, en = escalated(a.runs[0]), escalated(name)
            within = lambda rs: [i for i in common if rs.get(i, {}).get("success")]
            gained = [i for i in common if en.get(i, {}).get("success") and not eb.get(i, {}).get("success")]
            dropped = [i for i in common if eb.get(i, {}).get("success") and not en.get(i, {}).get("success")]
            print(f"   clean with more time where it ran out: {len(within(eb))} -> {len(within(en))} (won {len(gained)}, lost {len(dropped)})")
            for i in dropped:
                print(f"      lost {i}")
        lost = [i for i in common if base[i].get("success") and not new[i].get("success")]
        won = [i for i in common if new[i].get("success") and not base[i].get("success")]

        def why(r):
            if "referee_error" in r:
                return "referee failed"
            if router_failed(r) or "unconnected" not in r:
                return f"router failed (code {r.get('code')})"
            return f"open {r['unconnected']}" + (f", new {sorted(r['new_errors'])}" if r.get("new_errors") else "")
        for title, ids, rs, was in (("lost", lost, new, ""), ("won", won, base, "was ")):
            print(f"   {title} {len(ids)}" + "".join(f"\n      {i}: {was}{why(rs[i])}" for i in ids))
        # A failed router's run has no verdict (results judged before `router_failed` carry one).
        errs = lambda r: {} if router_failed(r) else r.get("new_errors") or {}
        gained = {i: sorted(set(errs(new[i])) - set(errs(base[i]))) for i in common}
        gained = {i: t for i, t in gained.items() if t}
        changed = [i for i in common if set(errs(new[i])) != set(errs(base[i]))]
        print(f"   error types differ on {len(changed)} boards, gained on {len(gained)}" + "".join(f"\n      {i}: {sorted(errs(base[i]))} -> {sorted(errs(new[i]))}" for i in changed))
        both = [i for i in common if base[i].get("success") and new[i].get("success")]
        vias = [sum(rs[i].get("vias") or 0 for i in both) for rs in (base, new)]
        wl = [sum(rs[i].get("wirelength") or 0 for i in both) for rs in (base, new)]
        print(f"   on the {len(both)} boards clean in both: vias {vias[0]} -> {vias[1]}, wirelength {wl[0]:.0f} -> {wl[1]:.0f} mm")
        limit = lambda m, i: m.get("board_timeouts", {}).get(i, m.get("timeout", 120)) - 10
        # On the clock the run's timeout counted: a run on the CPU's is at its limit by CPU seconds.
        spent = lambda m, r: r.get("cpu_seconds", r["seconds"]) if m.get("clock") == "cpu" else r["seconds"]
        print(f"   at the time limit (>= timeout - 10 s): {sum(spent(base_meta, base[i]) >= limit(base_meta, i) for i in common)} -> {sum(spent(meta, new[i]) >= limit(meta, i) for i in common)}")
        if base_meta.get("clock", "wall") != meta.get("clock", "wall"):
            print(f"   clocks differ: {base_meta.get('clock', 'wall')} -> {meta.get('clock', 'wall')}")
        if (base_meta.get("board_timeouts") or {}) != (meta.get("board_timeouts") or {}):
            print(f"   board timeouts differ: {len(base_meta.get('board_timeouts') or {})} -> {len(meta.get('board_timeouts') or {})} boards given their own")
        refs = [[i for i in common if "referee_error" in rs[i]] for rs in (base, new)]
        # Pours kept (planes and fills a route cuts into): a score, on boards clean in both.
        poured = [i for i in common if base[i].get("success") and new[i].get("success") and base[i].get("pours") and new[i].get("pours")]
        if poured:
            kept = lambda rs: sum(rs[i]["pours"]["kept"] * rs[i]["pours"]["mm2"] for i in poured) / max(1, sum(rs[i]["pours"]["mm2"] for i in poured))
            worse = sorted((i for i in poured if new[i]["pours"]["kept"] < base[i]["pours"]["kept"] - 0.05), key=lambda i: new[i]["pours"]["kept"] - base[i]["pours"]["kept"])
            print(f"   pours kept on {len(poured)} boards clean in both: {100 * kept(base):.0f}% -> {100 * kept(new):.0f}% of their area, more than 5 points less on {len(worse)}" + "".join(f"\n      {i}: {100 * base[i]['pours']['kept']:.0f}% -> {100 * new[i]['pours']['kept']:.0f}%" for i in worse[:5]))
        # Optimum widths (KiCad's opt): a score, not a verdict; on boards clean in both.
        wide = [i for i in common if base[i].get("success") and new[i].get("success") and (base[i].get("opt_widths") or new[i].get("opt_widths"))]
        if wide:
            share = lambda rs: (lambda w: f"{100 * w[0] / w[1]:.0f}% of {w[1] / 1000:.1f} m" if w[1] else "none")([sum(rs[i].get("opt_widths", {}).get(k, 0) for i in wide) for k in ("at_mm", "of_mm")])
            print(f"   optimum widths on {len(wide)} boards clean in both: track at its net's optimum {share(base)} -> {share(new)}")
        judged = [i for i in common if base[i].get("judge", 1) >= 2 and new[i].get("judge", 1) >= 2]
        if judged:
            necks = lambda r: r.get("necks", 0)
            more = sorted((i for i in judged if necks(new[i]) > necks(base[i])), key=lambda i: necks(base[i]) - necks(new[i]))
            print(f"   necks (copper under {NECK}x the minimum track width) on {len(judged)} boards judged for them: {sum(necks(base[i]) for i in judged)} -> {sum(necks(new[i]) for i in judged)}, boards with any {sum(necks(base[i]) > 0 for i in judged)} -> {sum(necks(new[i]) > 0 for i in judged)}, more on {len(more)}" + "".join(f"\n      {i}: {necks(base[i])} -> {necks(new[i])}" for i in more[:10]))
        trapped = [i for i in common if base[i].get("judge", 1) >= 3 and new[i].get("judge", 1) >= 3]
        if trapped:
            for kind in ("acute", "slots"):
                more = sorted((i for i in trapped if new[i].get(kind, 0) > base[i].get(kind, 0)), key=lambda i: base[i].get(kind, 0) - new[i].get(kind, 0))
                print(f"   {kind} (acid traps) on {len(trapped)} boards: {sum(base[i].get(kind, 0) for i in trapped)} -> {sum(new[i].get(kind, 0) for i in trapped)}, more on {len(more)}" + "".join(f"\n      {i}: {base[i].get(kind, 0)} -> {new[i].get(kind, 0)}" for i in more[:5]))
        print(f"   referee failures: {len(refs[0])} -> {len(refs[1])}" + "".join(f"\n      {i}: {new[i]['referee_error'][:120]!r}" for i in refs[1]))
        broke = [[i for i in common if router_failed(rs[i])] for rs in (base, new)]
        newly_broke = [i for i in broke[1] if i not in broke[0]]
        print(f"   router failures (crash, refusal, hang): {len(broke[0])} -> {len(broke[1])}" + "".join(f"\n      {i}: code {new[i].get('code')}" for i in broke[1]))
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
        # A crash is never noise.
        ok = len(lost) <= floor and not gained and not newly_broke and not (a.exact and moved)
        passed &= ok
        print(f"   verdict: {'PASS' if ok else 'FAIL'}: lost {len(lost)} (noise {floor}), won {len(won)}, boards gaining an error type {len(gained)}, new router failures {len(newly_broke)}")
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
        judged = [b for b in ids if results[b].get("judge", 1) >= 2]
        if judged:
            necked = [b for b in judged if results[b].get("necks")]
            print(f"   necks (copper under {NECK}x the board's minimum track width, apart from the verdict): {sum(results[b].get('necks', 0) for b in judged)} on {len(necked)} of {len(judged)} boards; clean boards with any: {sum(1 for b in necked if results[b].get('success'))}")
        else:
            print("   necks: not judged (pcbench.py rejudge)")
        traps = [b for b in ids if results[b].get("judge", 1) >= 3]
        if traps:
            print(f"   acid traps (apart from the verdict): {sum(results[b].get('acute', 0) for b in traps)} acute angles, {sum(results[b].get('slots', 0) for b in traps)} slots, on {len(traps)} boards judged for them")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("prepare")
    s.add_argument("--only")
    s = sub.add_parser("run")
    s.add_argument("run")
    s.add_argument("--boxed-text", action="store_true", help="route even with a router that boxes its text (no kicad-text helper beside it)")
    s.add_argument("--recipe", default=RECIPE)
    s.add_argument("--timeout", type=float, default=120)
    s.add_argument("--no-board-timeouts", action="store_true", help="every board at --timeout, none at its own from pcbench_timeouts.json")
    s.add_argument("--wall-clock", action="store_true", help="--timeout in wall seconds, as runs before 2026-11-03 were (default: the router's CPU seconds)")
    s.add_argument("--seed", type=int, default=0)
    s.add_argument("--only")
    s.add_argument("--sample", type=int, help="route this many boards, drawn at random")
    s.add_argument("--sample-seed", type=int, default=1)
    s.add_argument("--resume-anyway", action="store_true", help="resume a run started with another commit or binary")
    s.add_argument("--note", help="a note for run.json")
    s.add_argument("--escalate", help="seconds, comma-separated: route again at each longer limit the boards that failed for want of time (e.g. 600,1800)")
    s.add_argument("--attempts", type=int, default=4, help="after --escalate, route boards still open by at most --retry-open again with the router's --attempt 1..N, each its own run at the longest limit (0: none)")
    s.add_argument("--retry-open", type=int, default=3, help="the most open connections a board may have for --attempts")
    s.add_argument("--tier", help="only the boards of this tier (tools/pcbench_tiers.json)")
    s = sub.add_parser("ab", help="route with two routers side by side, then compare")
    s.add_argument("--boxed-text", action="store_true", help="route even with a router that boxes its text (no kicad-text helper beside it)")
    s.add_argument("base", help="the run of --base-bin's routes")
    s.add_argument("new", help="the run of this checkout's router (ULTRAROUTE_BIN)")
    s.add_argument("--base-bin", required=True, help="the reference router binary")
    s.add_argument("--wall-clock", action="store_true", help="--timeout in wall seconds (default: CPU seconds; both routers need --cpu-clock)")
    s.add_argument("--recipe", default=RECIPE)
    s.add_argument("--timeout", type=float, default=120)
    s.add_argument("--no-board-timeouts", action="store_true")
    s.add_argument("--seed", type=int, default=0)
    s.add_argument("--only")
    s.add_argument("--tier")
    s.add_argument("--sample", type=int)
    s.add_argument("--sample-seed", type=int, default=1)
    s.add_argument("--resume-anyway", action="store_true")
    s.add_argument("--note")
    s.add_argument("--escalate", help="seconds, comma-separated: route again at each longer limit the boards that failed for want of time (e.g. 600,1800)")
    s.add_argument("--attempts", type=int, default=4, help="after --escalate, route boards still open by at most --retry-open again with the router's --attempt 1..N, each its own run at the longest limit (0: none)")
    s.add_argument("--retry-open", type=int, default=3, help="the most open connections a board may have for --attempts")
    s.add_argument("--noise", default="0", help="boards the new run may lose and still pass")
    s = sub.add_parser("tiers", help="sort the boards into tiers by their history in these runs")
    s.add_argument("runs", nargs="+", help="full runs, same seed and recipe, oldest first")
    s.add_argument("--smoke", type=int, default=40, help="steady boards in smoke (and a fifth as many failures)")
    s.add_argument("--smoke-s", type=float, default=10)
    s.add_argument("--near", type=int, default=3, help="open connections a time-limited failure may have to be in focus")
    s.add_argument("--slow-s", type=float, default=60)
    s.add_argument("--steady", type=int, default=200, help="steady boards sampled into guard")
    s.add_argument("--marginal", type=int, default=40, help="clean boards a deadline touched sampled into guard")
    s.add_argument("--seed", type=int, default=1)
    s = sub.add_parser("report")
    s.add_argument("runs", nargs="+")
    s = sub.add_parser("rescore")
    s.add_argument("runs", nargs="+")
    s = sub.add_parser("rejudge")
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
    {"prepare": prepare, "run": run, "ab": ab, "tiers": tiers, "report": report, "rescore": rescore, "rejudge": rejudge, "compare": compare, "note": note}[a.cmd](a)


if __name__ == "__main__":
    main()
