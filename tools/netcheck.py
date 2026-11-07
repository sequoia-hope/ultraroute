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


def main():
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("board")
    a.add_argument("--nets", default="", help="comma-separated net names")
    a.add_argument("--open", default="", help="comma-separated pcbench runs whose open connections name the nets")
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
        clean = not errors and left == 0
        results[net] = {"clean": clean, "errors": errors, "open": left}
        why = "within the rules" if clean else ", ".join([f"{n} {t}" for t, n in sorted(errors.items())] + ([f"{left} open"] if left else []))
        print(f"{net}: {why}", flush=True)
    out.mkdir(parents=True, exist_ok=True)
    (out / "netcheck.json").write_text(json.dumps(results, indent=1))
    print(f"{a.board}: {sum(r['clean'] for r in results.values())} of {len(results)} nets routed by the designer within the rules")


if __name__ == "__main__":
    main()
