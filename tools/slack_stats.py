#!/usr/bin/env python3
"""Summarize COPPERROUTE_HEURISTIC_STATS lines: '<box distance> <coarse bound|unreachable>'."""
import statistics
import sys

box, coarse, unreachable = [], [], 0
for line in open(sys.argv[1]):
    b, c = line.split()[:2]
    if c == "unreachable":
        unreachable += 1
        continue
    box.append(float(b))
    coarse.append(float(c))
n = len(box) + unreachable
ratios = [c / b for b, c in zip(box, coarse) if b > 0]
print(f"searches={n} unreachable={unreachable} ({unreachable / max(n, 1):.0%})")
if ratios:
    print(f"coarse/box ratio: median={statistics.median(ratios):.2f} "
          f"p90={sorted(ratios)[int(0.9 * len(ratios))]:.2f} max={max(ratios):.2f}")
    print(f"coarse tighter than box in {sum(r > 1 for r in ratios)} searches "
          f"({sum(r > 1 for r in ratios) / len(ratios):.0%}); "
          f">1.5x in {sum(r > 1.5 for r in ratios)}, >2x in {sum(r > 2 for r in ratios)}")
