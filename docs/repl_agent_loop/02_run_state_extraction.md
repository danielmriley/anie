# PR 2 — Extract `AgentRunState`

**Goal:** Move the local variables currently scattered through
`AgentLoop::run` into a private `AgentRunState` struct with a
small set of helper methods. The loop body still reads
top-to-bottom, but mutation goes through state-owned helpers
instead of direct field manipulation.

This PR is a behavior-preserving refactor. PR 1's
characterization tests must pass unchanged.

## Rationale

Today the run state is implicit:

```rust
// crates/anie-agent/src/agent_loop.rs:440-441
let mut context = context;
let mut generated_messages = Vec::new();
```

Plus an unwritten convention: every place that appends to
`generated_messages` *also* appends to `context`, and the order
of those appends has to stay consistent with the event order so
the controller and TUI see things in sync. That convention is
enforced by hand at every callsite. PR 3 needs an explicit
`AgentObservation::AssistantCollected` / `ToolResults` /
`FollowUpsAppended` boundary, and that boundary has nowhere
clean to land without an owning struct.

PR 2 *only* extracts the struct. PR 3 builds the REPL on top.
Splitting them means the diff for PR 2 is mechanical: every
edit replaces a direct mutation with a method call.

## Design

### Struct shape

```rust
// crates/anie-agent/src/agent_loop.rs (private to this module)
struct AgentRunState {
    context: Vec<Message>,
    generated_messages: Vec<Message>,
    terminal_error: Option<ProviderError>,
    finished: bool,
}

impl AgentRunState {
    fn new(prompts: Vec<Message>, mut context: Vec<Message>) -> Self {
        context.extend(prompts.iter().cloned());
        Self {
            context,
            generated_messages: Vec::new(),
            terminal_error: None,
            finished: false,
        }
    }

    /// Append the assistant returned by the provider stream.
    /// Both context and generated_messages get it; this preserves
    /// the existing invariant.
    fn append_assistant(&mut self, assistant: AssistantMessage) {
        let msg = Message::Assistant(assistant);
        self.context.push(msg.clone());
        self.generated_messages.push(msg);
    }

    /// Append the tool results returned by `execute_tool_calls`.
    /// Same dual-append rule.
    fn append_tool_results(&mut self, results: Vec<ToolResultMessage>) {
        for r in results {
            let msg = Message::ToolResult(r);
            self.context.push(msg.clone());
            self.generated_messages.push(msg);
        }
    }

    /// Append steering messages emitted by tool execution.
    /// These appear in context only (controller sees them via
    /// final_context, not as separate generated entries) — match
    /// the *current* code's behavior; do not change it here.
    fn append_steering(&mut self, messages: Vec<Message>) {
        // EXACT current behavior: see agent_loop.rs:~635 for
        // where steering messages get pushed today. Replicate
        // that — both lists or context only — without
        // adjustment in this PR.
    }

    /// Append follow-up messages from `get_follow_up_messages`.
    fn append_follow_ups(&mut self, messages: Vec<Message>) { /* ... */ }

    fn finish_with_error(&mut self, error: ProviderError) {
        self.terminal_error = Some(error);
        self.finished = true;
    }

    fn finish(&mut self) {
        self.finished = true;
    }

    fn into_result(self) -> AgentRunResult {
        AgentRunResult {
            generated_messages: self.generated_messages,
            final_context: self.context,
            terminal_error: self.terminal_error,
        }
    }
}
```

> **Note on field names.** Match the *current* `AgentRunResult`
> field names exactly (`generated_messages`, `final_context`,
> `terminal_error` per `agent_loop.rs:401-407`). Do not rename
> in this PR.

### What stays in `AgentLoop::run`

- All event emission (`send_event(...)`).
- All provider/tool execution (`collect_stream`,
  `execute_tool_calls`).
- The `loop { ... }` body shape.
- All `?` propagation and error-message construction.

### What moves into helpers

- The `let mut context = context;` and `let mut
  generated_messages = Vec::new();` declarations
  (`agent_loop.rs:440-441`) collapse into
  `AgentRunState::new(prompts, context)`.
- The 6–8 places that push to `context` *and*
  `generated_messages` collapse into single `state.append_*`
  calls.
- The function's tail return (`AgentRunResult { ... }`) collapses
  into `state.into_result()`.

### Helper for `error_assistant_message`

`agent_loop.rs:1066` defines `error_assistant_message`. It
currently constructs the message and the caller pushes it. Keep
that helper as-is, but the callers now push via
`state.append_assistant(error_msg)`. No change to the helper's
signature.

### What the loop body looks like after PR 2

A sketch — exact line-for-line shape will fall out of the
refactor:

```rust
pub async fn run(
    &self,
    prompts: Vec<Message>,
    context: Vec<Message>,
    event_tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
) -> AgentRunResult {
    let mut state = AgentRunState::new(prompts, context);
    let event_tx = &event_tx;

    send_event(event_tx, AgentEvent::AgentStart).await;
    send_event(event_tx, AgentEvent::TurnStart).await;
    self.emit_prompt_message_events(&state, event_tx).await;

    loop {
        // ... existing body, but every `context.push(...)` and
        // `generated_messages.push(...)` becomes a `state.append_*`
        // call. Otherwise unchanged.
        if state.finished {
            break;
        }
    }

    state.into_result()
}
```

The `finished` flag is set only at the points where the current
code currently `return`s an `AgentRunResult`. Those `return`
points become `state.finish*(...); break;` pairs. The `loop`
already has all the right exit branches; we're just re-routing
them.

> **Important:** do not introduce new branches or change the
> *order* of operations. Specifically: the existing code emits
> `TurnEnd` *and* `AgentEnd` before some early returns
> (`agent_loop.rs:569+577`, `:610+618`, `:644+656`). The
> refactored code must emit those events in the same place,
> then break. PR 1's tests will catch any reordering.

## Files to touch

- `crates/anie-agent/src/agent_loop.rs` — extract struct, rewire
  body. Estimated diff: ~150 lines moved/restructured, ~40 lines
  net added (struct + helpers), no lines deleted that aren't
  immediately replaced.

That's the only file.

## Test plan

PR 1's 14 characterization tests are the test plan. Run them
before and after; nothing else should change:

- `cargo test -p anie-agent` (PR 1 tests + existing).
- `cargo test --workspace`.
- `cargo clippy --workspace --all-targets -- -D warnings`.

If any PR 1 test fails, the refactor reordered something. Fix
the refactor — do not edit the test.

Optional: add one new unit test in `agent_loop.rs::tests` that
constructs `AgentRunState::new(...)` directly and asserts
`into_result()` produces an `AgentRunResult` with prompts in
`final_context` and an empty `generated_messages`. This is a
sanity check on the helper itself, separate from the loop.

## Risks

- **Steering-message append rule is subtle.** The current code's
  behavior for steering messages might not match the obvious
  assumption ("dual-append like assistants"). Read
  `agent_loop.rs` around the `execute_tool_calls` callsite
  before writing `append_steering` and replicate exactly. Add a
  PR 1 test (test #12) if not already there.
- **`AgentRunState` and the loop body share a borrow.** The
  refactor must not introduce `&mut self.state` borrows that
  collide with `&self.config` accesses. Likely fine since state
  is a local, but watch for it during clippy.
- **The diff is "mostly mechanical" but visually big.**
  Mitigation: structure the commit so the struct + impl block
  is one logical chunk and the body rewire is contiguous. Avoid
  reformatting unrelated lines.

## Exit criteria

- [ ] `AgentRunState` is private to `agent_loop.rs` (not exposed
      outside the crate, not exposed outside the module).
- [ ] All `context.push(...)` and `generated_messages.push(...)`
      sites in `AgentLoop::run` go through state helpers.
- [ ] `AgentRunResult` construction is centralized in
      `state.into_result()`.
- [ ] PR 1's 14 characterization tests pass without
      modification.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.
- [ ] No public-API change.
- [ ] Optional unit test for `AgentRunState::new` + `into_result`
      added.

## Deferred

- Renaming any `AgentRunResult` field — out of scope.
- Adding new fields to `AgentRunState` (e.g. step counter,
  `next_intent`). PR 3 owns those.
- Moving `error_assistant_message` into `AgentRunState`. Stays
  free function for now.
- Any change to event order. If something feels off, file a
  follow-up; do not fix it here.
