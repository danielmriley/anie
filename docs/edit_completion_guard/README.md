# Edit-completion guard — push small models to actually edit

## Rationale (evidence)

SWE-bench Lite, official grader (docs/external_benchmarks/results_2026-06-13.md):
across all 4 runs **20–24 of 25 instances produced NO file edit**.
gemma4:12b had **zero timeouts** (4.6 mean turns) yet edited nothing
24/25 — it reads, reasons, often answers in prose, and stops. The
bottleneck is not budget, speed, or model size; it is **terminating
without committing to a change**. The anie task preamble already says
"your edits are the deliverable" and it still happens — a passive
prompt nudge is insufficient.

## Core design

**A completion-boundary guard, not a per-turn one.** It only engages
at the moment the agent loop would otherwise terminate (final
assistant message, no pending tool calls). When it engages and fires,
it injects a directive and lets the loop continue instead of ending.

It fires only when ALL hold:
1. **edit_expected** is true for the run (see gating below),
2. **zero successful file mutations** occurred this run
   (edit/write/apply_patch — reuse the checkpoint mutation-tracking),
3. the final message did **not** explicitly conclude no change is
   needed (suppression: "no change needed / no bug / cannot reproduce
   / already correct"), and
4. the **guard budget** (default 1 round, `ANIE_EDIT_GUARD_ROUNDS`)
   is not exhausted.

The injected directive gives an explicit out to avoid forced garbage:
"You ended without editing any file. If you have identified the fix,
make the edit now with edit/write/apply_patch. If no change is truly
needed, say so explicitly and why." Escalates by round; after the
budget, accept the no-edit outcome.

Gated to **rlm mode** by default (small-model scaffolding);
`ANIE_EDIT_GUARD=0` disables.

## Gating: model-judged edit-expectation (chosen 2026-06-14)

`edit_expected` is determined, in precedence:
1. **Explicit**: `[guard].require_edit` config / `--require-edit` CLI
   flag. When set, short-circuits the classifier (the benchmark sets
   it; guaranteed-correct, no extra call).
2. **Model-judged** (default when not explicit): a one-shot
   classification side-request at task start — "Does this task
   require editing files? Answer yes or no." Constrained, minimal
   context, drained channel (reuse the repair side-request pattern).
   Cached for the run (one call per task, never per turn).

The small-model classifier is a known weak point; therefore we
**measure** it: SWE-bench tasks are ground-truth "yes", so an eval
records the false-negative rate (classifier says "no" when an edit
was required). This converts the risk into a number.

## PRs (sequential — agent-loop core first)

- **PR1 (anie-agent):** completion-guard loop + mutation tracking +
  suppression heuristic + the classifier side-request; AgentLoopConfig
  gains `edit_expected: Option<bool>` (None ⇒ classify) and
  `edit_guard_rounds`. Mock-provider unit tests.
- **PR2 (anie-cli + anie-config):** `[guard]` config, `--require-edit`
  flag, controller wiring (flag > classifier), rlm-default gating,
  `ANIE_EDIT_GUARD*` envs.
- **PR3 (anie-cli + anie-evals):** RunMetrics `edit_guard
  { classified_expected, guard_fired, guard_rounds, edit_after_guard }`
  (schema v5→v6) + classifier-accuracy eval scenario.
- **PR4 (benchmarks):** run_anie.py passes `--require-edit`; note the
  classifier short-circuit; record classifier agreement on the subset.

## Risks / guardrails

- **Forced garbage edits**: the explicit "or say no change is needed"
  out + the suppression heuristic + the 1-round budget bound this.
- **Firing on legitimate no-edit tasks**: model-judged gating + "no
  change needed" suppression; measured via the eval.
- **Infinite loop**: hard round budget; the guard can never run more
  than `edit_guard_rounds` times per run.
- **Classifier cost/latency**: one cached call per task, skipped when
  the explicit flag is set.

## Exit criteria

- [ ] Guard never fires on a question-style task in tests; fires on a
      "fix the bug" task that ended with no edit.
- [ ] Bounded: at most `edit_guard_rounds` interventions per run.
- [ ] Classifier false-negative rate measured on the SWE-bench subset.
- [ ] Re-run the n=25 matrix (e4b + 12b): non-empty-patch count rises
      materially vs the pre-guard baseline (the real success metric).
- [ ] cargo test --workspace + clippy clean; hosted/non-rlm unchanged.
