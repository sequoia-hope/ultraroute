#!/usr/bin/env python3
"""How tidy a routed board's tracks are, apart from whether they are legal:

  turns     how much the tracks turn where two of a net's tracks meet away from pads and vias, in
            right angles per 10 mm of track (a chamfered corner is two half turns, one right angle)
  off45     the share of straight track length not at a multiple of 45 degrees
  doglegs   short sideways steps: a straight piece under DOGLEG mm whose tracks on either side run
            on in the same direction, the track stepping over by half its width or more and
            carrying on
  detour    track length over the shortest 45-degree tree joining each net's pads, for the nets
            no pour of their own helps join (1.00 is as short as can be)
  bundled   the share of straight track length running alongside another net's track on the same
            layer, parallel and at most BESIDE times the board's clearance apart, edge to edge
  bus       nets running between the same two parts from neighbouring pins at both ends (three or
            more side by side: a bus); bus_bundled is the share of their straight length running
            alongside another net of the same bus, as bundled measures it

  layers    layer coherence: over the copper layers that carry tracks, the share of the length
            running across or up-and-down that runs along its layer's own axis (the one of the
            two that carries more there; 45-degree pieces are left out). same_axis counts the
            neighbouring pairs of such layers whose axes are the same, of layer_pairs
  crossings places where a track crosses another net's track on a different layer, per 100 mm of
            track: what the layers are for, so fewer says nothing; a watch number beside vias

    tidy.py BOARD.kicad_pcb [--clearance MM] [--list] [--json] [--svg OUT.svg]

Prints the numbers, and with --list each dogleg and the buses; with --svg, draws the tracks
coloured by what it found (bundled beside their bus, beside another net, off 45 degrees); with --json, {"tidy": {...}}
alone, the raw sums beside the ratios so a set of boards can be added up (`pcbench.py`, JUDGE 4,
counts them on every routed board, apart from the verdict). Run on a board as its designer
routed it, it scores the designer: the measure to beat on a board with room.
"""
import argparse
import json
import math
from collections import defaultdict
from pathlib import Path

import pcbnew

MM = 1e6
SNAP = 1000  # nm: ends this close are one joint
DOGLEG = 1.0
BESIDE = 3.0
PARALLEL = math.radians(1.0)
STRAIGHT = math.radians(5.0)


def heading(dx, dy):
    return math.atan2(dy, dx)


def turn(a, b):
    """The angle from heading a to heading b, in [0, pi]."""
    d = abs(a - b) % (2 * math.pi)
    return min(d, 2 * math.pi - d)


def octile(p, q):
    dx, dy = abs(p[0] - q[0]), abs(p[1] - q[1])
    return max(dx, dy) + (math.sqrt(2) - 1) * min(dx, dy)


def mst(points):
    """Length of the minimum spanning tree of the points under the 45-degree (octile) metric."""
    if len(points) < 2:
        return 0.0
    best = {i: octile(points[0], p) for i, p in enumerate(points[1:], 1)}
    total = 0.0
    while best:
        i = min(best, key=best.get)
        total += best.pop(i)
        for j in best:
            best[j] = min(best[j], octile(points[i], points[j]))
    return total


def buses(b):
    """{net: bus number}: nets with one pad on each of the same two footprints, whose pads are
    neighbours on both, grouped where three or more lie side by side."""
    by_pair = defaultdict(list)
    for net, pads in pads_of(b).items():
        fps = defaultdict(list)
        for fp, p in pads:
            fps[fp].append(p)
        if len(pads) != 2 or len(fps) != 2:
            continue
        (fa, pa), (fb, pb) = sorted((fp, ps[0]) for fp, ps in fps.items())
        by_pair[(fa, fb)].append((net, pa, pb))
    pitch = {}
    for fp in b.Footprints():
        cs = [p.GetCenter() for p in fp.Pads()]
        gaps = [math.hypot(c.x - d.x, c.y - d.y) for i, c in enumerate(cs) for d in cs[i + 1:]]
        gaps = [g for g in gaps if g > 0]
        pitch[fp.GetReference() + "@" + str(fp.m_Uuid.AsString())] = min(gaps) if gaps else 0
    near = lambda fp, p, q: math.hypot(p.x - q.x, p.y - q.y) <= 1.5 * pitch[fp]
    group, n = {}, 0
    for (fa, fb), nets in by_pair.items():
        if len(nets) < 3:
            continue
        # Join neighbours at both ends, then keep the groups of three or more.
        parent = list(range(len(nets)))
        find = lambda i: i if parent[i] == i else find(parent[i])
        for i, (_, pa, pb) in enumerate(nets):
            for j in range(i + 1, len(nets)):
                if near(fa, pa, nets[j][1]) and near(fb, pb, nets[j][2]):
                    parent[find(i)] = find(j)
        sets = defaultdict(list)
        for i in range(len(nets)):
            sets[find(i)].append(nets[i][0])
        for members in sets.values():
            if len(members) >= 3:
                for net in members:
                    group[net] = n
                n += 1
    return group


def pads_of(b):
    pads = defaultdict(list)
    for fp in b.Footprints():
        key = fp.GetReference() + "@" + str(fp.m_Uuid.AsString())
        for p in fp.Pads():
            if p.GetNetname():
                pads[p.GetNetname()].append((key, p.GetCenter()))
    return pads


def bundled(segs, gap_of, group):
    """(straight length, length alongside another net's track, length of bus nets, length of bus
    nets alongside their own bus, each segment's (alongside, alongside its bus)) over the straight
    segments (net, layer, (x0, y0), (x1, y1), w)."""
    cell = int(2 * MM)
    grid = defaultdict(list)
    for i, (_, layer, a, z, _) in enumerate(segs):
        for cx in range(min(a[0], z[0]) // cell - 1, max(a[0], z[0]) // cell + 2):
            for cy in range(min(a[1], z[1]) // cell - 1, max(a[1], z[1]) // cell + 2):
                grid[(layer, cx, cy)].append(i)
    total = beside = bus_total = bus_beside = 0.0
    each = []
    for i, (net, layer, a, z, w) in enumerate(segs):
        length = math.hypot(z[0] - a[0], z[1] - a[1])
        ux, uy = (z[0] - a[0]) / length, (z[1] - a[1]) / length
        near = set()
        for cx in range(min(a[0], z[0]) // cell, max(a[0], z[0]) // cell + 1):
            for cy in range(min(a[1], z[1]) // cell, max(a[1], z[1]) // cell + 1):
                near.update(grid[(layer, cx, cy)])
        spans, own = [], []
        for j in near:
            onet, _, b0, b1, ow = segs[j]
            if onet == net:
                continue
            ol = math.hypot(b1[0] - b0[0], b1[1] - b0[1])
            if abs(ux * (b1[1] - b0[1]) - uy * (b1[0] - b0[0])) > math.sin(PARALLEL) * ol:
                continue
            side = abs(ux * (b0[1] - a[1]) - uy * (b0[0] - a[0]))
            if side - (w + ow) / 2 > BESIDE * gap_of:
                continue
            p0 = ux * (b0[0] - a[0]) + uy * (b0[1] - a[1])
            p1 = ux * (b1[0] - a[0]) + uy * (b1[1] - a[1])
            lo, hi = max(0.0, min(p0, p1)), min(length, max(p0, p1))
            if hi > lo:
                spans.append((lo, hi))
                if net in group and group.get(onet) == group[net]:
                    own.append((lo, hi))
        total += length
        beside += covered(spans)
        each.append((covered(spans), covered(own)))
        if net in group:
            bus_total += length
            bus_beside += covered(own)
    return total, beside, bus_total, bus_beside, each


def layer_axes(b, segs):
    """(length along each layer's own axis, length across or up-and-down at all, pairs of neighbouring track layers, pairs with the same
    axis, {layer name: [across, up-and-down, diagonal or other] mm}) over the straight segments."""
    by = defaultdict(lambda: [0.0, 0.0, 0.0])
    for _, layer, a, z, _ in segs:
        d = math.hypot(z[0] - a[0], z[1] - a[1])
        h = math.degrees(heading(z[0] - a[0], z[1] - a[1])) % 180
        by[layer][0 if min(h, 180 - h) < 22.5 else 1 if abs(h - 90) < 22.5 else 2] += d
    order = sorted(by, key=lambda l: (l == pcbnew.B_Cu, l != pcbnew.F_Cu, l))
    axis = {l: 0 if by[l][0] >= by[l][1] else 1 for l in order}
    along = sum(by[l][axis[l]] for l in order)
    same = sum(axis[p] == axis[q] for p, q in zip(order, order[1:]))
    return along, sum(by[l][0] + by[l][1] for l in order), max(len(order) - 1, 0), same, {b.GetLayerName(l): [round(x / MM, 3) for x in by[l]] for l in order}


def crossings(segs):
    """How many times a straight segment crosses another net's segment on a different layer."""
    cell = int(2 * MM)
    grid = defaultdict(list)
    for i, (_, _, a, z, _) in enumerate(segs):
        for cx in range(min(a[0], z[0]) // cell, max(a[0], z[0]) // cell + 1):
            for cy in range(min(a[1], z[1]) // cell, max(a[1], z[1]) // cell + 1):
                grid[(cx, cy)].append(i)
    side = lambda p, q, r: (q[0] - p[0]) * (r[1] - p[1]) - (q[1] - p[1]) * (r[0] - p[0])
    seen = set()
    for ids in grid.values():
        for x, i in enumerate(ids):
            ni, li, a, z, _ = segs[i]
            for j in ids[x + 1:]:
                nj, lj, c, d, _ = segs[j]
                if ni == nj or li == lj or (i, j) in seen:
                    continue
                if side(a, z, c) * side(a, z, d) < 0 and side(c, d, a) * side(c, d, z) < 0:
                    seen.add((i, j))
    return len(seen)


def covered(spans):
    """Length of the union of the intervals."""
    out, end = 0.0, -math.inf
    for lo, hi in sorted(spans):
        if hi > end:
            out += hi - max(lo, end)
            end = hi
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("board")
    ap.add_argument("--clearance", type=float, default=None)
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--svg", help="draw the tracks to this file, coloured by what tidy.py finds")
    a = ap.parse_args()
    b = pcbnew.LoadBoard(a.board)
    gap_mm = a.clearance if a.clearance is not None else pcbnew.ToMM(b.GetDesignSettings().m_MinClearance)
    gap = max(gap_mm, 0.1) * MM
    snap = lambda p: (round(p.x / SNAP), round(p.y / SNAP))
    # Each track as its pieces: a straight track one, an arc two chords through its middle, so the
    # chords' turn and the joints' add up to the arc's own.
    pieces = defaultdict(list)  # (net, layer) -> [(start, end, width, straight)]
    segs = []
    length = defaultdict(float)  # net -> track length, nm
    off = straight_len = 0.0
    for t in b.GetTracks():
        cls = t.GetClass()
        if cls not in ("PCB_TRACK", "PCB_ARC"):
            continue
        net, layer, w = t.GetNetname(), t.GetLayer(), t.GetWidth()
        s, e = t.GetStart(), t.GetEnd()
        if cls == "PCB_ARC":
            m = t.GetMid()
            pieces[(net, layer)] += [(snap(s), snap(m), w, False), (snap(m), snap(e), w, False)]
            length[net] += t.GetLength()
            continue
        if s == e:
            continue
        pieces[(net, layer)].append((snap(s), snap(e), w, True))
        d = math.hypot(e.x - s.x, e.y - s.y)
        length[net] += d
        straight_len += d
        h = math.degrees(heading(e.x - s.x, e.y - s.y)) % 45
        if min(h, 45 - h) > 0.5:
            off += d
        segs.append((net, layer, (s.x, s.y), (e.x, e.y), w))
    held = set()  # joints in a pad or a via: the tracks there turn to meet them, not to wander
    for t in b.GetTracks():
        if t.GetClass() == "PCB_VIA":
            held.add(snap(t.GetPosition()))
    pad_shapes = defaultdict(list)
    for fp in b.Footprints():
        for p in fp.Pads():
            for layer in (pcbnew.F_Cu, pcbnew.B_Cu):
                if p.IsOnLayer(layer):
                    pad_shapes[(p.GetNetname(), layer)].append(p.GetEffectivePolygon(layer))
    turned = 0.0
    doglegs = []
    for key, ps in pieces.items():
        ends = defaultdict(list)  # joint -> [(piece index, heading away from the joint)]
        for i, (s, e, _, _) in enumerate(ps):
            ends[s].append((i, heading(e[0] - s[0], e[1] - s[1])))
            ends[e].append((i, heading(s[0] - e[0], s[1] - e[1])))
        free = {}
        for at, arms in ends.items():
            if len(arms) != 2 or at in held:
                continue
            if any(poly.Contains(pcbnew.VECTOR2I(at[0] * SNAP, at[1] * SNAP)) for poly in pad_shapes[key]):
                continue
            # Away along one arm, back along the other: a straight run turns by pi less their angle.
            bend = math.pi - turn(arms[0][1], arms[1][1])
            turned += bend
            free[at] = (arms, bend)
        # A dogleg: a short straight piece, both ends free bends, whose neighbours run the same way.
        for i, (s, e, w, is_straight) in enumerate(ps):
            if not is_straight or s not in free or e not in free:
                continue
            if math.hypot(e[0] - s[0], e[1] - s[1]) * SNAP > DOGLEG * MM:
                continue
            (arms_s, bend_s), (arms_e, bend_e) = free[s], free[e]
            if bend_s < STRAIGHT or bend_e < STRAIGHT:
                continue
            before = next(h for j, h in arms_s if j != i)  # heading away from s, along the track before
            after = next(h for j, h in arms_e if j != i)
            # Running on the same way (the track before, reversed, heads as the track after), and
            # stepped over by half a track width or more: a smaller step doesn't show.
            step = abs(math.cos(after) * (e[1] - s[1]) - math.sin(after) * (e[0] - s[0])) * SNAP
            if turn(before + math.pi, after) < STRAIGHT and step >= w / 2:
                doglegs.append((key[0], b.GetLayerName(key[1]), (round(s[0] * SNAP / MM, 3), round(s[1] * SNAP / MM, 3))))
    poured = {z.GetNetname() for z in b.Zones() if not z.GetIsRuleArea()}
    tree = routed = 0.0
    for net, pads in pads_of(b).items():
        if net in poured or not length.get(net) or len(pads) < 2:
            continue
        tree += mst([(c.x, c.y) for _, c in pads])
        routed += length[net]
    group = buses(b)
    total, beside, bus_total, bus_beside, each = bundled(segs, gap, group)
    all_len = sum(length.values())
    along, squared, pairs, same, per_layer = layer_axes(b, segs)
    out = {
        "track_mm": round(all_len / MM, 3),
        "straight_mm": round(straight_len / MM, 3),
        "turned_quarters": round(turned / (math.pi / 2), 2),
        "off45_mm": round(off / MM, 3),
        "doglegs": len(doglegs),
        "detour_routed_mm": round(routed / MM, 3),
        "detour_tree_mm": round(tree / MM, 3),
        "bundled_mm": round(beside / MM, 3),
        "bus_nets": len(group),
        "bus_mm": round(bus_total / MM, 3),
        "bus_bundled_mm": round(bus_beside / MM, 3),
        "axis_mm": round(along / MM, 3),
        "axis_of_mm": round(squared / MM, 3),
        "crossed_mm": round(all_len / MM, 3),
        "track_layers": len(per_layer),
        "layer_pairs": pairs,
        "same_axis": same,
        "crossings": crossings(segs),
        "per_layer_mm": per_layer,
    }
    out.update(ratios(out))
    if a.svg:
        draw(a.svg, b, pieces, segs, each, doglegs, group)
    if a.json:
        print(json.dumps({"tidy": out}))
        return
    print(" ".join(f"{k} {v}" for k, v in ratios(out).items()) + f" (doglegs {len(doglegs)}, bus nets {len(group)}, clearance {gap / MM} mm)")
    if a.list:
        for x in doglegs:
            print("  dogleg", *x)
        members = defaultdict(list)
        for net, g in group.items():
            members[g].append(net)
        for g in sorted(members):
            print(f"  bus {g}:", " ".join(sorted(members[g])))


def draw(path, b, pieces, segs, each, doglegs, group):
    """An SVG of the board's tracks: green bundled beside their own bus, blue bundled beside another
    net, orange off 45 degrees, grey the rest (arcs too); red rings the doglegs, pads in outline."""
    box = b.GetBoardEdgesBoundingBox()
    if box.GetWidth() == 0:
        box = b.ComputeBoundingBox(False)
    x0, y0, w, h = box.GetX() / MM, box.GetY() / MM, box.GetWidth() / MM, box.GetHeight() / MM
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{x0 - 1:.3f} {y0 - 1:.3f} {w + 2:.3f} {h + 2:.3f}" style="background:#111">']
    for fp in b.Footprints():
        for p in fp.Pads():
            c, sz = p.GetCenter(), p.GetBoundingBox()
            out.append(f'<rect x="{sz.GetX() / MM:.3f}" y="{sz.GetY() / MM:.3f}" width="{sz.GetWidth() / MM:.3f}" height="{sz.GetHeight() / MM:.3f}" fill="none" stroke="#666" stroke-width="0.03"/>')
    line = lambda a, z, wd, col, op: f'<line x1="{a[0] / MM:.4f}" y1="{a[1] / MM:.4f}" x2="{z[0] / MM:.4f}" y2="{z[1] / MM:.4f}" stroke="{col}" stroke-width="{wd / MM:.4f}" stroke-linecap="round" opacity="{op}"/>'
    back = {pcbnew.F_Cu: 1.0}
    for (net, layer), ps in pieces.items():
        for s, e, wd, straight in ps:
            if not straight:
                out.append(line((s[0] * SNAP, s[1] * SNAP), (e[0] * SNAP, e[1] * SNAP), wd, "#999", back.get(layer, 0.55)))
    for (net, layer, a, z, wd), (beside, own) in zip(segs, each):
        length = math.hypot(z[0] - a[0], z[1] - a[1])
        hd = math.degrees(heading(z[0] - a[0], z[1] - a[1])) % 45
        col = "#3c3" if own > length / 2 else "#39f" if beside > length / 2 else "#f93" if min(hd, 45 - hd) > 0.5 else "#999"
        out.append(line(a, z, wd, col, back.get(layer, 0.55)))
    for t in b.GetTracks():
        if t.GetClass() == "PCB_VIA":
            c = t.GetPosition()
            out.append(f'<circle cx="{c.x / MM:.3f}" cy="{c.y / MM:.3f}" r="{t.GetWidth(pcbnew.F_Cu) / MM / 2:.3f}" fill="#777"/>')
    for _, _, (x, y) in doglegs:
        out.append(f'<circle cx="{x}" cy="{y}" r="0.35" fill="none" stroke="#f33" stroke-width="0.08"/>')
    out.append("</svg>")
    Path(path).write_text("\n".join(out))


def ratios(t):
    """The ratios from a sum of tidy.py's raw numbers (one board's, or a set's added up)."""
    per = lambda x, y, k=1.0: round(k * x / y, 3) if y else None
    return {
        "turns": per(t["turned_quarters"], t["track_mm"], 10),
        "off45": per(t["off45_mm"], t["straight_mm"]),
        "doglegs_per_m": per(t["doglegs"], t["track_mm"], 1000),
        "detour": per(t["detour_routed_mm"], t["detour_tree_mm"]),
        "bundled": per(t["bundled_mm"], t["straight_mm"]),
        "bus_bundled": per(t["bus_bundled_mm"], t["bus_mm"]),
        # Absent from boards scored before these were counted.
        "coherence": per(t.get("axis_mm", 0), t.get("axis_of_mm", 0)),
        "same_axis": per(t.get("same_axis", 0), t.get("layer_pairs", 0)),
        "crossings_per_100mm": per(t.get("crossings", 0), t.get("crossed_mm", 0), 100),
    }


if __name__ == "__main__":
    main()
