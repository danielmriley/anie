# PR 5 — Parallel execution of independent sub-tasks

## Status — shipped as a dry-run; PR 5.1 will add the executor

The first iteration of PR 5 ships the **plan parser +
round-structure renderer** without an actual concurrent
executor. When `ANIE_PARALLEL_DECOMPOSE>=2` is set on top
of `ANIE_DECOMPOSE=1`, the harness:

1. Parses PR 4's plan text for numbered sub-tasks +
   `(depends on N)` / `(after N, M)` / `(requires N)`
   markers.
2. Builds a topological round structure: each round groups
   sub-tasks that have no remaining dependencies on each
   other.
3. Renders the plan with explicit `Round 1: [1, 3]
   (independent — could run in parallel)` annotations
   visible to the model.
4. Falls back to the plain PR 4 plan on any parse failure
   (cycle, dangling reference, no numbered list).

The model sees the structure but executes sub-tasks itself
via its existing recurse-tool path. PR 5.1 adds the
executor that fans out the rounds via
`ControllerSubAgentFactory` and runs them concurrently.

This split lets us validate the parsing + round structure
in smoke before committing to the more invasive concurrent
executor.

## Rationale (revised)

The original plan called for "parallel recurse + critic
voting": N sub-agents tackle the same problem in
parallel, then a critic picks the best answer. After
discussion this was reframed:

- **Voting wastes work.** N−1 answers are produced and
  thrown away. The harness has to make a judgmental
  pick, which is opaque and prone to recency bias.
- **Parallel decomposition is cleaner.** Sub-agents
  tackle DIFFERENT pieces of one task; results compose
  mechanically into the whole. No discarded work, no
  judgmental selection.

The new PR 5 reads the plan PR 4 produces, spots
sub-tasks that are independent of each other, and
fans them out in parallel. Sub-tasks with dependencies
run in topological order. The composition step is just
"assemble the per-sub-task results into the agent's
context."

## Design

### Plan format extension (forward-compat with PR 4)

PR 4's plan is currently a free-form numbered list.
For PR 5 to know which sub-tasks can run in parallel,
the plan format needs structured dependency declarations.
Two options:

1. **Strict structured format** — the decompose system
   prompt asks for JSON / YAML with a `depends_on:
   [task_ids]` field per sub-task. Reliable but the
   model may struggle to produce valid output.
2. **Heuristic / lenient** — keep the numbered-list
   format but ask the model to mark dependencies with a
   suffix like `(depends on 1)` after the sub-task. We
   parse leniently; sub-tasks without a marker are
   assumed independent.

Recommend **option 2** for the first iteration —
matches what models reliably produce, fails open if
parsing fails (treat as sequential).

### Execution

1. Read the plan from the `<system-reminder source="decompose">`
   message in the agent's initial context.
2. Parse the numbered-list + dependency markers into a
   DAG.
3. Topologically sort. Independent sub-tasks group into
   "rounds" — sub-tasks within a round can run
   concurrently; rounds run sequentially.
4. For each round, fan out: spawn N concurrent recurse
   calls (one per sub-task), each with its own scope
   targeting the sub-task's natural input.
5. Collect results. Each sub-task's output is appended
   to a `RoundResult` keyed by sub-task ID.
6. After the final round, the composition step prompts
   the model with all sub-task results: "here are the
   pieces, assemble the final answer."

### Trigger

`ANIE_PARALLEL_DECOMPOSE=N` where N is the max
concurrency (default 1, which is sequential — same as
PR 4 alone). N>1 enables actual parallelism.

Setting N=1 = effectively no-op vs. PR 4. Setting N=4
= up to 4 sub-tasks run concurrently per round.

This composes cleanly with `ANIE_DECOMPOSE=1`:
- DECOMPOSE=0: no plan, original behavior.
- DECOMPOSE=1, PARALLEL=1: plan generated, executed
  sequentially via the agent's normal recurse flow.
- DECOMPOSE=1, PARALLEL=4: plan generated, independent
  sub-tasks run up to 4 at a time.

### Why no voting / critic

Per the design discussion: voting wastes the model's
work and forces opaque selection. Parallel
decomposition gives ALL sub-task outputs to the
composition step; the model that assembles them sees
the full picture and can integrate, not pick.

If a sub-task's result is wrong, the composition step
sees it (alongside the others) and can ask for a
re-run via a follow-up recurse. That's a deterministic
recovery path, not a judgmental selection.

### Failure modes

- **One sub-task fails:** the round captures the
  error in the `RoundResult`. Composition gets a
  partial set with explicit "sub-task K errored:
  <reason>." Model can decide to retry that one or
  proceed.
- **Parser fails on the plan:** fall through to PR
  4-only behavior (sequential). Logged at warn-level.
- **Cycle in dependency declarations:** detect at
  parse time, fall back to sequential.

## Files to touch

- `crates/anie-cli/src/decompose.rs` — extend with
  plan-parsing (numbered list + `(depends on N)`
  markers → DAG).
- New `crates/anie-cli/src/parallel_decompose.rs` —
  topological sort, round-based execution, result
  collection.
- `crates/anie-cli/src/controller.rs` — wire the
  parallel executor when `ANIE_PARALLEL_DECOMPOSE>1`.
- Tests on parser + executor independently (the
  composition step is exercised by smoke).

Estimated diff: ~400 LOC of code, ~200 LOC of tests.

## Phased PRs

Could split into:
- 5.1 — plan parser (numbered list + dependency
  markers → DAG).
- 5.2 — executor (rounds, fan-out, fan-in).
- 5.3 — controller wiring.

Reviewable as one PR but split if scope balloons.

## Test plan

Parser:
- `parser_assigns_no_dependencies_when_no_markers`
- `parser_recognises_depends_on_single_task`
- `parser_recognises_depends_on_multiple_tasks`
- `parser_falls_back_to_sequential_on_cycle`
- `parser_falls_back_to_sequential_on_unparseable_plan`

Executor:
- `executor_runs_independent_subtasks_in_parallel`
- `executor_respects_dependencies`
- `executor_continues_on_subtask_error_with_partial_result`
- `executor_serializes_when_max_concurrency_is_one`

## Risks

- **Plan parsing is brittle.** Models may not produce
  the exact format we expect. Mitigation: lenient
  parsing, fall-back-to-sequential on any parse error.
- **Concurrency stresses the provider.** Local Ollama
  has limited GPU; firing 4 simultaneous recurse calls
  could push it past memory limits. Mitigation: `N`
  defaults to 1; opt-in, user-selectable.
- **Composition step is its own LLM call.** Adds
  latency. Mitigation: only fires when N > 1 (in N=1
  mode, parallel-decompose collapses to PR 4's
  sequential flow).
- **Sub-tasks may need shared state.** A sub-task
  result feeding into another's input. Mitigation: the
  dependency markers in the plan handle this by
  construction — dependent sub-tasks see prior
  sub-task results as input.

## Exit criteria

- [ ] Plan parser handles numbered list + dependency
      markers, falls back gracefully on errors.
- [ ] Executor runs independent sub-tasks
      concurrently up to `ANIE_PARALLEL_DECOMPOSE`
      cap.
- [ ] Composition step assembles results into a
      single context message before the agent's main
      response.
- [ ] All parser + executor tests pass.
- [ ] `cargo test --workspace` + clippy clean.
- [ ] Smoke check: parallel-decompose run on the
      2026-05-02 C++ stall scenario reduces wall-clock
      vs. sequential decompose.

## Deferred

- A `parallel_decompose` slash command for
  inspecting the planned DAG before execution.
- Adaptive concurrency (start at 1, scale up if no
  errors). Defer until smoke shows demand.
- Re-running failed sub-tasks without re-running the
  whole plan. Useful but complicates the executor;
  add only when smoke shows the partial-failure case
  is common.

## Note on the rejected design

The original PR 5 — voting on parallel runs of the
same problem — is intentionally NOT in this plan.
The user flagged it as wasteful (discarded work +
opaque selection). Self-consistency / N-shot voting
remains a possible future tool for specific high-stakes
sub-problems, but the routine path is parallel
decomposition (this PR), not voting.
