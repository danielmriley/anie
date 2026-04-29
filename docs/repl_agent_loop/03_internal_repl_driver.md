# PR 3 — Internal REPL driver

**Goal:** Reshape `AgentLoop::run`'s body into an explicit
`Read → Eval → Print → Loop` driver. Introduce private
`AgentIntent`, `AgentObservation`, and `AgentDecision` types so
each loop iteration has a single bounded action and a single
observation.

This is still a behavior-preserving refactor. PR 1's
characterization tests pass unchanged.

## Rationale

After PR 2, the loop body uses an `AgentRunState` but its control
flow is still implicit: the body alternates between "stream the
provider" and "if there are tool calls, run them," with various
early returns scattered through the body. PR 3 makes that
control flow explicit so future PRs can:

- expose a step machine (PR 5);
- attach tracing spans per step (PR 4);
- attach policy hooks at the boundary (PR 7);
- add new intent kinds without rewriting the loop (deferred —
  e.g., `RetrieveContext`, `RepairToolCall`, `VerifyDiff`).

The extension points only become possible once the loop is
shaped around an explicit intent → observation → decision cycle.

## Design

### Internal types

```rust
// crates/anie-agent/src/agent_loop.rs (private)

enum AgentIntent {
    /// Run one provider stream, collect the assistant.
    ModelTurn,
    /// Execute the tool calls in the most recent assistant.
    ExecuteTools { tool_calls: Vec<ToolCall> },
    /// Append follow-up messages produced by the follow-up
    /// provider before the next ModelTurn.
    AppendFollowUps { messages: Vec<Message> },
    /// Append steering messages produced by tool execution
    /// before the next ModelTurn.
    AppendSteering { messages: Vec<Message> },
    Finish,
}

enum AgentObservation {
    AssistantCollected {
        assistant: AssistantMessage,
        terminal_error: Option<ProviderError>,
    },
    ToolResults {
        results: Vec<ToolResultMessage>,
        steering: Vec<Message>,
    },
    FollowUpsAppended,
    SteeringAppended,
    /// The intent was Finish — no work performed.
    Finished,
    /// Something failed to even start (resolver error, missing
    /// provider, stream creation failure). The error assistant
    /// has already been pushed to state by the eval.
    PreflightFailed {
        terminal_error: Option<ProviderError>,
    },
}

enum AgentDecision {
    Continue(AgentIntent),
    Finish,
}
```

> The exact split between `ToolResults` and `AppendSteering` may
> simplify if the current code packages them together. Read
> `execute_tool_calls`'s return shape before finalizing — match
> what's there. This plan errs on the side of explicit; collapse
> if the result type already carries both.

### Driver shape

`AgentLoop::run` becomes a small driver:

```rust
pub async fn run(
    &self,
    prompts: Vec<Message>,
    context: Vec<Message>,
    event_tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
) -> AgentRunResult {
    let mut state = AgentRunState::new(prompts, context);
    self.start_run(&mut state, &event_tx).await;

    let mut intent = AgentIntent::ModelTurn;
    while !state.finished {
        let input = self.read_step(&state, &intent).await;
        let observation = self.eval_step(input, &mut state, &event_tx, &cancel).await;
        self.print_step(&mut state, &observation, &event_tx).await;
        intent = match self.decide_next_step(&state, &observation, &cancel) {
            AgentDecision::Continue(next) => next,
            AgentDecision::Finish => {
                state.finish();
                break;
            }
        };
    }

    state.into_result()
}
```

### Phase responsibilities

| Phase | What it does | What it does *not* do |
|-------|--------------|------------------------|
| **Read** | Build the per-intent input snapshot. For `ModelTurn`: resolve `RequestOptionsResolver`, look up provider, sanitize replay context, build `LlmContext`, build `StreamOptions` (current `agent_loop.rs:470-552`). For `ExecuteTools`: borrow tool defs and current cancel token. For others: cheap. | Emit any events. Mutate state. Make any provider/tool calls. |
| **Eval** | Execute the intent. For `ModelTurn`: call `provider.stream(...)`, drive `collect_stream` (still emitting live `MessageStart`/`MessageDelta`/`MessageEnd` and any `TurnEnd`/`AgentEnd` markers that today's code emits *during* the stream). For `ExecuteTools`: call `execute_tool_calls`, still emitting `ToolExec*` events live. | Commit the assistant to state. Emit boundary events that today happen *after* the stream returns (those move to `Print`). |
| **Print** | Commit the observation to `state` (`append_assistant`, `append_tool_results`, etc.). Emit boundary events not already emitted live: e.g., `TurnEnd` after a no-tool-call assistant, `AgentEnd` on finish. | Make further provider/tool calls. |
| **Loop** | Inspect state + observation, return `AgentDecision`. The decision logic mirrors what today's body does: assistant has tool calls → `ExecuteTools`; assistant is clean and follow-ups exist → `AppendFollowUps`; ditto steering after tools → `AppendSteering`; otherwise → `Finish`. | Mutate state. Emit events. |

### Live vs deferred event emission

This is the most dangerous part of the refactor. The current
loop emits `TurnEnd` / `AgentEnd` from many places
(`agent_loop.rs:569, 577, 597, 610, 618, 644, 656, 742, 750`).
After PR 3, those emissions need to consistently land in
`Print` *or* in `Eval` — but each one must land in the same
place, in the same order, with the same triggering condition.

**Rule for this PR:**

- `MessageStart` / `MessageDelta` / `MessageEnd` for the
  assistant stream stay inside `collect_stream` — i.e., emitted
  by `Eval`. Unchanged.
- `ToolExecStart` / `ToolExecUpdate` / `ToolExecEnd` stay inside
  `execute_single_tool` — i.e., emitted by `Eval`. Unchanged.
- `TurnEnd` and `AgentEnd` move to `Print`. These are boundary
  events that *describe* what just happened, so emitting them
  after `Eval` returns is correct and easier to reason about.
- `TurnStart` is emitted by `Read` *or* by the driver before
  invoking `Eval` for a `ModelTurn` intent. Today it's
  scattered (`:450, 603, 711`); after PR 3, the driver emits it
  exactly once per `ModelTurn` intent.
- `AgentStart` and the prompt `MessageStart`/`MessageEnd`
  events emit once during `start_run` before the loop. No
  change.

PR 1's lifecycle tests (#1, #3) catch any reordering.

### Interaction with existing helpers

- `collect_stream` (`agent_loop.rs:757-858`) keeps its
  signature. Called from `eval_step` for `ModelTurn`.
- `execute_tool_calls` (`agent_loop.rs:893-918`) keeps its
  signature. Called from `eval_step` for `ExecuteTools`.
- `error_assistant_message` (`agent_loop.rs:1066`) keeps its
  signature. Called from `eval_step` when preflight fails;
  result returned via `Observation::PreflightFailed`.
- `extract_tool_calls` (`agent_loop.rs:1086`) is called from
  `decide_next_step` to inspect the most recent assistant.

### Decision logic

```rust
fn decide_next_step(
    &self,
    state: &AgentRunState,
    obs: &AgentObservation,
    cancel: &CancellationToken,
) -> AgentDecision {
    if cancel.is_cancelled() { return AgentDecision::Finish; }
    match obs {
        AgentObservation::AssistantCollected { assistant, terminal_error } => {
            if terminal_error.is_some() {
                return AgentDecision::Finish;
            }
            let tool_calls = extract_tool_calls(assistant);
            if !tool_calls.is_empty() {
                return AgentDecision::Continue(AgentIntent::ExecuteTools { tool_calls });
            }
            if let Some(messages) = self.config.follow_up_provider
                .as_ref()
                .and_then(|p| p.get_follow_up_messages(state.context()).ok().flatten())
            {
                return AgentDecision::Continue(AgentIntent::AppendFollowUps { messages });
            }
            AgentDecision::Finish
        }
        AgentObservation::ToolResults { steering, .. } => {
            if !steering.is_empty() {
                AgentDecision::Continue(AgentIntent::AppendSteering {
                    messages: steering.clone(),
                })
            } else {
                AgentDecision::Continue(AgentIntent::ModelTurn)
            }
        }
        AgentObservation::FollowUpsAppended | AgentObservation::SteeringAppended => {
            AgentDecision::Continue(AgentIntent::ModelTurn)
        }
        AgentObservation::PreflightFailed { .. } | AgentObservation::Finished => {
            AgentDecision::Finish
        }
    }
}
```

> The exact `follow_up_provider` access pattern depends on what
> exists in `AgentLoopConfig` today. Read it before writing —
> the call shape may differ.

## Files to touch

- `crates/anie-agent/src/agent_loop.rs` — introduce types, split
  body into phase methods, rewire driver.
- *No other files.* Public API unchanged.

Estimated diff: ~250 lines reorganized, ~150 lines net added
(types + phase methods), no public surface changes.

## Test plan

- PR 1's 14 characterization tests must pass unchanged.
- Add `agent_loop_repl_driver_invokes_phases_in_order` —
  instrument the phase methods (during testing only, via a
  trait-shaped `AgentDriver` indirection or a `cfg(test)`
  counter) to assert `Read → Eval → Print → Decide` runs once
  per iteration. This is the *only* new test; everything else
  rides on PR 1.
- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets -- -D warnings`
  clean.

If the phase-instrumentation test turns out to require invasive
production-code changes, defer it to PR 5 (where the public
step machine makes the same assertion more naturally) and ship
PR 3 with PR 1's tests as the only contract.

## Risks

- **Live event emission gets buffered by accident.** The biggest
  hazard. The TUI relies on assistant deltas streaming
  immediately. Mitigation: PR 1 test #1 asserts lifecycle order;
  add a manual smoke during review (open the TUI, run a 200-
  word prompt, watch the streaming feel). If PR 4's tracing
  spans are added in the same iteration, time deltas can
  confirm streaming is unbuffered.
- **`TurnEnd` ordering shifts subtly.** Mitigation: PR 1
  tests #1 and #3 lock down the exact order. Run them before
  every commit during the refactor.
- **The driver becomes a state machine that's harder to read
  than the original.** Mitigation: keep the driver function
  short (~20 lines). If it grows past 50 lines, split helpers
  rather than expanding the match arms inline. Match the
  legibility bar of `tui.rs::run_tui` after PR 1 of the TUI
  responsiveness plan.
- **Borrow conflicts on `&mut state` across `.await`.**
  Mitigation: phase methods take `&mut state` only where they
  need to mutate (`Print`); `Read` takes `&state`; `Eval` may
  need `&mut state` only for the `PreflightFailed` push, and
  even that can be deferred to `Print` by returning the error
  message in the observation.

## Exit criteria

- [ ] `AgentLoop::run`'s body fits on one screen and reads as a
      Read → Eval → Print → Decide driver.
- [ ] `AgentIntent`, `AgentObservation`, `AgentDecision` exist
      as private enums.
- [ ] Phase methods (`read_step`, `eval_step`, `print_step`,
      `decide_next_step`, `start_run`) exist as private methods
      on `AgentLoop`.
- [ ] PR 1's 14 characterization tests pass.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.
- [ ] No public-API changes.
- [ ] Manual smoke: open the TUI, run a long prompt; streaming
      and tool execution feel unchanged.

## Deferred

- Public `AgentRunMachine` type. PR 5.
- Tracing spans on phase methods. PR 4.
- New intent kinds (`RetrieveContext`, `CompactContext`,
  `RepairToolCall`, `VerifyDiff`, `AskUser`). Not in this
  series — each gets its own follow-up plan after PR 7.
- Per-step `AgentEvent` variants. Out of scope; tracing only.
