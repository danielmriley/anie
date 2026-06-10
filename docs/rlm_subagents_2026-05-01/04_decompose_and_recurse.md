# PR 4 — One-shot pre-loop decompose

## Rationale

The 2026-05-02 C++ smoke showed qwen3.5:9b can reason
about each constraint of a multi-constraint problem
correctly in isolation but fails to keep all constraints
coherent across one generation. Decomposition is the
established lever: break the task into sub-problems
small enough to fit in working memory, solve each, then
assemble. The skills system shipped a
`decompose-multi-constraint-task` skill (PR 3 of
`docs/skills_2026-05-02/`) that explains the pattern;
PR 4 of this series **executes** it via the harness
itself.

After conversation review the chosen execution model is
**one-shot pre-loop** rather than an inline tool. The
reasoning:

- The decomposition is most useful when the model sees
  it BEFORE it starts acting — sets the framing for the
  whole turn. An inline tool the model could call is
  more agentic but also harder to bound.
- One-shot pre-loop is a small, contained insertion: a
  separate LLM call that produces a plan, the plan goes
  into the agent's initial context as a
  `<system-reminder>` message, then the main loop runs
  normally.
- The next PR (parallel decomposition, PR 5) builds on
  the same plan output — sub-tasks marked as
  independent run concurrently. So PR 4's plan format
  is the contract PR 5 reads.

## Design

### Trigger

`ANIE_DECOMPOSE=1` (also accepts `true` / `yes`) +
`--harness-mode=rlm`. Default off — opt-in for now per
"be careful with implementation."

### Flow

1. User submits a prompt to the controller.
2. Controller calls `Decomposer::decompose(user_task)`.
3. `Decomposer` makes a one-shot LLM call to the same
   provider/model the agent will use, with a tight
   system prompt asking for 3-7 numbered sub-tasks.
4. Result trimmed and validated. If empty or matches
   `NO_PLAN_NEEDED` sentinel, skip injection.
5. If valid, wrap in
   `<system-reminder source="decompose">PLAN:\n{result}\n…</system-reminder>`
   and prepend as a leading user-role message in the
   agent's initial prompts.
6. Agent runs the main loop. The plan is visible in the
   model's context from turn 1.

### Failure modes — all best-effort

The decompose call is wrapped in a 30s timeout. Any
failure (resolver error, no provider, stream error,
empty output, NO_PLAN_NEEDED) results in `None` and
the original prompt runs without a plan. The user's
turn never blocks on decomposition.

### System prompt for the decompose call

```
You are a planning assistant. Given a user's task,
break it into 3-7 focused, independently-solvable
sub-tasks. Output as a numbered list, one sub-task per
line, no other prose. Each sub-task should be small
enough that a sub-agent could complete it in one
focused pass. If the task is trivial enough that a
plan would add no value, output the single line:
NO_PLAN_NEEDED.
```

The "independently-solvable" framing is deliberate —
PR 5 will read this same plan and run independent
sub-tasks in parallel.

### Output budget

Plans should be short. `max_tokens = 512` for the
decompose call caps blowup. 3-7 lines fits well under
that.

## Files to touch

- New `crates/anie-cli/src/decompose.rs` — `Decomposer`
  struct mirroring the `LlmSummarizer` pattern from
  Phase F. `decompose(&self, user_task)` returns
  `Option<String>`. Plus `render_plan_as_system_reminder`
  helper and `decompose_env_enabled` env check.
- `crates/anie-cli/src/lib.rs` — register the module.
- `crates/anie-cli/src/controller.rs` —
  `ControllerState::maybe_run_decompose` runs the call
  conditionally. `start_prompt_run` calls it before
  building the prompt message and prepends the plan
  message when non-`None`.

Estimated diff: ~250 LOC of code, ~30 LOC of tests
(integration tests deferred — unit tests cover the
sentinel + env handling; the LLM-call path is
exercised by smoke runs).

## Phased PRs

Single PR.

## Test plan

- `decompose_env_enabled_recognises_truthy_values` —
  env var parsing for "1" / "true" / "yes" /
  unset / "0".
- `render_plan_wraps_in_system_reminder_tags` — the
  rendered plan starts with the source-tagged opening
  and ends with `</system-reminder>`.
- Smoke check: re-run the 2026-05-02 C++ stall scenario
  with `ANIE_DECOMPOSE=1`. Compare to baseline (no
  decompose) and to the cpp-rule-of-five-skill-only
  case (skill but no decompose).

## Risks

- **Decompose call adds latency.** A 30s timeout but
  most calls take 2-5s on local Ollama. Acceptable —
  user opted in via env var.
- **Wrong-shape plans.** Model might output prose
  instead of a numbered list. Mitigation: the system
  prompt is firm; we don't parse the list, we just pass
  the text through. The model receiving the plan
  handles "free-form plan" the same as "numbered list."
- **Plan poisoning.** A bad plan could lead the model
  astray. Mitigation: opt-in only, smoke validates,
  user can disable.
- **Provider-specific behavior.** Some providers may
  not handle short single-shot calls well. Acceptable —
  provider-level concern.

## Exit criteria

- [ ] `Decomposer` exists and runs a one-shot call
      against the parent's provider/model.
- [ ] `ANIE_DECOMPOSE=1` env-gated; default off.
- [ ] Plan injected as a leading user-role message in
      the agent's prompts vec when non-empty + non-
      sentinel.
- [ ] Failure modes (timeout, empty, sentinel) skip
      gracefully without blocking the user's turn.
- [ ] All tests pass, clippy clean.
- [ ] Smoke check completes.

## Deferred

- Parallel execution of independent sub-tasks (PR 5).
- A status-bar segment showing the current plan.
- Per-sub-task progress tracking (would need a sub-task
  schema beyond plain text).
- An inline `decompose` tool the model can call mid-
  turn. Feasible but the one-shot pre-loop pattern
  covers the common case; revisit if smoke shows the
  inline pattern adds value.
