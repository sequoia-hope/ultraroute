#!/usr/bin/env python3
"""Manufacturing checks on a routed board's tracks, per net and copper layer:

  acute   tracks of a net meeting end to end at an angle under 90 degrees, away from pads: an acid
          trap in the inside corner
  slot    places where a net's copper comes within the board's clearance of itself without
          joining, a slot too narrow to etch cleanly (an acid trap): found by closing the net's
          copper by the clearance and keeping what the closing fills that a track borders, deep
          enough not to be the rounding of an ordinary bend's inside corner

    dfm.py BOARD.kicad_pcb [NET,...] [--clearance MM] [--list] [--json]

Prints the counts, and with --list each place; with --json, {"acute": N, "slots": N} alone.
PCBench's judge counts both on every routed board (`pcbench.py`, JUDGE 3), apart from the
verdict, as it counts necks. KiCad's own geometry (pcbnew) does the work, nothing else.
"""
import argparse
import json
import math
from collections import defaultdict

import pcbnew

MM = 1e6
ERR = 1000  # nm a polygonised arc may stray inward


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("board")
    ap.add_argument("nets", nargs="?", default="")
    ap.add_argument("--clearance", type=float, default=None)
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()
    b = pcbnew.LoadBoard(a.board)
    only = set(a.nets.split(",")) if a.nets else None
    gap_mm = a.clearance if a.clearance is not None else pcbnew.ToMM(b.GetDesignSettings().m_MinClearance)
    gap = int(max(gap_mm, 0.1) * MM)
    tracks = defaultdict(list)  # (net, layer) -> [track]
    copper = defaultdict(pcbnew.SHAPE_POLY_SET)
    pads = defaultdict(list)
    for t in b.GetTracks():
        net = t.GetNetname()
        if only and net not in only:
            continue
        if t.GetClass() == "PCB_VIA":
            for layer in (pcbnew.F_Cu, pcbnew.B_Cu):
                t.TransformShapeToPolygon(copper[(net, layer)], layer, 0, ERR, pcbnew.ERROR_INSIDE)
        elif t.GetClass() == "PCB_TRACK":
            tracks[(net, t.GetLayer())].append(t)
            t.TransformShapeToPolygon(copper[(net, t.GetLayer())], t.GetLayer(), 0, ERR, pcbnew.ERROR_INSIDE)
    for fp in b.Footprints():
        for p in fp.Pads():
            net = p.GetNetname()
            if only and net not in only:
                continue
            for layer in (pcbnew.F_Cu, pcbnew.B_Cu):
                if p.IsOnLayer(layer):
                    p.TransformShapeToPolygon(copper[(net, layer)], layer, 0, ERR, pcbnew.ERROR_INSIDE)
                    pads[(net, layer)].append(p.GetEffectivePolygon(layer))
    acute, slots = [], []
    for key, ts in tracks.items():
        net, layer = key
        # Acute angles where tracks of the net meet end to end, away from pads.
        ends = defaultdict(list)
        for t in ts:
            s, e = t.GetStart(), t.GetEnd()
            if s == e:
                continue
            ends[(s.x, s.y)].append((e.x - s.x, e.y - s.y))
            ends[(e.x, e.y)].append((s.x - e.x, s.y - e.y))
        for (x, y), dirs in ends.items():
            if len(dirs) < 2 or any(poly.Contains(pcbnew.VECTOR2I(x, y)) for poly in pads[key]):
                continue
            angles = sorted(math.atan2(d[1], d[0]) for d in dirs)
            least = min((angles[(i + 1) % len(angles)] - angles[i]) % (2 * math.pi) for i in range(len(angles))) * 180 / math.pi
            if least < 89.5:
                acute.append((net, b.GetLayerName(layer), (round(x / MM, 4), round(y / MM, 4)), round(least, 1)))
        # Slots: what closing the net's copper by the clearance fills, bordered by a track.
        whole = copper[key].CloneDropTriangulation()
        whole.Simplify()
        closed = whole.CloneDropTriangulation()
        closed.Inflate(gap // 2, pcbnew.CORNER_STRATEGY_ROUND_ALL_CORNERS, ERR)
        closed.Deflate(gap // 2, pcbnew.CORNER_STRATEGY_ROUND_ALL_CORNERS, ERR)
        closed.BooleanSubtract(whole)
        border = pcbnew.SHAPE_POLY_SET()
        for t in ts:
            t.TransformShapeToPolygon(border, layer, 2 * ERR, ERR, pcbnew.ERROR_OUTSIDE)
        border.Simplify()
        for i in range(closed.OutlineCount()):
            piece = pcbnew.SHAPE_POLY_SET()
            piece.AddOutline(closed.Outline(i))
            # The rounding the closing puts in an ordinary bend's inside corner is no slot: keep only
            # pieces holding a disc a quarter of the clearance wide (a right angle's holds 17%).
            deep = piece.CloneDropTriangulation()
            deep.Deflate(gap // 8, pcbnew.CORNER_STRATEGY_ROUND_ALL_CORNERS, ERR)
            if deep.OutlineCount() == 0 or deep.Area() <= 0:
                continue
            touch = piece.CloneDropTriangulation()
            touch.BooleanIntersection(border)
            if touch.OutlineCount() == 0:
                continue
            c = piece.Outline(0).Centre()
            slots.append((net, b.GetLayerName(layer), (round(c.x / MM, 3), round(c.y / MM, 3)), round(piece.Area() / 1e6)))
    if a.json:
        print(json.dumps({"acute": len(acute), "slots": len(slots)}))
        return
    print(f"acute {len(acute)} slot {len(slots)} (clearance {gap / MM} mm)")
    if a.list:
        for x in acute:
            print("  acute", *x)
        for x in slots:
            print("  slot ", *x, "um2")


if __name__ == "__main__":
    main()
