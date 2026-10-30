#!/usr/bin/env python3
"""lab.py — record routing experiments for the local lab page (`proj up ultraroute-lab`).

    tools/lab.py run EXP --title TITLE [--notes TEXT] [--timeout S] [--threads N] [--recipe ARGS]
                 NAME=BOARD.kicad_pcb[::EXTRA ARGS] ...
    tools/lab.py ingest EXP --title TITLE --dir DIR [--args ARGS] NAME ...
    tools/lab.py note EXP TEXT
    tools/lab.py sync

`run` routes every board at once with the core router (wrap it in ~/.claude/bin/reserve), streaming
each log into the lab as it is written, then refills the zones, asks KiCad's DRC what is open and
broken, and counts copper cut into blocks. `ingest` records runs made by hand: DIR/NAME.log,
DIR/NAME.stats.json, DIR/NAME.drc.json and DIR/NAME.out.kicad_pcb, whichever exist. The lab lives in
$ULTRAROUTE_LAB or ~/Software/ultraroute-lab, outside the repository, so the Pages site never
carries it; its pages are lab/*.html here, copied in on every write.
"""
import argparse
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LAB = Path(os.environ.get("ULTRAROUTE_LAB", Path.home() / "Software" / "ultraroute-lab"))
ROUTER = Path(os.environ.get("ULTRAROUTE_BIN", ROOT / "core/target/release/ultraroute")).resolve()
BLOCK_CHECK = ROOT / "core/target/release/examples/block_check"
# The router's default recipe (ultraroute::RECIPE), on the bare board.
RECIPE = "--strip-all --recipe default"


def now():
    return datetime.now().astimezone().isoformat(timespec="seconds")


def commit():
    head = subprocess.run(["git", "-C", ROOT, "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
    dirty = subprocess.run(["git", "-C", ROOT, "diff", "--quiet", "HEAD", "--", "core"]).returncode != 0
    return {"head": head, "dirty": dirty}


def write_json(path, value):
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(value, indent=1))
    tmp.replace(path)


def sync():
    """The pages, and the list of experiments and feature films the lab starts from."""
    LAB.mkdir(parents=True, exist_ok=True)
    for page in sorted((ROOT / "lab").glob("*.html")):
        shutil.copy(page, LAB / page.name)
    listed = []
    for f in sorted((LAB / "experiments").glob("*/experiment.json")):
        e = json.loads(f.read_text())
        listed.append({k: e.get(k) for k in ("id", "title", "status", "started", "finished", "commit")} | {"runs": len(e.get("runs", []))})
    listed.sort(key=lambda e: e["started"] or "", reverse=True)
    write_json(LAB / "index.json", listed)
    films_dir = LAB / "films"
    if films_dir.is_dir():
        order = ["dipole", "push", "flow", "comb", "clear-lane", "shove", "nudge", "prefanout", "pairs", "negotiate", "route", "rip-up", "chase", "tighten", "cleanup", "tune", "fill", "topo"]
        rank = {k: i for i, k in enumerate(order)}
        films = []
        for f in sorted(films_dir.glob("*/*.json")):
            try:
                v = json.loads(f.read_text())
            except Exception:
                continue
            feat, name = v.get("feature"), v.get("name")
            if feat and name:
                films.append({
                    "feature": feat,
                    "name": name,
                    "title": v.get("title") or name,
                    "subtitle": v.get("subtitle") or "",
                    "board": v.get("board") or "",
                    "cropped": bool(v.get("cropped")),
                    "frames": len(v.get("frames") or []),
                    "path": f"films/{feat}/{name}.json",
                })
        films.sort(key=lambda x: (rank.get(x["feature"], 999), x["feature"], x["name"]))
        write_json(films_dir / "index.json", films)


# Lines the router prints when a stage ends, and what each says.
SUMMARIES = [
    ("negotiate", r"negotiated (?P<done>\d+) of (?P<total>\d+) connections in (?P<rounds>\d+) rounds, (?P<vias>\d+) vias, (?P<t>[\d.]+) s"),
    ("rip-up", r"rip-up: (?P<tries>\d+) tries, (?P<kept>\d+) kept, (?P<ripped>\d+) ripped.*?; (?P<t>[\d.]+) s"),
    ("nudge", r"nudge: (?P<tried>\d+) steps tried, (?P<illegal>\d+) illegal, (?P<kept>\d+) kept \((?P<guided>\d+) as blame asked\), moved (?P<moved>\[.*?\]), (?P<t>[\d.]+) s"),
    ("tighten", r"pulled tight: (?P<before>\d+) -> (?P<after>\d+) segments, (?P<mm_before>\d+) -> (?P<mm_after>\d+) mm, (?P<t>[\d.]+) s"),
    ("cleanup", r"dropped (?P<doubled>\d+) doubled, pruned (?P<pruned>\d+) dangling, open (?P<open_before>\d+) -> (?P<open_after>\d+), (?P<t>[\d.]+) s"),
    ("result", r"routed (?P<routed>\d+) more, (?P<failed>\d+) failed, (?P<open>\d+) open, (?P<t>[\d.]+) s"),
]
VERBOSE = re.compile(r"^\s+\[\s*(?P<t>[\d.]+) s\] (?P<phase>[^:]+): (?P<text>.*)$")


def summaries(stats_path):
    """SUMMARIES's figures, read from the router's stats file (`--stats`) instead of its lines."""
    run = json.loads(Path(stats_path).read_text())
    stages = {s["name"]: s for s in run["stages"]}
    out = {}

    def at(s):
        return round(s["started"] + s["seconds"], 1)

    if "negotiation" in stages:
        s = stages["negotiation"]
        d = s["detail"]
        out["negotiate"] = {"done": d["committed"], "total": d["connections"], "rounds": d["rounds"], "vias": d["vias"], "t": at(s)}
    if "rip-up" in stages:
        s = stages["rip-up"]
        out["rip-up"] = {k: s["detail"][k] for k in ("tries", "kept", "ripped")} | {"t": at(s)}
    if "nudge" in stages:
        s = stages["nudge"]
        out["nudge"] = {k: s["detail"][k] for k in ("tried", "illegal", "kept", "guided", "moved")} | {"t": at(s)}
    if "tightening" in stages:
        # Told after the via pass, which may pull the copper tight again.
        s, d = stages["tightening"], stages["tightening"]["detail"]
        out["tighten"] = {"before": d["segments_before"], "after": d["segments_after"], "mm_before": round(d["mm_before"]), "mm_after": round(d["mm_after"]), "t": at(stages.get("vias", s))}
    if "cleaning" in stages and (stages["cleaning"]["detail"]["doubled"] or stages["cleaning"]["detail"]["pruned"]):
        s = stages["cleaning"]
        out["cleanup"] = {"doubled": s["detail"]["doubled"], "pruned": s["detail"]["pruned"], "open_before": s["open_before"], "open_after": s["open_after"], "t": at(s)}
    if "result" in stages:
        s = stages["result"]
        out["result"] = {"routed": s["detail"]["routed"], "failed": s["detail"]["failed"], "open": s["detail"]["open"], "t": at(s)}
    return out


def parse_log(text, stats=None):
    """Each stage's summary, and each phase's span and count from the verbose lines. With
    `stats`, the router's stats file (`--stats`), the summaries come from it; without, from the
    lines, as logs from before the stats file have them."""
    from_lines, phases, other = {}, {}, []
    for line in text.splitlines():
        m = VERBOSE.match(line)
        if m:
            t, phase = float(m["t"]), m["phase"]
            p = phases.setdefault(phase, {"phase": phase, "first": t, "last": t, "lines": 0})
            p["last"], p["lines"] = t, p["lines"] + 1
            continue
        for name, pattern in SUMMARIES:
            m = re.search(pattern, line)
            if m:
                from_lines[name] = {k: (v if not re.fullmatch(r"[\d.]+", v) else float(v) if "." in v else int(v)) for k, v in m.groupdict().items()}
                break
        else:
            if line.strip() and not line.startswith("stats: "):
                other.append(line)
    use_file = stats is not None and Path(stats).exists()
    out = {"stats": summaries(stats) if use_file else from_lines, "phases": sorted(phases.values(), key=lambda p: p["first"]), "other": other[:50]}
    if use_file:
        run = json.loads(Path(stats).read_text())
        out["summary"], out["findings"] = run["summary"], run["findings"][:200]
    return out


def judge(pcb, out_dir):
    """KiCad's verdict on a routed board after a refill, and copper cut into its blocks."""
    verdict = {}
    if not pcb.exists():
        return verdict
    filled, report = out_dir / "filled.kicad_pcb", out_dir / "drc.json"
    # The project and its custom rules go with the board, or KiCad judges by its defaults.
    for suffix in (".kicad_pro", ".kicad_dru"):
        if pcb.with_suffix(suffix).exists():
            shutil.copy(pcb.with_suffix(suffix), filled.with_suffix(suffix))
    subprocess.run([sys.executable, ROOT / "tools/refill_zones.py", pcb, filled], capture_output=True)
    subprocess.run(["kicad-cli", "pcb", "drc", "--format", "json", "--severity-all", "-o", report, filled], capture_output=True)
    if report.exists():
        verdict["drc"] = summarise_drc(json.loads(report.read_text()))
    if BLOCK_CHECK.exists():
        r = subprocess.run([BLOCK_CHECK, pcb], capture_output=True, text=True)
        verdict["blocks"] = blocks_of(r.stdout)
    return verdict


def blocks_of(report):
    """block_check's count of copper cut into blocks; none on a board without blocks."""
    m = re.search(r"(\d+) intrusions", report)
    if "no blocks" in report or not m:
        return None
    return {"intrusions": int(m[1]), "report": report[-4000:]}


def summarise_drc(d):
    def net(desc):
        m = re.search(r"\[([^\]]+)\]", desc)
        return m[1] if m else "?"

    errors, warnings, listed = {}, 0, []
    for v in d.get("violations", []):
        if v["severity"] == "error":
            errors[v["type"]] = errors.get(v["type"], 0) + 1
        else:
            warnings += 1
        item = v["items"][0] if v.get("items") else {}
        listed.append({"type": v["type"], "severity": v["severity"], "at": [round(item.get("pos", {}).get("x", 0), 3), round(item.get("pos", {}).get("y", 0), 3)], "what": item.get("description", "")})
    open_nets = {}
    for u in d.get("unconnected_items", []):
        n = net(u["items"][0]["description"]) if u.get("items") else "?"
        open_nets[n] = open_nets.get(n, 0) + 1
    listed.sort(key=lambda v: (v["severity"] != "error", v["type"]))
    return {"unconnected": len(d.get("unconnected_items", [])), "errors": errors, "warnings": warnings, "open_nets": open_nets, "violations": listed[:300]}


def experiment(exp, title, notes):
    d = LAB / "experiments" / exp
    d.mkdir(parents=True, exist_ok=True)
    f = d / "experiment.json"
    e = json.loads(f.read_text()) if f.exists() else {"id": exp, "notes": [], "runs": []}
    e.update(title=title or e.get("title") or exp, started=now(), finished=None, status="running", commit=commit())
    if notes:
        e["notes"].append({"at": now(), "text": notes})
    return d, f, e


def run(a):
    if not ROUTER.exists():
        sys.exit(f"no router at {ROUTER}: cargo build --release -p ultraroute")
    d, f, e = experiment(a.exp, a.title, a.notes)
    e["runs"] = []
    procs = []
    for spec in a.runs:
        name, _, rest = spec.partition("=")
        board, _, extra = rest.partition("::")
        board = Path(board).resolve()
        rd = d / name
        if rd.exists():
            shutil.rmtree(rd)
        rd.mkdir(parents=True)
        for src in [board, board.with_suffix(".kicad_pro"), board.with_suffix(".kicad_dru")]:
            if src.exists():
                shutil.copy(src, rd / ("input" + src.suffix))
        args = shlex.split(a.recipe) + shlex.split(extra) + ["--threads", str(a.threads), "--timeout", str(a.timeout), "--verbose", "--stats", str(rd / "stats.json")]
        cmd = ["/usr/bin/time", "-f", "%e %M", "-o", rd / "time.txt", ROUTER, "route", rd / "input.kicad_pcb", "-o", rd / "out.kicad_pcb", *args]
        log = open(rd / "log.txt", "w")
        procs.append((name, rd, subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=log), log))
        e["runs"].append({"name": name, "board": str(board), "args": args, "status": "running", "started": now()})
    write_json(f, e)
    sync()
    try:
        pending = {name for name, *_ in procs}
        while pending:
            time.sleep(2)
            for (name, rd, p, log), r in zip(procs, e["runs"]):
                if name in pending and p.poll() is not None:
                    pending.discard(name)
                    log.close()
                    if (rd / "out.kicad_pcb").exists():
                        for suffix in (".kicad_pro", ".kicad_dru"):
                            if (rd / ("input" + suffix)).exists():
                                shutil.copy(rd / ("input" + suffix), rd / ("out" + suffix))
                    finish(r, rd, p.returncode)
                    write_json(f, e)
    except KeyboardInterrupt:
        for name, rd, p, log in procs:
            p.terminate()
        for r in e["runs"]:
            if r["status"] == "running":
                r["status"] = "interrupted"
        e["status"] = "interrupted"
    else:
        e["status"] = "done"
    e["finished"] = now()
    write_json(f, e)
    sync()
    print(f"{LAB / 'experiments' / a.exp}")


def finish(r, rd, code):
    r["status"] = "done" if code == 0 else f"exit {code}"
    r["finished"] = now()
    r |= parse_log((rd / "log.txt").read_text(), rd / "stats.json")
    t = (rd / "time.txt").read_text().split() if (rd / "time.txt").exists() else []
    if len(t) >= 2:
        r["wall_s"], r["peak_mb"] = float(t[-2]), round(int(t[-1]) / 1024)
    r |= judge(rd / "out.kicad_pcb", rd)


def ingest(a):
    d, f, e = experiment(a.exp, a.title, a.notes)
    src = Path(a.dir)
    for name in a.runs:
        rd = d / name
        rd.mkdir(exist_ok=True)
        r = next((r for r in e["runs"] if r["name"] == name), None) or {"name": name}
        if r not in e["runs"]:
            e["runs"].append(r)
        r.update(args=shlex.split(a.args or ""), status="done", board=str(src / f"{name}.kicad_pcb"))
        if (src / f"{name}.stats.json").exists():
            shutil.copy(src / f"{name}.stats.json", rd / "stats.json")
        if (src / f"{name}.log").exists():
            shutil.copy(src / f"{name}.log", rd / "log.txt")
            r |= parse_log((rd / "log.txt").read_text(), rd / "stats.json")
        if (src / f"{name}.drc.json").exists():
            r["drc"] = summarise_drc(json.loads((src / f"{name}.drc.json").read_text()))
        if (src / f"{name}.out.kicad_pcb").exists() and BLOCK_CHECK.exists():
            out = subprocess.run([BLOCK_CHECK, src / f"{name}.out.kicad_pcb"], capture_output=True, text=True).stdout
            r["blocks"] = blocks_of(out)
    e["status"], e["finished"] = "done", now()
    write_json(f, e)
    sync()
    print(f"{d}")


def note(a):
    f = LAB / "experiments" / a.exp / "experiment.json"
    e = json.loads(f.read_text())
    e.setdefault("notes", []).append({"at": now(), "text": a.text})
    write_json(f, e)
    sync()


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("exp")
    r.add_argument("--title")
    r.add_argument("--notes")
    r.add_argument("--timeout", type=float, default=240)
    r.add_argument("--threads", type=int, default=4)
    r.add_argument("--recipe", default=RECIPE)
    r.add_argument("runs", nargs="+")
    i = sub.add_parser("ingest")
    i.add_argument("exp")
    i.add_argument("--title")
    i.add_argument("--notes")
    i.add_argument("--dir", required=True)
    i.add_argument("--args")
    i.add_argument("runs", nargs="+")
    n = sub.add_parser("note")
    n.add_argument("exp")
    n.add_argument("text")
    sub.add_parser("sync")
    a = ap.parse_args()
    {"run": run, "ingest": ingest, "note": note, "sync": lambda _: sync()}[a.cmd](a)


if __name__ == "__main__":
    main()
