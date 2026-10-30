#!/usr/bin/env python3
"""The place goldens: pcbnew 9 moves, turns and flips every footprint of each fixture board by a
fixed recipe, in two rounds (the second starts from the first's board, so from other sides and
angles). Each round's recipe is written as `<board>.r<N>.json` and the footprints pcbnew saved
are hashed into `hashes.json`; `tests/place.rs` holds `place::apply` to those hashes.

    python3 gen.py        # with KiCad 9's pcbnew importable; rewrites this directory

What a hash covers: every top-level footprint, from its `(footprint` to its closing paren, in file
order, joined with newlines (FNV-1a, 64 bit). Tracks are left out because pcbnew re-nets a track
that a moved pad now lands on, which `place` does not do.
"""
import json, os, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(HERE, "../../../../../fixtures")
BOARDS = ["earring", "bluebuzz-rm2", "earring-locked"]
NM = 1e6


def round_of(src, out, rnd):
    """One round in its own process: pcbnew loads a board once per process."""
    import pcbnew
    b = pcbnew.LoadBoard(src)
    moves = []
    for i, fp in enumerate(sorted(b.GetFootprints(), key=lambda f: f.m_Uuid.AsString())):
        j = i + 3 * rnd
        p = fp.GetPosition()
        x, y = round(p.x / NM + (j % 7) * 1.25 - 3, 6), round(p.y / NM + (j % 5) * 0.75 - 1.5, 6)
        rot = [0, 90, 180, -90, 30, -135, 12.5, 45, 270, 7.25, -45.5][j % 11]
        back = ((j % 3 == 0) != fp.IsFlipped()) if j % 2 == 0 else fp.IsFlipped()
        if back != fp.IsFlipped():
            fp.Flip(fp.GetPosition(), pcbnew.FLIP_DIRECTION_TOP_BOTTOM)
        fp.SetPosition(pcbnew.VECTOR2I(int(round(x * NM)), int(round(y * NM))))
        fp.SetOrientationDegrees(rot)
        moves.append({"ref": fp.GetReference(), "x": x, "y": y, "rotation": rot, "side": "back" if back else "front"})
    b.Save(out)
    return moves


def footprints_hash(path):
    lines = open(path).read().split("\n")
    blocks, cur = [], None
    for line in lines:
        if line.startswith("\t(footprint "):
            cur = [line[1:]]
        elif cur is not None:
            cur.append(line)
            if line == "\t)":
                blocks.append("\n".join(cur))
                cur = None
    h = 0xCBF29CE484222325
    for byte in "\n".join(blocks).encode():
        h = ((h ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{h:016x}"


def main():
    if len(sys.argv) == 4:
        moves = round_of(sys.argv[1], sys.argv[2], int(sys.argv[3]))
        print(json.dumps(moves))
        return
    hashes = {}
    with tempfile.TemporaryDirectory() as tmp:
        for board in BOARDS:
            src = os.path.join(FIXTURES, board + ".kicad_pcb")
            for rnd in (1, 2):
                out = os.path.join(tmp, f"{board}.r{rnd}.kicad_pcb")
                run = subprocess.run([sys.executable, __file__, src, out, str(rnd - 1)], capture_output=True, text=True, check=True)
                moves = json.loads(run.stdout.strip().splitlines()[-1])
                with open(os.path.join(HERE, f"{board}.r{rnd}.json"), "w") as f:
                    f.write("[\n" + ",\n".join(json.dumps(m) for m in moves) + "\n]\n")
                hashes[f"{board}.r{rnd}"] = footprints_hash(out)
                src = out
    with open(os.path.join(HERE, "hashes.json"), "w") as f:
        json.dump(hashes, f, indent=1)
        f.write("\n")
    print(json.dumps(hashes, indent=1))


if __name__ == "__main__":
    main()
