#!/usr/bin/env python3
"""Screenshot every move frame in the viewer's diff mode and count the pixels that light up.

Needs the project server (./serve.sh or `proj up ultraroute`); the URL comes from `proj url`.
Writes verify-diff.json: per run, the frames whose diff shows fewer than MIN_PIXELS changed.

    verify-diff.py [run ...]
"""
import json
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np
from PIL import Image

HERE = Path(__file__).resolve().parent
MIN_PIXELS = 20
WIDTH, HEIGHT = 1400, 900
STAGE = (0, 60, 1060, 830)


def lit_pixels(png):
    image = np.asarray(Image.open(png).convert("RGB"), dtype=np.int16)
    x0, y0, x1, y1 = STAGE
    stage = image[y0:y1, x0:x1]
    return int((stage.max(axis=2) > 40).sum())


def main():
    base = subprocess.run(["proj", "url", "ultraroute"], capture_output=True, text=True, check=True).stdout.strip()
    runs = json.loads((HERE / "runs.json").read_text())
    names = sys.argv[1:] or sorted(runs)
    report = {}
    with tempfile.TemporaryDirectory() as tmp:
        for name in names:
            frames = [json.loads(l) for l in (HERE / name / "frames.jsonl").read_text().splitlines() if l]
            faint = []
            for f in frames:
                if f["operators"] in (["start"], ["end"]):
                    continue
                png = Path(tmp) / f"{name}-{f['frame']}.png"
                url = f"{base.rstrip('/')}/morph-frames/?run={name}&frame={f['frame']}&mode=diff"
                subprocess.run(["google-chrome", "--headless=new", "--disable-gpu",
                                f"--window-size={WIDTH},{HEIGHT}", "--virtual-time-budget=6000",
                                f"--screenshot={png}", url], capture_output=True, timeout=120)
                n = lit_pixels(png) if png.exists() else 0
                if n < MIN_PIXELS:
                    faint.append({"frame": f["frame"], "pixels": n, "by_mm": f["by_mm"], "parts": f["parts"]})
            report[name] = {"frames": len(frames) - 2, "faint": faint}
            print(f"{name}: {len(frames) - 2 - len(faint)}/{len(frames) - 2} frames visible in diff"
                  + (f", faint: {[x['frame'] for x in faint]}" if faint else ""))
    (HERE / "verify-diff.json").write_text(json.dumps(report, indent=1))


if __name__ == "__main__":
    main()
