"""List the studio's recorded runs in studio/web/public/replays/index.json.

A recording is the `ultraroute route --live` stream of one run, gzipped, named
`<job>--<recipe>.jsonl.gz`.
"""

import gzip
import json
from pathlib import Path

REPLAYS = Path(__file__).resolve().parent.parent / "studio" / "web" / "public" / "replays"


def describe(path: Path) -> dict:
    job, recipe = path.name.removesuffix(".jsonl.gz").split("--", 1)
    last_ms, done = 0, {}
    with gzip.open(path, "rt") as lines:
        for line in lines:
            event = json.loads(line)
            last_ms = event.get("ms", last_ms)
            if event.get("t") == "done":
                done = event
    return {
        "file": path.name,
        "job": job,
        "recipe": recipe,
        "open": done.get("open"),
        "seconds": last_ms / 1000,
        "bytes": path.stat().st_size,
    }


def main() -> None:
    runs = [describe(p) for p in sorted(REPLAYS.glob("*.jsonl.gz"))]
    (REPLAYS / "index.json").write_text(json.dumps(runs, indent=1) + "\n")
    for run in runs:
        print(f"{run['job']} {run['recipe']}: {run['open']} open, {run['seconds']:.0f} s, {run['bytes'] >> 10} KiB")


if __name__ == "__main__":
    main()
