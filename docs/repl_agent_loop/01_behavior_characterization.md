# PR 1 — Behavior characterization tests

**Goal:** Lock down the current `AgentLoop::run` behavior with
focused tests *before* any refactor begins. These tests are the
contract that PRs 2–6 must preserve.

This PR adds tests only. No production code changes.

## Rationale

The architecture doc (`docs/repl_agent_loop_2026-04-27.md`) calls
out one risk above all others: event-ordering regressions. The
TUI depends on lifecycle order
(`AgentStart` → `TurnStart` → prompt `MessageStart`/`MessageEnd`
→ assistant `MessageStart`/`MessageDelta`*/`MessageEnd` → tool
events → `TurnEnd` → next `TurnStart` … → `AgentEnd`), and
`AgentRunResult` consumers (the controller, integration tests,
print-mode) depend on the message-accumulation contract.

The current loop has these implicit invariants that are not
tested as a unit today:

1. `AgentRunResult.generated_messages` excludes prompts.
2. `AgentRunResult.final_context` includes prompts + assistant
   replies + tool result messages + steering messages.
3. A prompt-only run (no tool calls) emits exactly two `TurnEnd`
   events: zero. (One `TurnStart`, one `TurnEnd`, one `AgentEnd`.)
   *Verify the actual count — the loop emits TurnStart at
   `agent_loop.rs:450, 603, 711` and TurnEnd at `:569, 597, 610,
   644, 742`.*
4. Tool calls cause an extra round trip: assistant → tool results
   appended to context → next `TurnStart` → next assistant.
5. Cancellation during the provider stream produces an aborted
   `AssistantMessage` with `StopReason::Aborted`, *not* a
   terminal provider error.
6. Provider stream errors produce an error `AssistantMessage`
   with `StopReason::Error` *and* a populated
   `terminal_error: Some(ProviderError)` so the controller's
   retry policy at `controller.rs:265-274` can fire.
7. Request-options resolution failure
   (`agent_loop.rs:470-493`) produces an error
   `AssistantMessage` and returns *without* the assistant
   appearing in `generated_messages`'s next-turn round trip
   (i.e., the loop terminates immediately).
8. Sequential tool mode preserves the order of tool calls in the
   assistant message; parallel mode returns one result per call,
   in any order, but all results are appended before the next
   model turn.
9. Follow-up messages from
   `agent_loop.rs:589-627` get appended after the assistant and
   before the next `TurnStart`.
10. Steering messages from `execute_tool_calls` get appended
    after tool results and before the next `TurnStart`.

PR 1 tests each of these as a discrete test with a name that
describes the behavior, not the function under test.

## Design

### Test file location

Add a new test module:
`crates/anie-agent/tests/agent_loop_behavior.rs`.

This is an integration-test file (in `tests/`, not `src/`) so it
exercises `AgentLoop` through its public surface. The existing
in-crate tests at `agent_loop.rs:1494-1905` cover sanitization
and config helpers; they stay as-is. We do *not* turn those into
the characterization layer — they're targeted unit tests with a
different purpose.

### Test harness

Reuse what already exists:

- `MockProvider` and `MockStreamScript` from `anie_provider::mock`.
- `StaticResolver` (currently at `agent_loop.rs:1516`) — promote
  to `pub(crate)` or expose via a `test-util` cfg-flag module
  so the integration test can construct one. If promotion is too
  invasive, copy the four-line struct into the integration test
  file; we'll consolidate later if we add more shared scaffolding.
- `TestTool` and `ConcurrencyGuard` from
  `agent_loop.rs::tests` — similarly, expose through a
  `pub(crate)` module gated on `#[cfg(any(test, feature =
  "test-util"))]`, or copy into the integration test. Prefer
  expose-via-feature over copy if the surface is small.

Add **one** new utility: an `EventCollector` that drains the
`mpsc::Receiver<AgentEvent>` into a `Vec<AgentEvent>` after the
run completes, plus a helper that classifies events into
*lifecycle* (start/end markers) vs *content* (deltas) so most
tests can assert on lifecycle order without coupling to delta
text. Live this in the integration test file; promote later if
PR 5's step-machine tests need it.

### Test list

Each test is one `#[tokio::test] async fn`. Names follow the
project convention (behavior under test, not function under
test).

| # | Test | Asserts |
|---|------|---------|
| 1 | `run_without_tools_emits_lifecycle_in_order` | `AgentStart`, `TurnStart`, prompt `MessageStart`/`MessageEnd` (one pair per prompt), assistant `MessageStart`/`MessageEnd`, `TurnEnd`, `AgentEnd` — in order, no extras. |
| 2 | `run_without_tools_returns_assistant_in_generated_messages` | `result.generated_messages` contains the one assistant; prompts excluded. `result.final_context` contains prompts + assistant. |
| 3 | `run_with_one_tool_call_appends_assistant_then_tool_result_then_continues` | Lifecycle: `TurnStart` → assistant (with tool call) → `ToolExecStart`/`End` → tool result `MessageStart`/`MessageEnd` → `TurnEnd` → `TurnStart` → next assistant → `TurnEnd` → `AgentEnd`. Both assistants and the tool result appear in `generated_messages` in that order. |
| 4 | `run_with_parallel_tool_calls_returns_one_result_per_call` | When `parallel_tool_execution=true` and the assistant emits 3 tool calls, all 3 results land in the next turn's context before the next assistant. Order *within* the result batch is not asserted. |
| 5 | `run_with_sequential_tool_calls_preserves_call_order` | Same setup as #4 with `parallel_tool_execution=false`; result order matches call order. |
| 6 | `provider_stream_error_returns_error_assistant_with_terminal_error` | Mock provider yields `Err(ProviderError::...)` mid-stream; result has an assistant with `StopReason::Error` and `terminal_error: Some(_)`. `AgentEnd` still emits. |
| 7 | `cancel_during_provider_stream_returns_aborted_assistant_without_terminal_error` | Fire `cancel.cancel()` after first delta; result has assistant with `StopReason::Aborted`, `terminal_error: None`. |
| 8 | `cancel_during_tool_execution_finishes_run_cleanly` | Assistant emits a tool call; tool waits on `cancel.cancelled()`; cancel mid-execution. Run finishes; tool result reflects cancellation; no orphaned `TurnStart` past the cancel. |
| 9 | `missing_provider_returns_error_assistant_without_terminal_error` | `agent_loop.rs:470-493` — provider lookup fails; result has error assistant; `terminal_error` is `None` (controller does not retry). |
| 10 | `request_options_resolution_failure_returns_terminal_error` | Resolver returns `Err`; result has error assistant *and* `terminal_error: Some(_)` so the retry path can fire. |
| 11 | `follow_up_messages_append_and_start_next_turn` | Configure a `FollowUpProvider` that returns one message after the first turn; lifecycle shows the follow-up's `MessageStart`/`MessageEnd` before the next `TurnStart`. |
| 12 | `steering_messages_append_after_tool_results_before_next_turn_start` | Configure a steering provider; verify steering message lands in the context between tool results and the next `TurnStart`. |
| 13 | `generated_messages_order_matches_emission_order` | A run with prompt → assistant → tool → tool-result → assistant ends with `generated_messages` in exactly that order. |
| 14 | `final_context_includes_prompts_and_all_generated_in_order` | `final_context = prompts ++ generated_messages` (modulo any internal reordering — assert exactly what the current code does, not what we wish it did). |

### What these tests deliberately do *not* assert

- Exact text of `MessageDelta` events. Stream chunking is not
  the contract; mid-stream chunk boundaries can shift.
- Tracing/log output. PR 4 adds spans, but the public contract
  is the event channel, not log lines.
- Internal field names of `AgentRunResult`. We assert against
  the public type's accessors.

If any test in the list above turns out to lock down a behavior
we *want* to change in a later PR (e.g., emitting a different
`StopReason` for cancellation), update the test in the same PR
that changes the behavior — never silently. The whole point of
this layer is that behavior changes get an obvious diff.

## Files to touch

- `crates/anie-agent/tests/agent_loop_behavior.rs` (new file).
- `crates/anie-agent/src/agent_loop.rs` *only* if `StaticResolver`
  / `TestTool` / `ConcurrencyGuard` need a `pub(crate)` exposure
  via a `test-util` feature. Prefer the feature-gate over
  refactoring the test module structure — keep the behavior
  characterization PR additive.
- `crates/anie-agent/Cargo.toml` if a `test-util` feature is
  added.

## Test plan

The tests *are* the deliverable. Beyond running them, PR 1 also
needs to pass the existing suite unchanged:

- `cargo test -p anie-agent` (existing + new tests pass).
- `cargo test --workspace`.
- `cargo clippy --workspace --all-targets -- -D warnings`.

## Risks

- **Test fragility around exact event order.** Mitigation: only
  assert on lifecycle markers in most tests; assert content
  events only in the one or two tests where they're the subject.
- **Exposing test scaffolding leaks into the public API.**
  Mitigation: gate the export behind a `test-util` cargo
  feature; no production code path references it.
- **Mock provider doesn't model the streaming-cancel race
  exactly.** Mitigation: use the existing `tokio::select!`
  pattern in `MockStreamScript`; if the test needs a deterministic
  cancel point, drive cancellation from a `Notify` the mock
  awaits between events.

## Exit criteria

- [ ] `crates/anie-agent/tests/agent_loop_behavior.rs` exists
      with all 14 tests above.
- [ ] All tests pass against unmodified `AgentLoop::run`.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.
- [ ] PR description links this plan and lists the tests.
- [ ] No production-code behavior change in the diff.

## Deferred

- Property-based or fuzzing tests over event sequences. Useful
  later for the step machine, not needed for characterization.
- Tests that exercise the controller's retry policy. Those
  belong in `anie-cli`'s tests; this PR is `anie-agent`-local.
- Step-level tests (one-step-at-a-time assertions). PR 5's
  public step machine will add those when there's something
  step-shaped to assert against.
