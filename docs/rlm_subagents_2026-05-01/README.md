# True sub-agents + decompose + parallel recurse (2026-05-01)

The follow-on plan series to
`docs/harness_mitigations_2026-05-01/`. Mitigations
fixed the loud "hallucinated success" failure mode;
this series targets the **long-tail reasoning gap** —
small-model inability to converge on multi-constraint
problems even when each individual error is correctly
diagnosed.

## What the smoke caught

The 2026-05-01 smoke against the new mitigations
(session `c8a67e6d`) showed PR 1 working as designed:
qwen3.5:9b stopped hallucinating success past
`[tool error]`. But T2 (write the DLL driver, compile,
run) **stalled at 43 minutes**, never converged. The
model correctly diagnosed the cascade of C++ access-
control bugs (private → protected → friend) but its
single-shot rewrites kept reintroducing variants of the
same problem. Each bash retry had varied args so PR 2's
loop detector (correctly) didn't fire.

Engagement-without-convergence is the new ceiling. The
mitigations made small-model failures **honest**;
they didn't make small-model reasoning **deeper**.

## Principles

- **Observability over hard caps** (carried over from
  mitigations). No pre-emptive numeric limits on depth,
  fan-out, or budget. Track everything, surface in
  TUI/logs, let users interrupt unproductive runs.
- **Sub-agents inherit tools, not state.** A sub-agent
  spawned via `recurse` gets the parent's `ToolRegistry`
  (or a filtered subset) so it can `bash`/`web_read`/
  `edit` independently. It does NOT get write access
  back to the parent's archive — it's a read-only view
  of context plus its own tool sandbox.
- **Default-conservative, opt-in for advanced.** Tool
  inheritance ships on by default in `--harness-mode=rlm`.
  Decompose and parallel-recurse ship behind env flags
  initially (`ANIE_DECOMPOSE=1`, `ANIE_RECURSE_PARALLEL=N`)
  until smoke shows they pay off.
- **Cargo-cult guard.** Every PR includes a smoke
  comparison against the 11-turn protocol baseline. A
  feature that doesn't change the regression markers in
  `docs/smoke_protocol_2026-05-01.md` either needs more
  work or shouldn't ship.

## PRs in order

| PR | Doc | What |
|---|---|---|
| 1 | [01_depth_observability.md](01_depth_observability.md) | Track recurse depth on `RecurseScope`. Log + surface in TUI. No abort. **Shipped:** depth tracking already in place pre-PR (SubAgentBuildContext.depth + RecurseTool.depth fields); this PR added the threshold-warning emission via a new `RecurseDepthDetector` in `anie-agent::recurse_depth`, mirroring the failure_loop pattern. Default threshold 5, `ANIE_RECURSE_DEPTH_WARN_AT` overrides, `ANIE_DISABLE_RECURSE_DEPTH_WARN=1` disables. Status-bar segment scoped out (deferred). |
| 2 | [02_tool_inheritance.md](02_tool_inheritance.md) | Sub-agents inherit a filtered tool registry (bash, read, edit, write, web_*; recurse gated by depth). **Shipped:** ControllerSubAgentFactory now takes `parent_tools: Arc<ToolRegistry>` + `recurse_inherit_limit: u8`. Filter inherits `bash`, `read`, `edit`, `write`, `grep`, `find`, `ls`, `web_search`, `web_read`, `skill` always; `recurse` only at `depth < limit` (default 3). Unknown future tools default to NOT-inherit (forces deliberate decision). |
| 3 | [03_resource_observability.md](03_resource_observability.md) | Track per-sub-agent token spend + wall-clock; bubble to parent. Log + TUI. No abort. **Shipped:** SubAgentStats aggregates input/output/cache tokens, tool calls, cost, and wall-clock from sub-agent's generated messages. Surfaced in recurse tool's `result.details` under `sub_agent_*` keys + info!-level log. |
| 4 | [04_decompose_and_recurse.md](04_decompose_and_recurse.md) | Optional pre-loop decomposition pass (one-shot LLM call producing sub-task list). Plan injected as `<system-reminder source="decompose">` leading message. Behind `ANIE_DECOMPOSE=1`. **Shipped:** Decomposer struct, controller hook in start_prompt_run, best-effort failure handling, NO_PLAN_NEEDED sentinel. |
| 5 | [05_parallel_decomposition.md](05_parallel_decomposition.md) | **REVISED:** parallel execution of independent sub-tasks from PR 4's plan (was originally voting + critic; rejected as wasteful). **Shipped as dry-run:** plan parser + topological round renderer that annotates the plan with "Round 1: [1, 3] (could run in parallel)" structure visible to the model. Behind `ANIE_PARALLEL_DECOMPOSE>=2` on top of `ANIE_DECOMPOSE=1`. PR 5.1 will add the actual concurrent executor that fans rounds out via ControllerSubAgentFactory. |
| 6 | [06_smoke_validation.md](06_smoke_validation.md) | Re-run the 11-turn protocol with all features enabled; document deltas in `smoke_protocol_2026-05-01.md`. |

PRs 1–3 are infrastructure. PRs 4–5 are the actual
capability additions. PR 6 is exit criteria for the
series.

## Exit criteria for the series

- [ ] All six PRs land on `dev_rlm` (or a successor
      branch).
- [ ] `cargo test --workspace` and
      `cargo clippy --workspace --all-targets -- -D warnings`
      clean after each PR.
- [ ] Smoke run T2 (DLL driver + compile) **converges**
      on a clean compile within 15 minutes (vs. the
      stall at 43+ min in the post-mitigations smoke).
      This is the headline metric — the long-tail
      reasoning gap is closed enough that productive
      iteration finishes.
- [ ] Smoke run T10 (wardrobe pivot) continues to
      autonomously call web tools (don't regress the
      mitigations win).
- [ ] No regressions on T1, T3-T6, T8-T9, T11.
- [ ] Per-PR exit criteria met.

## Design decisions

### Why not enforce a depth cap (PR 1)?

Hard caps are pre-emptive engineering for problems we
haven't yet measured. The recurse tool already has a
`recursion_budget: Arc<AtomicU32>` (see `RecurseTool`
construction in `controller.rs`). We track depth, log
when it exceeds N (warn-level), but don't refuse. If
smoke shows runaway recursion in practice, we add a
cap then.

### Why filtered tool inheritance (PR 2)?

A sub-agent that can call `recurse` itself can fork
infinitely. PR 1's depth tracking gives us the visibility
to debug that, but until depth-aware tool selection is in
place, sub-agents should NOT see `recurse` in their
registry. Bash/edit/read/write/web_* are safe — they're
self-contained. PR 2 implements the filter; depth-aware
re-enable of `recurse` lands in PR 4 (decompose) where it
becomes intentional.

### Why ANIE flags for decompose + parallel decomposition (PR 4-5)?

These are the two features most likely to regress
behavior or cost. Behind env flags they're easy to
A/B test in the smoke without affecting the default
path. Once smoke data shows they pay off, we promote to
default-on in `--harness-mode=rlm`.

### Why parallel decomposition instead of N-way voting (PR 5)?

The original PR 5 design — N sub-agents tackle the
same problem in parallel, vote on the best — was
rejected during review. Voting is wasteful: N−1
answers get discarded, and the harness has to make a
judgmental pick that's prone to opaque failure modes.

**Parallel decomposition** keeps all the work: each
sub-agent tackles a DIFFERENT piece of the task, and
results compose mechanically into the whole answer.
No discarded work, no judgmental selection.

## Reference

- Paper: [Recursive Language Models, arXiv 2512.24601](https://arxiv.org/abs/2512.24601)
  — the conceptual ancestor.
- Source: [github.com/alexzhang13/rlm](https://github.com/alexzhang13/rlm).
- Companion: `docs/rlm_2026-04-29/` (the original
  recurse tool + context virtualization series).
- Companion: `docs/harness_mitigations_2026-05-01/`
  (PR 1-3 + follow-up that motivated this series).
- Smoke: `docs/smoke_protocol_2026-05-01.md` (regression
  markers).
