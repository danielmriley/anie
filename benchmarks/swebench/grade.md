# Grading SWE-bench Lite predictions (01, PR4)

Both arms' predictions files are graded by the OFFICIAL, unmodified
SWE-bench evaluator — never by anything in this repo (series principle 1).
Two interchangeable paths; both consume our `predictions.jsonl` as-is and
emit the same report schema (`resolved_ids` et al.) that `harvest.py`
joins with the metrics sidecars.

The predictions files to grade (one submission per arm, same run_id
convention):

```
benchmarks/work/predictions/anie__rlm__gemma4-e4b.jsonl
benchmarks/work/predictions/mini-swe-agent__gemma4-e4b.jsonl
```

Commands below were verified against the installed tools on 2026-06-12:
`sb-cli 0.1.5` and `swebench 4.1.0` (re-run the `--help`s if either has
been upgraded since).

## Path A — sb-cli (cloud evaluator; no Docker, needs a free API key)

One-time setup. The key arrives by email together with a verification
code:

```bash
benchmarks/.venv/bin/pip install sb-cli
benchmarks/.venv/bin/sb-cli gen-api-key dmr05d@gmail.com
# check email, then:
export SWEBENCH_API_KEY=<key from the email>
benchmarks/.venv/bin/sb-cli verify-api-key <verification code from the email>
```

Submit each arm (submission auto-waits for evaluation and writes the
report JSON into `--output_dir`; a `run_id` is immutable once submitted,
so use a fresh one per generation run):

```bash
cd /home/daniel/Projects/agents/anie
mkdir -p benchmarks/work/reports

benchmarks/.venv/bin/sb-cli submit swe-bench_lite test \
    --predictions_path benchmarks/work/predictions/anie__rlm__gemma4-e4b.jsonl \
    --run_id anie-rlm-gemma4-e4b-n25 \
    -o benchmarks/work/reports

benchmarks/.venv/bin/sb-cli submit swe-bench_lite test \
    --predictions_path benchmarks/work/predictions/mini-swe-agent__gemma4-e4b.jsonl \
    --run_id mini-swe-agent-gemma4-e4b-n25 \
    -o benchmarks/work/reports
```

To re-fetch a report later:

```bash
benchmarks/.venv/bin/sb-cli get-report swe-bench_lite test anie-rlm-gemma4-e4b-n25 \
    -o benchmarks/work/reports
```

Notes:
- Subset/split names are positional: `swe-bench_lite test` (the CLI also
  accepts `swe-bench_verified` and `swe-bench-m`).
- Predictions leave the machine (sent to the SWE-bench team's API). All
  graded repos are public OSS; nothing sensitive rides along.
- `pip check` reports `sb-cli requires click<8.2` against the venv's
  click 8.4.1 (pulled in by `swebench`). Verified harmless — `sb-cli`
  runs fine; ignore the warning.
- If the downloaded report ever lacks `resolved_ids` (harvest.py will
  say so loudly), fall back to Path B, whose report format is the
  canonical one.

## Path B — official local evaluator (needs Docker)

```bash
benchmarks/.venv/bin/pip install swebench
cd /home/daniel/Projects/agents/anie
mkdir -p benchmarks/work/reports

benchmarks/.venv/bin/python -m swebench.harness.run_evaluation \
    --dataset_name princeton-nlp/SWE-bench_Lite \
    --split test \
    --predictions_path benchmarks/work/predictions/anie__rlm__gemma4-e4b.jsonl \
    --run_id anie-rlm-gemma4-e4b-n25 \
    --max_workers 4 \
    --report_dir benchmarks/work/reports

benchmarks/.venv/bin/python -m swebench.harness.run_evaluation \
    --dataset_name princeton-nlp/SWE-bench_Lite \
    --split test \
    --predictions_path benchmarks/work/predictions/mini-swe-agent__gemma4-e4b.jsonl \
    --run_id mini-swe-agent-gemma4-e4b-n25 \
    --max_workers 4 \
    --report_dir benchmarks/work/reports
```

The report lands at
`benchmarks/work/reports/<model_name_or_path>.<run_id>.json`
(e.g. `anie__rlm__gemma4-e4b.anie-rlm-gemma4-e4b-n25.json`).

Notes:
- Only instances present in the predictions file are evaluated — no
  `--instance_ids` needed for our subset.
- Default `--namespace swebench` pulls prebuilt per-instance images from
  Docker Hub (no local builds). Budget roughly 1-2 GB per instance of
  image pulls for a 25-instance subset; `--cache_level env` (the
  default) cleans instance images as it goes.
- `--timeout 1800` per instance is the default test-run cap; fine here.

## Which to choose on this machine

**Path A (sb-cli).** This machine has no Docker and no sudo, so Path B
needs a rootless-Docker install (or an admin) first — that's the "one
user-assisted step" called out in `docs/external_benchmarks/README.md`.
Path A needs only a free email-verified API key and uploads two small
jsonl files. Pick Path B instead if/when Docker exists and you want
grading fully offline, byte-identical reruns, or per-instance test logs
(`logs/run_evaluation/<run_id>/...`) for debugging.

## After grading: the lift-matrix row

```bash
benchmarks/.venv/bin/python benchmarks/swebench/harvest.py --header
benchmarks/.venv/bin/python benchmarks/swebench/harvest.py \
    --report benchmarks/work/reports/<anie report>.json \
    --predictions benchmarks/work/predictions/anie__rlm__gemma4-e4b.jsonl \
    --metrics-dir benchmarks/work/metrics/anie__rlm__gemma4-e4b
benchmarks/.venv/bin/python benchmarks/swebench/harvest.py \
    --report benchmarks/work/reports/<control report>.json \
    --predictions benchmarks/work/predictions/mini-swe-agent__gemma4-e4b.jsonl \
    --metrics-dir benchmarks/work/metrics/mini-swe-agent__gemma4-e4b
```

Report every row together with the subset rule (`first N of
SWE-bench_Lite test by instance_id`), the per-instance budget, and the
mini-swe-agent version from
`benchmarks/work/predictions/mini-swe-agent__gemma4-e4b.meta.json`.
