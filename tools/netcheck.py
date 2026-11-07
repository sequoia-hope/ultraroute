#!/usr/bin/env python3
"""Whether the designer routes a net within the board's own rules, one net at a time.

    netcheck.py BOARD_ID [--nets A,B,...] [--open RUN,...] [--out DIR]

For each net, the designer's board (PCBench's `board.kicad_pcb`) keeps only that net's tracks and
vias, and KiCad judges it as `pcbench.py` judges a route: zones refilled, only violations that
involve tracks or vias counted. A net is routed within the rules when that leaves no error and no
connection of the net open. Nets come from --nets, or from the connections left open in each
--open run's report of the board (pcbench run names), or both.

One net at a time, because KiCad names only the first net an opening bares: a drawn mask opening
over GND pads reports every other net's track crossing it as "text | Pad [GND]", which reads as
GND's fault. Judging each net alone on the bare board names it truly (SPEX_HAB Power, 2026-11).

Then with its neighbours: the net's copper and every other net's track or via within --near mm
of it on a layer they share (a via is on all), a long track cut to its stretch within reach. A
net clean alone may still fit only because its neighbours break the rules beside it, or clash
with them: hotstick's SCL is legal alone, but the designer's SDA beside it runs 0.4 mm inside
the board's edge clearance to leave it room (2026-11). Every error on that board counts against
the net: it involves the net's copper or a neighbour's within --near mm of it.
"""

import argparse
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import pcbench as P  # noqa: E402

KEEP = """
import sys, pcbnew
b = pcbnew.LoadBoard(sys.argv[1])
for t in [t for t in b.GetTracks() if t.GetNetname() != sys.argv[3]]:
    b.Remove(t)
b.Save(sys.argv[2])
"""


NEAR = """
import json, math, sys, pcbnew
b = pcbnew.LoadBoard(sys.argv[1])
net, near = sys.argv[3], float(sys.argv[4]) * 1e6


def shape(t):
    if t.GetClass() == "PCB_VIA":
        p = t.GetPosition()
        return (p.x, p.y, p.x, p.y, t.GetWidth(pcbnew.F_Cu) / 2)
    a, e = t.GetStart(), t.GetEnd()
    return (a.x, a.y, e.x, e.y, t.GetWidth() / 2)


def layer(t):
    # A track's layer; None for a via, on every layer.
    return None if t.GetClass() == "PCB_VIA" else t.GetLayer()


def shares(l, m):
    return l is None or m is None or l == m


def seg_point(s, px, py):
    ax, ay, bx, by, _ = s
    dx, dy = bx - ax, by - ay
    L = dx * dx + dy * dy
    u = 0.0 if L == 0 else max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / L))
    return math.hypot(ax + u * dx - px, ay + u * dy - py)


def gap(s, t):
    # Segments that cross are caught by their ends' distances only approximately; near enough here.
    d = min(seg_point(s, t[0], t[1]), seg_point(s, t[2], t[3]), seg_point(t, s[0], s[1]), seg_point(t, s[2], s[3]))
    return d - s[4] - t[4]


mine = [(shape(t), layer(t)) for t in b.GetTracks() if t.GetNetname() == net]
ids = [t.m_Uuid.AsString() for t in b.GetTracks() if t.GetNetname() == net]
kept = {}
for t in list(b.GetTracks()):
    if t.GetNetname() == net:
        continue
    lt = layer(t)
    d = min((gap(m, shape(t)) for m, lm in mine if shares(lm, lt)), default=math.inf)
    if d > near:
        b.Remove(t)
        continue
    kept[t.m_Uuid.AsString()] = [t.GetNetname(), round(d / 1e6, 4)]
    if t.GetClass() != "PCB_TRACK":
        continue
    # Only the stretch of a long track within reach is judged, so its errors far off stay its own.
    ax, ay, bx, by, r = shape(t)
    n = max(1, int(math.hypot(bx - ax, by - ay) / 50_000))
    inside = [k for k in range(n + 1) if min(gap(m, (ax + (bx - ax) * k / n, ay + (by - ay) * k / n, ax + (bx - ax) * k / n, ay + (by - ay) * k / n, r)) for m, lm in mine if shares(lm, lt)) <= near]
    if inside and (inside[0] > 0 or inside[-1] < n):
        k0, k1 = inside[0], inside[-1]
        t.SetStart(pcbnew.VECTOR2I(int(ax + (bx - ax) * k0 / n), int(ay + (by - ay) * k0 / n)))
        t.SetEnd(pcbnew.VECTOR2I(int(ax + (bx - ax) * k1 / n), int(ay + (by - ay) * k1 / n)))
b.Save(sys.argv[2])
json.dump({"mine": ids, "kept": kept}, open(sys.argv[5], "w"))
"""


def open_nets(board_id, runs):
    """The nets of the connections left open in each run's KiCad report of the board."""
    nets = set()
    for run in runs:
        report = P.HOME / "runs" / run / board_id / "out.drc.json"
        if not report.exists():
            print(f"{run}: no report of {board_id}", file=sys.stderr)
            continue
        for u in json.loads(report.read_text()).get("unconnected_items", []):
            for item in u["items"]:
                if m := re.search(r"\[(.*?)\]", item["description"]):
                    nets.add(m[1])
    return nets


def check(board_id, net, out):
    """(errors by type, connections of the net left open) with only the designer's copper of `net`."""
    src = P.HOME / "boards" / board_id
    d = out / re.sub(r"[^\w.-]", "_", net)
    d.mkdir(parents=True, exist_ok=True)
    pcb = d / "net.kicad_pcb"
    for suffix in (".kicad_pro", ".kicad_dru"):
        if (src / "board").with_suffix(suffix).exists():
            shutil.copy((src / "board").with_suffix(suffix), pcb.with_suffix(suffix))
    subprocess.run([sys.executable, "-c", KEEP, src / "board.kicad_pcb", pcb, net], check=True, capture_output=True)
    _, errors, _ = P.drc(pcb, d, "net")
    report = json.loads((d / "net.drc.json").read_text())
    tag = f"[{net}]"
    left = sum(1 for u in report.get("unconnected_items", []) if any(tag in i["description"] for i in u["items"]))
    return errors, left


def check_with_neighbours(board_id, net, out, near):
    """({error type: count} against `net`, {neighbour net: [error types]}) with the designer's copper
    of `net` and of every other net within `near` mm of it."""
    src = P.HOME / "boards" / board_id
    d = out / re.sub(r"[^\w.-]", "_", net)
    d.mkdir(parents=True, exist_ok=True)
    pcb = d / "near.kicad_pcb"
    for suffix in (".kicad_pro", ".kicad_dru"):
        if (src / "board").with_suffix(suffix).exists():
            shutil.copy((src / "board").with_suffix(suffix), pcb.with_suffix(suffix))
    items = d / "near.items.json"
    subprocess.run([sys.executable, "-c", NEAR, src / "board.kicad_pcb", pcb, net, str(near), items], check=True, capture_output=True)
    kept = json.loads(items.read_text())
    mine, theirs = set(kept["mine"]), kept["kept"]
    P.drc(pcb, d, "near")
    report = json.loads((d / "near.drc.json").read_text())
    errors, blamed = {}, {}
    for v in report.get("violations", []):
        if v["severity"] != "error" or not P.routed(v):
            continue
        ids = [i.get("uuid") for i in v.get("items", [])]
        if not any(i in mine or i in theirs for i in ids):
            continue
        errors[v["type"]] = errors.get(v["type"], 0) + 1
        for i in ids:
            if i in theirs:
                blamed.setdefault(theirs[i][0], set()).add(v["type"])
    return errors, {n: sorted(t) for n, t in blamed.items()}


def main():
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("board")
    a.add_argument("--nets", default="", help="comma-separated net names")
    a.add_argument("--open", default="", help="comma-separated pcbench runs whose open connections name the nets")
    a.add_argument("--near", type=float, default=1.0, help="mm round the net within which other nets' copper is judged with it (default 1)")
    a.add_argument("--out", type=Path, help="where to keep the per-net boards and reports (default: runs/netcheck/BOARD)")
    a = a.parse_args()
    nets = {n for n in a.nets.split(",") if n} | open_nets(a.board, [r for r in a.open.split(",") if r])
    out = a.out or P.HOME / "runs" / "netcheck" / a.board
    results = {}
    for net in sorted(nets):
        try:
            errors, left = check(a.board, net, out)
        except (P.RefereeError, subprocess.CalledProcessError) as e:
            print(f"{net}: not judged: {e}", flush=True)
            continue
        alone = not errors and left == 0
        together, blamed = {}, {}
        if alone:
            try:
                together, blamed = check_with_neighbours(a.board, net, out, a.near)
            except (P.RefereeError, subprocess.CalledProcessError) as e:
                print(f"{net}: with neighbours not judged: {e}", flush=True)
                together = {"not judged": 1}
        clean = alone and not together
        results[net] = {"clean": clean, "alone": alone, "errors": errors, "open": left, "with_neighbours": together, "neighbours_blamed": blamed}
        if clean:
            why = "within the rules, with its neighbours too"
        elif alone:
            why = "within the rules alone; with its neighbours " + ", ".join(f"{n} {t}" for t, n in sorted(together.items()))
            why += "".join(f"; {n}: {', '.join(ts)}" for n, ts in sorted(blamed.items()))
        else:
            why = ", ".join([f"{n} {t}" for t, n in sorted(errors.items())] + ([f"{left} open"] if left else []))
        print(f"{net}: {why}", flush=True)
    out.mkdir(parents=True, exist_ok=True)
    (out / "netcheck.json").write_text(json.dumps(results, indent=1))
    print(f"{a.board}: {sum(r['clean'] for r in results.values())} of {len(results)} nets routed by the designer within the rules")


if __name__ == "__main__":
    main()
