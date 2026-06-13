#!/usr/bin/env python3
"""Join a SWE-bench grader report with our metrics sidecars (01, PR4).

One invocation per arm: takes the official evaluator's report JSON (sb-cli
or `swebench.harness.run_evaluation` — both emit `resolved_ids`), the arm's
predictions.jsonl, and its metrics sidecar dir, and prints one markdown
lift-matrix row: resolved rate, mean tokens/instance, mean turns, tool
calls/failures. Run with --header first, then once per arm, and paste the
rows together:

    harvest.py --header
    harvest.py --report <anie report> \\
        --predictions work/predictions/anie__rlm__gemma4-e4b.jsonl \\
        --metrics-dir work/metrics/anie__rlm__gemma4-e4b
    harvest.py --report <control report> \\
        --predictions work/predictions/mini-swe-agent__gemma4-e4b.jsonl \\
        --metrics-dir work/metrics/mini-swe-agent__gemma4-e4b

Both sidecar shapes share the paths read here (tokens.total_tokens, turns,
tools.calls, tools.failures): anie's RunMetrics (run_metrics.rs) and the
control arm's swebench-control-arm-v1 (run_control.py) were aligned on
purpose. `--fake-report` runs a self-contained self-test of the join with
synthetic sidecars of both shapes — no graded run needed.
"""

from __future__ import annotations

import argparse
import json
import sys
import tempfile
from pathlib import Path

HEADER = (
    "| arm | instances | resolved | resolved % | mean tokens | mean turns "
    "| tool calls | tool failures |\n"
    "|---|---|---|---|---|---|---|---|"
)


def load_predictions(path: Path) -> tuple[str, list[str]]:
    """(model_name_or_path, instance_ids) from a predictions jsonl."""
    arm = ""
    ids: list[str] = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        row = json.loads(line)
        ids.append(row["instance_id"])
        arm = row.get("model_name_or_path", arm)
    if not ids:
        sys.exit(f"error: no prediction rows in {path}")
    return arm, ids


def resolved_ids_from_report(report: dict) -> set[str]:
    """The official report schema carries `resolved_ids` (list of instance_ids).

    sb-cli's report mirrors the local harness report. A report without the
    key is a schema we don't know — fail loudly rather than print a 0% row.
    """
    if "resolved_ids" not in report:
        sys.exit(
            "error: report has no 'resolved_ids' key — not an official "
            "SWE-bench evaluator report? (see grade.md)"
        )
    return set(report["resolved_ids"])


def load_sidecars(metrics_dir: Path, instance_ids: list[str]) -> list[dict]:
    sidecars = []
    for iid in instance_ids:
        path = metrics_dir / f"{iid}.metrics.json"
        if path.exists():
            sidecars.append(json.loads(path.read_text()))
    return sidecars


def lift_row(arm: str, instance_ids: list[str], resolved: set[str], sidecars: list[dict]) -> str:
    n = len(instance_ids)
    n_resolved = sum(1 for iid in instance_ids if iid in resolved)
    k = len(sidecars)
    mean_tokens = sum(s["tokens"]["total_tokens"] for s in sidecars) / k if k else 0.0
    mean_turns = sum(s["turns"] for s in sidecars) / k if k else 0.0
    tool_calls = sum(s["tools"]["calls"] for s in sidecars)
    tool_failures = sum(s["tools"]["failures"] for s in sidecars)
    sidecar_note = "" if k == n else f" ({k}/{n} sidecars)"
    fail_pct = f" ({100.0 * tool_failures / tool_calls:.0f}%)" if tool_calls else ""
    return (
        f"| {arm} | {n} | {n_resolved} | {100.0 * n_resolved / n:.1f}% "
        f"| {mean_tokens:.0f}{sidecar_note} | {mean_turns:.1f} "
        f"| {tool_calls} | {tool_failures}{fail_pct} |"
    )


# ---------------------------------------------------------------------------
# Self-test: synthetic report + one sidecar of each arm's shape, so the join
# is exercised end to end without a graded run.
# ---------------------------------------------------------------------------

FAKE_ANIE_SIDECAR = {  # RunMetrics shape (the fields harvest reads)
    "schema_version": 5,
    "harness_mode": "rlm",
    "turns": 10,
    "tokens": {"input_tokens": 60000, "output_tokens": 3000, "total_tokens": 63000},
    "tools": {"calls": 9, "failures": 1},
}

FAKE_CONTROL_SIDECAR = {  # swebench-control-arm-v1 shape (run_control.py)
    "schema": "swebench-control-arm-v1",
    "harness": "mini-swe-agent",
    "turns": 11,
    "tokens": {"input_tokens": 36000, "output_tokens": 2000, "total_tokens": 38000},
    "tools": {"calls": 6, "failures": 2},
    "format_errors": 5,
}


def self_test() -> int:
    with tempfile.TemporaryDirectory(prefix="harvest-selftest-") as tmp:
        root = Path(tmp)
        predictions = root / "predictions.jsonl"
        predictions.write_text(
            "".join(
                json.dumps({"instance_id": iid, "model_name_or_path": "fake__arm", "model_patch": "diff"}) + "\n"
                for iid in ("repo__a-1", "repo__b-2", "repo__c-3")
            )
        )
        metrics_dir = root / "metrics"
        metrics_dir.mkdir()
        (metrics_dir / "repo__a-1.metrics.json").write_text(json.dumps(FAKE_ANIE_SIDECAR))
        (metrics_dir / "repo__b-2.metrics.json").write_text(json.dumps(FAKE_CONTROL_SIDECAR))
        # repo__c-3 deliberately has no sidecar -> exercises the k/n note.
        report = root / "report.json"
        report.write_text(
            json.dumps(
                {
                    "total_instances": 300,
                    "submitted_instances": 3,
                    "resolved_instances": 1,
                    "resolved_ids": ["repo__a-1"],
                    "unresolved_ids": ["repo__b-2", "repo__c-3"],
                    "error_ids": [],
                }
            )
        )

        arm, ids = load_predictions(predictions)
        resolved = resolved_ids_from_report(json.loads(report.read_text()))
        row = lift_row(arm, ids, resolved, load_sidecars(metrics_dir, ids))
        print(HEADER)
        print(row)

        expected = "| fake__arm | 3 | 1 | 33.3% | 50500 (2/3 sidecars) | 10.5 | 15 | 3 (20%) |"
        if row != expected:
            print(f"self-test FAILED:\n  expected: {expected}\n  got:      {row}", file=sys.stderr)
            return 1
    print("self-test passed")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Join an official SWE-bench grader report with metrics "
        "sidecars into one markdown lift-matrix row per arm.",
    )
    parser.add_argument("--report", type=Path, help="grader report JSON (sb-cli or run_evaluation; must carry resolved_ids)")
    parser.add_argument("--predictions", type=Path, help="the arm's predictions.jsonl (defines the instance set)")
    parser.add_argument("--metrics-dir", type=Path, help="the arm's sidecar dir (benchmarks/work/metrics/<run>)")
    parser.add_argument("--arm", default=None, help="row label (default: model_name_or_path from predictions)")
    parser.add_argument("--header", action="store_true", help="print the markdown table header and exit")
    parser.add_argument("--fake-report", action="store_true", help="self-test the join with synthetic inputs and exit")
    args = parser.parse_args()

    if args.fake_report:
        return self_test()
    if args.header:
        print(HEADER)
        return 0
    if not (args.report and args.predictions and args.metrics_dir):
        parser.error("--report, --predictions and --metrics-dir are required (or use --header / --fake-report)")

    arm, ids = load_predictions(args.predictions)
    resolved = resolved_ids_from_report(json.loads(args.report.read_text()))
    sidecars = load_sidecars(args.metrics_dir, ids)
    print(lift_row(args.arm or arm, ids, resolved, sidecars))
    return 0


if __name__ == "__main__":
    sys.exit(main())
