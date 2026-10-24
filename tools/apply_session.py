#!/usr/bin/env python3
"""Apply a routed session to a KiCad board, the way copperroute reads it.

A Specctra session can only add copper. When copperroute bends or retires the design's fixed
copper, it writes `<session>.superseded.json` beside the session, listing the fixed tracks and
vias the session replaces. This removes those from the board first, then imports the session,
refills the zones and saves a copy (the source board is never written).

    apply_session.py BOARD.kicad_pcb SESSION.ses OUT.kicad_pcb [--dsn BOARD.dsn [--from-dsn]] [--outline OUT.outline.json] [--drc REPORT.json]
"""
import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

import pcbnew

TOLERANCE_NM = 1000

UNIT_NM = {"um": 1000.0, "mm": 1_000_000.0, "mil": 25_400.0, "inch": 25_400_000.0, "cm": 10_000_000.0}


def dsn_unit_nm(dsn):
    if dsn is None:
        return UNIT_NM["um"]
    match = re.search(r"\(unit\s+(\w+)\)", Path(dsn).read_text(errors="replace"))
    return UNIT_NM[match.group(1).lower()] if match else UNIT_NM["um"]


def to_kicad(point, unit_nm):
    """Specctra's y axis points up, KiCad's down."""
    return (round(point[0] * unit_nm), round(-point[1] * unit_nm))


def near(a, b):
    return abs(a[0] - b[0]) <= TOLERANCE_NM and abs(a[1] - b[1]) <= TOLERANCE_NM


def remove_superseded(board, superseded, unit_nm):
    tracks = list(board.GetTracks())
    doomed, missing = {}, []
    for entry in superseded:
        points = [to_kicad(p, unit_nm) for p in entry["dsn_points"]]
        nets = set(entry["nets"])
        if entry["layer"] is None:
            found = [t for t in tracks if t.GetClass() == "PCB_VIA" and t.GetNetname() in nets
                     and near((t.GetPosition().x, t.GetPosition().y), points[0])]
        else:
            layer = board.GetLayerID(entry["layer"])
            pairs = list(zip(points, points[1:]))
            found = []
            for t in tracks:
                if t.GetClass() != "PCB_TRACK" or t.GetLayer() != layer or t.GetNetname() not in nets:
                    continue
                ends = ((t.GetStart().x, t.GetStart().y), (t.GetEnd().x, t.GetEnd().y))
                if any((near(ends[0], a) and near(ends[1], b)) or (near(ends[0], b) and near(ends[1], a))
                       for a, b in pairs):
                    found.append(t)
        if found:
            doomed.update((t.m_Uuid.AsString(), t) for t in found)
        else:
            missing.append(entry)
    for t in doomed.values():
        board.Delete(t)
    return len(doomed), missing


def dsn_wiring(dsn, unit_nm):
    """The DSN's wiring: wires as (layer, width, net, points) and vias as (padstack, centre, net),
    in KiCad nanometres."""
    text = Path(dsn).read_text(errors="replace")
    wiring = text[text.index("(wiring"):] if "(wiring" in text else ""
    wires, vias = [], []
    for m in re.finditer(r'\(wire\s*\(path\s+(\S+)\s+([-\d.]+)\s+([-\d.\s]+)\)\s*\(net\s+("[^"]*"|[^)\s]+)\)', wiring):
        values = list(map(float, m.group(3).split()))
        points = [to_kicad(p, unit_nm) for p in zip(values[::2], values[1::2])]
        wires.append((m.group(1).strip('"'), round(float(m.group(2)) * unit_nm), m.group(4).strip('"'), points))
    for m in re.finditer(r'\(via\s+("[^"]*"|\S+)\s+([-\d.]+)\s+([-\d.]+)\s*\(net\s+("[^"]*"|[^)\s]+)\)', wiring):
        centre = to_kicad((float(m.group(2)), float(m.group(3))), unit_nm)
        vias.append((m.group(1).strip('"'), centre, m.group(4).strip('"')))
    return wires, vias


def via_size(padstack, unit_nm):
    """KiCad's DSN export names via padstacks `Via[0-5]_<diameter>:<drill>_um`."""
    m = re.search(r"_([\d.]+):([\d.]+)_um", padstack)
    if not m:
        sys.exit(f"can't read a via size from padstack {padstack}")
    return round(float(m.group(1)) * 1000), round(float(m.group(2)) * 1000)


def rebuild_from_dsn(board, dsn, unit_nm):
    """Replaces the board's tracks and vias with the DSN's wiring, locked, so the board holds
    exactly what the router was given."""
    for t in list(board.GetTracks()):
        board.Delete(t)
    wires, vias = dsn_wiring(dsn, unit_nm)
    nets = board.GetNetsByName()
    for layer, width, net, points in wires:
        for a, b in zip(points, points[1:]):
            t = pcbnew.PCB_TRACK(board)
            t.SetStart(pcbnew.VECTOR2I(*a))
            t.SetEnd(pcbnew.VECTOR2I(*b))
            t.SetWidth(width)
            t.SetLayer(board.GetLayerID(layer))
            t.SetNet(nets[net])
            t.SetLocked(True)
            board.Add(t)
    for padstack, centre, net in vias:
        diameter, drill = via_size(padstack, unit_nm)
        v = pcbnew.PCB_VIA(board)
        v.SetPosition(pcbnew.VECTOR2I(*centre))
        v.SetWidth(diameter)
        v.SetDrill(drill)
        v.SetLayerPair(pcbnew.F_Cu, pcbnew.B_Cu)
        v.SetNet(nets[net])
        v.SetLocked(True)
        board.Add(v)
    return sum(len(w[3]) - 1 for w in wires), len(vias)


def replace_outline(board, outline_mm):
    """Replaces the board's Edge.Cuts drawings with one closed polygon (DSN millimetres)."""
    edge = board.GetLayerID("Edge.Cuts")
    for drawing in list(board.GetDrawings()):
        if drawing.GetLayer() == edge:
            board.Delete(drawing)
    points = [(round(x * 1_000_000), round(-y * 1_000_000)) for x, y in outline_mm]
    for a, b in zip(points, points[1:] + points[:1]):
        line = pcbnew.PCB_SHAPE(board, pcbnew.SHAPE_T_SEGMENT)
        line.SetStart(pcbnew.VECTOR2I(*a))
        line.SetEnd(pcbnew.VECTOR2I(*b))
        line.SetLayer(edge)
        line.SetWidth(50_000)
        board.Add(line)
    return len(points)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("board")
    ap.add_argument("session")
    ap.add_argument("out")
    ap.add_argument("--dsn", help="the DSN the session was routed from, for its units (default um)")
    ap.add_argument("--from-dsn", action="store_true",
                    help="first replace the board's tracks and vias with the DSN's wiring (needs --dsn)")
    ap.add_argument("--outline", help="a reshape .outline.json: replace Edge.Cuts with its outline_mm first")
    ap.add_argument("--drc", help="run kicad-cli DRC on the result and write its JSON report here")
    args = ap.parse_args()

    out = Path(args.out).resolve()
    if out == Path(args.board).resolve():
        sys.exit("refusing to overwrite the source board")
    board = pcbnew.LoadBoard(args.board)
    if args.from_dsn:
        if not args.dsn:
            sys.exit("--from-dsn needs --dsn")
        segments, vias = rebuild_from_dsn(board, args.dsn, dsn_unit_nm(args.dsn))
        print(f"rebuilt the board's copper from the DSN: {segments} segments, {vias} vias")
    if args.outline:
        n = replace_outline(board, json.loads(Path(args.outline).read_text())["outline_mm"])
        print(f"replaced the board outline with {n} points")
    sidecar = Path(args.session).with_suffix(".superseded.json")
    if sidecar.exists():
        superseded = json.loads(sidecar.read_text())
        removed, missing = remove_superseded(board, superseded, dsn_unit_nm(args.dsn))
        print(f"removed {removed} KiCad track/via item(s) for {len(superseded)} superseded entries")
        if missing:
            sys.exit(f"{len(missing)} superseded entries match nothing on the board, first: {missing[0]}")
    if not pcbnew.ImportSpecctraSES(board, str(Path(args.session).resolve())):
        sys.exit(f"KiCad could not import {args.session}")
    board.BuildConnectivity()
    pcbnew.ZONE_FILLER(board).Fill(board.Zones())
    board.Save(str(out))
    print(f"wrote {out}")
    if args.drc:
        subprocess.run(["kicad-cli", "pcb", "drc", "--format", "json", "--severity-all",
                        "-o", args.drc, str(out)], check=True)
        report = json.loads(Path(args.drc).read_text())
        print(f"KiCad DRC: {len(report.get('violations', []))} violations, "
              f"{len(report.get('unconnected_items', []))} unconnected")


if __name__ == "__main__":
    main()
