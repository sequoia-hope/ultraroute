#!/usr/bin/env python3
"""search.py — an iterated search over routed boards, judged by KiCad, recorded in the lab.

    tools/search.py NAME [--rounds R] [--seeds K] [--keep N] [--timeout S] [--args ARGS]
                    [--from-lab EXP[:RUN,...]]... [BOARD.kicad_pcb ...]

Each round runs the router on every board kept so far with K fresh seeds (`--args`, by default
the seeded plough search, which adopts the board's copper), as one `tools/lab.py run` experiment
NAME-rN, and keeps the N boards KiCad finds fewest unconnected on (errors count as unconnected,
so a broken board never wins). It stops after R rounds, or when a round's best is no better than
the last's. Starting boards are given as files, or as the outputs of runs of a lab experiment.
Wrap it in ~/.claude/bin/reserve: every run of a round goes at once.
"""
import argparse
import json
import shlex
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
import lab  # noqa: E402

PLOUGH = "--adopt --shove-plough rip --shove-repair 20 --shove-ties --shove-passes 60 --shove-patience 8 --tighten"


def score(run):
    d = run.get("drc") or {}
    if d.get("unconnected") is None:
        return None
    errors = d.get("errors") or {}
    return d["unconnected"] + (sum(errors.values()) if isinstance(errors, dict) else int(errors))


def from_lab(spec):
    exp, _, names = spec.partition(":")
    e = json.loads((lab.LAB / "experiments" / exp / "experiment.json").read_text())
    wanted = set(names.split(",")) if names else None
    out = []
    for r in e["runs"]:
        if (wanted is None or r["name"] in wanted) and score(r) is not None:
            out.append((score(r), r["name"], lab.LAB / "experiments" / exp / r["name"] / "out.kicad_pcb"))
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("name")
    ap.add_argument("boards", nargs="*")
    ap.add_argument("--from-lab", action="append", default=[])
    ap.add_argument("--rounds", type=int, default=4)
    ap.add_argument("--seeds", type=int, default=3)
    ap.add_argument("--keep", type=int, default=5)
    ap.add_argument("--timeout", type=float, default=400)
    ap.add_argument("--threads", type=int, default=2)
    ap.add_argument("--args", default=PLOUGH)
    a = ap.parse_args()

    pool = [(None, Path(b).stem, Path(b)) for b in a.boards]
    for spec in a.from_lab:
        pool += from_lab(spec)
    pool.sort(key=lambda p: (p[0] is None, p[0] or 0))
    pool = pool[: a.keep]
    best = min((p[0] for p in pool if p[0] is not None), default=None)
    print(f"start: {[(s, n) for s, n, _ in pool]}", flush=True)
    seed = 100
    for rnd in range(1, a.rounds + 1):
        runs = []
        for _, name, board in pool:
            for _ in range(a.seeds):
                seed += 1
                runs.append(f"{name.split('~')[0]}~r{rnd}s{seed}={board}::{a.args} --seed {seed}")
        exp = f"{a.name}-r{rnd}"
        cmd = [sys.executable, str(ROOT / "tools/lab.py"), "run", exp, "--title", f"{a.name}: round {rnd}",
               "--notes", f"Iterated search (tools/search.py), round {rnd}: {len(pool)} boards x {a.seeds} seeds, keep {a.keep}. Args: {a.args}",
               "--recipe", "", "--timeout", str(a.timeout), "--threads", str(a.threads), *runs]
        subprocess.run(cmd, check=True, stdout=subprocess.DEVNULL)
        found = from_lab(exp)
        found.sort(key=lambda p: p[0])
        print(f"round {rnd}: {[(s, n) for s, n, _ in found[: a.keep]]}", flush=True)
        if not found:
            break
        round_best = found[0][0]
        # Keep the best of the old pool and the new, so a bad round loses nothing.
        pool = sorted(pool + found, key=lambda p: (p[0] is None, p[0] or 0))[: a.keep]
        if best is not None and round_best >= best:
            print(f"round {rnd} found nothing better than {best}; stopping", flush=True)
            break
        best = round_best if best is None else min(best, round_best)
    print(f"best: {[(s, n, str(b)) for s, n, b in pool[:3]]}", flush=True)


if __name__ == "__main__":
    main()
