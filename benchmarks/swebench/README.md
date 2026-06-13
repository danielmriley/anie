# SWE-bench Lite adapter (anie arm + mini-swe-agent control arm)

Generation adapters for `docs/external_benchmarks/01_swebench_lite.md`
(PR2-PR4). Both arms produce `predictions.jsonl` in the official
SWE-bench format plus per-instance metrics sidecars. Grading is NOT
done here — predictions go to the unmodified official evaluator (see
[grade.md](grade.md)), and `harvest.py` joins the grader's report with
the sidecars into the lift-matrix rows.

## Setup (one-time)

```bash
cd benchmarks
python3 -m venv .venv
.venv/bin/pip install datasets
```

(`run_control.py` pip-installs `mini-swe-agent` into the venv on first
run; `grade.md` covers the grader installs.)

Also required: a current debug build of anie (`cargo build` at repo
root) and Ollama serving the target model locally.

## Usage

```bash
benchmarks/.venv/bin/python benchmarks/swebench/run_anie.py \
    --limit 25 --model gemma4:e4b --mode rlm --budget-s 480
```

Flags:

| flag | default | meaning |
|------|---------|---------|
| `--limit` | 25 | subset size: first N of the test split sorted by `instance_id` (the deterministic subset rule from `docs/external_benchmarks/README.md`) |
| `--model` | `gemma4:e4b` | model id passed to `anie --model` |
| `--mode` | `rlm` | `anie --harness-mode` (`baseline` / `current` / `rlm`) |
| `--budget-s` | 480 | wall-clock kill per instance; the whole anie process group is SIGKILLed at the deadline and whatever is in the worktree becomes the prediction |
| `--out` | `benchmarks/work/predictions/anie__<mode>__<model>.jsonl` | predictions file |

The runner is **resumable**: instance_ids already present in `--out`
are skipped, so an interrupted run can be relaunched with the same
arguments.

## Control arm (mini-swe-agent)

The same-model control: mini-swe-agent (the SWE-bench team's ~100-line
reference scaffold) on the identical subset, model, and budget. The
anie-vs-control delta on identical instances is the series' primary
claim (`docs/external_benchmarks/README.md`, principle 2).

```bash
benchmarks/.venv/bin/python benchmarks/swebench/run_control.py \
    --limit 25 --model gemma4:e4b --budget-s 480
```

Same `--limit` / `--budget-s` / `--out` semantics and the same resume
behavior as `run_anie.py` (shared `common.py`, so the arms cannot drift
on subset or checkout state). Never run the two arms concurrently —
single GPU; the plan's budget math assumes serial Ollama access.

How it differs from mini-swe-agent's own batch runner (and why):

- `mini-extra swebench` needs per-instance Docker images; this machine
  has none. We drive `DefaultAgent` + `LocalEnvironment` as a library
  inside the same git worktrees the anie arm uses, with the installed
  package's official `swebench.yaml` prompts verbatim — only the
  docker-ism `/testbed` is rewritten to the worktree path. Both arms
  therefore work under identical environment conditions (plain checkout,
  no installed test deps), which is the controlled comparison we want.
- The litellm model id defaults to `ollama_chat/<model>`, not
  `ollama/<model>`: both target the local Ollama server, but litellm's
  plain `ollama/` provider has no `tools` support and the scaffold's
  `drop_params` would silently strip its bash tool — every step would
  fail by wiring, not by model. Override with `--litellm-model` if
  needed.
- Budget kill semantics are identical to the anie arm (SIGKILL of the
  whole process group at `--budget-s`). The prediction is the agent's
  own submission when it submitted one, otherwise the worktree diff at
  the deadline — the same "partial work still counts" rule as
  `run_anie.py`, so the truncation pressure is symmetric.
- mini-swe-agent prompt/format failures with small models (e.g.
  `RepeatedFormatError`) are part of the measurement, not a bug to fix.

Per instance it writes the prediction row, a trajectory + log under
`work/logs/<run>/`, and a sidecar at `work/metrics/<run>/` in the
`swebench-control-arm-v1` shape, which mirrors the RunMetrics paths
`harvest.py` reads (`tokens.total_tokens`, `turns`, `tools.calls`,
`tools.failures`). The installed mini-swe-agent version is recorded in
`work/predictions/<run>.meta.json`.

## Grading and the lift matrix

[grade.md](grade.md) is the runbook: two official-evaluator paths
(sb-cli cloud, or the local Docker harness), verbatim commands for our
predictions files, and which to pick on this machine. After grading:

```bash
benchmarks/.venv/bin/python benchmarks/swebench/harvest.py --header
benchmarks/.venv/bin/python benchmarks/swebench/harvest.py \
    --report <report.json> --predictions <run.jsonl> --metrics-dir work/metrics/<run>
```

prints one markdown lift-matrix row per arm (resolved rate, mean
tokens, mean turns, tool calls/failures). `harvest.py --fake-report`
self-tests the join against synthetic sidecars of both shapes without
a graded run.

## What one instance does

1. Cached bare clone of the GitHub repo under
   `benchmarks/work/repos/<owner>__<repo>.git` (fetched only if the
   needed commit is missing).
2. Detached worktree at the instance's `base_commit` under
   `benchmarks/work/instances/<instance_id>` (hard-reset + cleaned if
   it already exists, so every attempt starts pristine).
3. `target/debug/anie --print --harness-mode=<mode> --model <model>
   -C <worktree> --metrics-out <id>.metrics.json <prompt>`, where the
   prompt is the instance's `problem_statement` behind a fixed preamble
   ("fix the issue; modify source, not tests"). anie's stdout/stderr go
   to `benchmarks/work/logs/<run>/<id>.log`.
4. `git add -A && git diff --cached` in the worktree → the
   `model_patch` (staging first so files the agent *created* are in the
   patch; this matches what mini-swe-agent and other SWE-bench scaffolds
   submit). The raw diff is submitted as-is — no test-file filtering;
   the evaluator applies it unmodified.
5. Append `{instance_id, model_name_or_path, model_patch}` to the
   predictions jsonl. Metrics sidecars land in
   `benchmarks/work/metrics/<run>/`.

## Layout

```
benchmarks/
  .venv/                      # local venv (gitignored)
  work/                       # all state (gitignored)
    repos/<owner>__<repo>.git # cached bare clones
    instances/<instance_id>/  # per-instance worktrees
    metrics/<run>/<id>.metrics.json
    logs/<run>/<id>.log       # control arm also: <id>.traj.json etc.
    predictions/<run>.jsonl   # control arm also: <run>.meta.json
    reports/                  # grader reports (grade.md)
  swebench/
    common.py                 # dataset subset + clone/worktree plumbing
    run_anie.py               # anie generation arm
    run_control.py            # mini-swe-agent control arm
    grade.md                  # official-evaluator grading runbook
    harvest.py                # grader report + sidecars -> lift-matrix rows
```

## Notes

- A run is recorded even when the patch is empty or the agent timed
  out — an unsolved instance is a data point, not a retry candidate
  (both arms). Only git-level failures (clone/worktree errors) are
  left unrecorded so a resume retries them.
