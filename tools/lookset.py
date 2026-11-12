#!/usr/bin/env python3
"""The look set (docs/spec/structured-routing.md, S0): a page showing, for a dozen chosen boards,
the designer's routing beside ours on every copper layer that carries tracks. It is the review by
eye every stage gets; the scoreboard (tools/scoreboard.py) is the backstop.

    lookset.py OURS_RUN [--label NAME] [--designer RUN] [--set tools/lookset.json] [--only id,...]

OURS_RUN and the designers' run are pcbench runs; boards the set gives as files are taken from
those files whatever the run. Writes ultraroute-lab/planned/lookset/NAME/ (NAME defaults to the
run's name): index.html, and per board one SVG and PNG per layer and side. Routes nothing.
"""
import argparse
import html
import json
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import cairosvg
import pcbnew

ROOT = Path(__file__).resolve().parent.parent
LAB = ROOT.parent / "ultraroute-lab"
RUNS = LAB / "pcbench" / "runs"
WIDTH = 1500


def run_board(run, bid):
    """(board file, result.json or {}) of a pcbench run's board."""
    d = RUNS / run / bid
    res = json.loads((d / "result.json").read_text()) if (d / "result.json").exists() else {}
    return d / "out.kicad_pcb", res


def path_of(p):
    p = Path(p).expanduser()
    return p if p.is_absolute() else LAB / p


def track_layers(pcb, bare):
    """(names of the copper layers carrying tracks, via count); writes a copy with every pour's
    fill taken out to `bare`, the one that is drawn: a filled pour hides the tracks in it."""
    b = pcbnew.LoadBoard(str(pcb))
    for z in b.Zones():
        z.UnFill()
    pcbnew.SaveBoard(str(bare), b)
    ids = {t.GetLayer() for t in b.GetTracks() if t.GetClass() in ("PCB_TRACK", "PCB_ARC")}
    order = sorted(ids, key=lambda l: (l == pcbnew.B_Cu, l != pcbnew.F_Cu, l))
    vias = sum(t.GetClass() == "PCB_VIA" for t in b.GetTracks())
    return [b.GetLayerName(l) for l in order], vias


def tidy_of(pcb):
    r = subprocess.run([sys.executable, str(ROOT / "tools/tidy.py"), str(pcb), "--json"], capture_output=True, text=True, timeout=900)
    return json.loads(r.stdout.strip().splitlines()[-1])["tidy"] if r.returncode == 0 else {}


def draw(pcb, layer, stem):
    """One layer of the board with its outline, as STEM.svg and STEM.png. True if drawn."""
    svg = Path(f"{stem}.svg")
    # Board area first; the whole page where KiCad gets the board's own bounding box wrong (a
    # picture with no height, or one too tall to draw).
    for mode in ("2", "0"):
        r = subprocess.run(["kicad-cli", "pcb", "export", "svg", "--layers", f"{layer},Edge.Cuts", "--exclude-drawing-sheet",
                            "--page-size-mode", mode, "--black-and-white", "-o", str(svg), str(pcb)], capture_output=True, text=True)
        if r.returncode != 0 or not svg.exists():
            continue
        try:
            cairosvg.svg2png(url=str(svg), write_to=str(Path(f"{stem}.png")), output_width=WIDTH, background_color="white")
            return True
        except Exception:
            continue
    return False


def one(board, a, out):
    bid = board["id"]
    d = out / bid
    d.mkdir(parents=True, exist_ok=True)
    if "pcbench" in board:
        theirs, their_res = run_board(a.designer, board["pcbench"])
        ours, our_res = run_board(a.ours, board["pcbench"])
    else:
        theirs, their_res, ours, our_res = path_of(board["designer"]), {}, path_of(board["ours"]), {}
    sides = {"designer": (theirs, their_res), "ours": (ours, our_res)}
    info = {"id": bid, "sides": {}, "layers": []}
    names, drawn = [], {}
    for side, (pcb, res) in sides.items():
        if not pcb.exists():
            info["sides"][side] = {"missing": str(pcb)}
            continue
        bare = d / f"{side}.nofill.kicad_pcb"
        layers, vias = track_layers(pcb, bare)
        drawn[side] = bare
        names += [l for l in layers if l not in names]
        t = tidy_of(pcb)
        info["sides"][side] = {"board": str(pcb), "layers": layers, "vias": vias, "open": res.get("unconnected"),
                               "clean": res.get("success"), "off45": t.get("off45"), "bundled": t.get("bundled"),
                               "bus_bundled": t.get("bus_bundled"), "track_mm": t.get("track_mm"), "per_layer_mm": t.get("per_layer_mm")}
    order = ["F.Cu"] + sorted(l for l in names if l not in ("F.Cu", "B.Cu")) + ["B.Cu"]
    for layer in [l for l in order if l in names]:
        row = {"layer": layer}
        for side, (pcb, _) in sides.items():
            if side in drawn and draw(drawn[side], layer, d / f"{side}-{layer}"):
                row[side] = f"{bid}/{side}-{layer}"
        info["layers"].append(row)
    return info


def pct(v):
    return "-" if v is None else f"{100 * v:.1f}%"


def page(out, a, boards, infos, run_info):
    e = html.escape
    parts = []
    for b, i in zip(boards, infos):
        s = i["sides"]
        cell = lambda side, k, f=str: "-" if k not in s.get(side, {}) or s[side][k] is None else f(s[side][k])
        rows = "".join(
            f"<tr><td>{label}</td><td>{cell('designer', k, f)}</td><td>{cell('ours', k, f)}</td></tr>"
            for label, k, f in (("Off 45 degrees", "off45", pct), ("Beside another net", "bundled", pct), ("Bus members beside a sibling", "bus_bundled", pct),
                                ("Vias", "vias", str), ("Track, mm", "track_mm", lambda v: f"{v:,.0f}"), ("Open connections", "open", str)))
        layers = ""
        for row in i["layers"]:
            imgs = "".join(
                f'<figure><figcaption>{side}</figcaption>' + (f'<a href="{e(row[side])}.svg"><img loading="lazy" src="{e(row[side])}.png"></a>' if side in row else '<p class="none">no tracks drawn</p>') + "</figure>"
                for side in ("designer", "ours"))
            layers += f'<h3>{e(row["layer"])}</h3><div class="pair">{imgs}</div>'
        note = f'<p class="note">{e(b["ours_note"])}</p>' if b.get("ours_note") else ""
        parts.append(f'<section id="{e(b["id"])}"><h2>{e(b["title"])}</h2><p>{e(b["why"])}</p>{note}'
                     f'<table><tr><th></th><th>Designer</th><th>Ours</th></tr>{rows}</table>{layers}</section>')
    nav = " · ".join(f'<a href="#{e(b["id"])}">{e(b["title"])}</a>' for b in boards)
    (out / "index.html").write_text(f"""<!doctype html><meta charset="utf-8"><title>Look set: {e(a.label)}</title>
<style>body{{font:15px/1.5 system-ui;margin:1.5rem auto;max-width:110rem;padding:0 1rem;background:#111;color:#ddd}}
a{{color:#9cf}}table{{border-collapse:collapse;margin:.5rem 0}}td,th{{padding:.2rem .8rem;border-bottom:1px solid #333;text-align:right}}
td:first-child{{text-align:left}}.pair{{display:grid;grid-template-columns:1fr 1fr;gap:1rem}}figure{{margin:0}}
figcaption{{text-transform:capitalize;color:#aaa}}img{{width:100%;background:#fff}}section{{border-top:1px solid #444;margin-top:2rem}}
.note,.none{{color:#ca8}}</style>
<h1>Look set: {e(a.label)}</h1>
<p>The designer's routing beside ours, one row per copper layer that carries tracks. Ours is pcbench run <code>{e(a.ours)}</code>
(commit {e(str(run_info.get('commit')))}); the designers' is run <code>{e(a.designer)}</code>. Click a picture for its SVG.
Pours are not drawn, so the tracks under them show.</p><p>{nav}</p>{''.join(parts)}""")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("ours")
    ap.add_argument("--label")
    ap.add_argument("--designer", default="designer")
    ap.add_argument("--set", default=str(ROOT / "tools/lookset.json"))
    ap.add_argument("--only")
    ap.add_argument("--jobs", type=int, default=1)
    a = ap.parse_args()
    a.label = a.label or a.ours
    boards = json.loads(Path(a.set).read_text())["boards"]
    if a.only:
        boards = [b for b in boards if b["id"] in a.only.split(",")]
    out = LAB / "planned" / "lookset" / a.label
    out.mkdir(parents=True, exist_ok=True)
    run_info = json.loads((RUNS / a.ours / "run.json").read_text())
    with ThreadPoolExecutor(a.jobs) as pool:
        infos = list(pool.map(lambda b: one(b, a, out), boards))
    (out / "lookset.json").write_text(json.dumps({"ours": a.ours, "designer": a.designer, "boards": infos}, indent=1))
    page(out, a, boards, infos, run_info)
    top = out.parent
    labels = sorted(p.name for p in top.iterdir() if (p / "index.html").exists())
    (top / "index.html").write_text("<!doctype html><meta charset=utf-8><title>Look sets</title><body style='font:15px system-ui;background:#111;color:#ddd'><h1>Look sets</h1><ul>"
                                    + "".join(f"<li><a style='color:#9cf' href='{html.escape(l)}/'>{html.escape(l)}</a></li>" for l in labels) + "</ul>")
    for b, i in zip(boards, infos):
        missing = [s for s, v in i["sides"].items() if "missing" in v]
        print(f"{b['id']}: {len(i['layers'])} layers" + (f"; missing {', '.join(missing)}" if missing else ""))
    print(f"wrote {out}/index.html")


if __name__ == "__main__":
    main()
