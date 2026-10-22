#!/usr/bin/env python3
"""Render a coarse obstacle grid per signal layer and mark search starts the grid walls in.

usage: render_walls.py <grid file> <stats file> <info.json> <out.png> [layer,layer,...]
"""
import collections
import json
import sys

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

grid_path, stats_path, info_path, out_path = sys.argv[1:5]
with open(grid_path) as f:
    ox, oy, cell, width, height, layers = map(int, f.readline().split())
    cells = np.loadtxt(f, dtype=np.int64).reshape(layers, height, width)
info = json.load(open(info_path))
names = {n["number"]: n["name"] for n in info["nets"]}
layer_names = [l["name"] for l in info.get("layers", [])] or [f"L{i}" for i in range(layers)]
signal = [int(x) for x in sys.argv[5].split(",")] if len(sys.argv) > 5 else list(range(layers))

walled = collections.defaultdict(list)
for line in open(stats_path):
    parts = line.split()
    if len(parts) == 6 and parts[1] == "unreachable":
        _, _, net, x, y, layer = parts
        walled[int(layer)].append((float(x), float(y), names.get(int(net), net)))

fig, axes = plt.subplots(1, len(signal), figsize=(5 * len(signal), 9), squeeze=False)
mm = 1 / (cell / 0.1486) if cell else 1
for ax, layer in zip(axes[0], signal):
    plane = cells[layer]
    image = np.ones((height, width, 3))
    image[plane == -1] = (0.15, 0.15, 0.18)
    image[plane > 0] = (0.62, 0.66, 0.72)
    ax.imshow(image, origin="lower", interpolation="nearest")
    seen = set()
    for x, y, net in walled.get(layer, []):
        cx, cy = (x - ox) / cell, (y - oy) / cell
        ax.plot(cx, cy, "o", ms=5, mfc="none", mec="#d6336c", mew=1.5)
        key = (round(cx / 4), round(cy / 4), net)
        if key not in seen:
            seen.add(key)
            ax.annotate(net, (cx, cy), xytext=(4, 3), textcoords="offset points",
                        fontsize=7, color="#d6336c")
    ax.set_title(f"{layer_names[layer] if layer < len(layer_names) else layer}: "
                 f"{len(walled.get(layer, []))} walled-in starts", fontsize=10)
    ax.set_xticks([])
    ax.set_yticks([])
fig.suptitle("Fixed obstacles (dark: blocks every net; grey: pin, blocks other nets) "
             "and search starts with no obstacle-free path", fontsize=10)
fig.tight_layout()
fig.savefig(out_path, dpi=110)
print(out_path)
