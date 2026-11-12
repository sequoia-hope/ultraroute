#!/usr/bin/env python3
"""Are the schematics of PCBench's boards on GitHub? PCBench keeps only each board file; its
metadata names the source repository and commit. This lists the repository's files at that commit
(GitHub's API through `gh`), finds the board file, and looks for schematic files beside it.

    pcbench_schematics.py survey [--ids FILE.json | --tier gate] [--out DIR]
    pcbench_schematics.py fetch ID... [--out DIR]

`survey` writes DIR/survey.json (default ultraroute-lab/pcbench/schematics): per board the board
file's path in the repository, the schematic files in its directory, and whether a root sheet
with the board's own name is among them. It downloads no files. `fetch` downloads a board's
schematic files (and its project and library-cache files) into DIR/ID/. The files keep their
authors' licences, as PCBench's NOTICE.md files say: they stay in the lab, out of the repository.
"""
import argparse
import json
import subprocess
import sys
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parent.parent
LAB = ROOT.parent / "ultraroute-lab"
PCBS = ROOT / "PCBench" / "PCBs"
SCH = (".sch", ".kicad_sch")
EXTRA = (".pro", ".kicad_pro", ".lib", ".kicad_sym", ".net", "sym-lib-table")


def gh(path):
    r = subprocess.run(["gh", "api", path], capture_output=True, text=True)
    if r.returncode != 0:
        return None, (r.stderr or r.stdout).strip().splitlines()[-1][:200] if (r.stderr or r.stdout).strip() else "failed"
    return json.loads(r.stdout), None


def tree_of(repo, commit, cache):
    key = (repo, commit)
    if key not in cache:
        ref = commit
        if not ref:
            info, err = gh(f"repos/{repo}")
            ref = info["default_branch"] if info else None
            if not ref:
                cache[key] = (None, err)
                return cache[key]
        t, err = gh(f"repos/{repo}/git/trees/{ref}?recursive=1")
        cache[key] = ([e["path"] for e in t["tree"] if e["type"] == "blob"], "truncated" if t.get("truncated") else None) if t else (None, err)
    return cache[key]


def look(bid, cache):
    meta = json.loads((PCBS / bid / "metadata.json").read_text())
    repo = meta["source"].split("github.com/")[1].strip("/")
    commit = (meta.get("licenses") or {}).get("source_commit")
    out = {"id": bid, "repo": repo, "commit": commit}
    files, err = tree_of(repo, commit, cache)
    if files is None:
        return {**out, "error": err}
    # The id is "<repository>_<board file's name>"; the name may itself hold underscores.
    name = bid[len(repo.split("/")[1]) + 1:] if bid.startswith(repo.split("/")[1] + "_") else bid.split("_", 1)[-1]
    boards = [f for f in files if f.endswith(".kicad_pcb") and PurePosixPath(f).stem == name]
    if not boards:
        boards = [f for f in files if f.endswith(".kicad_pcb") and PurePosixPath(f).stem.lower() == name.lower()]
    out["repo_schematics"] = sum(f.endswith(SCH) for f in files)
    if not boards:
        return {**out, "error": "board file not found at that commit", "name": name}
    board = min(boards, key=len)
    here = str(PurePosixPath(board).parent)
    beside = [f for f in files if str(PurePosixPath(f).parent) == here]
    under = [f for f in files if here in (".", "") or f.startswith(here + "/")]
    sheets = [f for f in under if f.endswith(SCH)]
    root = [f for f in beside if f.endswith(SCH) and PurePosixPath(f).stem == PurePosixPath(board).stem]
    return {**out, "board": board, "other_boards_named_so": len(boards) - 1, "root_sheet": root[0] if root else None, "sheets": sheets,
            "extra": [f for f in beside if f.endswith(EXTRA)], "note": err}


def ids_of(a):
    if a.ids:
        d = json.loads(Path(a.ids).read_text())
        return sorted(d["boards"] if isinstance(d, dict) and "boards" in d else d)
    return sorted(p.name for p in PCBS.iterdir() if (p / "metadata.json").exists())


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("survey")
    s.add_argument("--ids", help="a JSON list of board ids, or a file with a `boards` object keyed by id (scoreboard.json)")
    s.add_argument("--out", default=str(LAB / "pcbench" / "schematics"))
    f = sub.add_parser("fetch")
    f.add_argument("ids", nargs="+")
    f.add_argument("--out", default=str(LAB / "pcbench" / "schematics"))
    a = ap.parse_args()
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    cache = {}
    if a.cmd == "survey":
        rows = []
        for n, bid in enumerate(ids_of(a)):
            rows.append(look(bid, cache))
            if n % 25 == 24:
                print(f"{n + 1} boards looked at", file=sys.stderr)
        (out / "survey.json").write_text(json.dumps(rows, indent=1))
        found = [r for r in rows if r.get("root_sheet")]
        some = [r for r in rows if not r.get("root_sheet") and r.get("sheets")]
        errs = [r for r in rows if r.get("error")]
        print(f"{len(rows)} boards: root sheet beside the board {len(found)}; other sheets in the board's directory {len(some)}; "
              f"none there {len(rows) - len(found) - len(some) - len(errs)}; could not look {len(errs)}")
        for r in errs[:12]:
            print(f"  {r['id']}: {r['error']}")
        return
    for bid in a.ids:
        r = look(bid, cache)
        if not r.get("sheets"):
            print(f"{bid}: no schematic found ({r.get('error') or 'none beside the board'})")
            continue
        d = out / bid
        d.mkdir(exist_ok=True)
        ref = r["commit"] or "HEAD"
        base = str(PurePosixPath(r["board"]).parent)
        for path in r["sheets"] + r["extra"]:
            rel = path if base in (".", "") else path[len(base) + 1:]
            dest = d / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            got = subprocess.run(["gh", "api", f"repos/{r['repo']}/contents/{path}?ref={ref}", "-H", "Accept: application/vnd.github.raw"], capture_output=True)
            if got.returncode == 0:
                dest.write_bytes(got.stdout)
        (d / "SOURCE.json").write_text(json.dumps(r, indent=1))
        print(f"{bid}: {len(r['sheets'])} sheets, root {r['root_sheet']}, into {d}")


if __name__ == "__main__":
    main()
