# REPL agent-loop architecture plan

## Executive stance

Yes — I agree that a REPL-shaped agent loop should become anie's next
major architectural priority. Only unavoidable CI/security hotfixes
should preempt it; new harness-capability work should either wait for
this foundation or be designed to plug into it. This is not only a
local-small-model idea. Frontier models also benefit from an explicit
step loop because it gives the harness clean places to recover from
errors, compact context, augment context, accept user steering, validate
work, and apply policy without turning the provider stream into a
monolith.

The key design direction:

> anie should move from an implicit provider/tool loop to an explicit
> **Read → Eval → Print → Loop** runtime, where each agent step has
> structured state, a bounded action, an observation, and a policy
> decision before the next step.

A basic REPL loop should preserve today's behavior first. Once it is in
place, we can add local-model enhancements, recursive task
decomposition, verifier loops, dynamic context retrieval, and richer
progress/cancellation semantics on top of it.

## Review tightening notes

After re-reading this plan, the most important constraints are:

1. **Tests first is not optional.** Behavior-characterization tests
   should be their own first PR and should pass before any REPL refactor
   begins. Every later refactor PR should be judged against those tests.
2. **The first REPL implementation is a refactor, not a feature.** Do
   not add planner calls, recursive subtasks, verifier loops, model
   profiles, context retrieval, or new UI events in the initial loop
   rewrite.
3. **Streaming events stay live.** Provider deltas and tool progress
   events must still be emitted while the model/tool step is running.
   In the MVP, `Eval` may emit live stream/progress events; `Print`
   commits the final observation to run state and emits boundary events.
4. **Session/controller policy stays where it is.** The MVP should not
   move retry policy, session persistence, queued-prompt policy, or
   compaction ownership into `anie-agent`.
5. **Public step-machine APIs are second-stage.** The basic definition
   of done is an internal REPL-shaped `AgentLoop::run`. Exposing a
   controller-driven stepper is valuable, but it should come only after
   the internal shape is stable and tested.

## What REPL means for anie

Traditional REPL means:

```text
Read → Eval → Print → Loop
```

For anie:

| REPL phase | anie meaning |
|---|---|
| **Read** | Build the next step's input from current run state: messages, task context, available tools, request options, model profile, budgets, cancellation state, queued user steering, and any policy-provided context augmentation. |
| **Eval** | Execute exactly one bounded action: usually one provider model turn or one tool batch. For streaming actions, this phase may emit live `AgentEvent` deltas/progress while it runs. Later this can also be a verifier call, planner call, context-retrieval call, or recursive subtask step. |
| **Print** | Commit the final observation to the run state and observable transcript: append assistant/tool messages to the in-run context, update generated messages, emit boundary events not already emitted live, record diagnostics, and surface durable progress to UI/RPC/print mode. |
| **Loop** | Decide the next intent: finish, call tools, call the model again, compact, retrieve more context, ask the user, retry, abort, or hand control back to the controller. |

"Print" should be interpreted broadly. It does not only mean stdout; it
means publishing/recording the result of a step into the run state and
observable event stream.

## Why this should be a priority

A REPL loop creates architectural leverage in several directions at once:

1. **Local model quality.** Small models perform better on narrow,
   explicit steps with tool feedback than on broad one-shot tasks.
2. **Frontier model reliability.** Strong models still benefit from
   explicit validation, context refresh, user steering, and recovery
   boundaries.
3. **Compaction opportunities.** Context can be compacted at deliberate
   boundaries instead of only as overflow recovery.
4. **Context augmentation.** anie can retrieve or summarize more context
   between steps when the current observation shows that the model needs
   it.
5. **Error recovery.** Provider errors, malformed tool calls, tool
   failures, and validation failures can become observations handled by
   policy rather than terminal surprises.
6. **Persistent-agent friendliness.** Long tasks can make observable
   progress step by step without arbitrary hard total-runtime caps.
7. **Human steering.** Queued user follow-ups can be folded into the next
   `Read` phase at a safe boundary.
8. **Future recursive techniques.** Decomposition, self-critique,
   branch-and-score planning, and Reflexion-style lessons all need a
   step runtime to avoid becoming ad hoc prompts.

## Current architecture summary

Current code already has a loop, but it is not yet an explicit REPL
runtime.

Evidence from current code:

- `AgentLoop::run` is the monolithic provider/tool loop
  (`crates/anie-agent/src/agent_loop.rs:355`).
- It emits run/turn start events internally
  (`crates/anie-agent/src/agent_loop.rs:371`).
- It resolves request options, chooses a provider, sanitizes context,
  streams a provider response, and collects an assistant message inside
  the same loop body.
- Provider streaming is collected by `collect_stream`, called from the
  run loop (`crates/anie-agent/src/agent_loop.rs:477`).
- Tool calls are extracted and executed internally before the caller sees
  the run result (`crates/anie-agent/src/agent_loop.rs:553`).
- Tool execution itself is a separate helper, but still internal to the
  monolithic run (`crates/anie-agent/src/agent_loop.rs:772`).
- Interactive mode currently treats an active run as a single spawned
  task returning one `AgentRunResult`; the controller stores this in
  `current_run: Option<CurrentRun>` (`crates/anie-cli/src/controller.rs:56`).
- The controller owns retry/backoff, pending retry state, and queued
  prompts around run boundaries (`crates/anie-cli/src/controller.rs:65`,
  `crates/anie-cli/src/controller.rs:74`).
- The TUI/controller boundary already has explicit `UiAction` values
  (`crates/anie-tui/src/app.rs:222`), including queued prompt handling
  in the current work (`crates/anie-tui/src/app.rs:1110`).

This structure works, but the important policy boundaries are implicit:

- before model request;
- after assistant message;
- after tool results;
- before the next provider call;
- before final answer;
- on cancellation;
- on retry/compaction/context refresh.

A REPL refactor should make those boundaries explicit and testable.

## Target architecture

### Short version

Create a step-oriented agent runtime inside `anie-agent`:

```text
AgentLoop::run
  -> create AgentRunState
  -> emit AgentStart / initial prompt events
  -> while not finished:
       Read next AgentStepInput from state + policy
       Eval one AgentIntent
       Print one AgentObservation into state/events
       Loop decide next AgentIntent or Finish
  -> return AgentRunResult
```

The first implementation should keep `AgentLoop::run(...) ->
AgentRunResult` as the public compatibility wrapper. Internally it should
be rewritten around explicit REPL concepts. Later, the controller can
optionally drive the same state machine step-by-step.

### Longer-term direction

After the behavior-preserving internal REPL lands, expose a controller-
driven stepper:

```rust
pub struct AgentRunMachine { /* owned run state */ }

impl AgentRunMachine {
    pub async fn next_step(
        &mut self,
        event_tx: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> AgentStepBoundary;

    pub fn finish(self) -> AgentRunResult;
}
```

Then `AgentLoop::run` becomes a simple helper:

```rust
pub async fn run(...) -> AgentRunResult {
    let mut machine = AgentRunMachine::new(...);
    while !machine.is_finished() {
        machine.next_step(&event_tx, &cancel).await;
    }
    machine.finish()
}
```

This preserves existing callers while opening a path for interactive mode
to interpose controller policy between steps.

## Core terminology

| Term | Meaning |
|---|---|
| **Run** | One controller-started agent execution for a user prompt or continuation. Today this maps to one `AgentLoop::run` call. |
| **Turn** | Existing protocol/UI concept: assistant message plus any tool results that belong to it. Preserve current `TurnStart` / `TurnEnd` semantics. |
| **Step** | One REPL iteration. A step evaluates one bounded intent, such as a model turn or a tool batch. |
| **Intent** | What the next step is supposed to do: call model, execute tools, append follow-up messages, compact, retrieve context, finish, etc. |
| **Observation** | What happened during a step: assistant message, tool results, provider error, cancellation, context update, validation failure, etc. |
| **Decision** | The loop-phase result: continue with another intent, finish, abort, ask controller policy, or request user input. |
| **Boundary** | A stable point where policy can safely inspect/update state before the next step. |

## Basic REPL MVP

The first REPL milestone should be intentionally conservative.

### MVP goals

- Preserve current user-visible behavior.
- Preserve current public `AgentLoop::run` signature.
- Preserve current `AgentEvent` sequence as much as possible.
- Preserve provider contract and tool contract.
- Preserve session persistence ownership in `anie-cli`.
- Make run state and loop decisions explicit inside `anie-agent`.
- Create future extension points without implementing all future features.

### MVP non-goals

- No recursive task decomposition yet.
- No verifier/critic loop yet.
- No new planner model calls yet.
- No session schema change.
- No broad prompt-template overhaul.
- No concurrent independent agent runs.
- No provider API rewrite.
- No new tool approval/sandbox system.

## Proposed internal types

Names are illustrative. The exact Rust shape should be chosen during
implementation.

### `AgentRunState`

Owns mutable run-local state that is currently spread through local
variables in `AgentLoop::run`.

```rust
struct AgentRunState {
    context: Vec<Message>,
    generated_messages: Vec<Message>,
    prompts: Vec<Message>,
    next_intent: AgentIntent,
    turn_state: TurnState,
    finished: bool,
    terminal_error: Option<ProviderError>,
}
```

Responsibilities:

- own canonical in-run context;
- own generated assistant/tool messages returned to controller;
- track whether `TurnStart` has been emitted for the current turn;
- track terminal error/cancellation/final state;
- provide helpers for appending assistant/tool messages consistently.

### `AgentIntent`

Represents the next bounded action.

```rust
enum AgentIntent {
    ModelTurn,
    ExecuteTools { tool_calls: Vec<ToolCall> },
    AppendFollowUps { messages: Vec<Message> },
    Finish,
}
```

Later additions can include:

```rust
enum AgentIntent {
    ModelTurn,
    ExecuteTools { tool_calls: Vec<ToolCall> },
    RetrieveContext { query: ContextQuery },
    CompactContext { reason: CompactionReason },
    VerifyDiff,
    RepairToolCall { invalid_output: String },
    AskUser { question: String },
    Finish,
}
```

### `AgentStepInput`

The read-phase snapshot for a step.

```rust
struct AgentStepInput<'a> {
    intent: &'a AgentIntent,
    context: &'a [Message],
    model: &'a Model,
    tools: &'a [ToolDef],
    system_prompt: &'a str,
    cancel: &'a CancellationToken,
}
```

For a model turn, `Read` also resolves request options, provider, replay
sanitization, and `LlmContext`.

### `AgentObservation`

The eval-phase result.

```rust
enum AgentObservation {
    AssistantCollected {
        assistant: AssistantMessage,
        provider_error: Option<ProviderError>,
    },
    ToolResults {
        results: Vec<ToolResultMessage>,
    },
    FollowUpsAppended,
    Finished,
}
```

Later this can grow to include verifier results, retrieval results,
compaction summaries, and user-steering observations.

### `AgentDecision`

The loop-phase decision.

```rust
enum AgentDecision {
    Continue(AgentIntent),
    Finish,
}
```

Later:

```rust
enum AgentDecision {
    Continue(AgentIntent),
    YieldToController(AgentBoundary),
    AskUser(String),
    Finish,
}
```

## Basic REPL control flow

The MVP should look roughly like this:

```rust
pub async fn run(...) -> AgentRunResult {
    let mut state = AgentRunState::new(prompts, context);
    self.start_run(&mut state, event_tx).await;

    while !state.finished {
        let input = self.read_step(&state).await;
        let observation = self.eval_step(input, event_tx, cancel).await;
        self.print_step(&mut state, observation, event_tx).await;
        let decision = self.decide_next_step(&state, cancel).await;
        state.apply_decision(decision);
    }

    state.into_result()
}
```

In a more explicit intent-driven shape:

```rust
let mut intent = AgentIntent::ModelTurn;
loop {
    let observation = match intent {
        AgentIntent::ModelTurn => {
            let request = self.read_model_request(&state).await?;
            self.eval_model_turn(request, event_tx, cancel).await
        }
        AgentIntent::ExecuteTools { tool_calls } => {
            self.eval_tool_batch(tool_calls, &state, event_tx, cancel).await
        }
        AgentIntent::AppendFollowUps { messages } => {
            self.eval_append_followups(messages).await
        }
        AgentIntent::Finish => break,
    };

    self.print_observation(&mut state, &observation, event_tx).await;
    intent = self.decide_next_intent(&state, &observation, cancel).await;
}
```

## Mapping current behavior to REPL phases

### Run start

Current behavior:

- append prompts to context;
- emit `AgentStart`;
- emit initial `TurnStart`;
- emit `MessageStart`/`MessageEnd` for each user prompt.

REPL placement:

- `AgentRunState::new` appends prompts to context;
- `start_run` prints run-start and prompt events;
- initial intent is `AgentIntent::ModelTurn`.

### Model request

Current behavior inside `AgentLoop::run`:

- resolve request options;
- look up provider;
- apply base URL override;
- sanitize replay context;
- convert messages;
- build `LlmContext`;
- call `provider.stream`;
- collect stream into an assistant message.

REPL placement:

- `Read`: resolve request options, provider, sanitized context,
  `LlmContext`, `StreamOptions`.
- `Eval`: stream provider events into `CollectedAssistant`, emitting
  live `MessageStart` / `MessageDelta` / `MessageEnd` events as today.
- `Print`: append the completed assistant to context/generated messages,
  record any terminal provider error, and emit only boundary events that
  were not already emitted live.
- `Loop`: inspect terminal error, stop reason, and tool calls.

### Tool calls

Current behavior:

- extract tool calls from the assistant;
- execute sequentially or in parallel;
- validate arguments;
- send tool execution events;
- append tool result messages;
- append steering messages;
- emit `TurnEnd`;
- start next turn.

REPL placement:

- `Loop` after assistant chooses `AgentIntent::ExecuteTools` if tool
  calls exist.
- `Eval` executes the tool batch, including live `ToolExecStart`,
  `ToolExecUpdate`, and `ToolExecEnd` events as today.
- `Print` appends tool result messages, appends steering messages, and
  emits `TurnEnd`.
- `Loop` chooses next `ModelTurn`, unless cancellation finished the run.

### No tool calls

Current behavior:

- if follow-up messages exist, append them, end/start turn, and continue;
- otherwise emit `TurnEnd`, `AgentEnd`, return.

REPL placement:

- `Loop` after assistant checks follow-up provider.
- If follow-ups exist: `Continue(AppendFollowUps)` then `ModelTurn`.
- If none: `Finish`.

### Errors and cancellation

Current behavior:

- request/provider build failures create error assistant messages;
- stream errors produce error assistant messages and terminal errors;
- cancellation during stream creates aborted assistant message;
- controller handles retry policy after `AgentRunResult` returns.

REPL placement:

- request/provider build failures are `AgentObservation::AssistantCollected`
  or a dedicated `RequestFailed` observation that prints an error
  assistant and finishes;
- stream errors remain assistant observations with terminal errors;
- cancellation remains an observation that prints an aborted assistant and
  finishes;
- retry policy remains controller-owned in the MVP.

## Event compatibility requirements

The MVP should avoid changing the public event protocol unless absolutely
necessary.

Preserve:

- `AgentStart` once per run;
- `AgentEnd` once per run;
- `TurnStart` / `TurnEnd` semantics;
- `MessageStart`, `MessageDelta`, `MessageEnd` for assistant streams;
- `ToolExecStart`, `ToolExecUpdate`, `ToolExecEnd`;
- generated messages returned in `AgentRunResult`;
- final context returned in `AgentRunResult`;
- terminal provider error returned for controller retry policy.

New step-debug events can be deferred. If useful, they should probably be
tracing spans first, not `AgentEvent` variants, so the TUI protocol does
not churn during the refactor.

Important streaming rule: do not buffer all provider/tool progress until
`Print`. The current UI responsiveness depends on live deltas. The REPL
split should separate **live event emission** from **state commit**, not
turn streaming into a batch-only operation.

## Controller boundary

### MVP

Keep the controller boundary unchanged:

```text
InteractiveController
  -> spawn AgentLoop::run(...)
  -> receive AgentRunResult at completion
  -> persist generated messages
  -> apply retry/compaction/queue policy between runs
```

This keeps the first REPL refactor lower risk.

### Follow-up architecture

After the internal REPL is stable, expose step boundaries so the
controller can optionally drive policy inside a run:

```text
InteractiveController
  -> create AgentRunMachine
  -> loop next_step()
      -> after assistant: maybe inspect user queue or ask verifier
      -> after tool results: maybe compact/retrieve context
      -> before provider request: maybe refresh context files
      -> on error: maybe repair/retry at step level
  -> persist generated messages at run finish
```

This should be additive: `AgentLoop::run` remains the simple
run-to-completion API for print mode, RPC mode, and tests until callers
explicitly need stepwise control.

## Policy ownership

Keep these ownership boundaries:

| Policy | MVP owner | Future REPL opportunity |
|---|---|---|
| Provider/tool streaming mechanics | `anie-agent` | unchanged |
| Session persistence | `anie-cli` / `anie-session` | unchanged |
| Retry/backoff after terminal provider errors | `anie-cli` | step-level retry could be added later, but not MVP |
| Context-overflow compaction retry | `anie-cli` | can move to before-model boundary later if cleaner |
| Tool validation/execution | `anie-agent` | unchanged |
| Context retrieval/augmentation | currently ad hoc/hooks | add as REPL policy at boundaries |
| User queued prompts | `anie-cli` today | feed into future `Read` phase at safe boundaries |
| Model-specific local behavior | config/provider/model metadata | use in future `Read`/`Eval` phases |

Do not move everything into `anie-agent`. The goal is not a god object;
it is a clean step runtime with policy hooks/boundaries.

## Phased implementation plan

### Phase 0 — Behavior characterization

Before refactoring, add focused tests that lock down current behavior.
This should be a standalone PR: no loop restructuring, no new runtime
features, and no semantics changes.

Files likely touched:

- `crates/anie-agent/src/agent_loop.rs`
- test helpers in `anie-agent`, possibly a new test module

Add fake provider/tool infrastructure if needed. The ideal test harness
has:

- a scripted provider that returns controlled `ProviderEvent` streams;
- a scripted request-options resolver that can succeed or fail;
- stub tools that can succeed, fail, emit updates, or wait for
  cancellation;
- an event collector that records `AgentEvent` order without making tests
  brittle around unimportant text-delta details.

Recommended tests:

- `run_without_tools_emits_run_turn_message_events_and_returns_assistant`
- `run_with_tool_call_appends_assistant_then_tool_result_then_continues`
- `provider_stream_error_returns_terminal_error_and_error_assistant`
- `cancel_during_provider_stream_returns_aborted_assistant`
- `missing_provider_finishes_with_error_assistant_without_terminal_provider_error`
- `request_option_resolution_failure_returns_terminal_error`
- `follow_up_messages_append_and_start_next_turn`
- `steering_messages_append_after_tool_results_before_next_model_turn`
- `sequential_tool_mode_preserves_tool_call_order`
- `parallel_tool_mode_returns_one_result_per_call`

Exit criteria:

- Current behavior is documented by tests.
- Refactor PRs can prove behavior preservation.

### Phase 1 — Extract run state

Refactor local variables from `AgentLoop::run` into an explicit private
`AgentRunState`.

Scope:

- `context` and `generated_messages` move into `AgentRunState`.
- Prompt appending and prompt event emission move into helpers.
- Error assistant finishing uses state helpers.
- No new behavior.

Suggested helpers:

```rust
impl AgentRunState {
    fn new(prompts: Vec<Message>, context: Vec<Message>) -> Self;
    fn append_assistant(&mut self, assistant: AssistantMessage);
    fn append_tool_results(&mut self, results: &[ToolResultMessage]);
    fn finish(self) -> AgentRunResult;
}
```

Exit criteria:

- Public API unchanged.
- Event sequence tests pass.
- Diff is mostly mechanical.

### Phase 2 — Introduce internal intents and observations

Add private REPL types and route the existing loop through them.

Scope:

- Add `AgentIntent` with at least `ModelTurn`, `ExecuteTools`,
  `AppendFollowUps`, `Finish`.
- Add `AgentObservation` for assistant/tool/follow-up observations.
- Split current loop body into:
  - `read_model_request`;
  - `eval_model_turn`;
  - `print_assistant_observation`;
  - `eval_tool_batch`;
  - `print_tool_observation`;
  - `decide_next_intent`.
- Keep `collect_stream` and `execute_tool_calls` largely intact.

Exit criteria:

- `AgentLoop::run` visibly reads as a REPL driver.
- All behavior characterization tests pass.
- No controller changes required.

### Phase 3 — Add tracing around REPL steps

Add structured tracing spans for step boundaries.

Scope:

- `agent_repl_step` span with fields:
  - run step index;
  - intent kind;
  - model/provider;
  - context message count;
  - generated message count;
  - tool call count;
  - cancellation state;
  - terminal decision.
- Do not add user-visible `AgentEvent` variants yet unless a clear UI
  need appears.

Exit criteria:

- Debug logs make step progression inspectable.
- No TUI/protocol churn.

### Phase 4 — Public step machine, still behavior-compatible

Expose an additive step-machine API while keeping `AgentLoop::run` as the
run-to-completion wrapper. This phase is valuable, but it is not required
for the basic internal REPL definition of done; do not rush it into the
first refactor PR.

Scope:

```rust
pub struct AgentRunMachine { /* state + config handles */ }

pub enum AgentStepBoundary {
    Continue,
    Finished(AgentRunResult),
}
```

A minimal first public shape can be intentionally small. It does not need
to expose every internal detail yet. Pick one ownership pattern for
finish results: either `next_step` returns the final `AgentRunResult`, or
`next_step` reports `Finished` and a separate consuming `finish()` returns
the result. Avoid exposing two competing ways to retrieve the same final
state.

Potential API:

```rust
impl AgentLoop {
    pub fn start_run_machine(
        &self,
        prompts: Vec<Message>,
        context: Vec<Message>,
    ) -> AgentRunMachine;
}

impl AgentRunMachine {
    pub async fn next_step(
        &mut self,
        event_tx: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> AgentStepBoundary;
}
```

Exit criteria:

- `AgentLoop::run` delegates to `AgentRunMachine`.
- Existing callers do not have to change.
- Tests can drive one step at a time.

### Phase 5 — Controller pilot integration

Use the step machine in interactive mode behind a small, reversible
change.

Scope:

- Replace spawned `AgentLoop::run` task with a spawned task that drives
  the machine step-by-step, or keep the controller's current run task but
  make that task use the machine internally.
- Do not yet move retry/session policy inside the step loop.
- Surface step boundaries through tracing, not UI, at first.

Exit criteria:

- Interactive behavior unchanged.
- Controller still receives one `AgentRunResult` per run.
- The internals are ready for future controller interposition.

### Phase 6 — First policy extension: before-model boundary

After the basic REPL loop is stable, add one real extension point. The
best first one is a before-model boundary because it unlocks context
refresh, context augmentation, and proactive compaction.

Potential boundary:

```rust
pub enum AgentPolicyRequest<'a> {
    BeforeModelRequest {
        context: &'a [Message],
        generated_messages: &'a [Message],
        model: &'a Model,
    },
}
```

Possible policy response:

```rust
pub enum AgentPolicyResponse {
    Continue,
    AppendMessages(Vec<Message>),
    FinishEarly(AssistantMessage),
    RequestControllerIntervention(AgentIntervention),
}
```

`RequestControllerIntervention` is intentionally a future-facing shape.
For the first policy extension, prefer a very small default-noop hook or
an append-messages-only hook. Actual session compaction should remain
controller/session-owned unless a later plan deliberately moves that
boundary.

Keep this phase separate from the behavior-preserving refactor.

Exit criteria:

- At least one policy hook exists at a REPL boundary.
- Default policy preserves old behavior.

## Future capabilities unlocked by REPL

Once the basic loop is explicit, these become much easier to implement.

### 1. Proactive compaction

Before a model request:

- estimate context pressure;
- compact if near threshold;
- preserve task state and recent evidence;
- continue without waiting for provider overflow.

### 2. Context augmentation

Before a model request or after a tool failure:

- retrieve file/symbol summaries;
- refresh context files;
- add repo map excerpts;
- ask the model a focused context-query step;
- keep augmentation bounded by context budget.

### 3. User steering inside long tasks

At safe boundaries:

- read queued user follow-ups;
- classify as correction, new prompt, or interrupt;
- update task state;
- continue from the current boundary instead of waiting for a full run
  restart where appropriate.

### 4. Tool-call repair

If a local model emits malformed tool calls:

- capture parse/validation failure as an observation;
- ask for a corrected call;
- budget repair attempts;
- only execute validated calls.

### 5. Verifier loops

After an edit or before final answer:

- review diff;
- run focused tests;
- critique plan;
- repair failures;
- produce evidence-based final response.

### 6. Recursive task decomposition

If a task is too broad:

- decompose into subtasks;
- solve each subtask with the same REPL engine;
- integrate and validate;
- enforce recursion budgets.

### 7. Model profile adaptation

At `Read` time:

- choose prompt template;
- choose tool-call format;
- choose context budget;
- choose verifier depth;
- choose local backend options.

### 8. Better observability

Every step can report:

- intent;
- model/provider;
- context tokens/messages;
- tool count;
- duration;
- cancellation state;
- validation state;
- repair count.

This is valuable for local model tuning and for persistent-agent
operations.

## Interaction with active-input plans

The active-input queue work is complementary to REPL.

Current queued prompts run after the current run completes. With REPL,
we can eventually offer more nuanced behavior:

- typed drafts remain local UI state;
- queued prompts become controller state;
- at a safe REPL boundary, queued user input can be folded into the next
  `Read` phase;
- interrupt-and-send can cancel the current step and queue a new prompt;
- future task-state logic can classify a queued message as:
  - correction to current task;
  - independent next task;
  - instruction to abort;
  - additional context.

The basic REPL MVP should not change active-input semantics. It should
only create the boundaries that make better semantics possible later.

## Interaction with persistent-agent timeout policy

REPL supports the persistent-agent stance better than global hard
wall-clock timeouts.

Prefer budgets like:

- max repeated identical action;
- max consecutive invalid tool calls;
- max repair rounds;
- max branch depth;
- max no-progress iterations;
- idle/no-output budgets for subprocesses or streams;
- user cancellation at every step boundary;
- context growth thresholds.

Avoid making the REPL loop impose arbitrary short total run durations.
A persistent agent should be allowed to work for a long time if it is
making observable progress and remains cancellable.

## Invariants to preserve

- `anie-agent` remains provider/tool agnostic.
- `anie-cli` remains owner of session persistence and high-level
  interactive policy.
- `anie-tui` remains UI-only and does not call providers/tools directly.
- Provider replay sanitization remains correct for thinking/signature
  requirements.
- Tool definitions remain deterministic.
- Tool arguments remain JSON-schema validated before execution.
- Cancellation reaches provider streaming and tool execution.
- Generated messages are returned in session order.
- User prompts are persisted by the controller, not opportunistically by
  the agent loop.
- Retry policy remains structured around `ProviderError`, not strings.

## Risks and mitigations

### Risk: event ordering regressions

The TUI depends on event lifecycle ordering. Add characterization tests
before the refactor and avoid new `AgentEvent` variants in the MVP.

### Risk: session ordering bugs

The controller persists generated messages after `AgentRunResult`. Keep
that boundary unchanged until the step machine is well tested.

### Risk: over-generalizing too early

Do not implement recursive planning, verifier loops, memory, and context
retrieval in the first REPL PR. First make the existing loop explicit.

### Risk: async state-machine complexity

Avoid holding borrows across `.await` in complex enums. Prefer owned
step request structs where needed. Keep provider stream collection as a
single async helper in the MVP.

### Risk: performance regressions

The refactor should not clone full context unnecessarily. Preserve the
existing sanitizer fast path and deterministic tool-definition cache.
Add tracing only at coarse step boundaries.

### Risk: unclear policy ownership

Use explicit boundary types. Do not sneak controller policy into
`anie-agent` just because the loop is now explicit.

### Risk: tests become brittle around exact events

Event-order tests should check meaningful lifecycle order without
requiring every text delta detail unless that detail is the behavior
under test.

## Test strategy

### `anie-agent` tests

Focus on behavior preservation and step semantics:

- no-tool run;
- tool run;
- multiple tool calls in sequential and parallel modes;
- provider request resolution failure;
- provider stream creation failure;
- provider stream mid-stream error;
- cancellation during stream;
- cancellation during tools;
- follow-up messages;
- steering messages;
- generated message ordering;
- final context ordering;
- terminal error propagation.

### `anie-cli` tests

After public step-machine/controller integration:

- retry policy still triggers after terminal provider error;
- compaction retry still works;
- queued prompt still drains after run boundary;
- quit/abort still cancels active run;
- print-mode exit still waits for the run result.

### `anie-tui` tests

Should mostly remain unchanged for the MVP. Later step events/progress
can add tests if the UI starts rendering step-level state.

### Workspace validation

For every non-doc PR:

- `cargo fmt --all -- --check`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`

## Definition of "basic REPL loop done"

The basic REPL architecture is done when:

- [ ] Behavior-characterization tests landed before the refactor and
      still pass after it.
- [ ] `AgentLoop::run` is implemented as a small driver over explicit
      run state, intents, observations, and decisions.
- [ ] Current provider/tool behavior is preserved.
- [ ] Current event lifecycle is preserved.
- [ ] Provider streaming deltas and tool progress updates are still
      emitted live, not replayed only after a step completes.
- [ ] Current cancellation behavior is preserved or improved with tests.
- [ ] Current controller/session persistence boundary is preserved.
- [ ] Tests cover no-tool, tool, error, cancellation, follow-up, and
      steering paths.
- [ ] Tracing makes step boundaries inspectable.
- [ ] `docs/arch/anie-rs_architecture.md` is updated to describe the
      REPL loop as current architecture.
- [ ] There is a clear next extension point for context augmentation or
      proactive compaction.

## Suggested next planning documents

This document is an architecture basis. The implementation should be
split into concrete plan files, likely under:

```text
docs/repl_agent_loop/
  README.md
  01_behavior_characterization.md
  02_run_state_extraction.md
  03_internal_repl_driver.md
  04_step_machine_api.md
  05_controller_pilot.md
  06_first_policy_boundary.md
  execution/README.md
```

The first three plans should be behavior-preserving refactors. The first
feature-bearing plan should be a single policy boundary, preferably
before-model context policy, not the entire local-agent vision at once.

## Final recommendation

Make REPL the foundation, but land it carefully:

1. Lock down current behavior with tests.
2. Extract explicit run state.
3. Convert the internal loop into intents, observations, and decisions.
4. Keep the public API stable at first.
5. Add step-machine access once internals are stable.
6. Add one policy extension point.
7. Build local-model, recursive, verifier, and context-intelligence
   features on top.

This gives anie a much stronger architecture without destabilizing the
working provider/tool/session stack. It is the right foundation for both
frontier-model robustness and best-in-class local small-model behavior.
