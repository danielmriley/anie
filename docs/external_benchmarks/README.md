# External benchmarks — real scores against other harnesses

Goal: apples-to-apples numbers for anie vs other harnesses. The
comparison that matters at our model tier is **same model, same
instances, different harness** — absolute scores for 4-8B local
models will be low single digits on these benchmarks; the harness
delta is the product claim.

## Principles

1. **Adapt anie TO the official evaluator, never the reverse.** We
   generate predictions/artifacts; the benchmark's own unmodified
   grader scores them. Reimplementing a grader kills comparability.
2. **Same-model control arm.** mini-swe-agent (≈100-line scaffold,
   runs any Ollama model via litellm) runs the identical subset with
   the identical model. anie-vs-control on identical instances is the
   primary claim; absolute score is secondary.
3. **Outcome grading only** — these benchmarks grade "do the tests
   pass", which also retires our scenario-suite instrument debt
   (plan 00).
4. **Honest subsets.** This machine runs ~8-min-budget instances; we
   report subset size + selection rule (deterministic: first-N of
   SWE-bench Lite test split by instance_id) alongside every number.
5. **RunMetrics sidecars ride along** — every benchmark instance also
   feeds the lift matrix (tokens/turns/failures per instance).

## Environment reality (2026-06-12)

This machine has Python 3.12 + pip, 509GB free disk, **no Docker, no
tmux**. Consequences:
- SWE-bench **generation** runs now (git clones + anie runs only).
- SWE-bench **grading** needs one user-assisted step: either install
  Docker (official local evaluator) or register a free key for
  `sb-cli` (the SWE-bench team's cloud evaluator). Both documented in
  plan 01.
- Terminal-Bench (plan 02) is **blocked on Docker** end to end.

## Plans

| # | Plan | Status |
|---|------|--------|
| 00 | [Scenario-check reform](00_scenario_check_reform.md) | prerequisite — un-poisons our own matrix |
| 01 | [SWE-bench Lite subset](01_swebench_lite.md) | generation unblocked now; grading = one user step |
| 02 | [Terminal-Bench](02_terminal_bench.md) | blocked on Docker install |
| 03 | Aider polyglot | deferred until 01/02 produce numbers |

## Exit criteria (series)

- [ ] A graded SWE-bench Lite subset score for anie (rlm mode,
      gemma4:e4b) AND for mini-swe-agent on the same instances/model,
      from the OFFICIAL evaluator, recorded with subset rule.
- [ ] Per-instance RunMetrics archived for the lift matrix.
- [ ] Terminal-Bench adapter merged and smoke-passing once Docker
      exists.
