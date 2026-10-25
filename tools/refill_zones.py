"""Refill a board's zones with KiCad's own filler and save the result to a new file.

    python3 tools/refill_zones.py in.kicad_pcb out.kicad_pcb

The router leaves old fills in place; KiCad's DRC judges a board after its zones are refilled.
"""

import sys

import pcbnew


def main() -> None:
    source, target = sys.argv[1], sys.argv[2]
    board = pcbnew.LoadBoard(source)
    pcbnew.ZONE_FILLER(board).Fill(board.Zones())
    pcbnew.SaveBoard(target, board)


if __name__ == "__main__":
    main()
