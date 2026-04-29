# Plan 07 — Evaluation harness + harness mode flags

**Branch:** TBD (likely `dev_rlm`).
**Status:** ready to spec; ships in parallel with Phase A of
Plan 06 so we have measurement infrastructure before the
first capability lands.

## Rationale

We're proposing to invert the context-management story for
small local models. The pitch — "small models perform like
frontier models when the harness owns context" — is
falsifiable. It either replicates the paper's RLM-Qwen3-8B
result (28% average lift, GPT-5 quality on three of four
long-context tasks) or it doesn't.

If we're going to write a paper extending the RLM paradigm
into agent-harness territory, we need:

- **Reproducible scenarios.** Same prompt, same starting
  state, same model. Multiple runs to estimate variance.
- **Three modes** — baseline (model only, no anie), current
  anie (compaction-only), RLM anie (full virtualization).
  Same scenarios across all three.
- **Automated scoring** where possible (correctness checks,
  rubric-based pass/fail). Manual review for the parts that
  resist automation.
- **Cross-model comparison.** Anie supports many models; the
  story changes shape across qwen3.5:9b, qwen3.5:35b,
  gpt-4o-mini, claude-haiku, etc.
- **Cost / latency / token reporting.** Quality lifts that
  cost 10x more tokens aren't free wins.

## Three harness modes

A new top-level CLI flag controls which harness profile
applies to a run:

```bash
anie --harness-mode baseline   <prompt>   # no tools, no compaction, no recurse
anie --harness-mode current    <prompt>   # current anie (compaction, web tools, etc.) but no RLM
anie --harness-mode rlm        <prompt>   # full RLM virtualization (Plan 06 phases)
```

Implementation: a `HarnessMode` enum in `anie-cli`, threaded
through to `build_agent` / `build_compaction_gate` / tool
registry. Each mode is a profile switch:

| Mode | Compaction gate | Tools available | BeforeModelPolicy | Active ceiling |
|------|-----------------|-----------------|-------------------|----------------|
| `baseline` | none | none | noop | none (model's window) |
| `current` | `ControllerCompactionGate` | full set incl. web | noop | none |
| `rlm` | `ControllerCompactionGate` (fallback) | full set + `recurse` | `ContextVirtualizationPolicy` | configured (default 16k for small models) |

Modes are profiles, not feature flags — turning on `rlm` doesn't
disable other features; it adds the virtualization layer on
top of `current`'s features. The compaction gate stays in
`rlm` mode as a fallback for cases where the active ceiling
isn't tight enough.

A future mode `--harness-mode rlm_native` selects the Plan 04
shape (native RLM model + recursion-friendly defaults) when
available.

## Eval crate / binary

A new `anie-evals` workspace member. Lives in
`crates/anie-evals/` with:

```
crates/anie-evals/
  Cargo.toml
  src/
    lib.rs              -- shared types: Scenario, Score, RunResult
    bin/
      evals.rs          -- the eval runner CLI
    scenarios/
      mod.rs            -- registry + types
      repo_navigation/  -- one folder per scenario family
        ...
    scoring/
      mod.rs            -- scoring trait + helpers
      automated/        -- regex / structured-output checkers
      rubric/           -- LLM-as-judge for soft scoring
    reports/
      json.rs           -- machine-readable results
      markdown.rs       -- human-readable summary
```

The eval binary takes a scenario file or scenario glob and
runs each scenario under each enabled mode against each
enabled model:

```bash
cargo run --release -p anie-evals -- \
  --scenarios scenarios/long_context/*.toml \
  --modes baseline,current,rlm \
  --models qwen3.5:9b,qwen3.5:35b,gpt-4o-mini \
  --runs 3 \
  --output results/2026-04-29-run1.json
```

## Scenario format

Each scenario is a TOML file describing setup, prompt(s), and
scoring rules.

```toml
# scenarios/long_context/recall_after_30_turns.toml

name = "recall_after_30_turns"
family = "long_context"
description = """
The agent reads a file early in the run, then engages in 30
turns of unrelated work, then is asked a specific question
about the original file. Tests whether the harness preserves
access to old content.
"""

# Setup runs before the user prompt is issued. Establishes
# fixtures: files written, directories prepared, etc.
[setup]
files = [
  { path = "/tmp/eval-input.txt", contents_path = "fixtures/long_doc.txt" },
]

# The exchange — list of user prompts. Multi-turn scenarios
# run them sequentially; each gets its own evaluator.
[[turns]]
prompt = "Please read /tmp/eval-input.txt and remember the contents."
[turns.expect]
must_call_tool = "read"
[turns.score]
weight = 0.0  # no scoring on this setup turn

[[turns]]
prompt = "Now please calculate 17 * 234, then 89 - 12, then 100 / 4. Use bash for each."
[turns.expect]
must_call_tool = "bash"
min_tool_calls = 3
[turns.score]
weight = 0.1
rubric = "did all three calculations execute correctly"

# ... 28 more turns of similar busy work ...

[[turns]]
prompt = "What was the third paragraph of /tmp/eval-input.txt about?"
[turns.score]
weight = 1.0
# Automated check: the third paragraph contains the exact
# string "carbon cycle"; the answer must reference it.
automated_check = { contains = ["carbon cycle"] }
# Plus rubric for soft eval.
rubric = "did the agent correctly recall the third paragraph"
```

Scenario types we want at least:

| Family | Scenarios | What it stresses |
|--------|-----------|------------------|
| `long_context` | recall_after_N_turns, multi_file_synthesis, evidence_chain | RLM virtualization vs. compaction |
| `repo_navigation` | find_function_in_crate, identify_state_machine, locate_test | Active ceiling + ledger + recurse |
| `editing` | small_config_field, fix_off_by_one, update_test | Verifier loops, evidence-based answers |
| `tool_use` | search_then_read, recover_from_error, parallel_tool_dispatch | Tool reliability, error handling |
| `cancellation` | cancel_mid_stream, cancel_during_tool | Aborts under different harness modes |

Initial corpus: 5–8 scenarios per family, 30–40 total. Enough
to see signal; small enough to iterate on.

## Scoring

Two kinds of scores, both reported per scenario:

**Automated checks.**

```rust
pub trait AutomatedCheck {
    fn evaluate(&self, transcript: &Transcript) -> CheckResult;
}

pub enum CheckResult {
    Pass,
    Fail { reason: String },
    NotApplicable,
}
```

Built-ins:

- `Contains { strings: Vec<String> }` — final assistant text contains all listed strings.
- `RegexMatch { pattern: String }` — final assistant text matches a regex.
- `MustCallTool { name: String, min: usize }` — at least `min` calls to the named tool.
- `MaxTokens { limit: u64 }` — total token usage under a cap.
- `MaxLatency { ms: u64 }` — wall-clock under a cap.
- `MaxRecursionDepth { limit: u8 }` — for RLM mode.

**Rubric-based scoring** (LLM-as-judge):

```toml
[turns.score.rubric]
prompt = """
Score from 1-5: did the agent correctly recall the contents
of /tmp/eval-input.txt? 5 = quoted exact text. 1 = no recall
or hallucinated content.
"""
judge_model = "claude-sonnet-4-6"
runs = 3   # use median across multiple judge calls
```

Judge model defaults to a frontier model (so the eval isn't
self-evaluation noise). Configurable per-scenario for budget
or offline runs.

## Output format

```json
{
  "run_id": "2026-04-29-091525",
  "harness_commit": "a2863e7",
  "scenarios": [
    {
      "name": "recall_after_30_turns",
      "results": [
        {
          "mode": "baseline",
          "model": "qwen3.5:9b",
          "run": 1,
          "score": { "automated": { "pass": false, "reason": "missing 'carbon cycle'" }, "rubric": 1.5 },
          "metrics": { "input_tokens": 12450, "output_tokens": 1230, "wall_clock_ms": 8400, "recurse_calls": 0 }
        },
        {
          "mode": "current",
          "model": "qwen3.5:9b",
          "run": 1,
          "score": { "automated": { "pass": false, "reason": "missing 'carbon cycle'" }, "rubric": 1.0 },
          "metrics": { "input_tokens": 8900, "output_tokens": 1450, "wall_clock_ms": 9200, "recurse_calls": 0 }
        },
        {
          "mode": "rlm",
          "model": "qwen3.5:9b",
          "run": 1,
          "score": { "automated": { "pass": true }, "rubric": 4.0 },
          "metrics": { "input_tokens": 6200, "output_tokens": 980, "wall_clock_ms": 12100, "recurse_calls": 2 }
        }
      ]
    }
  ]
}
```

The Markdown report aggregates this into per-mode means with
variance, plus a per-scenario delta table:

```
recall_after_30_turns
  baseline  | rubric 1.7 ± 0.4 | tokens 21k | 8.5s
  current   | rubric 1.2 ± 0.5 | tokens 14k | 9.0s
  rlm       | rubric 4.1 ± 0.6 | tokens  9k | 12.0s
  Δ (rlm vs current) | +2.9 rubric | -36% tokens | +33% latency
```

## Reproducibility

For paper-quality findings:

- **Pin model versions.** `qwen3.5:9b` from Ollama: include
  the exact digest in the result file. Frontier models:
  the dated alias (`gpt-4o-2024-08-06`).
- **Pin harness commit.** Already in the result file.
- **Pin eval scenario hashes.** SHA256 of each scenario TOML
  file at run time.
- **Seed where possible.** Not all providers honor it;
  document which do.
- **Multiple runs.** Default 3 per (scenario, mode, model).
  More for the headline numbers.
- **Variance reporting.** Mean ± stddev for rubric scores.
  For automated checks: pass rate (n/k).

## Cross-model comparison

The flag `--models` takes a comma-separated list. The runner
creates per-model agent instances and runs the same scenarios
against each. Required for the "small model approaches GPT-5"
story — we need at least one frontier baseline.

Suggested initial model set:

| Tier | Model | Provider |
|------|-------|----------|
| small local | qwen3.5:9b | Ollama |
| medium local | qwen3.5:35b | Ollama |
| small cloud | claude-haiku-4-5 | Anthropic |
| frontier reference | claude-opus-4-7 | Anthropic |

Cost note: frontier baseline runs are budgeted; default the
`--models` list to small models only and require explicit
opt-in for cloud frontier runs.

## What we'd report

The paper-shaped output is something like:

> Across N long-context scenarios drawn from agent-harness
> usage, RLM-mode anie (Plan 06 phases A–E) running
> qwen3.5:9b achieves a rubric-mean of X (vs. Y for
> compaction-only mode and Z for baseline-no-tools), at
> a token cost W and a latency cost V. The lift is
> consistent with the paper's findings on offline RLM and
> extends them into the agent-harness setting.

For mid-flight readability, we want to be able to say:

> Phase D (ledger injection) lifted rubric on
> `recall_after_30_turns` from 2.1 to 3.8 and reduced token
> usage by 22%. Phase E (smart inclusion) added a further
> 0.4 lift.

That requires running the eval suite at every phase boundary
and recording the deltas. Phase A → B → C → D → E → F gives
us five differential measurements over the course of Plan 06.

## Implementation order

1. **Mode flag plumbing** — `--harness-mode` flag,
   `HarnessMode` enum, threaded through `build_agent` etc.
   Defaults to `current` for backward compat. ~3 days.
2. **Scenario format + minimal runner** — TOML format,
   single-scenario invocation, basic transcript capture.
   ~1 week.
3. **Initial scenario corpus** — 10–15 scenarios spanning
   the families above. ~1 week (mostly content writing).
4. **Automated checks** — `Contains`, `MustCallTool`,
   `MaxTokens`, `MaxLatency`. ~3 days.
5. **Rubric scoring** — LLM-as-judge with
   `claude-sonnet-4-6` default. ~1 week.
6. **Multi-run aggregation + reporting** — JSON output,
   Markdown summary. ~3 days.
7. **Phase A baseline run** — measure current anie vs.
   baseline before any RLM work. ~1 day.

Total: roughly 4 weeks. Lands ahead of Phase A of Plan 06
ideally, so the first RLM ship has measurements waiting.

## Risks

- **Rubric judge bias.** A frontier judge model might
  systematically prefer answers that "look like a frontier
  model wrote them." Mitigate with multiple judge runs
  (median of 3) and at least one cross-judge sanity check
  per family (different judge model, same scenarios).
- **Scenario gaming.** If we tune Plan 06 phases to the
  scenarios, we overfit. Keep half the scenario corpus as
  a held-out set we don't look at during development.
- **Variance in local model output.** qwen3.5:9b without
  seed support will produce different outputs across runs.
  3+ runs per cell + variance reporting handles this; if
  variance is too high, bump to 5.
- **Cost.** Rubric scoring with a frontier judge across
  every (scenario, mode, model, run) cell is expensive.
  Subset reasonably; full runs are for paper deadlines, not
  every commit.

## Exit criteria

- [ ] `--harness-mode {baseline,current,rlm}` flag works in
      `anie-cli` and gets the right capabilities for each
      mode (verified by inspecting the gate / tool registry
      / policy at run time).
- [ ] `anie-evals` crate exists and runs at least one
      scenario end-to-end against `current` mode.
- [ ] Initial scenario corpus (10+ scenarios, 3+ families)
      lands.
- [ ] Automated checks + rubric scoring both produce
      result entries.
- [ ] Multi-run aggregation produces a Markdown summary.
- [ ] First baseline measurement lands as a checked-in
      `results/` artifact.

## Deferred

- Distributed runs (parallelize across machines). Single-
  machine is fine for the corpus size we're starting with.
- A web UI / dashboard for browsing results. JSON + Markdown
  is enough for paper-shaped output.
- Continuous integration of the eval suite. Useful but
  expensive; add when the corpus is stable.
- Statistical significance testing across runs. Mean ±
  stddev is enough for the first paper draft.
