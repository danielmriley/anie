# bench/PR2-4 — SWE-bench Lite subset (anie vs mini-swe-agent)

## Design

Adapter lives in `benchmarks/swebench/` (Python glue — the benchmark
ecosystem is Python; this is harness-adjacent tooling, not product
code; own venv at `benchmarks/.venv`).

**PR2 — generation adapter (`run_anie.py`)**
1. Load `princeton-nlp/SWE-bench_Lite` (test split) via `datasets`;
   deterministic subset: first N by sorted instance_id (default 25,
   `--limit`).
2. Per instance: clone the repo (cached bare clone per repo under
   `benchmarks/work/repos/`, worktree per instance at `base_commit`),
   run `anie --print --harness-mode=rlm --model <m> -C <worktree>
   --metrics-out <id>.metrics.json` with the instance's
   `problem_statement` wrapped in a fixed task preamble ("fix the
   issue; modify source, not tests"), under a wall-clock kill
   (default 480s).
3. Extract `git diff` (exclude test files per SWE-bench convention?
   NO — submit the raw diff; the evaluator applies it as-is) →
   `predictions.jsonl` rows {instance_id, model_name_or_path,
   model_patch}.
4. Archive metrics sidecars next to predictions.

**PR3 — control arm (`run_control.py`)**
mini-swe-agent (pip) configured for the same Ollama model
(litellm `ollama/<model>`), same subset, same wall-clock budget,
emitting predictions.jsonl in the same format. Record its version.

**PR4 — grading runbook + harvest (`grade.md` + `harvest.py`)**
Two documented grading paths (user picks one):
- `sb-cli submit swe-bench_lite test --predictions_path ...`
  (cloud; needs free SWEBENCH_API_KEY registration), or
- official local evaluator (`python -m swebench.harness.run_evaluation`,
  needs Docker).
`harvest.py` joins the grader's report with our metrics sidecars into
the lift-matrix row (resolved rate, tokens/instance, failures).

## Risks
- 8-min budget truncates hard instances for BOTH arms equally —
  that's the controlled variable, state it in reporting.
- Ollama serial: ~25 × 2 × ≤8 min ≈ ≤ 6.7h; run overnight, arms
  sequential (never concurrent — single GPU).
- mini-swe-agent prompt/format failures with small models are part
  of the measurement, not a bug to fix.

## Exit criteria
- One-command generation for each arm; predictions.jsonl validates
  against the swebench schema check; one instance smoke-tested end to
  end through generation; grading runbook verified against the
  evaluator's documented interface.
