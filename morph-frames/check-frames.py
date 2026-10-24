#!/usr/bin/env python3
"""Check that every recorded run shows movement, and summarise the runs into runs.json.

A move frame "moves" when its board geometry differs from the frame before it; the highlight
colour, the dipole overlay and the ratsnest don't count. Runs with no moving frame are dropped
(their directory, session and report are deleted), since there is nothing to watch.
"""
import collections
import hashlib
import json
import re
import shutil
from pathlib import Path

HERE = Path(__file__).resolve().parent


def geometry(svg):
    text = svg.read_text()
    text = text.replace("#ffffff", "#edcf86")
    text = re.sub(r'<g class="(ratsnest|dipole)".*?</g>', "", text)
    return hashlib.md5(text.encode()).hexdigest()


ELEMENT = re.compile(r"<(polygon|polyline|circle)\b[^>]*/>")


def elements(svg):
    text = svg.read_text().replace("#ffffff", "#edcf86")
    text = re.sub(r'<g class="(ratsnest|dipole)".*?</g>', "", text)
    return {m.group(0) for m in ELEMENT.finditer(text)}


def bounds(element):
    if element.startswith("<circle"):
        a = dict(re.findall(r'(\w+)="([^"]*)"', element))
        x, y, r = float(a["cx"]), float(a["cy"]), float(a["r"])
        return [x - r, y - r, x + r, y + r]
    pts = re.search(r'points="([^"]*)"', element).group(1).split()
    xy = [tuple(map(float, p.split(","))) for p in pts]
    xs, ys = [p[0] for p in xy], [p[1] for p in xy]
    return [min(xs), min(ys), max(xs), max(ys)]


def focus(before, after):
    """Where the viewer zooms in, in SVG coordinates: the smallest pad that changed, which moved
    by the whole step, else the smallest changed copper with some extent in both directions."""
    changed = after - before
    if not changed:
        return None
    pads = [e for e in changed if 'fill="#edcf86"' in e and not e.startswith("<polyline")]
    boxes = [bounds(e) for e in (pads or changed)]
    solid = [b for b in boxes if b[2] > b[0] and b[3] > b[1]] or boxes
    return min(solid, key=lambda b: (b[2] - b[0]) * (b[3] - b[1]))


def main():
    runs = json.loads((HERE / "runs.json").read_text())
    for name in sorted(runs):
        frames_file = HERE / name / "frames.jsonl"
        if not frames_file.exists():
            print(f"{name}: no frames, dropped")
            runs.pop(name)
            continue
        frames = [json.loads(line) for line in frames_file.read_text().splitlines() if line]
        hashes = [geometry(HERE / name / f["file"]) for f in frames]
        moves = [f for f in frames if f["operators"] not in (["start"], ["end"])]
        still = [f["frame"] for f, before, after in zip(frames[1:], hashes, hashes[1:])
                 if f["operators"] not in (["start"], ["end"]) and before == after]
        moving = len(moves) - len(still)
        if moving == 0:
            print(f"{name}: no frame moves anything, dropped")
            shutil.rmtree(HERE / name)
            for suffix in (".ses", ".superseded.json", ".report.json"):
                (HERE / f"{name}{suffix}").unlink(missing_ok=True)
            runs.pop(name)
            continue
        sets = [elements(HERE / name / f["file"]) for f in frames]
        for f, before, after in zip(frames[1:], sets, sets[1:]):
            f["focus"] = focus(before, after)
        frames_file.write_text("".join(json.dumps(f) + "\n" for f in frames))
        first, last = collections.Counter(frames[0]["open"]), collections.Counter(frames[-1]["open"])
        runs[name]["summary"] = {
            "moves": len(moves),
            "still_frames": still,
            "operators": dict(collections.Counter("/".join(f["operators"]) for f in moves)),
            "parts": len({p for f in moves for p in f["parts"]}),
            "open_start": len(frames[0]["open"]),
            "open_end": len(frames[-1]["open"]),
            "opened": sorted((last - first).elements()),
            "closed": sorted((first - last).elements()),
        }
        flag = f", {len(still)} frame(s) without movement: {still}" if still else ""
        print(f"{name}: {moving}/{len(moves)} frames move{flag}")
    (HERE / "runs.json").write_text(json.dumps(runs, indent=1, sort_keys=True))


if __name__ == "__main__":
    main()
