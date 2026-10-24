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
