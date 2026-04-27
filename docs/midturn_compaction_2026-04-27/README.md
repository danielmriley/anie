# midturn_compaction_2026-04-27: robust context handling for local models

## Motivation

Anie's compaction today fires only at the *user-prompt boundary*:
`InteractiveController::run_prompt` calls
`maybe_auto_compact` (`crates/anie-cli/src/controller.rs:631`) before
sending each new user prompt. Once a single prompt's agent loop is
underway, no further compaction can happen — and a single agent loop
can fan out into many sampling requests, each consuming context
(tool-call → tool-result → next sampling request → ...).

For workstation-class hosts running Ollama, this is the practical
limit. A 36B-parameter model at Q4 with 256K context will swap-thrash
to a halt; the same model at 32K–65K context fits comfortably but
gives the agent loop very little headroom. A typical coding task
that reads several files, runs a build, and asks the model to
synthesize will easily blow past the threshold *inside one user
turn* — at which point anie either truncates, errors, or (more
commonly) silently submits a request larger than `num_ctx` and
relies on the provider to surface a `ContextOverflow` after the
fact.

Reactive recovery already exists: `ProviderError::ContextOverflow`
flows into `retry_policy::RetryDecision::Compact`
(`crates/anie-cli/src/retry_policy.rs:92-99`), which does a compact-
and-retry once. But that path fires *after* a failed sampling
request. We want **proactive mid-turn compaction**: notice the
context is filling, compact between sampling requests, and avoid
the failed call.

Two reference implementations were studied:

- **pi** (`/home/daniel/Projects/agents/pi`): same prompt-boundary
  shape as anie, plus a reactive overflow path very similar to ours
  (`packages/coding-agent/src/core/agent-session.ts:1755-1833`).
  Does **not** compact between sampling requests within a turn.
- **codex** (`/home/daniel/Projects/agents/codex`): adds explicit
  *mid-turn* compaction (`codex-rs/core/src/codex.rs:6452-6468`,
  `CompactionPhase::MidTurn`). After each sampling request, if
  total token usage is at or over the auto-compact limit and the
  agent still needs to call the model again, codex compacts
  in-place and `continue`s the loop. That is the shape we want.

This plan set ports codex's mid-turn pattern, adds small-context-
aware sizing so reserve and tool output budgets scale with the
configured context window, and lands the observability needed to
diagnose context pressure when it shows up in real workloads.

## Principles

1. **Match codex's mid-turn shape unless we have a documented reason
   not to.** Codex compacts between sampling requests inside the
   agent loop, with a clear phase enum
   (`StandaloneTurn` / `MidTurn` / pre-turn variants). Anie should
   adopt the same boundary.
2. **Cancellation is mandatory; mid-turn compaction is opt-in.** A
   user abort or shutdown must cancel a mid-turn compaction the
   same way it cancels a streaming model call. Mid-turn compaction
   must not turn into a hidden non-cancellable workload.
3. **No silent thrash.** Every compaction (pre-turn, mid-turn,
   reactive) must be surfaced via the existing `CompactionStart` /
   `CompactionEnd` events and visible in the activity row. A budget
   on *number* of compactions per user turn prevents runaway loops
   if compaction itself fails to free enough headroom.
4. **Small contexts deserve special handling.** A 16,384-token
   reserve is meaningless when `context_window = 8192`. Reserve
   should adapt to the configured window. Tool output caps should
   too — a 4K-context model can't ingest a 64KB bash output.
5. **No regressions for cloud models.** Anthropic/OpenAI/etc. with
   200K+ context windows should see exactly the current behavior
   unless the new triggers are explicitly enabled. Defaults must
   remain pi-shaped at the prompt boundary.
6. **Reuse existing machinery before adding new.** anie has
   `Session::auto_compact`, `CompactionConfig`, the
   `CompactionStart`/`CompactionEnd` event pair, and the retry
   policy's `RetryDecision::Compact` path. Mid-turn should land as
   *a new caller of existing primitives*, not a parallel system.

## Plan inventory

| # | Plan | Scope | Depends on |
|---|---|---|---|
| 00 | [`00_baseline.md`](00_baseline.md) | Comparative analysis: anie vs pi vs codex compaction. Not a PR; reference doc. | none |
| 01 | [`01_context_aware_reserve.md`](01_context_aware_reserve.md) | Make `reserve_tokens` scale to a fraction of `context_window` for small windows. | none |
| 02 | [`02_per_turn_compaction_budget.md`](02_per_turn_compaction_budget.md) | Cap compactions per user turn (default 2). Anti-thrash. | none |
| 03 | [`03_agent_loop_compaction_signal.md`](03_agent_loop_compaction_signal.md) | Introduce a callback / event so the agent loop can ask the controller to compact before the next sampling request. Pure infra. | none |
| 04 | [`04_midturn_compaction_execution.md`](04_midturn_compaction_execution.md) | Use the signal from 03 to fire compaction mid-turn, then resume the loop with the compacted context. The codex pattern. | 01, 02, 03 |
| 05 | [`05_tool_output_caps_scale_with_context.md`](05_tool_output_caps_scale_with_context.md) | Make per-tool output budgets scale with `effective_context_window`. | 01 (uses the same effective-window readout) |
| 06 | [`06_compaction_telemetry.md`](06_compaction_telemetry.md) | Per-session counters and structured logs for pre-turn / mid-turn / overflow compactions. | 04 |

## Suggested landing order

1. **01 + 02** can land in parallel. Both are tiny prep changes
   that improve correctness today and unblock 04.
2. **03** lands next — pure plumbing, no behavior change. Once
   merged, the controller has a hook the agent loop can call.
3. **04** builds on 01/02/03. This is the load-bearing change and
   should ship with thorough fault-injection tests.
4. **05** is independent of 04 in implementation but conceptually
   complements it: 04 reduces context pressure across turns; 05
   keeps a single tool result from being the thing that pushes
   into the red.
5. **06** lands last, against the now-stable mid-turn machinery.
   Telemetry is most useful when the underlying behavior is
   no longer in flux.

## Milestone exit criteria

- [ ] `reserve_tokens` is auto-clamped against `context_window` for
      windows under a documented threshold; documented in plan 01.
- [ ] Per-turn compaction budget enforced; mid-turn compaction
      cannot run more than the configured maximum within a single
      user prompt.
- [ ] Agent loop emits a "consider compaction" signal after each
      sampling request that has follow-ups pending; controller
      decides whether to compact before the next call.
- [ ] Mid-turn compaction surfaces as a normal
      `CompactionStart` / `CompactionEnd` pair — same UI affordance
      as pre-turn compaction.
- [ ] Tool output caps shrink for small context windows.
- [ ] Per-session telemetry counts pre-turn, mid-turn, and overflow
      compactions; logs include trigger reason and tokens
      before/after.
- [ ] No regression for cloud models with large windows: existing
      pre-turn behavior matches today's golden tests.
- [ ] `cargo test --workspace`, `cargo clippy --workspace
      --all-targets -- -D warnings`, and a manual smoke against a
      small-context Ollama model that overruns its window in one
      turn, with a successful mid-turn compaction observed.

## Anti-goals / not in scope

- **Mid-stream compaction.** The model is generating against a
  fixed context once a sampling request is in flight; we cannot
  compact mid-stream. Mid-turn means *between* sampling requests
  inside one user prompt's agent loop, never *inside* one.
- **Custom summarizer model.** The summarizer continues to be the
  current model. Switching summarization to a smaller helper model
  is a separate plan and not blocking for the small-context story.
- **Persistent across-session memory.** This plan set is about
  bounded context within one session; it does not introduce a
  long-term knowledge store.
- **Automatic context-window discovery.** The user already sets
  `context_window` via config or the runtime override. We do not
  attempt to introspect Ollama's loaded `num_ctx` and reconcile
  with anie's expectation here.
