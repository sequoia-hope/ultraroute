#!/usr/bin/env python3
"""Routes boards as their files stand and with the files shuffled, and says whether the copper and
the log come out the same: the router's answer must not depend on how a file orders its items.

    shuffle_check.py [--bin ROUTER] [--shuffles N] [--seed N] [--timeout S] [--args "..."] BOARD ...

BOARD is a .kicad_pcb, or a directory holding board.kicad_pcb; the .kicad_pro and .kicad_dru beside
it go along. A shuffle moves top-level items, nets, footprints, pads and fills, renumbers the nets
and makes every uuid new. Copper is compared as sets of pieces named by net name, uuids included
when --uuid-seed is among the arguments. Exits 1 if any board differs.
"""

import argparse
import random
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

RECIPE = "--recipe default --threads 1 --cpu-clock --strip-all --uuid-seed 1"
TOP = re.compile(r"^(net|footprint|segment|arc|via|zone|group|gr_\w+)$")
INNER = {"footprint": re.compile(r"^(pad|zone|fp_\w+)$"), "zone": re.compile(r"^filled_polygon$")}
UUID = re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
NET = re.compile(r"\(net (\d+)(?=[ )])")


def children(text, start):
    """The spans of the list at `start`'s children, by a scan that minds strings: each its start,
    end and head."""
    out, depth, k, n = [], 0, start, len(text)
    begin, head = None, None
    while k < n:
        c = text[k]
        if c == '"':
            k += 1
            while text[k] != '"':
                k += 2 if text[k] == "\\" else 1
        elif c == "(":
            depth += 1
            if depth == 2:
                begin = k
                m = re.compile(r"\(\s*([^\s()\"]+)").match(text, k)
                head = m.group(1) if m else ""
        elif c == ")":
            depth -= 1
            if depth == 1:
                out.append((begin, k + 1, head))
            elif depth == 0:
                return out, k + 1
        k += 1
    raise ValueError("unbalanced")


def shuffled(text, start, moves, rng):
    """The list at `start` with the children `moves` matches changing places."""
    kids, end = children(text, start)
    pieces = []
    for a, b, head in kids:
        pieces.append(shuffled(text, a, INNER[head], rng) if head in INNER and moves is not INNER["zone"] else text[a:b])
    slots = [k for k, (_, _, head) in enumerate(kids) if moves.match(head)]
    order = slots[:]
    rng.shuffle(order)
    moved = [pieces[k] for k in order]
    for slot, piece in zip(slots, moved):
        pieces[slot] = piece
    out, at = [], start
    for (a, b, _), piece in zip(kids, pieces):
        out += [text[at:a], piece]
        at = b
    out.append(text[at:end])
    return "".join(out)


def reordered(text, rng):
    text = shuffled(text, text.index("("), TOP, rng)
    numbers = sorted({int(m) for m in NET.findall(text)} - {0})
    to = numbers[:]
    rng.shuffle(to)
    new = dict(zip(numbers, to))
    text = NET.sub(lambda m: f"(net {new.get(int(m[1]), int(m[1]))}", text)
    seen = {}

    def fresh(m):
        if m[0] not in seen:
            h = f"{rng.getrandbits(128):032x}"
            seen[m[0]] = f"{h[:8]}-{h[8:12]}-4{h[13:16]}-8{h[17:20]}-{h[20:]}"
        return seen[m[0]]

    return UUID.sub(fresh, text)


def forms(text, heads):
    kids, _ = children(text, text.index("("))
    return [(h, text[a:b]) for a, b, h in kids if h in heads]


def field(form, name):
    m = re.search(r"\(" + name + r"\s+([^()]*)\)", form)
    return m[1].strip() if m else ""


def copper(text):
    """Every track, arc, via, footprint place and zone fill of a file, named by net name, as a
    sorted list. Fills are counted and summed per zone: what stale_fills took out shows there."""
    names = {}
    for _, form in forms(text, {"net"}):
        m = re.match(r'\(net\s+(\d+)\s+"((?:[^"\\]|\\.)*)"', form)
        if m:
            names[int(m[1])] = m[2]
    net = lambda form: names.get(int(field(form, "net").split()[0] or 0), "?") if field(form, "net") else ""
    out = []
    for head, form in forms(text, {"segment", "arc", "via"}):
        geometry = " ".join(field(form, n) for n in ("start", "mid", "end", "at", "size", "drill", "width", "layer", "layers"))
        out.append(f"{head} {net(form)} {geometry} {field(form, 'uuid')}")
    for _, form in forms(text, {"footprint"}):
        ref = re.search(r'\(property\s+"Reference"\s+"((?:[^"\\]|\\.)*)"', form) or re.search(r'\(fp_text\s+reference\s+"?([^"\s]*)', form)
        out.append(f"part {ref[1] if ref else '?'} {field(form, 'at')}")
    for _, form in forms(text, {"zone"}):
        kids, _ = children(form, 0)
        fills = sorted(hash_of(form[a:b]) for a, b, h in kids if h == "filled_polygon")
        out.append(f"zone {field(form, 'net_name')} {field(form, 'layer')}{field(form, 'layers')} {hash_of(field_list(form, 'polygon'))} {len(fills)} {sum(fills)}")
    return sorted(out)


def field_list(form, name):
    kids, _ = children(form, 0)
    return "".join(form[a:b] for a, b, h in kids if h == name)


def hash_of(s):
    h = 0xcbf29ce484222325
    for c in s.encode():
        h = ((h ^ c) * 0x100000001b3) & 0xffffffffffffffff
    return h


def route(args, board, text, where, name):
    d = where / name
    d.mkdir()
    for ext in (".kicad_pro", ".kicad_dru"):
        side = board.with_suffix(ext)
        if side.exists():
            shutil.copy(side, d / f"board{ext}")
    (d / "board.kicad_pcb").write_text(text)
    cmd = ["systemd-run", "--user", "--scope", "-q", "-p", f"MemoryMax={args.memory}", "-p", "MemorySwapMax=0", str(args.bin), "route", "board.kicad_pcb", "-o", "out.kicad_pcb", "--seed", str(args.seed), "--timeout", str(args.timeout), *args.args.split()]
    r = subprocess.run(cmd, cwd=d, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    out = d / "out.kicad_pcb"
    # The log without what the clock or the allocator says.
    log = re.sub(r"\b\d+(\.\d+)? ?(s|ms)\b", "T", r.stdout)
    log = "\n".join(line for line in log.splitlines() if not line.startswith("memory: peak"))
    return log, (out.read_text() if out.exists() else "")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("boards", nargs="+")
    p.add_argument("--bin", default=str(Path(__file__).resolve().parent.parent / "core/target/release/ultraroute"))
    p.add_argument("--shuffles", type=int, default=2)
    p.add_argument("--seed", type=int, default=1)
    p.add_argument("--timeout", type=int, default=1000)
    p.add_argument("--memory", default="8G")
    p.add_argument("--args", default=RECIPE)
    a = p.parse_args()
    bad = 0
    for b in a.boards:
        board = Path(b)
        board = board / "board.kicad_pcb" if board.is_dir() else board
        text = board.read_text()
        rng = random.Random(a.seed)
        with tempfile.TemporaryDirectory() as tmp:
            where = Path(tmp)
            log, out = route(a, board, text, where, "file")
            want = copper(out)
            line = f"{board.parent.name}: {sum(x.startswith(('segment', 'arc')) for x in want)} tracks, {sum(x.startswith('via') for x in want)} vias"
            same = bool(out)
            for k in range(a.shuffles):
                log2, out2 = route(a, board, reordered(text, rng), where, f"s{k}")
                got = copper(out2)
                if got != want:
                    same = False
                    only = sorted(set(want) ^ set(got))
                    line += f"; shuffle {k}: copper differs in {len(only)} pieces, first {only[:2]}"
                if log2 != log:
                    same = False
                    diff = next((i for i, (x, y) in enumerate(zip(log.splitlines(), log2.splitlines())) if x != y), -1)
                    line += f"; shuffle {k}: log differs at line {diff + 1}: {log.splitlines()[diff][:120]!r} vs {log2.splitlines()[diff][:120]!r}"
            print(f"{'same' if same else 'DIFFERS'}  {line}", flush=True)
            bad += not same
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
