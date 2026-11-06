#!/usr/bin/env python3
"""Our routing against the designer's, on what the designer evidently cared about: the first
check of the Examiner's "no worse than the designer" (docs/spec/verification.md). It needs no
intent: what the designer did is the evidence.

  width   a net the designer widened (its usual width at least WIDEN times the board's usual
          track width) that ours routes narrower somewhere than the designer's usual width
  length  a net ours routes more than LONGER times as long as the designer, and 10 mm longer
  vias    a widened net ours puts more than 2 vias more on than the designer
  decap   a decoupling capacitor (one pad on ground, the other on a net an IC's pin is on) whose
          track path to the nearest such IC pin is 3 mm and 1.5 times longer in ours
  switch  a switch node (a regulator's switching pin, its inductor and diode: Circuit Composer's
          intent class SWITCH) whose track copper is 1.5 times the designer's area and 2 mm²
          more: copper that radiates. Its length and area are totalled too; less area is not
          better on its own, as a narrower track has less (the width check covers that)
  loop    a regulator's hot loop, from Circuit Composer's buck and boost blocks: the track path
          from the input capacitor (a buck's) or the output capacitor (a boost's) to the
          regulator's supply pin or the diode, plus the path from the capacitor's ground pad to
          the regulator's power ground pin, 3 mm and 1.5 times longer in ours. The loop carries
          the switched current, so its length is its area's proxy

    designer.py DESIGNER.kicad_pcb OURS.kicad_pcb [--list] [--json]
    designer.py --run RUN [--boards DIR] [--intent DIR] [--workers N]

The first form compares two boards. The second compares every board of a PCBench run
(RUN/<id>/out.kicad_pcb) with the designer's (DIR/<id>/board.kicad_pcb), writes each to
RUN/<id>/designer.json and prints the totals. Switch nodes and regulator blocks come from the intent DIR/<id>/intent.json. Paths are followed along tracks, vias and pads,
joined wherever their copper touches as KiCad's connectivity has it; a pad reached only
through a pour has no track path, and is said so. KiCad's own geometry
(pcbnew) reads both boards, nothing of the router's.
"""
import argparse
import heapq
import json
import math
import os
import re
import sys
from collections import Counter, defaultdict
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

MM = 1e6
WIDEN = 1.5  # a net is widened at this many times the board's usual width
NARROW = 0.9  # ours is narrower when under this share of the designer's width
LONGER = 2.0
CELL = 2 * MM  # the bucket of track ends looked through for ends whose copper overlaps
GROUND = re.compile(r"(^|/)(A|D|P|S|C)?(GND|VSS|GROUND|EARTH|0V)", re.I)
IC = re.compile(r"^(U|IC)\d")


def norm(net):
    return net.lstrip("/")


class Copper:
    """A board's tracks, vias and pads per net, and the paths along them."""

    def __init__(self, path):
        import pcbnew
        self.pcbnew = pcbnew
        self.board = pcbnew.LoadBoard(str(path))
        self.tracks = defaultdict(list)  # net -> [(a, b, layer, width, length)]
        self.vias = defaultdict(list)  # net -> [(pos, radius)]
        # (ref, number) -> (net, [(pos, pad)]): every pad of a pin, as an exposed pad split in
        # parts shares its number
        self.pads = {}
        for t in self.board.GetTracks():
            net = norm(t.GetNetname())
            if t.GetClass() == "PCB_VIA":
                p = t.GetPosition()
                self.vias[net].append(((p.x, p.y), t.GetFrontWidth() / 2))
            else:
                a, b = t.GetStart(), t.GetEnd()
                self.tracks[net].append(((a.x, a.y), (b.x, b.y), t.GetLayer(), t.GetWidth(), t.GetLength()))
        self.by_net = defaultdict(list)
        for fp in self.board.Footprints():
            ref = fp.GetReference()
            for p in fp.Pads():
                net = norm(p.GetNetname())
                if not net:
                    continue
                pos = p.GetPosition()
                key = (ref, p.GetNumber())
                if key not in self.pads:
                    self.pads[key] = (net, [])
                    self.by_net[net].append(key)
                self.pads[key][1].append(((pos.x, pos.y), p))

    def usual_width(self, net=None):
        """The width covering the most track length, of one net or the whole board."""
        c = Counter()
        for n, ts in self.tracks.items():
            if net is None or n == net:
                for t in ts:
                    c[t[3]] += t[4]
        return c.most_common(1)[0][0] if c else None

    def net_stats(self):
        out = {}
        for net in set(self.tracks) | set(self.vias):
            ts = self.tracks.get(net, [])
            out[net] = {
                "length": sum(t[4] for t in ts) / MM,
                "usual": (self.usual_width(net) or 0) / MM,
                "narrowest": min((t[3] for t in ts), default=0) / MM,
                "vias": len(self.vias.get(net, [])),
            }
        return out

    def graph(self, net):
        """Nodes and edges of a net's track copper: track ends, points where an end lands on
        another track, vias, and the net's pads."""
        pcbnew = self.pcbnew
        ts = self.tracks.get(net, [])
        vias = self.vias.get(net, [])
        edges = defaultdict(list)
        ends = [(e, t[2], t[3]) for t in ts for e in (t[0], t[1])]
        # Each track's points along it: its ends, and other tracks' ends whose copper reaches
        # it (KiCad joins copper that touches, not only an end on the centre line).
        for i, (a, b, layer, w, _) in enumerate(ts):
            pts = [a, b]
            for e, l, we in ends:
                if l == layer and e not in (a, b) and on_segment(e, a, b, (w + we) / 2):
                    pts.append(e)
            # A via the track runs over joins it there.
            for k, (v, r) in enumerate(vias):
                if v not in (a, b) and on_segment(v, a, b, r + w / 2):
                    pts.append(v)
                    edges[("v", k)].append((("p", v, layer), 0.0))
                    edges[("p", v, layer)].append((("v", k), 0.0))
            pts.sort(key=lambda p: (p[0] - a[0]) ** 2 + (p[1] - a[1]) ** 2)
            for p, q in zip(pts, pts[1:]):
                d = math.dist(p, q)
                edges[("p", p, layer)].append((("p", q, layer), d))
                edges[("p", q, layer)].append((("p", p, layer), d))
        # Ends whose round copper overlaps join where they are.
        cells = defaultdict(list)
        for e, l, w in ends:
            cells[(l, e[0] // CELL, e[1] // CELL)].append((e, w))
        for (l, cx, cy), here in cells.items():
            near = [x for dx in (-1, 0, 1) for dy in (-1, 0, 1) for x in cells.get((l, cx + dx, cy + dy), ())]
            for e, w in here:
                for f, wf in near:
                    if e != f and math.dist(e, f) < (w + wf) / 2:
                        edges[("p", e, l)].append((("p", f, l), math.dist(e, f)))
        # Vias join the layers at their point; a track whose end's copper reaches a via's ring is
        # on it, as KiCad's connectivity has it.
        for k, (v, r) in enumerate(vias):
            for e, l, w in ends:
                if math.dist(e, v) <= r + w / 2:
                    edges[("v", k)].append((("p", e, l), 0.0))
                    edges[("p", e, l)].append((("v", k), 0.0))
        # Pads: an end whose round copper reaches a pad, on a layer the pad is on.
        for key in self.by_net.get(net, []):
            for pos, pad in self.pads[key][1]:
                for e, l, w in ends:
                    if pad.IsOnLayer(l) and pad.HitTest(pcbnew.VECTOR2I(int(e[0]), int(e[1])), int(w / 2)):
                        edges[("pad", key)].append((("p", e, l), math.dist(pos, e)))
                        edges[("p", e, l)].append((("pad", key), math.dist(pos, e)))
                for k, (v, r) in enumerate(vias):
                    if pad.HitTest(pcbnew.VECTOR2I(int(v[0]), int(v[1])), int(r)):
                        edges[("pad", key)].append((("v", k), math.dist(pos, v)))
                        edges[("v", k)].append((("pad", key), math.dist(pos, v)))
        # Pads whose copper overlaps join, as a part's pad laid over another's does.
        mine = [(key, pos, pad) for key in self.by_net.get(net, []) for pos, pad in self.pads[key][1]]
        for i, (k1, p1, a) in enumerate(mine):
            for k2, p2, b in mine[i + 1:]:
                if k1 != k2 and touches(pcbnew, a, b):
                    d = math.dist(p1, p2)
                    edges[("pad", k1)].append((("pad", k2), d))
                    edges[("pad", k2)].append((("pad", k1), d))
        return edges

    def path(self, edges, src, targets):
        """Shortest track path in mm from pad `src` to the nearest of `targets`, or None."""
        dist = {("pad", src): 0.0}
        heap = [(0.0, 0, ("pad", src))]
        n = 1
        targets = {("pad", t) for t in targets}
        while heap:
            d, _, u = heapq.heappop(heap)
            if u in targets:
                return d / MM
            if d > dist.get(u, math.inf):
                continue
            for v, w in edges.get(u, ()):
                nd = d + w
                if nd < dist.get(v, math.inf):
                    dist[v] = nd
                    n += 1
                    heapq.heappush(heap, (nd, n, v))
        return None

    def copper(self, net):
        """A net's track length in mm and its track area in mm² (each track's length by its width)."""
        ts = self.tracks.get(net, [])
        return sum(t[4] for t in ts) / MM, sum(t[4] * t[3] for t in ts) / MM / MM

    def decaps(self):
        """Each decoupling capacitor's supply pad and the IC pads on its supply net."""
        caps = defaultdict(dict)
        for (ref, num), (net, _) in self.pads.items():
            if ref.startswith("C") and ref[1:2].isdigit():
                caps[ref][num] = net
        out = {}
        for ref, pads in caps.items():
            if len(pads) != 2:
                continue
            nets = list(pads.items())
            ground = [n for _, n in nets if GROUND.search(n)]
            supply = [(num, n) for num, n in nets if not GROUND.search(n)]
            if len(ground) != 1 or len(supply) != 1:
                continue
            num, net = supply[0]
            ics = [k for k in self.by_net[net] if IC.match(k[0])]
            if ics:
                out[ref] = (num, net, ics)
        return out

    def loop(self, legs):
        """A hot loop's length in mm: the sum of its legs' track paths, each from a pad to the
        nearest of its targets on the same net; None when a leg has no track path."""
        total = 0.0
        for src, targets in legs:
            net = self.pads[src][0] if src in self.pads else None
            mm = self.path(self.graph(net), src, targets) if net else None
            if mm is None:
                return None
            total += mm
        return total

    def decap_paths(self, which):
        graphs = {}
        out = {}
        for ref, (num, net, ics) in which.items():
            if net not in graphs:
                graphs[net] = self.graph(net)
            out[ref] = self.path(graphs[net], (ref, num), ics)
        return out


def touches(pcbnew, a, b):
    """Whether pads `a` and `b` share a copper layer and their copper meets: either's centre
    within the other grown by its smaller half-size, a close enough test for pads."""
    if not any(b.IsOnLayer(l) for l in a.GetLayerSet().CuStack()):
        return False
    pa, pb = a.GetPosition(), b.GetPosition()
    sa, sb = a.GetBoundingBox(), b.GetBoundingBox()
    if not sa.Intersects(sb):
        return False
    rb = min(sb.GetWidth(), sb.GetHeight()) // 2
    ra = min(sa.GetWidth(), sa.GetHeight()) // 2
    return a.HitTest(pb, rb) or b.HitTest(pa, ra)


def on_segment(p, a, b, tol):
    ax, ay = a
    dx, dy = b[0] - ax, b[1] - ay
    L2 = dx * dx + dy * dy
    if L2 == 0:
        return math.dist(p, a) <= tol
    t = ((p[0] - ax) * dx + (p[1] - ay) * dy) / L2
    if t <= 0 or t >= 1:
        return False
    return math.dist(p, (ax + t * dx, ay + t * dy)) <= tol


def compare(designer_path, ours_path, switches=(), blocks=()):
    d, o = Copper(designer_path), Copper(ours_path)
    ds, os_ = d.net_stats(), o.net_stats()
    board_usual = (d.usual_width() or 0) / MM
    findings = []
    widened = {n for n, s in ds.items() if board_usual and s["usual"] >= WIDEN * board_usual and s["length"] > 0}
    for net in sorted(widened):
        mine = os_.get(net)
        if mine and mine["length"] > 0 and mine["narrowest"] < NARROW * ds[net]["usual"]:
            findings.append({"check": "width", "net": net, "designer_mm": round(ds[net]["usual"], 3), "ours_mm": round(mine["narrowest"], 3)})
        if mine and mine["vias"] > ds[net]["vias"] + 2:
            findings.append({"check": "vias", "net": net, "designer": ds[net]["vias"], "ours": mine["vias"]})
    for net, s in ds.items():
        mine = os_.get(net)
        if s["length"] > 0 and mine and mine["length"] > LONGER * s["length"] and mine["length"] > s["length"] + 10:
            findings.append({"check": "length", "net": net, "designer_mm": round(s["length"], 1), "ours_mm": round(mine["length"], 1)})
    caps = d.decaps()
    dp, op = d.decap_paths(caps), o.decap_paths(caps)
    decap = {}
    for ref in sorted(caps):
        decap[ref] = {"net": caps[ref][1], "designer_mm": rnd(dp[ref]), "ours_mm": rnd(op[ref])}
        a, b = dp[ref], op[ref]
        if a is not None and (b is None or (b > a + 3 and b > 1.5 * a)):
            findings.append({"check": "decap", "part": ref, "net": caps[ref][1], "designer_mm": rnd(a), "ours_mm": rnd(b)})
    switch = {}
    for net in sorted({norm(n) for n in switches}):
        (dl, da), (ol, oa) = d.copper(net), o.copper(net)
        if dl == 0 and ol == 0:
            continue
        switch[net] = {"designer_mm": rnd(dl), "designer_mm2": rnd(da), "ours_mm": rnd(ol), "ours_mm2": rnd(oa)}
        if oa > 1.5 * da and oa > da + 2:
            findings.append({"check": "switch", "net": net, "designer_mm2": rnd(da), "ours_mm2": rnd(oa)})
    loop = {}
    for name, legs in hot_loops(d, blocks):
        dl, ol = d.loop(legs), o.loop(legs)
        loop[name] = {"designer_mm": rnd(dl), "ours_mm": rnd(ol)}
        if dl is not None and (ol is None or (ol > dl + 3 and ol > 1.5 * dl)):
            findings.append({"check": "loop", "block": name, "designer_mm": rnd(dl), "ours_mm": rnd(ol)})
    return {
        "board_usual_width_mm": round(board_usual, 3),
        "widened_nets": len(widened),
        "decaps": len(caps),
        "designer": {"length_mm": round(sum(s["length"] for s in ds.values())), "vias": sum(s["vias"] for s in ds.values())},
        "ours": {"length_mm": round(sum(s["length"] for s in os_.values())), "vias": sum(s["vias"] for s in os_.values())},
        "counts": dict(Counter(f["check"] for f in findings)),
        "findings": findings,
        "decap": decap,
        "switch": switch,
        "loop": loop,
    }


def rnd(x):
    return None if x is None else round(x, 2)


def switch_nets(intent):
    """The nets Circuit Composer's intent file `intent` classes as switch nodes."""
    try:
        nets = json.loads(Path(intent).read_text())["nets"]
    except (OSError, ValueError, KeyError):
        return []
    return [n for n, v in nets.items() if v.get("class") == "SWITCH"]


def regulator_blocks(intent):
    """Circuit Composer's buck and boost blocks in the intent file `intent`."""
    try:
        blocks = json.loads(Path(intent).read_text())["blocks"]
    except (OSError, ValueError, KeyError):
        return []
    return [(n, b) for n, b in sorted(blocks.items()) if b.get("type") in ("buck", "boost")]


def hot_loops(copper, blocks):
    """Each regulator block's hot loop as legs (pad, [target pads]) on `copper`'s pads: a
    buck's input capacitor to its supply pin and its ground to the power ground pin; a boost's
    output capacitor to the parts on the switch node that drive the output (its diode or
    regulator) and its ground to the power ground pin. A block missing a role is skipped."""
    pads = copper.pads

    def pin(role):
        ref, _, num = role.partition(".")
        return (ref, num) if (ref, num) in pads else None

    def cap_pad(cap, net):
        return next(((r, n) for (r, n), v in pads.items() if r == cap and v[0] == net), None)

    out = []
    for name, b in blocks:
        roles = b.get("roles", {})
        pgnd, sw = pin(roles.get("pgnd", "")), pin(roles.get("sw", ""))
        if not pgnd or not sw:
            continue
        gnet, swnet = pads[pgnd][0], pads[sw][0]
        if b["type"] == "buck":
            cap, vin = roles.get("cin"), pin(roles.get("vin", ""))
            if not cap or not vin:
                continue
            hot, targets = cap_pad(cap, pads[vin][0]), [vin]
        else:
            cap = roles.get("cout")
            other = [(r, n) for (r, n), v in pads.items() if r == cap and v[0] != gnet]
            if not cap or len(other) != 1:
                continue
            hot = other[0]
            drivers = {r for (r, n), v in pads.items() if v[0] == swnet and r != roles.get("inductor")}
            targets = [k for k, v in pads.items() if k[0] in drivers and v[0] == pads[hot][0]]
        cold = cap_pad(cap, gnet)
        if hot and cold and targets:
            out.append((name, [(hot, targets), (cold, [pgnd])]))
    return out


def one(job):
    b, designer, ours, out, intent = job
    try:
        res = compare(designer, ours, switch_nets(intent), regulator_blocks(intent))
    except Exception as e:  # a board KiCad cannot load is reported, not fatal
        res = {"error": f"{type(e).__name__}: {e}"}
    res["id"] = b
    Path(out).write_text(json.dumps(res, indent=1) + "\n")
    return res


def run(a):
    run_dir = Path(a.run)
    boards = Path(a.boards)
    jobs = []
    for d in sorted(run_dir.iterdir()):
        r = d / "result.json"
        if not r.exists() or not (d / "out.kicad_pcb").exists() or not (boards / d.name / "board.kicad_pcb").exists():
            continue
        if not a.redo and (d / "designer.json").exists():
            jobs.append(None)
            continue
        jobs.append((d.name, str(boards / d.name / "board.kicad_pcb"), str(d / "out.kicad_pcb"), str(d / "designer.json"), str(Path(a.intent) / d.name / "intent.json")))
    todo = [j for j in jobs if j]
    with ProcessPoolExecutor(a.workers) as ex:
        for i, res in enumerate(ex.map(one, todo, chunksize=1), 1):
            if i % 50 == 0:
                print(f"[{i}/{len(todo)}]", file=sys.stderr)
    summarize(run_dir)


def summarize(run_dir):
    clean, rows = 0, []
    totals, boards_with = Counter(), Counter()
    errors = 0
    for d in sorted(Path(run_dir).iterdir()):
        f = d / "designer.json"
        if not f.exists():
            continue
        res = json.loads(f.read_text())
        if "error" in res:
            errors += 1
            continue
        ok = json.loads((d / "result.json").read_text()).get("success")
        if not ok:
            continue
        clean += 1
        rows.append(res)
        for k, n in res["counts"].items():
            totals[k] += n
            boards_with[k] += 1
    dl = sum(r["designer"]["length_mm"] for r in rows)
    ol = sum(r["ours"]["length_mm"] for r in rows)
    dv = sum(r["designer"]["vias"] for r in rows)
    ov = sum(r["ours"]["vias"] for r in rows)
    print(f"{run_dir}: {clean} clean boards compared with the designer's ({errors} could not be read)")
    print(f"   track length {dl / 1000:.1f} m designer -> {ol / 1000:.1f} m ours; vias {dv} -> {ov}")
    print(f"   widened nets {sum(r['widened_nets'] for r in rows)}, decoupling capacitors {sum(r['decaps'] for r in rows)}")
    sw = [v for r in rows for v in r.get("switch", {}).values()]
    if sw:
        print(f"   switch nodes {len(sw)}: track {sum(v['designer_mm'] for v in sw):.0f} mm, {sum(v['designer_mm2'] for v in sw):.0f} mm² designer -> {sum(v['ours_mm'] for v in sw):.0f} mm, {sum(v['ours_mm2'] for v in sw):.0f} mm² ours")
    lp = [v for r in rows for v in r.get("loop", {}).values() if v["designer_mm"] is not None and v["ours_mm"] is not None]
    if lp:
        print(f"   hot loops {len(lp)} with track paths on both: {sum(v['designer_mm'] for v in lp):.0f} mm designer -> {sum(v['ours_mm'] for v in lp):.0f} mm ours")
    for k in ("width", "vias", "length", "decap", "switch", "loop"):
        print(f"   {k:7} {totals[k]:6} findings on {boards_with[k]} boards")
    worst = sorted(rows, key=lambda r: -len(r["findings"]))[:10]
    print("   most findings: " + ", ".join(f"{r['id']} {len(r['findings'])}" for r in worst))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("designer", nargs="?")
    ap.add_argument("ours", nargs="?")
    ap.add_argument("--run")
    ap.add_argument("--boards", default=str(Path(os.environ.get("ULTRAROUTE_LAB", Path.home() / "Software" / "ultraroute-lab")) / "pcbench" / "boards"))
    ap.add_argument("--intent", default=str(Path(os.environ.get("ULTRAROUTE_LAB", Path.home() / "Software" / "ultraroute-lab")) / "pcbench" / "intent"), help="Circuit Composer intent files, DIR/<id>/intent.json, for the switch nodes")
    ap.add_argument("--intent-file", help="with DESIGNER OURS: Circuit Composer's intent.json for the switch nodes and regulator blocks")
    ap.add_argument("--workers", type=int, default=int(os.environ.get("RESERVED_CORES", "4")))
    ap.add_argument("--redo", action="store_true")
    ap.add_argument("--summary", action="store_true", help="with --run: only print the totals of what is written")
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()
    if a.run:
        return summarize(Path(a.run)) if a.summary else run(a)
    if not (a.designer and a.ours):
        ap.error("give DESIGNER and OURS, or --run RUN")
    res = compare(a.designer, a.ours, switch_nets(a.intent_file), regulator_blocks(a.intent_file)) if a.intent_file else compare(a.designer, a.ours)
    if a.json:
        print(json.dumps(res, indent=1))
        return
    print(f"designer {res['designer']}  ours {res['ours']}  usual width {res['board_usual_width_mm']} mm, widened nets {res['widened_nets']}, decaps {res['decaps']}")
    print("findings: " + (", ".join(f"{k} {n}" for k, n in res["counts"].items()) or "none"))
    if a.list:
        for f in res["findings"]:
            print("  ", f)


if __name__ == "__main__":
    main()
