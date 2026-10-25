#!/usr/bin/env python3
"""add_block.py — add a functional block to a KiCad board, as a group the router keeps whole.

    tools/add_block.py BOARD.kicad_pcb NAME ZONE[,ZONE...] [--parts REF,...]
    tools/add_block.py --job jobs/NAME

A job declares its blocks in job.json, so they come back when its input is pulled again:
    "blocks": {"rp2350_power": {"zones": ["vreg_vdd_core_3", ...], "parts": ["U1"]}}

The group holds the named zones and every part with a pad on one of their nets whose centre lies
inside one of them, plus any parts named with --parts. The router keeps a group's pours as planes
other nets stay clear of and holds its parts still. A group of that name already on the board is
replaced. The same group can be drawn in KiCad instead: select the items and press Ctrl+G.
"""
import argparse
import json
import math
import re
import sys
import uuid as uuidlib
from pathlib import Path


def forms(text, head):
    """Each `(head ...)` form one level inside the outermost one: (start, end, text)."""
    out, depth, i, n = [], 0, 0, len(text)
    while i < n:
        c = text[i]
        if c == '"':
            i += 1
            while text[i] != '"':
                i += 2 if text[i] == "\\" else 1
        elif c == "(":
            depth += 1
            if depth == 2 and text.startswith(head, i + 1) and not text[i + 1 + len(head)].isalnum():
                start, d, j = i, 0, i
                while True:
                    if text[j] == '"':
                        j += 1
                        while text[j] != '"':
                            j += 2 if text[j] == "\\" else 1
                    elif text[j] == "(":
                        d += 1
                    elif text[j] == ")":
                        d -= 1
                        if d == 0:
                            break
                    j += 1
                out.append((start, j + 1, text[start : j + 1]))
                i, depth = j, depth - 1
        elif c == ")":
            depth -= 1
        i += 1
    return out


def first(pattern, s, default=None):
    m = re.search(pattern, s)
    return m.group(1) if m else default


def inside(p, poly):
    x, y, hit = p[0], p[1], False
    for (x1, y1), (x2, y2) in zip(poly, poly[1:] + poly[:1]):
        if (y1 > y) != (y2 > y) and x < x1 + (y - y1) * (x2 - x1) / (y2 - y1):
            hit = not hit
    return hit


def add_block(path, block, zone_names, part_names=()):
    """Groups the zones and the parts on them (and any named) into `block` on the board at `path`."""
    text = open(path).read()
    wanted = list(zone_names)
    zones = []
    for _, _, z in forms(text, "zone"):
        name = first(r'\(name "([^"]*)"', z)
        if name in wanted:
            pts = re.search(r"\(polygon\s*\(pts(.*?)\)\s*\)", z, re.S).group(1)
            poly = [(float(x), float(y)) for x, y in re.findall(r"\(xy ([-\d.e]+) ([-\d.e]+)\)", pts)]
            zones.append((name, first(r'\(uuid "([^"]*)"', z), first(r'\(net_name "([^"]*)"', z), poly))
    missing = set(wanted) - {z[0] for z in zones}
    if missing:
        sys.exit(f"no zone named {', '.join(sorted(missing))} on {path}")

    named = set(part_names)
    members, parts = [z[1] for z in zones], []
    for _, _, fp in forms(text, "footprint"):
        ref = first(r'\(property "Reference" "([^"]*)"', fp)
        fx, fy, rot = re.search(r"\(at ([-\d.e]+) ([-\d.e]+)(?: ([-\d.e]+))?\)", fp).groups()
        fx, fy, r = float(fx), float(fy), math.radians(float(rot or 0))
        on = ref in named
        for _, _, pad in forms(fp, "pad"):
            net = first(r'\(net \d+ "([^"]*)"\)', pad) or first(r'\(net "([^"]*)"\)', pad)
            px, py = (float(v) for v in re.search(r"\(at ([-\d.e]+) ([-\d.e]+)", pad).groups())
            # KiCad turns a part counter-clockwise on screen, with Y down.
            at = (fx + px * math.cos(r) + py * math.sin(r), fy - px * math.sin(r) + py * math.cos(r))
            on = on or any(net == zn and inside(at, poly) for _, _, zn, poly in zones)
        if on:
            parts.append(ref)
            members.append(first(r'\(uuid "([^"]*)"', fp))
    missing = named - set(parts)
    if missing:
        sys.exit(f"no part {', '.join(sorted(missing))} on {path}")

    for start, end, g in reversed(forms(text, "group")):
        if first(r'\(group "([^"]*)"', g) == block:
            text = text[:start].rstrip("\t") + text[end:].lstrip("\n")
    listed = " ".join(f'"{m}"' for m in members)
    # Named, not random, so applying a block again leaves the file as it was.
    uuid = uuidlib.uuid5(uuidlib.NAMESPACE_URL, f"ultraroute-block:{block}")
    group = f'\t(group "{block}"\n\t\t(uuid "{uuid}")\n\t\t(members {listed})\n\t)\n'
    close = text.rstrip().rfind(")")
    text = text[:close].rstrip("\n") + "\n" + group + text[close:]
    open(path, "w").write(text)
    print(f"{path}: {block}: zones {', '.join(z[0] for z in zones)}; parts {', '.join(sorted(parts))}")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("board", nargs="?")
    ap.add_argument("name", nargs="?")
    ap.add_argument("zones", nargs="?")
    ap.add_argument("--parts", default="")
    ap.add_argument("--job", help="apply the blocks a job's job.json declares to its input board")
    a = ap.parse_args()
    if a.job:
        job = Path(a.job)
        blocks = json.loads((job / "job.json").read_text()).get("blocks", {})
        if not blocks:
            sys.exit(f"{job}/job.json declares no blocks")
        for board in sorted((job / "input").glob("*.kicad_pcb")):
            for name, b in blocks.items():
                add_block(board, name, b["zones"], b.get("parts", []))
        return
    if not (a.board and a.name and a.zones):
        ap.error("give BOARD NAME ZONES, or --job DIR")
    add_block(a.board, a.name, [z for z in a.zones.split(",") if z], [r for r in a.parts.split(",") if r])


if __name__ == "__main__":
    main()
