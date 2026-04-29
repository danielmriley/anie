# REPL agent-loop plan

This plan series refactors the monolithic `AgentLoop::run`
(`crates/anie-agent/src/agent_loop.rs:433-712`) into an explicit
**Read → Eval → Print → Loop** runtime. The goal of the *series*
is structural; the goal of each individual PR is small and
behavior-preserving until late in the sequence.

The architecture rationale and target shape live in
`docs/repl_agent_loop_2026-04-27.md` — read that first if you
have not. This folder is the implementation contract: one PR per
plan, each one short enough to review on its own diff.

## Guiding principles

1. **Tests first, structurally.** The first PR adds behavior-
   characterization tests against the *current* loop. Every later
   PR proves itself against those tests. No refactor lands without
   a test that would fail if the refactor changed user-visible
   behavior.
2. **Refactor before feature.** PRs 02–06 do not add functionality.
   They reshape the existing loop into REPL boundaries while
   keeping the public API, event sequence, provider/tool contract,
   and controller boundary unchanged. PR 07 is the first plan
   that introduces a real new capability, and it does so behind a
   noop default.
3. **Streaming stays live.** Provider deltas
   (`MessageStart`/`MessageDelta`/`MessageEnd`) and tool progress
   (`ToolExecStart`/`ToolExecUpdate`/`ToolExecEnd`) are emitted
   *during* `Eval`, not buffered until `Print`. The REPL split
   separates **state commit** from **live event emission** — it
   does not turn streaming into batch.
4. **Policy stays where it is.** Retry/backoff
   (`crates/anie-cli/src/controller.rs:265-339`), session
   persistence, queued-prompt handling, and compaction-budget
   ownership remain in `anie-cli` for the entire MVP. The agent
   loop owns *step* mechanics, not run-level policy.
5. **One commit per plan.** Each numbered file maps to one
   logical commit prefixed `repl_loop/PR{N}: ...`. A plan can
   internally describe sub-PRs (e.g. PR 1A / PR 1B) when a single
   commit would be too coarse, but the default is one PR per file.

## Execution — seven PRs, in order

| PR  | Scope | Behavior change | Cost |
|-----|-------|-----------------|------|
| **PR 1** — [Behavior characterization tests](01_behavior_characterization.md) | Lock down the current loop's event order, message accumulation, error/cancel handling, follow-up/steering paths in `anie-agent` tests using existing `MockProvider` + `TestTool` infra. | None. | Small (~300 LOC of new tests, no production code). |
| **PR 2** — [Extract `AgentRunState`](02_run_state_extraction.md) | Move `context` / `generated_messages` / terminal-error tracking into a private struct with helpers. Mechanical rewrite of `AgentLoop::run` body to use them. | None. | Medium (~150 LOC moved, body shrinks). |
| **PR 3** — [Internal REPL driver](03_internal_repl_driver.md) | Introduce private `AgentIntent`, `AgentObservation`, `AgentDecision` enums and split the loop into `read_step` / `eval_step` / `print_step` / `decide_next_step`. `AgentLoop::run` becomes a small driver. | None. | Medium-large (most of the work; still pure refactor). |
| **PR 4** — [Step tracing spans](04_step_tracing.md) | Add structured `tracing` spans at each REPL boundary (`agent_repl_step`, `agent_eval`, `agent_print`). No new `AgentEvent` variants. | None visible — only debug logs. | Small (~50 LOC). |
| **PR 5** — [Public step-machine API](05_step_machine_api.md) | Expose `AgentRunMachine` with `next_step` / `is_finished` / `finish`. `AgentLoop::run` becomes a thin wrapper that drives the machine to completion. | None. | Medium (~100 LOC + tests that drive one step at a time). |
| **PR 6** — [Controller pilot integration](06_controller_pilot.md) | Optionally route the controller's spawned task through the step machine instead of `AgentLoop::run`, while still consuming a single `AgentRunResult` per run. Behind a feature flag if needed. | None visible. | Small-medium. |
| **PR 7** — [First policy boundary: before-model](07_first_policy_boundary.md) | Add a `BeforeModelRequest` policy hook with a default-noop implementation. No real consumer yet — this PR proves the extension shape works. | None visible at default. | Small. |

The first feature-bearing extension (context augmentation,
proactive compaction, tool-call repair, verifier loop, etc.) is
explicitly **out of scope** for this series. After PR 7 lands we
plan a separate folder per capability that plugs into the policy
boundary.

## Milestone exit criteria

- [ ] All seven PRs merged in order.
- [ ] `cargo test --workspace` green at each PR boundary.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean at each PR boundary.
- [ ] PR 1's characterization tests pass after every later PR
      without modification (modulo deliberately-versioned tests
      that the plan flags as needing an update).
- [ ] `AgentLoop::run`'s public signature is unchanged after PR 6.
- [ ] `AgentRunResult`'s public shape is unchanged after PR 6.
- [ ] `AgentEvent` enum has no new variants after PR 6 (PR 7 may
      add a *boundary-internal* enum for policy responses, but
      not a new public event).
- [ ] Manual smoke (per `.claude/skills/live-provider-smoke/`):
      one OpenRouter free-tier prompt, one prompt with a tool
      call, one prompt cancelled with Ctrl+C — all behave
      identically to pre-refactor.
- [ ] `docs/arch/anie-rs_architecture.md` is updated to describe
      the REPL loop as the current architecture (in PR 6 or 7).

## What we're explicitly not doing in this series

- **No new `AgentEvent` variants** for step boundaries. Use
  tracing spans (PR 4); if the TUI ever needs step-level UI it
  can be added in a separate plan after the protocol stabilizes.
- **No recursive task decomposition, verifier/critic loops,
  planner calls, model profiles, prompt-template overhauls,
  context retrieval, or tool-call repair.** All of these become
  reachable after PR 7 — none are part of the refactor.
- **No retry/session/queued-prompt policy moves out of
  `anie-cli`.** The step machine (PR 5) is owned by `anie-agent`
  but does not absorb controller policy. PR 6 leaves the
  controller's `select!` arm structurally identical.
- **No public step-event protocol changes.** `MessageStart`,
  `MessageDelta`, `MessageEnd`, `TurnStart`, `TurnEnd`,
  `AgentStart`, `AgentEnd`, `ToolExecStart`, `ToolExecUpdate`,
  `ToolExecEnd`, and the `AgentRunResult` shape are all frozen
  by PR 1's tests.
- **No concurrent independent agent runs.** One run at a time
  remains the controller invariant.
- **No session schema bump.** The refactor touches in-memory run
  state only.

## Reference

- Architecture vision:
  `docs/repl_agent_loop_2026-04-27.md`.
- Local-small-model context (why this matters beyond frontier):
  `docs/local_small_model_harness_ideas.md`.
- Current loop body:
  `crates/anie-agent/src/agent_loop.rs:433-712` (`AgentLoop::run`).
- Current stream collector:
  `crates/anie-agent/src/agent_loop.rs:757-858` (`collect_stream`).
- Current tool dispatch:
  `crates/anie-agent/src/agent_loop.rs:893-1050`
  (`execute_tool_calls`, `execute_single_tool`).
- Current controller boundary:
  `crates/anie-cli/src/controller.rs:1003-1006` (spawn) and
  `crates/anie-cli/src/controller.rs:262-413` (result consumption).
- Plan-style precedent: `docs/tui_responsiveness/` (multi-PR
  plan with `execution/` tracker) and `docs/pi_adoption_plan/`
  (numbered plan series).
