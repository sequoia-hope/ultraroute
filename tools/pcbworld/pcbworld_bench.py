#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Benchmark harness to evaluate ultraroute on PCBWorld's D3 boards and evaluator.

This script lives in ultraroute (MIT) and vendors NO code or data from PCBWorld
(BSD-3-Clause) or PCBWorld-Engine / KiCad / Freerouting (GPLv3). Instead, it
drives a user's external PCBWorld checkout (`--pcbworld-root` / `$PCBWORLD_ROOT`)
and external Python environment (`--pcbworld-python` / `$PCBWORLD_PYTHON`) as
separate child processes on board files on disk.

Subcommands (all resumable across bounded time slices):
  prepare          Run PCBWorld's 5-step PCBench->D3 preparation + d3.json check
  score-reference  Score PCBWorld's Reference + Bare boards under PCBWorld's evaluator
  freerouting      Run & score Freerouting 2.1.0 via PCBWorld's baseline runner
  route            Route PCBWorld's unrouted D3 boards with ultraroute (8 GB cap, CPU clock)
  score            Score ultraroute's routed boards with PCBWorld's evaluator
  pcbench-judge    Judge the exact same routed boards with ultraroute's KiCad DRC judge
  summarize        Aggregate @1 and @5 metrics, failures, and cross-judge comparisons
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import math
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_ROUTER = Path(
    os.environ.get("ULTRAROUTE_BIN", REPO_ROOT / "core/target/release/ultraroute")
)
MEMORY_CAP_CMD = [
    "systemd-run",
    "--user",
    "--scope",
    "-q",
    "-p",
    "MemoryMax=8G",
    "-p",
    "MemorySwapMax=0",
]

# Standalone helper snippet passed via `python -c` to the external PCBWorld Python
# interpreter so PCBWorld's `SubprocEvalPool` / `evaluate_one` runs in its own process.
_EXTERNAL_BATCH_SCORER = r"""
import json, sys, traceback
from pathlib import Path

cfg = json.loads(Path(sys.argv[1]).read_text())
pcbworld_root = Path(cfg["pcbworld_root"]).resolve()
rl_dir = pcbworld_root / "build_rl" / "pcbnew" / "python" / "rl"
pcbnew_dir = pcbworld_root / "build_rl" / "pcbnew"
for p in (pcbworld_root, rl_dir, pcbnew_dir):
    s = str(p)
    if s not in sys.path:
        sys.path.insert(0, s)

jobs = cfg["jobs"]  # list of {"key": str, "pcb": str, "pro": str, "out_json": str}
reward_config = cfg["reward_config"]
check_angle = int(cfg["check_angle"])
workers = int(cfg["workers"])

pairs = [(j["pcb"], j["pro"]) for j in jobs]

def save_one(idx, res):
    out_p = Path(jobs[idx]["out_json"])
    out_p.parent.mkdir(parents=True, exist_ok=True)
    tmp_p = out_p.with_suffix(".json.tmp")
    tmp_p.write_text(json.dumps(res, indent=2))
    tmp_p.replace(out_p)
    ok = "error" not in res
    cp = res.get("clean_pass") if ok else "ERR"
    rout = f"{res.get('routability', 0.0):.3f}" if ok else "-"
    print(f"[{idx+1}/{len(jobs)}] {jobs[idx]['key']}: clean_pass={cp} rout={rout}", flush=True)

if workers > 1 and len(pairs) > 1:
    from pcb_world.vec.subproc_pool import SubprocEvalPool
    with SubprocEvalPool(workers, reward_config=reward_config, check_angle=check_angle) as pool:
        pool.evaluate_many(pairs, on_result=save_one)
else:
    from eval.metrics import evaluate_one
    for i, (pcb, pro) in enumerate(pairs):
        try:
            res = evaluate_one(pcb, pro or None, reward_config_name=reward_config, check_angle=check_angle)
        except Exception as exc:
            res = {"error": f"{type(exc).__name__}: {exc}", "traceback": traceback.format_exc()}
        save_one(i, res)
"""


def default_workers() -> int:
    return max(1, int(os.environ.get("RESERVED_CORES", "4")))


def pcbworld_env(pcbworld_root: Path, data_root: Path, python_bin: Path) -> dict[str, str]:
    env = os.environ.copy()
    build_rl = pcbworld_root / "build_rl"
    py_parts = [
        str(build_rl / "pcbnew" / "python" / "rl"),
        str(build_rl / "pcbnew"),
        str(pcbworld_root),
    ]
    if env.get("PYTHONPATH"):
        py_parts.append(env["PYTHONPATH"])
    env["PYTHONPATH"] = ":".join(py_parts)
    env["PCBWORLD_DATA_ROOT"] = str(data_root)
    env["PCBNEW_PYTHON"] = str(python_bin)
    cli = build_rl / "kicad" / "kicad-cli"
    if cli.is_file():
        env["KICAD_CLI"] = str(cli)
        env["KICAD_RUN_FROM_BUILD_DIR"] = "1"
    # Put python_bin's directory ahead on PATH so child scripts find its tools.
    bin_dir = str(python_bin.resolve().parent)
    env["PATH"] = f"{bin_dir}:{env.get('PATH', '')}"
    return env


def load_d3_boards(pcbworld_root: Path, tier: str, split: str) -> list[str]:
    d3_path = pcbworld_root / "configs" / "datasets" / "d3.json"
    d3 = json.loads(d3_path.read_text())
    if tier == "all":
        # Union of all 677 D3 boards in deterministic order
        out: list[str] = []
        seen: set[str] = set()
        for t in ("easy", "medium", "hard"):
            for b in d3[t]["train"]:
                if b not in seen:
                    seen.add(b)
                    out.append(b)
        return out
    return list(d3[tier][split])


def sorted_boards_dir(data_root: Path) -> Path:
    return data_root / "pcbench" / "exacad_sorted"


def run_external_scorer(
    jobs: list[dict[str, str]],
    *,
    pcbworld_root: Path,
    data_root: Path,
    python_bin: Path,
    reward_config: str,
    check_angle: int,
    workers: int,
) -> None:
    pending = [j for j in jobs if not Path(j["out_json"]).is_file()]
    if not pending:
        print(f"All {len(jobs)} board evaluations already cached for {reward_config}.")
        return
    print(
        f"Scoring {len(pending)}/{len(jobs)} boards via PCBWorld evaluator "
        f"(reward={reward_config}, angle={check_angle}, workers={workers})...",
        flush=True,
    )
    env = pcbworld_env(pcbworld_root, data_root, python_bin)
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as tf:
        json.dump(
            {
                "pcbworld_root": str(pcbworld_root),
                "reward_config": reward_config,
                "check_angle": check_angle,
                "workers": workers,
                "jobs": pending,
            },
            tf,
        )
        cfg_path = Path(tf.name)
    try:
        subprocess.run(
            [str(python_bin), "-c", _EXTERNAL_BATCH_SCORER, str(cfg_path)],
            cwd=str(pcbworld_root),
            env=env,
            check=True,
        )
    finally:
        cfg_path.unlink(missing_ok=True)


# ---------------------------------------------------------------------------
# Step 1: Prepare PCBWorld's D3 dataset + verify d3.json
# ---------------------------------------------------------------------------


def cmd_prepare(args: argparse.Namespace) -> int:
    pcbworld_root = args.pcbworld_root.resolve()
    data_root = args.data_root.resolve()
    runs_dir = args.runs_dir.resolve()
    python_bin = args.pcbworld_python.resolve()
    workers = args.workers

    data_root.mkdir(parents=True, exist_ok=True)
    runs_dir.mkdir(parents=True, exist_ok=True)
    stamp_dir = runs_dir / "prep_stamps"
    stamp_dir.mkdir(parents=True, exist_ok=True)

    env = pcbworld_env(pcbworld_root, data_root, python_bin)
    pcbs_root = (
        args.pcbench_pcbs_root.resolve()
        if args.pcbench_pcbs_root
        else (data_root / "PCBench" / "PCBs")
    )
    if not pcbs_root.is_dir():
        print("[prepare] cloning official https://github.com/PCBench/PCBench.git ...", flush=True)
        subprocess.run(
            ["git", "clone", "--depth", "1", "https://github.com/PCBench/PCBench.git", str(pcbs_root.parent)],
            check=True,
        )
    env["PCBENCH_PCBS_ROOT"] = str(pcbs_root)
    env["PCBENCH_V9_ROOT"] = str(data_root / "pcbench_work" / "v9")
    env["PCBENCH_NEWDRC_OUT"] = str(data_root / "pcbench_work" / "newdrc")
    env["PCBENCH_SORTED_OUT"] = str(data_root / "pcbench" / "exacad_sorted")
    env["PCBENCH_DSN_ROOT"] = str(data_root / "pcbench" / "exacad_sorted_dsn")

    pp = pcbworld_root / "tools" / "datagen" / "pcbench_prep"
    guide_cmd = [
        str(python_bin),
        str(pp / "make_guide.py"),
        "--base-dir",
        env["PCBENCH_NEWDRC_OUT"],
        "--stem",
        "processed_v9",
        "--suffix",
        "_guide_v3",
        "--workers",
        str(workers),
    ]
    newdrc_dir = Path(env["PCBENCH_NEWDRC_OUT"])
    if newdrc_dir.is_dir() and not (stamp_dir / "03_make_guide.done").is_file():
        all_dirs = sorted(d for d in newdrc_dir.iterdir() if d.is_dir())
        done_dirs = [
            d
            for d in all_dirs
            if (d / "processed_v9_guide_v3.kicad_pcb").is_file()
            and (d / "processed_v9_guide_v3.kicad_pro").is_file()
            and (d / "processed_v9_guide_v3_unrouted.kicad_pcb").is_file()
        ]
        if done_dirs and len(done_dirs) <= len(all_dirs):
            # Re-process the 5 most recently written folders as well in case an earlier
            # bounded slice was interrupted mid-write on one of them.
            recent = sorted(done_dirs, key=lambda d: (d / "processed_v9_guide_v3_unrouted.kicad_pcb").stat().st_mtime)[-5:]
            done_safe = set(done_dirs) - set(recent)
            targets = [d.name for d in all_dirs if d not in done_safe]
            if targets:
                guide_cmd.extend(["--targets", *targets])

    steps: list[tuple[str, list[str], set[int]]] = [
        ("01_convert_v9", [str(python_bin), str(pp / "convert_v9.py"), "--workers", str(workers)], {0}),
        ("02_drc_fix_v9", [str(python_bin), str(pp / "drc_fix_v9.py"), "--workers", str(workers)], {0}),
        ("03_make_guide", guide_cmd, {0}),
        ("04_sort_prefix", [str(python_bin), str(pp / "sort_prefix.py")], {0}),
        ("05_make_dsn_orp", [str(python_bin), str(pp / "make_dsn_orp.py")], {0, 3}),
    ]

    for name, cmd, ok_codes in steps:
        stamp = stamp_dir / f"{name}.done"
        if stamp.is_file():
            print(f"[prepare] {name}: already complete ({stamp})")
            continue
        t0 = time.time()
        print(f"[prepare] running {name}: {' '.join(cmd)}", flush=True)
        proc = subprocess.run(cmd, cwd=str(pcbworld_root), env=env)
        if proc.returncode not in ok_codes:
            print(f"[prepare] {name} failed with exit code {proc.returncode}", file=sys.stderr)
            return proc.returncode
        dt = round(time.time() - t0, 2)
        stamp.write_text(json.dumps({"step": name, "returncode": proc.returncode, "seconds": dt}, indent=2))
        print(f"[prepare] {name} finished in {dt}s (rc={proc.returncode})", flush=True)

    # Rebuild d3.json via PCBWorld's experiments/kdd/d3_dataset/build.py and compare
    sorted_out = Path(env["PCBENCH_SORTED_OUT"])
    csv_path = sorted_out.parent / "pcb_characteristics_exacad_sorted.csv"
    if not csv_path.is_file():
        # Check inside sorted_out as well
        alt = sorted_out / "pcb_characteristics_exacad_sorted.csv"
        if alt.is_file():
            csv_path = alt
    rebuilt_path = runs_dir / "d3_rebuilt.json"
    subprocess.run(
        [
            str(python_bin),
            str(pcbworld_root / "experiments" / "kdd" / "d3_dataset" / "build.py"),
            "--csv",
            str(csv_path),
            "--sorted-dir",
            str(sorted_out),
            "--out",
            str(rebuilt_path),
        ],
        cwd=str(pcbworld_root),
        env=env,
        check=True,
    )

    shipped = json.loads((pcbworld_root / "configs" / "datasets" / "d3.json").read_text())
    rebuilt = json.loads(rebuilt_path.read_text())
    diff_report: dict[str, Any] = {"csv": str(csv_path), "splits": {}}
    for tier in ("easy", "medium", "hard"):
        for split in ("train", "test"):
            s_list = shipped[tier][split]
            r_list = rebuilt[tier][split]
            only_shipped = [b for b in s_list if b not in set(r_list)]
            only_rebuilt = [b for b in r_list if b not in set(s_list)]
            present = sum((sorted_out / b / "processed_v9_guide_v3_unrouted.kicad_pcb").is_file() for b in s_list)
            diff_report["splits"][f"{tier}.{split}"] = {
                "shipped_count": len(s_list),
                "rebuilt_count": len(r_list),
                "exact_match": s_list == r_list,
                "only_in_shipped": only_shipped,
                "only_in_rebuilt": only_rebuilt,
                "unrouted_files_present": present,
            }
            print(
                f"[prepare] d3 {tier}.{split}: shipped={len(s_list)} rebuilt={len(r_list)} "
                f"exact_match={s_list == r_list} present={present}/{len(s_list)}"
            )
            if only_shipped or only_rebuilt:
                print(f"          only_shipped={only_shipped} only_rebuilt={only_rebuilt}")

    (runs_dir / "d3_build_diff.json").write_text(json.dumps(diff_report, indent=2))
    return 0


# ---------------------------------------------------------------------------
# Step 2a: Score Reference and Bare boards
# ---------------------------------------------------------------------------


def cmd_score_reference(args: argparse.Namespace) -> int:
    pcbworld_root = args.pcbworld_root.resolve()
    data_root = args.data_root.resolve()
    runs_dir = args.runs_dir.resolve()
    python_bin = args.pcbworld_python.resolve()
    sdir = sorted_boards_dir(data_root)

    boards = load_d3_boards(pcbworld_root, args.tier, args.split)
    ref_out = runs_dir / "reference_eval" / args.reward_config
    bare_out = runs_dir / "bare_eval" / args.reward_config

    ref_jobs = []
    bare_jobs = []
    for b in boards:
        bdir = sdir / b
        pro = str(bdir / "processed_v9_guide_v3.kicad_pro")
        ref_jobs.append(
            {
                "key": f"ref:{b}",
                "pcb": str(bdir / "processed_v9_guide_v3.kicad_pcb"),
                "pro": pro,
                "out_json": str(ref_out / f"{b}.json"),
            }
        )
        bare_jobs.append(
            {
                "key": f"bare:{b}",
                "pcb": str(bdir / "processed_v9_guide_v3_unrouted.kicad_pcb"),
                "pro": pro,
                "out_json": str(bare_out / f"{b}.json"),
            }
        )

    run_external_scorer(
        ref_jobs,
        pcbworld_root=pcbworld_root,
        data_root=data_root,
        python_bin=python_bin,
        reward_config=args.reward_config,
        check_angle=args.check_angle,
        workers=args.workers,
    )
    if not args.skip_bare:
        run_external_scorer(
            bare_jobs,
            pcbworld_root=pcbworld_root,
            data_root=data_root,
            python_bin=python_bin,
            reward_config=args.reward_config,
            check_angle=args.check_angle,
            workers=args.workers,
        )

    # Print aggregate summary for the requested split
    recs = [json.loads((ref_out / f"{b}.json").read_text()) for b in boards]
    n = len(recs)
    cp = sum(1 for r in recs if r.get("clean_pass")) / n
    pot_gain = sum(float(r["potential_gain"]) for r in recs) / n
    rout = sum(float(r["routability"]) for r in recs) / n
    drv = sum(int(r["drv_errors_only_count"]) for r in recs) / n
    drv_prom = sum(int(r["drv_errors_and_promoted_count"]) for r in recs) / n
    wl = sum(float(r["wirelength_mm"]) for r in recs) / n
    via = sum(int(r["via_count"]) for r in recs) / n
    print(
        f"[reference {args.tier}.{args.split} n={n} reward={args.reward_config}] "
        f"CP={cp:.4f} Pot.Gain={pot_gain:.4f} Rout={rout:.4f} "
        f"DRV={drv:.2f} (err+prom={drv_prom:.2f}) WL={wl:.2f} Via={via:.2f}"
    )
    return 0


# ---------------------------------------------------------------------------
# Step 2b: Reproduce Freerouting baseline via PCBWorld's runner
# ---------------------------------------------------------------------------


def cmd_freerouting(args: argparse.Namespace) -> int:
    pcbworld_root = args.pcbworld_root.resolve()
    data_root = args.data_root.resolve()
    runs_dir = args.runs_dir.resolve()
    python_bin = args.pcbworld_python.resolve()
    env = pcbworld_env(pcbworld_root, data_root, python_bin)
    if args.active_processors > 0:
        prev = env.get("JAVA_TOOL_OPTIONS", "")
        opt = f"-XX:ActiveProcessorCount={args.active_processors}"
        env["JAVA_TOOL_OPTIONS"] = f"{prev} {opt}".strip()

    out_root = runs_dir / "freerouting"
    out_root.mkdir(parents=True, exist_ok=True)
    runner = pcbworld_root / "methods" / "baselines" / "rule_based" / "run_rule_based_routers.py"
    expected_n = len(load_d3_boards(pcbworld_root, args.tier, args.split))
    ds_tag = f"PCBench_{args.tier}_{args.split}"

    # Run each rollout into its own subdirectory `rollout_<k>` with `--seeds 1` so an
    # interrupted 20-minute slice preserves every completed rollout pass cleanly.
    for k in range(args.rollout_start, args.rollout_start + args.seeds):
        r_root = out_root / f"rollout_{k}"
        mpath = r_root / ds_tag / "freerouting" / "seed0" / "manifest.json"
        if mpath.is_file():
            try:
                mdata = json.loads(mpath.read_text())
                if len(mdata.get("results", [])) == expected_n:
                    print(f"[freerouting] rollout_{k} ({ds_tag}) already complete ({mpath})")
                    continue
            except (OSError, ValueError):
                pass
        cmd = [
            str(python_bin),
            str(runner),
            "--baseline",
            "freerouting",
            "--dataset",
            "pcbench",
            "--difficulty",
            args.tier,
            "--split",
            args.split,
            "--seeds",
            "1",
            "--workers",
            str(args.workers),
            "--max-passes",
            str(args.max_passes),
            "--timeout",
            str(args.timeout),
            "--reward-config",
            args.reward_config,
            "--check-angle",
            str(args.check_angle),
            "--output-root",
            str(r_root),
        ]
        if args.no_eval:
            cmd.append("--no-eval")
        print(f"[freerouting] rollout_{k} ({ds_tag}): {' '.join(cmd)}", flush=True)
        proc = subprocess.run(cmd, cwd=str(pcbworld_root), env=env)
        # Note: run_rule_based_routers returns 1 if ANY board timed out or errored,
        # which is a valid per-board benchmark outcome when manifest.json was written.
        if not mpath.is_file():
            return proc.returncode or 1
    return 0


# ---------------------------------------------------------------------------
# Step 3 & 4: Route PCBWorld boards with ultraroute
# ---------------------------------------------------------------------------


def _deadlines_from_log(log_text: str) -> tuple[list[str], bool]:
    limited = [
        m
        for line in log_text.splitlines()
        if line.startswith("deadline: time ran out in ")
        for m in line.split(" in ", 1)[1].split(", ")
    ]
    hard = any(line.startswith("deadline: the run's hard limit passed") for line in log_text.splitlines())
    return limited, hard


def _route_one_job(job: dict[str, Any]) -> dict[str, Any]:
    board_id: str = job["board_id"]
    seed: int = job["seed"]
    src_pcb = Path(job["src_pcb"])
    src_pro = Path(job["src_pro"])
    work_dir = Path(job["work_dir"])
    router = Path(job["router"])
    recipe: str = job["recipe"]
    timeout_s: float = job["timeout"]

    meta_path = work_dir / "route_meta.json"
    if meta_path.is_file():
        return json.loads(meta_path.read_text())

    work_dir.mkdir(parents=True, exist_ok=True)
    in_pcb = work_dir / "in.kicad_pcb"
    in_pro = work_dir / "in.kicad_pro"
    out_pcb = work_dir / "out.kicad_pcb"
    out_pro = work_dir / "out.kicad_pro"
    stats_path = work_dir / "stats.json"
    timing_path = work_dir / "time.txt"
    log_path = work_dir / "route.log"

    shutil.copy2(src_pcb, in_pcb)
    shutil.copy2(src_pro, in_pro)
    out_pcb.unlink(missing_ok=True)

    cmd = [
        *MEMORY_CAP_CMD,
        "/usr/bin/time",
        "-f",
        "%e %U %S %M",
        "-o",
        str(timing_path),
        str(router),
        "route",
        str(in_pcb),
        "-o",
        str(out_pcb),
        "--recipe",
        recipe,
        "--seed",
        str(seed),
        "--uuid-seed",
        str(seed),
        "--threads",
        "1",
        "--timeout",
        str(timeout_s),
        "--cpu-clock",
        "--stats",
        str(stats_path),
    ]

    t0 = time.monotonic()
    meta: dict[str, Any] = {
        "board_id": board_id,
        "seed": seed,
        "timeout": timeout_s,
        "recipe": recipe,
    }
    try:
        proc = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=int(timeout_s * 3 + 300),
        )
        stderr_tail = proc.stderr[-200_000:]
        log_path.write_text(stderr_tail)
        meta["returncode"] = proc.returncode
        meta["time_limited"], meta["hard_deadline"] = _deadlines_from_log(stderr_tail)
        if m := re.search(r"^text: (.*)$", stderr_tail, re.M):
            meta["text_helper_status"] = m.group(1)
    except subprocess.TimeoutExpired:
        meta["returncode"] = "hung"
    meta["wall_seconds"] = round(time.monotonic() - t0, 3)

    if timing_path.is_file():
        try:
            parts = timing_path.read_text().split()
            if len(parts) >= 4:
                meta["time_wall_seconds"] = round(float(parts[-4]), 3)
                meta["user_seconds"] = round(float(parts[-3]), 3)
                meta["sys_seconds"] = round(float(parts[-2]), 3)
                meta["cpu_seconds"] = round(float(parts[-3]) + float(parts[-2]), 3)
                meta["peak_mb"] = round(int(parts[-1]) / 1024.0, 1)
        except (OSError, ValueError):
            pass

    if meta.get("returncode") == 0 and out_pcb.is_file():
        shutil.copy2(src_pro, out_pro)
        meta["routed_pcb"] = str(out_pcb)
        meta["routed_pro"] = str(out_pro)
        if stats_path.is_file():
            try:
                st = json.loads(stats_path.read_text())
                meta["router_opens"] = st.get("summary", {}).get("opens")
                meta["router_connections"] = st.get("summary", {}).get("connections")
                meta["router_vias"] = st.get("summary", {}).get("vias")
            except (OSError, ValueError):
                pass
    else:
        out_pcb.unlink(missing_ok=True)

    # Remove the temporary input copy to keep disk usage small; src_pcb remains in data_root.
    in_pcb.unlink(missing_ok=True)
    in_pro.unlink(missing_ok=True)

    tmp_meta = meta_path.with_suffix(".json.tmp")
    tmp_meta.write_text(json.dumps(meta, indent=2))
    tmp_meta.replace(meta_path)
    return meta


def _router_provenance(router: Path, workers: int) -> dict[str, Any]:
    router = router.resolve()
    helper_out = subprocess.run([str(router), "text-helper"], capture_output=True, text=True).stdout.strip()
    helper = Path(helper_out) if helper_out and Path(helper_out).is_file() else None
    if helper is None:
        raise SystemExit(
            f"ERROR: kicad-text helper not found beside {router} (`ultraroute text-helper` returned {helper_out!r}). "
            "Build both `-p ultraroute -p kicad-text`!"
        )
    return {
        "router_binary": str(router),
        "router_sha256": hashlib.sha256(router.read_bytes()).hexdigest(),
        "text_helper": str(helper),
        "text_helper_sha256": hashlib.sha256(helper.read_bytes()).hexdigest(),
        "hostname": platform.node(),
        "workers": workers,
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    }


def cmd_route(args: argparse.Namespace) -> int:
    pcbworld_root = args.pcbworld_root.resolve()
    data_root = args.data_root.resolve()
    runs_dir = args.runs_dir.resolve()
    router = args.router.resolve()
    sdir = sorted_boards_dir(data_root)

    prov = _router_provenance(router, args.workers)
    boards = load_d3_boards(pcbworld_root, args.tier, args.split)
    seeds = [int(s) for s in args.seeds.split(",") if s.strip()]

    ur_root = runs_dir / "ultraroute"
    ur_root.mkdir(parents=True, exist_ok=True)
    (ur_root / "provenance.json").write_text(json.dumps(prov, indent=2))

    jobs: list[dict[str, Any]] = []
    for seed in seeds:
        for b in boards:
            bdir = sdir / b
            work_dir = ur_root / f"seed{seed}" / b
            jobs.append(
                {
                    "board_id": b,
                    "seed": seed,
                    "src_pcb": str(bdir / "processed_v9_guide_v3_unrouted.kicad_pcb"),
                    "src_pro": str(bdir / "processed_v9_guide_v3.kicad_pro"),
                    "work_dir": str(work_dir),
                    "router": str(router),
                    "recipe": args.recipe,
                    "timeout": args.timeout,
                }
            )

    pending = [j for j in jobs if not (Path(j["work_dir"]) / "route_meta.json").is_file()]
    print(
        f"[route] {args.tier}.{args.split} seeds={seeds}: {len(pending)}/{len(jobs)} runs to execute "
        f"on {args.workers} workers (router={router.name} sha256={prov['router_sha256'][:12]})",
        flush=True,
    )
    if not pending:
        return 0

    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
        futs = {pool.submit(_route_one_job, j): j for j in pending}
        for idx, fut in enumerate(concurrent.futures.as_completed(futs), 1):
            m = fut.result()
            print(
                f"[{idx}/{len(pending)}] seed={m['seed']} {m['board_id']}: rc={m.get('returncode')} "
                f"opens={m.get('router_opens')} cpu={m.get('cpu_seconds')}s wall={m.get('wall_seconds')}s "
                f"peak={m.get('peak_mb')}MB",
                flush=True,
            )
    return 0


# ---------------------------------------------------------------------------
# Score ultraroute outputs with PCBWorld's evaluator (+ optional zone refill)
# ---------------------------------------------------------------------------


def _refill_one(src_pcb: Path, src_pro: Path, dst_pcb: Path, python_bin: Path, env: dict[str, str]) -> None:
    if dst_pcb.is_file():
        return
    dst_pcb.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src_pro, dst_pcb.with_suffix(".kicad_pro"))
    code = (
        "import pcbnew, sys; "
        "b = pcbnew.LoadBoard(sys.argv[1]); "
        "pcbnew.ZONE_FILLER(b).Fill(b.Zones()); "
        "pcbnew.SaveBoard(sys.argv[2], b)"
    )
    subprocess.run([str(python_bin), "-c", code, str(src_pcb), str(dst_pcb)], env=env, check=True)


def cmd_score(args: argparse.Namespace) -> int:
    pcbworld_root = args.pcbworld_root.resolve()
    data_root = args.data_root.resolve()
    runs_dir = args.runs_dir.resolve()
    python_bin = args.pcbworld_python.resolve()
    boards = load_d3_boards(pcbworld_root, args.tier, args.split)
    seeds = [int(s) for s in args.seeds.split(",") if s.strip()]
    ur_root = runs_dir / "ultraroute"
    env = pcbworld_env(pcbworld_root, data_root, python_bin)

    jobs: list[dict[str, str]] = []
    for seed in seeds:
        for b in boards:
            wdir = ur_root / f"seed{seed}" / b
            out_pcb = wdir / "out.kicad_pcb"
            out_pro = wdir / "out.kicad_pro"
            if not out_pcb.is_file():
                continue
            if args.refill:
                filled_pcb = wdir / "out.filled.kicad_pcb"
                _refill_one(out_pcb, out_pro, filled_pcb, python_bin, env)
                target_pcb = filled_pcb
                target_pro = filled_pcb.with_suffix(".kicad_pro")
                eval_name = f"pcbworld_eval_refilled_{args.reward_config}.json"
            else:
                target_pcb = out_pcb
                target_pro = out_pro
                eval_name = f"pcbworld_eval_{args.reward_config}.json"
            jobs.append(
                {
                    "key": f"s{seed}:{b}",
                    "pcb": str(target_pcb),
                    "pro": str(target_pro),
                    "out_json": str(wdir / eval_name),
                }
            )

    run_external_scorer(
        jobs,
        pcbworld_root=pcbworld_root,
        data_root=data_root,
        python_bin=python_bin,
        reward_config=args.reward_config,
        check_angle=args.check_angle,
        workers=args.workers,
    )
    return 0


# ---------------------------------------------------------------------------
# Step 5: Judge the exact same routed boards with ultraroute's KiCad DRC judge
# ---------------------------------------------------------------------------

POUR_AFTER_ROUTING = {"starved_thermal"}


def _routed_copper_violation(v: dict[str, Any]) -> bool:
    return v.get("type") in POUR_AFTER_ROUTING or any(
        re.match(r"(Track|Via|Arc) \[", item.get("description", ""))
        for item in v.get("items", [])
    )


def _tally_kicad_drc(report: dict[str, Any]) -> tuple[int, dict[str, int], dict[str, int]]:
    errors: dict[str, int] = {}
    warnings: dict[str, int] = {}
    for v in report.get("violations", []):
        if not _routed_copper_violation(v):
            continue
        bucket = errors if v.get("severity") == "error" else warnings
        kind = v.get("type", "unknown")
        bucket[kind] = bucket.get(kind, 0) + 1
    return len(report.get("unconnected_items", [])), errors, warnings


def _run_kicad_judge_on_pcb(
    pcb: Path,
    pro: Path,
    out_dir: Path,
    stem: str,
    kicad_cli: str,
    python_bin: Path,
    env: dict[str, str],
) -> tuple[int, dict[str, int], dict[str, int]]:
    out_dir.mkdir(parents=True, exist_ok=True)
    filled = out_dir / f"{stem}.pcbench_filled.kicad_pcb"
    report_path = out_dir / f"{stem}.pcbench_drc.json"
    if not report_path.is_file():
        shutil.copy2(pro, filled.with_suffix(".kicad_pro"))
        code = (
            "import pcbnew, sys; "
            "b = pcbnew.LoadBoard(sys.argv[1]); "
            "pcbnew.ZONE_FILLER(b).Fill(b.Zones()); "
            "pcbnew.SaveBoard(sys.argv[2], b)"
        )
        subprocess.run([str(python_bin), "-c", code, str(pcb), str(filled)], env=env, check=True, timeout=600)
        subprocess.run(
            [kicad_cli, "pcb", "drc", "--format", "json", "--severity-all", "-o", str(report_path), str(filled)],
            env=env,
            check=True,
            timeout=600,
        )
        filled.unlink(missing_ok=True)
        filled.with_suffix(".kicad_pro").unlink(missing_ok=True)
    return _tally_kicad_drc(json.loads(report_path.read_text()))


def cmd_pcbench_judge(args: argparse.Namespace) -> int:
    pcbworld_root = args.pcbworld_root.resolve()
    data_root = args.data_root.resolve()
    runs_dir = args.runs_dir.resolve()
    python_bin = args.pcbworld_python.resolve()
    sdir = sorted_boards_dir(data_root)
    boards = load_d3_boards(pcbworld_root, args.tier, args.split)
    seeds = [int(s) for s in args.seeds.split(",") if s.strip()]

    # Use stock /usr/bin/kicad-cli + /usr/bin/python3 (exact match to tools/pcbench.py)
    # when available; otherwise fall back to python_bin's pcbnew environment.
    kicad_cli = args.kicad_cli
    if kicad_cli == "/usr/bin/kicad-cli" and Path("/usr/bin/python3").is_file():
        judge_py = Path("/usr/bin/python3")
        judge_env = os.environ.copy()
    else:
        judge_py = python_bin
        judge_env = pcbworld_env(pcbworld_root, data_root, python_bin)
        if kicad_cli == "/usr/bin/kicad-cli":
            judge_env.pop("KICAD_RUN_FROM_BUILD_DIR", None)

    bare_cache_dir = runs_dir / "pcbench_judge_bare"

    def judge_one(job: tuple[int, str]) -> dict[str, Any]:
        seed, b = job
        wdir = runs_dir / "ultraroute" / f"seed{seed}" / b
        out_json = wdir / "pcbench_judge.json"
        if out_json.is_file():
            return json.loads(out_json.read_text())
        out_pcb = wdir / "out.kicad_pcb"
        src_pro = sdir / b / "processed_v9_guide_v3.kicad_pro"
        bare_pcb = sdir / b / "processed_v9_guide_v3_unrouted.kicad_pcb"
        if not out_pcb.is_file():
            res = {"board_id": b, "seed": seed, "success": False, "reason": "router_failed"}
            out_json.write_text(json.dumps(res, indent=2))
            return res
        _, bare_err, bare_warn = _run_kicad_judge_on_pcb(
            bare_pcb, src_pro, bare_cache_dir / b, "bare", kicad_cli, judge_py, judge_env
        )
        unconn, err, warn = _run_kicad_judge_on_pcb(
            out_pcb, src_pro, wdir, "out", kicad_cli, judge_py, judge_env
        )
        new_err = {t: n - bare_err.get(t, 0) for t, n in err.items() if n > bare_err.get(t, 0)}
        new_warn = {t: n - bare_warn.get(t, 0) for t, n in warn.items() if n > bare_warn.get(t, 0)}
        res = {
            "board_id": b,
            "seed": seed,
            "unconnected": unconn,
            "errors": err,
            "bare_errors": bare_err,
            "new_errors": new_err,
            "new_warnings": new_warn,
            "success": unconn == 0 and not new_err,
            "kicad_cli": kicad_cli,
        }
        out_json.write_text(json.dumps(res, indent=2))
        return res

    all_jobs = [(seed, b) for seed in seeds for b in boards]
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
        futs = {pool.submit(judge_one, j): j for j in all_jobs}
        for idx, fut in enumerate(concurrent.futures.as_completed(futs), 1):
            r = fut.result()
            print(
                f"[{idx}/{len(all_jobs)}] pcbench-judge s{r['seed']} {r['board_id']}: "
                f"success={r.get('success')} open={r.get('unconnected')} new_err={r.get('new_errors')}",
                flush=True,
            )
    return 0


# ---------------------------------------------------------------------------
# Summarize @1 / @5 metrics, Freerouting, Reference, and cross-judge diffs
# ---------------------------------------------------------------------------


PAPER_WL_PENALTY = 0.002
PAPER_VIA_PENALTY = 0.1


def _table3_wv_adjustment(ev: dict[str, Any]) -> float:
    """Adjustment from a reward config's (wl, via) weights to paper Table 3's (0.002, 0.1)."""
    pw = ev.get("phi_weights") or {}
    w_cfg = float(pw.get("wirelength_penalty", 0.0) or 0.0)
    v_cfg = float(pw.get("via_penalty", 0.0) or 0.0)
    wl = float(ev.get("wirelength_mm", 0.0) or 0.0)
    via = float(ev.get("via_count", 0) or 0)
    return -((PAPER_WL_PENALTY - w_cfg) * wl + (PAPER_VIA_PENALTY - v_cfg) * via)


def _load_bare_phi0(boards: list[str], runs_dir: Path, reward_config: str) -> dict[str, float]:
    bare_dir = runs_dir / "bare_eval" / reward_config
    ref_dir = runs_dir / "reference_eval" / reward_config
    bare_phi0: dict[str, float] = {}
    for b in boards:
        bp = bare_dir / f"{b}.json"
        if bp.is_file():
            bd = json.loads(bp.read_text())
            if bd.get("final_potential") is not None:
                bare_phi0[b] = float(bd["final_potential"])
                continue
        rp = ref_dir / f"{b}.json"
        if rp.is_file():
            rd = json.loads(rp.read_text())
            if rd.get("initial_potential") is not None:
                bare_phi0[b] = float(rd["initial_potential"])
    return bare_phi0


def _winner_key(row: dict[str, Any]) -> tuple[float, ...]:
    """Matches PCBWorld's selection_key(..., 'final_potential') using paper Table 3 potential."""
    if "error" in row or row.get("final_potential_table3") is None:
        return (-math.inf, -math.inf, -math.inf, -math.inf, -math.inf, -math.inf)
    return (
        float(row["final_potential_table3"]),
        float(row["routability"]),
        -float(row["drv_errors_only_count"]),
        -float(row["wirelength_mm"]),
        -float(row["via_count"]),
        -float(row.get("rollout_idx", 0)),
    )


def summarize_ultraroute_split(
    boards: list[str],
    seeds: list[int],
    runs_dir: Path,
    reward_config: str,
    refilled: bool = False,
) -> dict[str, Any]:
    ur_root = runs_dir / "ultraroute"
    eval_file = (
        f"pcbworld_eval_refilled_{reward_config}.json"
        if refilled
        else f"pcbworld_eval_{reward_config}.json"
    )
    bare_phi0 = _load_bare_phi0(boards, runs_dir, reward_config)

    per_seed_summary: dict[int, dict[str, Any]] = {}
    by_board_rollouts: dict[str, list[dict[str, Any]]] = {b: [] for b in boards}
    failures: list[dict[str, Any]] = []

    for idx_s, seed in enumerate(seeds):
        cp_list, pot_t3_list, pot_raw_list, pot_true_list, rout_list = [], [], [], [], []
        drv_list, drv_prom_list, wl_list, via_list = [], [], [], []
        wall_list, cpu_list = [], []
        for b in boards:
            wdir = ur_root / f"seed{seed}" / b
            meta = json.loads((wdir / "route_meta.json").read_text())
            ev_path = wdir / eval_file
            ev = json.loads(ev_path.read_text()) if ev_path.is_file() else {"error": "missing_eval"}
            wall_s = float(meta.get("time_wall_seconds", meta.get("wall_seconds", 0.0)))
            cpu_s = float(meta.get("cpu_seconds", wall_s))
            wall_list.append(wall_s)
            cpu_list.append(cpu_s)

            if "error" in ev or meta.get("returncode") != 0:
                rec = {
                    "board_id": b,
                    "seed": seed,
                    "rollout_idx": idx_s,
                    "clean_pass": False,
                    "success": False,
                    "routability": 0.0,
                    "final_potential": bare_phi0.get(b),
                    "final_potential_table3": bare_phi0.get(b),
                    "potential_gain_table3": 0.0,
                    "potential_gain_raw": 0.0,
                    "potential_gain_true_phi0": 0.0,
                    "drv_errors_only_count": None,
                    "drv_errors_and_promoted_count": None,
                    "wirelength_mm": None,
                    "via_count": None,
                    "wall_seconds": wall_s,
                    "cpu_seconds": cpu_s,
                    "error": ev.get("error") or f"rc={meta.get('returncode')}",
                }
                cp_list.append(0.0)
                rout_list.append(0.0)
                pot_t3_list.append(0.0)
                pot_raw_list.append(0.0)
                pot_true_list.append(0.0)
            else:
                fp = float(ev["final_potential"])
                adj = _table3_wv_adjustment(ev)
                fp_t3 = fp + adj
                raw_gain = float(ev["potential_gain"])
                true_gain = (fp - bare_phi0[b]) if b in bare_phi0 else raw_gain
                t3_gain = true_gain + adj
                rec = {
                    "board_id": b,
                    "seed": seed,
                    "rollout_idx": idx_s,
                    "clean_pass": bool(ev["clean_pass"]),
                    "success": bool(ev["success"]),
                    "routability": float(ev["routability"]),
                    "final_potential": fp,
                    "final_potential_table3": fp_t3,
                    "initial_potential_raw": float(ev["initial_potential"]),
                    "initial_potential_bare": bare_phi0.get(b),
                    "potential_gain_table3": t3_gain,
                    "potential_gain_raw": raw_gain,
                    "potential_gain_true_phi0": true_gain,
                    "drv_errors_only_count": int(ev["drv_errors_only_count"]),
                    "drv_errors_and_promoted_count": int(ev["drv_errors_and_promoted_count"]),
                    "drv_errors_by_type": ev.get("drv_breakdown", {}).get("errors_only_by_type", []),
                    "drv_promoted_by_type": ev.get("drv_breakdown", {}).get("errors_and_promoted_by_type", []),
                    "wirelength_mm": float(ev["wirelength_mm"]),
                    "via_count": int(ev["via_count"]),
                    "wall_seconds": wall_s,
                    "cpu_seconds": cpu_s,
                    "time_limited": meta.get("time_limited", []),
                    "hard_deadline": meta.get("hard_deadline", False),
                    "router_opens": meta.get("router_opens"),
                }
                cp_list.append(1.0 if rec["clean_pass"] else 0.0)
                rout_list.append(rec["routability"])
                pot_t3_list.append(t3_gain)
                pot_raw_list.append(raw_gain)
                pot_true_list.append(true_gain)
                drv_list.append(float(rec["drv_errors_only_count"]))
                drv_prom_list.append(float(rec["drv_errors_and_promoted_count"]))
                wl_list.append(rec["wirelength_mm"])
                via_list.append(float(rec["via_count"]))

            if not rec["clean_pass"]:
                failures.append(rec)
            by_board_rollouts[b].append(rec)

        per_seed_summary[seed] = {
            "n_boards": len(boards),
            "clean_pass_count": int(sum(cp_list)),
            "clean_pass": sum(cp_list) / len(boards),
            "potential_gain_table3": sum(pot_t3_list) / len(boards),
            "potential_gain_true_phi0": sum(pot_true_list) / len(boards),
            "potential_gain_raw": sum(pot_raw_list) / len(boards),
            "routability": sum(rout_list) / len(boards),
            "drv_errors_only": sum(drv_list) / len(drv_list) if drv_list else None,
            "drv_errors_and_promoted": sum(drv_prom_list) / len(drv_prom_list) if drv_prom_list else None,
            "wirelength_mm": sum(wl_list) / len(wl_list) if wl_list else None,
            "via_count": sum(via_list) / len(via_list) if via_list else None,
            "wall_seconds_mean": sum(wall_list) / len(boards),
            "cpu_seconds_mean": sum(cpu_list) / len(boards),
            "wall_seconds_total": sum(wall_list),
            "cpu_seconds_total": sum(cpu_list),
        }

    # @K aggregation across `seeds` (selecting winner per board by final_potential,
    # and averaging time across all rollouts of the board, matching PCBWorld Table 3)
    winners = {b: max(by_board_rollouts[b], key=_winner_key) for b in boards}
    w_list = [winners[b] for b in boards]
    w_drv = [float(w["drv_errors_only_count"]) for w in w_list if w["drv_errors_only_count"] is not None]
    w_drv_p = [
        float(w["drv_errors_and_promoted_count"])
        for w in w_list
        if w["drv_errors_and_promoted_count"] is not None
    ]
    w_wl = [float(w["wirelength_mm"]) for w in w_list if w["wirelength_mm"] is not None]
    w_via = [float(w["via_count"]) for w in w_list if w["via_count"] is not None]
    mean_wall_per_board = [
        sum(r["wall_seconds"] for r in by_board_rollouts[b]) / len(by_board_rollouts[b]) for b in boards
    ]
    mean_cpu_per_board = [
        sum(r["cpu_seconds"] for r in by_board_rollouts[b]) / len(by_board_rollouts[b]) for b in boards
    ]

    at_k = {
        "k": len(seeds),
        "n_boards": len(boards),
        "clean_pass_count": sum(1 for w in w_list if w["clean_pass"]),
        "clean_pass": sum(1.0 if w["clean_pass"] else 0.0 for w in w_list) / len(boards),
        "potential_gain_table3": sum(float(w["potential_gain_table3"]) for w in w_list) / len(boards),
        "potential_gain_true_phi0": sum(float(w["potential_gain_true_phi0"]) for w in w_list) / len(boards),
        "potential_gain_raw": sum(float(w["potential_gain_raw"]) for w in w_list) / len(boards),
        "routability": sum(float(w["routability"]) for w in w_list) / len(boards),
        "drv_errors_only": sum(w_drv) / len(w_drv) if w_drv else None,
        "drv_errors_and_promoted": sum(w_drv_p) / len(w_drv_p) if w_drv_p else None,
        "wirelength_mm": sum(w_wl) / len(w_wl) if w_wl else None,
        "via_count": sum(w_via) / len(w_via) if w_via else None,
        "wall_seconds_mean": sum(mean_wall_per_board) / len(boards),
        "cpu_seconds_mean": sum(mean_cpu_per_board) / len(boards),
        "failed_boards_at_k": [w for w in w_list if not w["clean_pass"]],
    }

    # Mean +- sample std across seeds (@1 distribution)
    at_1_mean_std: dict[str, Any] = {}
    if len(seeds) >= 2:
        for metric in (
            "clean_pass",
            "potential_gain_table3",
            "potential_gain_true_phi0",
            "potential_gain_raw",
            "routability",
            "drv_errors_only",
            "wirelength_mm",
            "via_count",
            "wall_seconds_mean",
            "cpu_seconds_mean",
        ):
            vals = [per_seed_summary[s][metric] for s in seeds if per_seed_summary[s][metric] is not None]
            if len(vals) >= 2:
                at_1_mean_std[metric] = {
                    "mean": statistics.mean(vals),
                    "std": statistics.stdev(vals),
                }

    out_split: dict[str, Any] = {
        "seeds": seeds,
        "per_seed": per_seed_summary,
        "at_k": at_k,
    }
    if at_1_mean_std:
        out_split["at_1_mean_std"] = at_1_mean_std
    if len(seeds) > 1:
        out_split["failures_per_rollout"] = failures
    return out_split


def summarize_freerouting_split(
    boards: list[str],
    runs_dir: Path,
    ds_tag: str,
    reward_config: str,
) -> dict[str, Any] | None:
    fr_root = runs_dir / "freerouting"
    if not fr_root.is_dir():
        return None
    bare_phi0 = _load_bare_phi0(boards, runs_dir, reward_config)

    rollouts: list[int] = []
    per_rollout_summary: dict[int, dict[str, Any]] = {}
    by_board: dict[str, list[dict[str, Any]]] = {b: [] for b in boards}

    for rdir in sorted(fr_root.glob("rollout_*"), key=lambda p: int(p.name.split("_")[1])):
        k = int(rdir.name.split("_")[1])
        mpath = rdir / ds_tag / "freerouting" / "seed0" / "manifest.json"
        if not mpath.is_file():
            continue
        mdata = json.loads(mpath.read_text())
        if mdata.get("reward_config") != reward_config:
            continue
        by_id = {r["board_id"]: r for r in mdata.get("results", [])}
        if not all(b in by_id for b in boards):
            continue
        rollouts.append(k)

        cp_l, pot_t3_l, pot_raw_l, pot_true_l, rout_l = [], [], [], [], []
        drv_l, drv_p_l, wl_l, via_l, time_l = [], [], [], [], []
        n_timeout = n_error = 0
        for b in boards:
            r = by_id[b]
            st = r.get("status")
            t_s = float(r.get("elapsed_routing_s", 0.0))
            time_l.append(t_s)
            ev = r.get("eval_metrics") or {}
            ev_log_p = Path(r["eval_log"]) if r.get("eval_log") else None
            if ev_log_p and ev_log_p.is_file():
                try:
                    ev = json.loads(ev_log_p.read_text())
                except (OSError, ValueError):
                    pass
            if st == "timeout":
                n_timeout += 1
            elif st != "ok":
                n_error += 1
            if st != "ok" or not ev or "error" in ev:
                rec = {
                    "board_id": b,
                    "rollout_idx": k,
                    "status": st,
                    "clean_pass": False,
                    "success": False,
                    "routability": 0.0,
                    "final_potential": bare_phi0.get(b),
                    "final_potential_table3": bare_phi0.get(b),
                    "potential_gain_table3": 0.0,
                    "potential_gain_raw": 0.0,
                    "potential_gain_true_phi0": 0.0,
                    "drv_errors_only_count": None,
                    "drv_errors_and_promoted_count": None,
                    "wirelength_mm": None,
                    "via_count": None,
                    "elapsed_routing_s": t_s,
                    "error": r.get("error") or ev.get("error", st),
                }
                cp_l.append(0.0)
                rout_l.append(0.0)
                pot_t3_l.append(0.0)
                pot_raw_l.append(0.0)
                pot_true_l.append(0.0)
            else:
                fp = float(ev["final_potential"])
                adj = _table3_wv_adjustment(ev)
                fp_t3 = fp + adj
                true_g = (fp - bare_phi0[b]) if b in bare_phi0 else float(ev.get("potential_gain", 0.0))
                raw_g = float(ev["potential_gain"]) if "potential_gain" in ev else true_g
                t3_g = true_g + adj
                is_cp = (
                    bool(ev["clean_pass"])
                    if "clean_pass" in ev
                    else bool(ev.get("success") and int(ev["drv_errors_only_count"]) == 0)
                )
                rec = {
                    "board_id": b,
                    "rollout_idx": k,
                    "status": st,
                    "clean_pass": is_cp,
                    "success": bool(ev["success"]),
                    "routability": float(ev["routability"]),
                    "final_potential": fp,
                    "final_potential_table3": fp_t3,
                    "potential_gain_table3": t3_g,
                    "potential_gain_raw": raw_g,
                    "potential_gain_true_phi0": true_g,
                    "drv_errors_only_count": int(ev["drv_errors_only_count"]),
                    "drv_errors_and_promoted_count": int(ev["drv_errors_and_promoted_count"]),
                    "drv_errors_by_type": ev.get("drv_breakdown", {}).get("errors_only_by_type", []),
                    "wirelength_mm": float(ev["wirelength_mm"]),
                    "via_count": int(ev["via_count"]),
                    "elapsed_routing_s": t_s,
                }
                cp_l.append(1.0 if rec["clean_pass"] else 0.0)
                rout_l.append(rec["routability"])
                pot_t3_l.append(t3_g)
                pot_raw_l.append(raw_g)
                pot_true_l.append(true_g)
                drv_l.append(float(rec["drv_errors_only_count"]))
                drv_p_l.append(float(rec["drv_errors_and_promoted_count"]))
                wl_l.append(rec["wirelength_mm"])
                via_l.append(float(rec["via_count"]))
            by_board[b].append(rec)

        per_rollout_summary[k] = {
            "n_boards": len(boards),
            "clean_pass_count": int(sum(cp_l)),
            "clean_pass": sum(cp_l) / len(boards),
            "potential_gain_table3": sum(pot_t3_l) / len(boards),
            "potential_gain_true_phi0": sum(pot_true_l) / len(boards),
            "potential_gain_raw": sum(pot_raw_l) / len(boards),
            "routability": sum(rout_l) / len(boards),
            "drv_errors_only": sum(drv_l) / len(drv_l) if drv_l else None,
            "drv_errors_and_promoted": sum(drv_p_l) / len(drv_p_l) if drv_p_l else None,
            "wirelength_mm": sum(wl_l) / len(wl_l) if wl_l else None,
            "via_count": sum(via_l) / len(via_l) if via_l else None,
            "time_s_mean": sum(time_l) / len(boards),
            "timeouts": n_timeout,
            "errors": n_error,
        }

    if not rollouts:
        return None

    winners = [max(by_board[b], key=_winner_key) for b in boards]
    w_drv = [float(w["drv_errors_only_count"]) for w in winners if w["drv_errors_only_count"] is not None]
    w_drv_p = [float(w["drv_errors_and_promoted_count"]) for w in winners if w["drv_errors_and_promoted_count"] is not None]
    w_wl = [float(w["wirelength_mm"]) for w in winners if w["wirelength_mm"] is not None]
    w_via = [float(w["via_count"]) for w in winners if w["via_count"] is not None]
    mean_t = [sum(r["elapsed_routing_s"] for r in by_board[b]) / len(by_board[b]) for b in boards]

    at_1_mean_std: dict[str, Any] = {}
    if len(rollouts) >= 2:
        for m in (
            "clean_pass",
            "potential_gain_table3",
            "potential_gain_true_phi0",
            "routability",
            "drv_errors_only",
            "wirelength_mm",
            "via_count",
            "time_s_mean",
        ):
            vals = [per_rollout_summary[k][m] for k in rollouts if per_rollout_summary[k][m] is not None]
            if len(vals) >= 2:
                at_1_mean_std[m] = {"mean": statistics.mean(vals), "std": statistics.stdev(vals)}

    return {
        "rollouts": rollouts,
        "per_rollout": per_rollout_summary,
        "at_1_mean_std": at_1_mean_std,
        "at_k": {
            "k": len(rollouts),
            "n_boards": len(boards),
            "clean_pass_count": sum(1 for w in winners if w["clean_pass"]),
            "clean_pass": sum(1.0 if w["clean_pass"] else 0.0 for w in winners) / len(boards),
            "potential_gain_table3": sum(float(w["potential_gain_table3"]) for w in winners) / len(boards),
            "potential_gain_true_phi0": sum(float(w["potential_gain_true_phi0"]) for w in winners) / len(boards),
            "potential_gain_raw": sum(float(w["potential_gain_raw"]) for w in winners) / len(boards),
            "routability": sum(float(w["routability"]) for w in winners) / len(boards),
            "drv_errors_only": sum(w_drv) / len(w_drv) if w_drv else None,
            "drv_errors_and_promoted": sum(w_drv_p) / len(w_drv_p) if w_drv_p else None,
            "wirelength_mm": sum(w_wl) / len(w_wl) if w_wl else None,
            "via_count": sum(w_via) / len(w_via) if w_via else None,
            "time_s_mean": sum(mean_t) / len(boards),
            "failed_boards_at_k": [w for w in winners if not w["clean_pass"]],
        },
    }


def summarize_reference_split(boards: list[str], runs_dir: Path, reward_config: str) -> dict[str, Any] | None:
    ref_dir = runs_dir / "reference_eval" / reward_config
    if not all((ref_dir / f"{b}.json").is_file() for b in boards):
        return None
    recs = [json.loads((ref_dir / f"{b}.json").read_text()) for b in boards]
    n = len(recs)
    return {
        "n_boards": n,
        "clean_pass_count": sum(1 for r in recs if r.get("clean_pass")),
        "clean_pass": sum(1.0 if r.get("clean_pass") else 0.0 for r in recs) / n,
        "potential_gain_table3": sum(float(r["potential_gain"]) + _table3_wv_adjustment(r) for r in recs) / n,
        "potential_gain_no_wv": sum(float(r["potential_gain"]) for r in recs) / n,
        "routability": sum(float(r["routability"]) for r in recs) / n,
        "drv_errors_only": sum(int(r["drv_errors_only_count"]) for r in recs) / n,
        "drv_errors_and_promoted": sum(int(r["drv_errors_and_promoted_count"]) for r in recs) / n,
        "wirelength_mm": sum(float(r["wirelength_mm"]) for r in recs) / n,
        "via_count": sum(int(r["via_count"]) for r in recs) / n,
    }


def summarize_cross_judge(boards: list[str], seed: int, runs_dir: Path, reward_config: str) -> dict[str, Any] | None:
    ur_root = runs_dir / "ultraroute" / f"seed{seed}"
    if not all((ur_root / b / "pcbench_judge.json").is_file() for b in boards):
        return None
    disagreements = []
    both_clean = both_fail = pcbworld_only = pcbench_only = 0
    for b in boards:
        pj = json.loads((ur_root / b / "pcbench_judge.json").read_text())
        ev_p = ur_root / b / f"pcbworld_eval_{reward_config}.json"
        if not ev_p.is_file():
            continue
        ev = json.loads(ev_p.read_text())
        pw_ok = bool(ev.get("clean_pass"))
        pb_ok = bool(pj.get("success"))
        if pw_ok and pb_ok:
            both_clean += 1
        elif not pw_ok and not pb_ok:
            both_fail += 1
        elif pw_ok and not pb_ok:
            pcbworld_only += 1
            disagreements.append(
                {
                    "board_id": b,
                    "seed": seed,
                    "pcbworld_clean_pass": True,
                    "pcbench_success": False,
                    "pcbworld_drv_errors": ev.get("drv_breakdown", {}).get("errors_only_by_type", []),
                    "pcbworld_drv_promoted": ev.get("drv_breakdown", {}).get("errors_and_promoted_by_type", []),
                    "pcbench_unconnected": pj.get("unconnected"),
                    "pcbench_new_errors": pj.get("new_errors"),
                }
            )
        else:
            pcbench_only += 1
            disagreements.append(
                {
                    "board_id": b,
                    "seed": seed,
                    "pcbworld_clean_pass": False,
                    "pcbench_success": True,
                    "pcbworld_success": ev.get("success"),
                    "pcbworld_drv_errors": ev.get("drv_breakdown", {}).get("errors_only_by_type", []),
                    "pcbench_unconnected": pj.get("unconnected"),
                    "pcbench_new_errors": pj.get("new_errors"),
                }
            )
    return {
        "n_boards": len(boards),
        "both_clean": both_clean,
        "both_fail": both_fail,
        "pcbworld_clean_pcbench_fail": pcbworld_only,
        "pcbench_clean_pcbworld_fail": pcbench_only,
        "disagreements": disagreements,
    }


def cmd_summarize(args: argparse.Namespace) -> int:
    pcbworld_root = args.pcbworld_root.resolve()
    runs_dir = args.runs_dir.resolve()
    out_json = args.out.resolve()

    summary: dict[str, Any] = {
        "reward_config": args.reward_config,
        "reference": {},
        "freerouting": {},
        "ultraroute": {},
        "cross_judge_seed1": {},
    }
    prov_path = runs_dir / "ultraroute" / "provenance.json"
    if prov_path.is_file():
        summary["ultraroute_provenance"] = json.loads(prov_path.read_text())
    diff_path = runs_dir / "d3_build_diff.json"
    if diff_path.is_file():
        summary["d3_build_diff"] = json.loads(diff_path.read_text())

    for label, tier, split, ds_tag in [
        ("D3-A", "easy", "test", "PCBench_easy_test"),
        ("D3-B", "medium", "test", "PCBench_medium_test"),
        ("D3-C", "hard", "test", "PCBench_hard_test"),
        ("medium_all", "medium", "train", None),
        ("hard_all", "hard", "train", None),
    ]:
        boards = load_d3_boards(pcbworld_root, tier, split)
        ref = summarize_reference_split(boards, runs_dir, args.reward_config)
        if ref:
            summary["reference"][label] = ref
            print(
                f"[summarize] Reference {label}: CP={ref['clean_pass']:.4f} "
                f"Pot(table3)={ref['potential_gain_table3']:.2f} "
                f"Rout={ref['routability']:.4f} WL={ref['wirelength_mm']:.2f} Via={ref['via_count']:.2f}"
            )
        if ds_tag:
            fr = summarize_freerouting_split(boards, runs_dir, ds_tag, args.reward_config)
            if fr:
                summary["freerouting"][label] = fr
                print(
                    f"[summarize] Freerouting {label} (k={fr['at_k']['k']}): "
                    f"@k CP={fr['at_k']['clean_pass']:.4f} "
                    f"Pot(table3)={fr['at_k']['potential_gain_table3']:.2f} "
                    f"Rout={fr['at_k']['routability']:.4f} Time={fr['at_k']['time_s_mean']:.2f}s"
                )
        cj = summarize_cross_judge(boards, 1, runs_dir, args.reward_config)
        if cj:
            summary["cross_judge_seed1"][label] = cj

    for label, tier, split, seeds in [
        ("D3-A", "easy", "test", [1, 2, 3, 4, 5]),
        ("D3-B", "medium", "test", [1, 2, 3, 4, 5]),
        ("D3-C", "hard", "test", [1, 2, 3, 4, 5]),
        ("medium_all", "medium", "train", [1]),
        ("hard_all", "hard", "train", [1]),
    ]:
        boards = load_d3_boards(pcbworld_root, tier, split)
        avail_seeds = [
            s
            for s in seeds
            if all((runs_dir / "ultraroute" / f"seed{s}" / b / "route_meta.json").is_file() for b in boards)
        ]
        if not avail_seeds:
            continue
        summary["ultraroute"][label] = summarize_ultraroute_split(
            boards, avail_seeds, runs_dir, args.reward_config, refilled=False
        )
        print(
            f"[summarize] {label} (seeds={avail_seeds}): "
            f"seed1 CP={summary['ultraroute'][label]['per_seed'][avail_seeds[0]]['clean_pass']:.4f} "
            f"@{len(avail_seeds)} CP={summary['ultraroute'][label]['at_k']['clean_pass']:.4f} "
            f"Pot(table3)={summary['ultraroute'][label]['at_k']['potential_gain_table3']:.2f} "
            f"Pot(no_wv)={summary['ultraroute'][label]['at_k']['potential_gain_true_phi0']:.2f} "
            f"Rout={summary['ultraroute'][label]['at_k']['routability']:.4f}"
        )
        ref_eval = f"pcbworld_eval_refilled_{args.reward_config}.json"
        avail_refill = [
            s
            for s in seeds
            if all((runs_dir / "ultraroute" / f"seed{s}" / b / ref_eval).is_file() for b in boards)
        ]
        if avail_refill:
            summary.setdefault("ultraroute_refilled", {})[label] = summarize_ultraroute_split(
                boards, avail_refill, runs_dir, args.reward_config, refilled=True
            )

    out_json.parent.mkdir(parents=True, exist_ok=True)
    out_json.write_text(json.dumps(summary, indent=2))
    print(f"[summarize] wrote {out_json}")
    return 0


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument(
        "--pcbworld-root",
        type=Path,
        default=Path(os.environ.get("PCBWORLD_ROOT", Path.home() / "ab" / "pcbworld")),
        help="Path to external PCBWorld checkout.",
    )
    p.add_argument(
        "--data-root",
        type=Path,
        default=Path(os.environ.get("PCBWORLD_DATA_ROOT", Path.home() / "ab" / "pcbworld-runs" / "datasets")),
        help="Path to PCBWORLD_DATA_ROOT.",
    )
    p.add_argument(
        "--runs-dir",
        type=Path,
        default=Path(os.environ.get("PCBWORLD_RUNS_DIR", Path.home() / "ab" / "pcbworld-runs")),
        help="Directory for run outputs and cached evaluations.",
    )
    p.add_argument(
        "--pcbworld-python",
        type=Path,
        default=Path(
            os.environ.get(
                "PCBWORLD_PYTHON",
                Path.home() / "ab" / "pcbworld-runs" / "mamba" / "envs" / "pcbworld" / "bin" / "python3",
            )
        ),
        help="Python interpreter for external PCBWorld scripts/engine.",
    )
    p.add_argument("--workers", type=int, default=default_workers())

    sub = p.add_subparsers(dest="cmd", required=True)

    sp = sub.add_parser("prepare", help="Run PCBWorld's PCBench->D3 prep and verify d3.json.")
    sp.add_argument("--pcbench-pcbs-root", type=Path, default=None)
    sp.set_defaults(func=cmd_prepare)

    sr = sub.add_parser("score-reference", help="Score PCBWorld Reference + Bare boards.")
    sr.add_argument("--tier", choices=("easy", "medium", "hard", "all"), default="easy")
    sr.add_argument("--split", choices=("train", "test"), default="test")
    sr.add_argument("--reward-config", default="drc_dense_promoted")
    sr.add_argument("--check-angle", type=int, default=45)
    sr.add_argument("--skip-bare", action="store_true")
    sr.set_defaults(func=cmd_score_reference)

    sf = sub.add_parser("freerouting", help="Run Freerouting 2.1.0 via PCBWorld's baseline runner.")
    sf.add_argument("--tier", choices=("easy", "medium", "hard"), default="easy")
    sf.add_argument("--split", choices=("train", "test"), default="test")
    sf.add_argument("--seeds", type=int, default=5)
    sf.add_argument("--max-passes", type=int, default=10)
    sf.add_argument("--timeout", type=int, default=1800)
    sf.add_argument("--reward-config", default="drc_dense_promoted")
    sf.add_argument("--check-angle", type=int, default=45)
    sf.add_argument("--active-processors", type=int, default=0)
    sf.add_argument("--rollout-start", type=int, default=0, help="Starting rollout index (default: 0).")
    sf.add_argument("--no-eval", action="store_true")
    sf.set_defaults(func=cmd_freerouting)

    sroute = sub.add_parser("route", help="Route PCBWorld unrouted boards with ultraroute.")
    sroute.add_argument("--router", type=Path, default=DEFAULT_ROUTER)
    sroute.add_argument("--tier", choices=("easy", "medium", "hard", "all"), required=True)
    sroute.add_argument("--split", choices=("train", "test"), default="test")
    sroute.add_argument("--seeds", default="1", help="Comma-separated seed list, e.g. 1,2,3,4,5")
    sroute.add_argument("--recipe", default="default")
    sroute.add_argument("--timeout", type=float, default=120.0)
    sroute.set_defaults(func=cmd_route)

    sscore = sub.add_parser("score", help="Score ultraroute outputs with PCBWorld's evaluator.")
    sscore.add_argument("--tier", choices=("easy", "medium", "hard", "all"), required=True)
    sscore.add_argument("--split", choices=("train", "test"), default="test")
    sscore.add_argument("--seeds", default="1")
    sscore.add_argument("--reward-config", default="drc_dense_promoted")
    sscore.add_argument("--check-angle", type=int, default=45)
    sscore.add_argument("--refill", action="store_true", help="Refill copper zones before scoring.")
    sscore.set_defaults(func=cmd_score)

    spj = sub.add_parser("pcbench-judge", help="Judge ultraroute outputs with ultraroute's KiCad DRC judge.")
    spj.add_argument("--tier", choices=("easy", "medium", "hard", "all"), required=True)
    spj.add_argument("--split", choices=("train", "test"), default="test")
    spj.add_argument("--seeds", default="1")
    spj.add_argument("--kicad-cli", default="/usr/bin/kicad-cli")
    spj.set_defaults(func=cmd_pcbench_judge)

    ssum = sub.add_parser("summarize", help="Aggregate @1/@5 metrics into summary JSON.")
    ssum.add_argument("--reward-config", default="drc_dense_promoted")
    ssum.add_argument("--out", type=Path, default=REPO_ROOT / "docs/research/pcbworld_eval.json")
    ssum.set_defaults(func=cmd_summarize)

    return p


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
