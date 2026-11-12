#!/usr/bin/env python3
"""The structured-routing scoreboard (docs/spec/structured-routing.md, section 1): the designers'
own routing beside ours on the same boards, one row per measure.

    scoreboard.py OURS_RUN [--designer RUN] [--out DIR] [--jobs N]

Reads two pcbench runs (the designers' is the stand-in run `designer`, which writes each input
board back unchanged), scores every routed board of OURS_RUN again with today's tools/tidy.py
(older runs were judged before the layer and crossing scores existed), and counts the repairs the
Write stage made from each board's log. Tidy scores are summed over the boards both runs wrote, so
the two columns describe the same boards. Writes index.html and scoreboard.json to DIR (default
ultraroute-lab/planned/scoreboard). Routes nothing and asks KiCad nothing.
"""
import argparse
import html
import json
import re
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LAB = ROOT.parent / "ultraroute-lab"
RUNS = LAB / "pcbench" / "runs"
sys.path.insert(0, str(ROOT / "tools"))

REPAIR = re.compile(r"^(mended|bridged|split) (\d+) ", re.M)


def tidy_of(pcb):
    r = subprocess.run([sys.executable, str(ROOT / "tools/tidy.py"), str(pcb), "--json"], capture_output=True, text=True, timeout=900)
    if r.returncode != 0:
        return None
    return json.loads(r.stdout.strip().splitlines()[-1])["tidy"]


def board_of(run, bid):
    d = RUNS / run / bid
    res = d / "result.json"
    if not res.exists():
        return None
    out = json.loads(res.read_text())
    pcb = d / "out.filled.kicad_pcb"
    if not pcb.exists():
        pcb = d / "out.kicad_pcb"
    out["tidy"] = tidy_of(pcb) if pcb.exists() and out.get("code") == 0 else None
    log = d / "log.txt"
    out["repairs"] = sum(int(n) for _, n in REPAIR.findall(log.read_text(errors="replace"))) if log.exists() else None
    return out


def column(rows, many):
    """One run's figures over its rows: `many` are the rows of 3+ track layers."""
    from tidy import ratios
    held = [r["tidy"] for r in rows if r.get("tidy")]
    keys = {k for t in held for k, v in t.items() if isinstance(v, (int, float))}
    t = ratios({k: sum(x.get(k) or 0 for x in held) for k in keys}) if held else {}
    deep = [r["tidy"] for r in many if r.get("tidy")]
    d = ratios({k: sum(x.get(k) or 0 for x in deep) for k in keys}) if deep else {}
    add = lambda k: sum(r.get(k) or 0 for r in rows)
    return {
        "boards": len(rows),
        "clean": sum(bool(r.get("success")) for r in rows),
        "off45": t.get("off45"),
        "vias": add("vias"),
        "bundled": t.get("bundled"),
        "bus_bundled": t.get("bus_bundled"),
        "coherence_3plus": d.get("coherence"),
        "same_axis_3plus": d.get("same_axis"),
        "boards_3plus": len(deep),
        "crossings_per_100mm": t.get("crossings_per_100mm"),
        "turns": t.get("turns"),
        "doglegs_per_m": t.get("doglegs_per_m"),
        "detour": t.get("detour"),
        "track_mm": round(sum(x.get("track_mm") or 0 for x in held)),
        "repairs_per_board": round(add("repairs") / len(rows), 1) if rows else None,
        "necks": add("necks"),
        "acute": add("acute"),
        "slots": add("slots"),
    }


ROWS = [  # key, label, target, how to print
    ("clean", "Boards clean (KiCad DRC, own rules)", "no board lost", "n"),
    ("off45", "Off 45 degrees", "under 2%", "%"),
    ("vias", "Vias", "no more than the designers'", "n"),
    ("bundled", "Length beside another net", "10% or more", "%"),
    ("bus_bundled", "Bus members beside a sibling", "25% or more", "%"),
    ("coherence_3plus", "Along the layer's axis, boards with 3+ track layers", "60% or more", "%"),
    ("same_axis_3plus", "Neighbouring layers on the same axis, 3+ track layers", "watch", "%"),
    ("crossings_per_100mm", "Crossings of another layer's track, per 100 mm", "watch", "f"),
    ("repairs_per_board", "Repairs at write (mends, bridges, splits), per board", "0", "f"),
    ("necks", "Necks", "0", "n"),
    ("acute", "Acute angles", "watch", "n"),
    ("slots", "Same-net slots", "watch", "n"),
    ("turns", "Turns per 10 mm", "watch", "f"),
    ("doglegs_per_m", "Doglegs per metre", "watch", "f"),
    ("detour", "Detour over the shortest tree", "watch", "f"),
    ("track_mm", "Track length, mm", "allowed to rise", "n"),
]


def show(v, how):
    if v is None:
        return "-"
    return f"{100 * v:.1f}%" if how == "%" else f"{v:,}" if how == "n" else f"{v:.2f}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("ours")
    ap.add_argument("--designer", default="designer")
    ap.add_argument("--out", default=str(LAB / "planned" / "scoreboard"))
    ap.add_argument("--jobs", type=int, default=4)
    a = ap.parse_args()
    info = json.loads((RUNS / a.ours / "run.json").read_text())
    ids = sorted(p.name for p in (RUNS / a.ours).iterdir() if (p / "result.json").exists())
    with ThreadPoolExecutor(a.jobs) as pool:
        ours = dict(zip(ids, pool.map(lambda i: board_of(a.ours, i), ids)))
        theirs = dict(zip(ids, pool.map(lambda i: board_of(a.designer, i), ids)))
    both = [i for i in ids if ours[i] and theirs[i] and ours[i].get("tidy") and theirs[i].get("tidy")]
    deep = [i for i in both if theirs[i]["tidy"].get("track_layers", 0) >= 3]
    cols = {
        "designers": column([theirs[i] for i in both], [theirs[i] for i in deep]),
        "ours": column([ours[i] for i in both], [ours[i] for i in deep]),
    }
    cols["designers"]["repairs_per_board"] = None  # nothing wrote their boards
    all_ours = [ours[i] for i in ids if ours[i]]
    data = {
        "ours_run": a.ours, "designer_run": a.designer, "commit": info.get("commit"), "tier": info.get("tier"),
        "boards_in_run": len(ids), "clean_in_run": sum(bool(r.get("success")) for r in all_ours),
        "boards_compared": len(both), "boards_3plus": len(deep), "columns": cols,
        "boards": {i: {"ours": {k: ours[i].get(k) for k in ("success", "vias", "necks", "repairs", "tidy")},
                       "designers": {k: theirs[i].get(k) for k in ("success", "vias", "necks", "tidy")}} for i in both},
    }
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    (out / "scoreboard.json").write_text(json.dumps(data, indent=1))
    e = html.escape
    rows = "".join(
        f"<tr><td>{e(label)}</td><td>{e(target)}</td><td>{show(cols['designers'][k], how)}</td><td>{show(cols['ours'][k], how)}</td></tr>"
        for k, label, target, how in ROWS)
    (out / "index.html").write_text(f"""<!doctype html><meta charset="utf-8"><title>Structured routing scoreboard</title>
<style>body{{font:15px/1.5 system-ui;margin:2rem auto;max-width:60rem;padding:0 1rem;background:#111;color:#ddd}}
table{{border-collapse:collapse;width:100%}}td,th{{padding:.35rem .6rem;border-bottom:1px solid #333;text-align:left}}
td:nth-child(n+3),th:nth-child(n+3){{text-align:right;font-variant-numeric:tabular-nums}}code{{color:#9cf}}</style>
<h1>Structured routing scoreboard</h1>
<p>Run <code>{e(a.ours)}</code> (commit {e(str(info.get('commit')))}, tier {e(str(info.get('tier')))}): {data['clean_in_run']} of {len(ids)} boards clean.
The table compares the {len(both)} boards that both this run and the designers' run <code>{e(a.designer)}</code> wrote,
whether or not either is clean; {len(deep)} of them have tracks on three or more layers.</p>
<table><tr><th>Measure</th><th>Target</th><th>Designers</th><th>Ours</th></tr>{rows}</table>
<p>Not yet measured: any-angle share by cause, and the share of length at minimum clearance where more room exists.
Targets are those of <code>docs/spec/structured-routing.md</code>, section 1. Per-board figures: <a href="scoreboard.json">scoreboard.json</a>.</p>
""")
    print(f"{len(both)} boards compared ({len(deep)} with 3+ track layers); wrote {out}/index.html")
    for k, label, _, how in ROWS:
        print(f"  {label}: designers {show(cols['designers'][k], how)}, ours {show(cols['ours'][k], how)}")


if __name__ == "__main__":
    main()
