# PCBWorld D3 Evaluation Harness (`tools/pcbworld/`)

MIT-licensed driver scripts for reproducing PCBWorld's D3 benchmark dataset and
scoring `ultraroute` (and PCBWorld's Reference / Freerouting 2.1.0 baselines)
under **PCBWorld's own evaluator** (`eval.metrics.evaluate_one`).

## Licensing boundary

`ultraroute` is MIT-licensed. Nothing from `LGAI-Research/PCBWorld` (BSD-3-Clause)
or `PCBWorld-Engine` / KiCad / Freerouting (GPLv3) is copied, linked, or vendored
into this repository. [`pcbworld_bench.py`](pcbworld_bench.py) uses only the
Python standard library and invokes a user's external `PCBWorld` checkout as
separate subprocesses over `.kicad_pcb` / `.kicad_pro` files on disk.

## Prerequisites

1. Build `ultraroute` and `kicad-text` in `core/`:
   ```bash
   cd core
   CARGO_BUILD_BUILD_DIR=$PWD/target/build cargo build --release -p ultraroute -p kicad-text
   ```
2. Clone `LGAI-Research/PCBWorld` (tested at `v1.1.1`, commit `1989a63`) to an
   external directory (`$PCBWORLD_ROOT`), build its `PCBWorld-Engine` submodule
   with `BUILD_CLI=1 BUILD_PCBNEW=1 bash engine/build_rl_router.sh --conda`
   (note: pin `nng<2` and `occt<8` on conda-forge for KiCad 9.0.8), and place
   `freerouting-2.1.0.jar` at `$PCBWORLD_ROOT/external/freerouting/freerouting-2.1.0.jar`
   when reproducing the Freerouting baseline.

## End-to-end reproduction

Export paths (or pass `--pcbworld-root`, `--data-root`, `--runs-dir`,
`--pcbworld-python`, `--router`):

```bash
export PCBWORLD_ROOT=$HOME/ab/pcbworld
export PCBWORLD_DATA_ROOT=$HOME/ab/pcbworld-runs/datasets
export PCBWORLD_RUNS_DIR=$HOME/ab/pcbworld-runs
export PCBWORLD_PYTHON=$HOME/ab/pcbworld-runs/mamba/envs/pcbworld/bin/python3
export ULTRAROUTE_BIN=$PWD/core/target/release/ultraroute
```

1. **Build PCBWorld's D3 boards from PCBench and verify `configs/datasets/d3.json`:**
   ```bash
   python3 tools/pcbworld/pcbworld_bench.py prepare --workers 5 \
       --pcbench-pcbs-root $HOME/Software/ultraroute/PCBench/PCBs
   ```

2. **Verify the evaluator on Reference and Freerouting 2.1.0 (`-mp 10`):**
   ```bash
   # Reference + bare-board baseline potentials (Table 3 uses drc_dense_promoted)
   python3 tools/pcbworld/pcbworld_bench.py score-reference --tier all --reward-config drc_dense_promoted
   python3 tools/pcbworld/pcbworld_bench.py score-reference --tier easy --split test --reward-config drc_dense_promoted
   python3 tools/pcbworld/pcbworld_bench.py score-reference --tier medium --split test --reward-config drc_dense_promoted

   # Freerouting 2.1.0 on D3-A (99 boards, 5 rollouts for @5 and @1)
   python3 tools/pcbworld/pcbworld_bench.py freerouting --tier easy --split test --seeds 5 --workers 2 \
       --reward-config drc_dense_promoted
   ```

3. **Route PCBWorld's unrouted D3 boards with `ultraroute` (`--threads 1 --cpu-clock --recipe default --timeout 120`, 8 GB cap):**
   ```bash
   # D3-A (99), D3-B (10), D3-C (10) across 5 seeds (@1 + @5)
   for tier in easy medium hard; do
       python3 tools/pcbworld/pcbworld_bench.py route --tier "$tier" --split test --seeds 1,2,3,4,5 --workers 5
       python3 tools/pcbworld/pcbworld_bench.py score --tier "$tier" --split test --seeds 1,2,3,4,5 --workers 5 \
           --reward-config drc_dense_promoted
   done

   # Full medium (286) and hard (292) pools at seed 1
   python3 tools/pcbworld/pcbworld_bench.py route --tier medium --split train --seeds 1 --workers 5
   python3 tools/pcbworld/pcbworld_bench.py score --tier medium --split train --seeds 1 --workers 5
   python3 tools/pcbworld/pcbworld_bench.py route --tier hard --split train --seeds 1 --workers 5
   python3 tools/pcbworld/pcbworld_bench.py score --tier hard --split train --seeds 1 --workers 5
   ```

4. **Cross-judge the exact same routed boards with `ultraroute`'s KiCad DRC judge (`pcbench` rules) and write summary JSON:**
   ```bash
   for tier in easy medium hard; do
       python3 tools/pcbworld/pcbworld_bench.py pcbench-judge --tier "$tier" --split test --seeds 1
   done
   python3 tools/pcbworld/pcbworld_bench.py summarize --out docs/research/pcbworld_eval.json
   ```

Every subcommand skips already-completed per-board artifacts under `$PCBWORLD_RUNS_DIR`,
so long runs can be split safely across bounded `reserve` time slices.
