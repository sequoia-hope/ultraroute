#!/usr/bin/env python3
"""Makes one of our board variants from PCBench's file of its base board.

    apply_variant.py ID -o OUT.kicad_pcb [--pcbench DIR]
    apply_variant.py --list

A variant (tools/pcbench_variants.json) is a PCBench board with an exact patch of our own: a
unified diff, tools/variants/ID.patch, of PCBs/BASE/processed.kicad_pcb as it is at the PCBench
commit the entry names. The script checks the base file is that file (its sha256), then applies
the patch with no fuzz, so the variant is the same board wherever it is made. The repository
ships the patch and this script, never the board.
"""

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

TOOLS = Path(__file__).resolve().parent
VARIANTS = TOOLS / "pcbench_variants.json"


def variants():
    """{variant id: its entry}."""
    return {v["id"]: v for v in json.loads(VARIANTS.read_text())["variants"]}


def make(v, pcbench, out):
    """Writes variant `v` to `out` from the PCBench checkout at `pcbench`; raises ValueError
    when the base file is not the one the patch was made against, or the patch does not apply."""
    base = pcbench / "PCBs" / v["base"] / "processed.kicad_pcb"
    if not base.exists():
        raise ValueError(f"{v['id']}: no {base}")
    sha = hashlib.sha256(base.read_bytes()).hexdigest()
    if sha != v["base_sha256"]:
        raise ValueError(f"{v['id']}: {base} is not the file the patch was made against (sha256 {sha}, want {v['base_sha256']} from PCBench {v['pcbench_commit']})")
    patch = TOOLS / v["patch"]
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp) / "processed.kicad_pcb"
        shutil.copy(base, work)
        r = subprocess.run(["patch", "--fuzz=0", "--no-backup-if-mismatch", "--quiet", "-i", str(patch), str(work)], capture_output=True, text=True)
        if r.returncode != 0:
            raise ValueError(f"{v['id']}: {patch.name} does not apply: {(r.stdout + r.stderr).strip()}")
        shutil.copy(work, out)


def main():
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("id", nargs="?")
    a.add_argument("-o", "--out", type=Path)
    a.add_argument("--pcbench", type=Path, default=TOOLS.parent / "PCBench", help="the PCBench checkout (default: the repository's PCBench/)")
    a.add_argument("--list", action="store_true", help="the variants and what each changes")
    a = a.parse_args()
    if a.list:
        for k, v in variants().items():
            print(f"{k}\n  base {v['base']} at PCBench {v['pcbench_commit'][:12]}\n  {v['why']}")
        return
    if not a.id or not a.out:
        sys.exit("apply_variant.py ID -o OUT.kicad_pcb (or --list)")
    v = variants().get(a.id)
    if v is None:
        sys.exit(f"no variant {a.id}: {', '.join(variants())}")
    try:
        make(v, a.pcbench, a.out)
    except ValueError as e:
        sys.exit(str(e))
    print(f"{a.out}: {a.id}, {v['base']} with {v['patch']}")


if __name__ == "__main__":
    main()
