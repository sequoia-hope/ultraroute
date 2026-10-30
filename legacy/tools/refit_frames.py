#!/usr/bin/env python3
"""Fit every frame of a run to the copper the run actually drew.

A reshape run fixes one view for the whole plan, so a run that stops early, or never reaches
the plan's widest shape, shows a small board in a large empty frame. This sets every frame's
view (and its background) to the bounds of the board outline over all frames, with a margin.
  refit_frames.py RUN_DIR...
"""
import re
import sys
from pathlib import Path

MARGIN = 0.03
NUMBER = r"-?[\d.]+(?:e-?\d+)?"


def bounds(text):
    xs, ys = [], []
    for points in re.findall(r'<polygon points="([^"]*)" fill="none" stroke="#83c5af"', text):
        for x, y in re.findall(rf"({NUMBER}),({NUMBER})", points):
            xs.append(float(x))
            ys.append(float(y))
    return (min(xs), min(ys), max(xs), max(ys)) if xs else None


def refit(run):
    files = sorted(run.glob("*.svg"))
    boxes = [b for b in (bounds(f.read_text()) for f in files) if b]
    if not boxes:
        return
    x0, y0 = min(b[0] for b in boxes), min(b[1] for b in boxes)
    x1, y1 = max(b[2] for b in boxes), max(b[3] for b in boxes)
    pad = MARGIN * max(x1 - x0, y1 - y0)
    x, y, w, h = x0 - pad, y0 - pad, x1 - x0 + 2 * pad, y1 - y0 + 2 * pad
    for f in files:
        text = f.read_text()
        text = re.sub(r'viewBox="[^"]*"', f'viewBox="{x:.0f} {y:.0f} {w:.0f} {h:.0f}"', text, count=1)
        text = re.sub(rf'<rect x="{NUMBER}" y="{NUMBER}" width="{NUMBER}" height="{NUMBER}"',
                      f'<rect x="{x:.0f}" y="{y:.0f}" width="{w:.0f}" height="{h:.0f}"', text, count=1)
        f.write_text(text)
    print(f"{run.name}: {len(files)} frames fitted to {w:.0f} x {h:.0f}")


if __name__ == "__main__":
    for arg in sys.argv[1:]:
        refit(Path(arg))
