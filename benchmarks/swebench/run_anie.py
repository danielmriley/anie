#!/usr/bin/env python3
"""SWE-bench Lite generation arm for anie (docs/external_benchmarks/01, PR2).

Per instance: check out a pristine worktree at base_commit, run the anie
debug binary in one-shot print mode against the issue text, then capture
the worktree's git diff as the prediction. Grading happens elsewhere, via
the OFFICIAL SWE-bench evaluator (plan PR4) — this script only generates
predictions.jsonl plus RunMetrics sidecars.

Resumable: instances whose instance_id is already present in --out are
skipped, so a crashed or interrupted run can be re-launched as-is.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

import common

ANIE_BIN = common.BENCHMARKS_DIR.parent / "target" / "debug" / "anie"

# Fixed task preamble (plan: "fix the issue; modify source, not tests").
# Identical wording for every instance and every run so prompt phrasing is
# never a variable between runs.
PREAMBLE = """\
You are working in a checkout of a Python repository. Fix the issue \
described below by editing the repository's source code. Modify source \
files only — do not modify tests, and do not create new test files. \
Make the smallest change that resolves the issue. Your edits to the \
working tree are the deliverable; do not just describe a fix.

Issue:
"""


def run_one(instance: dict, args: argparse.Namespace, run_name: str) -> tuple[str, float, str]:
    """Run anie on one instance. Returns (model_patch, wall_secs, status)."""
    iid = instance["instance_id"]
    worktree = common.ensure_worktree(instance)

    metrics_dir = common.METRICS_DIR / run_name
    metrics_dir.mkdir(parents=True, exist_ok=True)
    metrics_path = metrics_dir / f"{iid}.metrics.json"

    log_dir = common.LOGS_DIR / run_name
    log_dir.mkdir(parents=True, exist_ok=True)
    log_path = log_dir / f"{iid}.log"

    prompt = PREAMBLE + instance["problem_statement"]
    cmd = [
        str(ANIE_BIN),
        "--print",
        f"--harness-mode={args.mode}",
        "--model", args.model,
        "-C", str(worktree),
        "--metrics-out", str(metrics_path),
        prompt,
    ]

    started = time.monotonic()
    status = "ok"
    with log_path.open("w") as log:
        # New session so the wall-clock kill takes out anie's tool
        # subprocesses (bash etc.) too, not just the harness process.
        proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            proc.wait(timeout=args.budget_s)
            if proc.returncode != 0:
                status = f"exit={proc.returncode}"
        except subprocess.TimeoutExpired:
            status = "timeout"
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass  # exited in the window between wait() timing out and the kill
            proc.wait()
    wall = time.monotonic() - started

    # Belt-and-suspenders for the lift matrix: anie now flushes its
    # metrics sidecar incrementally (crash-safe), so a SIGKILL'd run
    # leaves its latest totals. If even that is absent (killed before
    # the first turn boundary), drop a minimal stub so the instance is
    # still counted in the denominator rather than silently dropped.
    if not metrics_path.exists():
        stub = {
            "instance_id": iid,
            "status": status,
            "wall_clock_ms": int(wall * 1000),
            "incomplete": True,
        }
        metrics_path.write_text(json.dumps(stub, indent=2))

    # Whatever is in the tree when the budget expires is the prediction —
    # a partial fix from a killed run is still a legitimate submission.
    patch = common.worktree_diff(worktree)
    return patch, wall, status


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Generate SWE-bench Lite predictions with anie "
        "(deterministic subset: first N of the test split by instance_id).",
    )
    parser.add_argument("--limit", type=int, default=25, help="subset size (default: 25)")
    parser.add_argument("--model", default="gemma4:e4b", help="model id passed to anie (default: gemma4:e4b)")
    parser.add_argument("--mode", default="rlm", choices=["baseline", "current", "rlm"], help="anie --harness-mode (default: rlm)")
    parser.add_argument("--budget-s", type=int, default=900, help="wall-clock kill per instance, seconds (default: 900; the n=25 e4b run timed out 13/25 at 480)")
    parser.add_argument("--out", type=Path, default=None, help="predictions.jsonl path (default: benchmarks/work/predictions/anie__<mode>__<model>.jsonl)")
    args = parser.parse_args()

    if not ANIE_BIN.exists():
        print(f"error: anie binary not found at {ANIE_BIN} (run `cargo build` first)", file=sys.stderr)
        return 1

    model_name = f"anie__{args.mode}__{common.sanitize(args.model)}"
    out = args.out or common.PREDICTIONS_DIR / f"{model_name}.jsonl"
    run_name = out.stem

    instances = common.load_subset(args.limit)
    done = common.load_done_ids(out)
    todo = [i for i in instances if i["instance_id"] not in done]
    print(f"subset: first {args.limit} of {common.DATASET} [{common.SPLIT}] by instance_id")
    print(f"out: {out} ({len(done)} already done, {len(todo)} to run)")

    for n, instance in enumerate(todo, 1):
        iid = instance["instance_id"]
        print(f"[{n}/{len(todo)}] {iid} ...", flush=True)
        try:
            patch, wall, status = run_one(instance, args, run_name)
        except subprocess.CalledProcessError as e:
            print(f"  git failure: {e.stderr.strip() if e.stderr else e}", file=sys.stderr)
            print("  skipping (not recorded; will retry on resume)", file=sys.stderr)
            continue
        common.append_prediction(out, iid, model_name, patch)
        print(f"  {status}, {wall:.0f}s, patch {len(patch)} bytes")

    print(f"done: {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
