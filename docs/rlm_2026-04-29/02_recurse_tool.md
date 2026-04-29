# Plan 02 — RLM `recurse` tool (shape 1)

**Branch:** `dev_rlm` (a fresh branch off `main` after Plan 01
lands).
**Status:** ready to spec; ship after Plan 01.

## Rationale

The smallest concrete adoption of RLM that captures the core
idea: a `recurse` tool that the master agent calls like any
other tool, which spawns a fresh `AgentRunMachine` over a
focused subset of context, runs it to completion, and
returns the sub-call's final assistant text.

From the model's perspective it's just another tool. From
the harness's perspective it's a recursive agent invocation
with a clean context boundary. The master's context window
stops being a constraint on input size — it's a constraint
on how many sub-call summaries you accumulate, which is
much smaller.

This is shape 1 of three RLM shapes (the others are at
plans 03 and 04). Shape 1 lets us measure the win against
the eval suite before committing to architectural moves.

## What "context that's external to the model" means here

The `recurse` tool takes a *scope*: a description of which
messages from the parent's `AgentRunState.context` the
sub-call should see, plus a sub-query the sub-call should
answer.

Initial scope kinds (extensible):

```rust
enum RecurseScope {
    /// All messages between two indexes (exclusive end) of the
    /// parent's context. Used for "navigate the older half of
    /// the conversation."
    MessageRange { start: usize, end: usize },
    /// All messages whose content matches a regex. Used for
    /// "find every place I read a file."
    MessageGrep { pattern: String },
    /// A specific tool result by call id. Used for "examine
    /// what `bash` returned three turns ago."
    ToolResult { tool_call_id: String },
    /// The contents of a file path on disk. Used for "read
    /// this file and answer this." Note: this is just a
    /// convenience — the sub-agent could also use `read`,
    /// but starting it with the file in its prompt skips the
    /// extra round-trip.
    File { path: String },
}
```

The model picks a scope, supplies a sub-query, and the tool
spawns a sub-agent with that scope as its only prompt
context plus the sub-query as its user message.

## Design

### Tool definition

```rust
// crates/anie-tools/src/recurse.rs (new file)

pub struct RecurseTool {
    /// Factory that yields a fresh AgentLoop for sub-calls.
    /// The factory pattern keeps the master loop's
    /// AgentLoopConfig (system prompt, model, etc.) usable
    /// at sub-call time without the model having to
    /// re-specify it.
    sub_agent: Arc<dyn SubAgentFactory>,
    /// Total recursion budget across all sub-calls in a run.
    /// Each `recurse` invocation decrements; when zero, the
    /// tool errors with a clear "recursion budget
    /// exhausted" message.
    budget: Arc<AtomicU32>,
    /// Maximum recursion depth. A sub-agent calling
    /// `recurse` itself is allowed (cleanly nested
    /// AgentRunMachines) up to this depth. Default 2.
    max_depth: u8,
}
```

### Tool argument schema

```json
{
  "type": "object",
  "properties": {
    "query": {
      "type": "string",
      "description": "The sub-question for the recursive agent to answer."
    },
    "scope": {
      "type": "object",
      "oneOf": [
        { "properties": { "kind": {"const": "message_range"}, "start": {"type": "integer"}, "end": {"type": "integer"} } },
        { "properties": { "kind": {"const": "message_grep"}, "pattern": {"type": "string"} } },
        { "properties": { "kind": {"const": "tool_result"}, "tool_call_id": {"type": "string"} } },
        { "properties": { "kind": {"const": "file"}, "path": {"type": "string"} } }
      ]
    }
  },
  "required": ["query", "scope"]
}
```

### Tool execution

```rust
async fn execute(
    &self,
    call_id: &str,
    args: serde_json::Value,
    cancel: CancellationToken,
    update_tx: Option<mpsc::Sender<ToolResult>>,
    ctx: &ToolExecutionContext,
) -> Result<ToolResult, ToolError> {
    // 1. Decrement budget. If exhausted, return an error
    //    result that names the budget so the master agent
    //    can adapt.
    if self.budget.fetch_update(...).is_err() { ... }

    // 2. Resolve `scope` against the parent's context. The
    //    parent context isn't in `ctx` today; either:
    //    (a) thread parent context through ToolExecutionContext, or
    //    (b) inject scope-resolved messages into the tool via a
    //        `ContextProvider` field that the controller installs
    //        per run.
    //    (b) is cleaner and matches how `update_tx` is plumbed.
    let prompt_messages = self.context_provider.resolve(scope)?;

    // 3. Build a sub-agent and drive it.
    let sub_agent = self.sub_agent.build(...)?;
    let (sub_event_tx, sub_event_rx) = mpsc::channel(64);
    // Best-effort phase update so the user sees the recursion
    // happening in the TUI.
    emit_phase(&update_tx, "recurse", "running").await;

    let mut machine = sub_agent
        .start_run_machine(
            vec![Message::User(UserMessage { /* the sub-query */ })],
            prompt_messages,
            &sub_event_tx,
        )
        .await;
    while !machine.is_finished() {
        machine.next_step(&sub_event_tx, &cancel).await;
    }
    let sub_result = machine.finish(&sub_event_tx).await;

    // 4. Forward sub-events as ToolExecUpdate? Initial
    //    version: don't — keep the master event stream
    //    clean. The recursion is opaque from the master's
    //    perspective. Future enhancement: bubble up
    //    sub-events with a recursion-depth tag.

    // 5. Build the tool result from the sub-call's final
    //    assistant text. If terminal_error, return a tool
    //    error.
    if let Some(err) = sub_result.terminal_error {
        return Ok(error_tool_result(format!("recurse: {err}")));
    }
    let final_text = extract_final_assistant_text(&sub_result);
    Ok(ToolResult {
        content: vec![ContentBlock::Text { text: final_text }],
        details: serde_json::json!({
            "tool": "recurse",
            "scope": scope_for_details,
            "sub_call_message_count": sub_result.generated_messages.len(),
        }),
    })
}
```

### `SubAgentFactory` and `ContextProvider`

Two new traits, both implemented by the controller:

```rust
// In anie-agent
pub trait SubAgentFactory: Send + Sync {
    fn build(&self, parent_ctx: &SubAgentBuildContext)
        -> Result<AgentLoop, anyhow::Error>;
}

pub struct SubAgentBuildContext {
    pub depth: u8,
    pub recursion_budget: Arc<AtomicU32>,
    /// The model the sub-agent should use. Defaults to the
    /// parent's model; can be overridden (e.g., use a smaller
    /// summarizer model for sub-calls).
    pub model_override: Option<Model>,
}
```

```rust
// In anie-agent
pub trait ContextProvider: Send + Sync {
    fn resolve(&self, scope: &RecurseScope)
        -> Result<Vec<Message>, anyhow::Error>;
}
```

The controller is the implementer. The controller's
`ContextProvider` reads from the **parent's**
`AgentRunState.context` (the run that spawned the recursion)
plus the on-disk file system for `RecurseScope::File`.

Wiring: when the controller spawns a run via
`run_via_step_machine`, it constructs a `RecurseTool`
configured with:

- `SubAgentFactory` that yields a clone of the same
  `AgentLoop` config (system prompt, model, providers) but
  with depth-aware tool registry.
- `ContextProvider` that holds a (read-only) view of the
  parent run's context.
- Shared `recursion_budget: Arc<AtomicU32>` initialized to
  e.g. 16 per top-level run.

### Sub-agent system prompt

The sub-agent gets a different system prompt:

```text
You are a focused sub-agent invoked via the `recurse` tool.
Your only job is to answer the sub-query directly, using
the messages provided as your only context. Do not invoke
`recurse` recursively unless the parent agent's question
genuinely requires another level of decomposition. Keep
your final answer concise — the parent agent will receive
your final assistant text as a tool result, so brevity is
important.
```

Built via the same `build_system_prompt` machinery in
`anie-cli` but switched on a `SystemPromptKind::SubAgent`
flag.

### Recursion depth

`max_depth = 2` for the first ship. Sub-agent at depth 1 can
call `recurse` (depth 2 sub-sub-agent), but its
`SubAgentBuildContext.depth` is checked at tool-build time —
if depth is at max, the sub-agent's tool registry doesn't
include `recurse`. The model literally can't see the tool
when it's exhausted the depth, which is a cleaner contract
than runtime errors.

## Why the REPL refactor makes this easy

The whole shape is possible because:

1. `AgentRunMachine::start_run_machine + next_step + finish`
   is a clean recursion boundary. We can spawn a fresh
   machine inside a tool call and drive it to completion
   without any new infrastructure.
2. Streaming events from the sub-call go to a *separate*
   `mpsc::channel` — the master's event stream is
   undisturbed. The model sees a single tool-result message
   with the sub-call's final answer.
3. `BeforeModelPolicy` on the sub-agent can be different
   from the parent. We can install a "sub-agent doesn't
   inject the repo map" policy if the parent's repo-map
   injection isn't relevant for the sub-call's scope.

Without the REPL refactor, this would require carving a
recursion boundary out of the old monolithic `AgentLoop::run`
— which would have been a multi-week refactor on its own.

## Files to touch

- `crates/anie-tools/src/recurse.rs` — new file, the tool
  implementation.
- `crates/anie-tools/src/lib.rs` — export `RecurseTool`.
- `crates/anie-agent/src/lib.rs` — add `SubAgentFactory` and
  `ContextProvider` traits.
- `crates/anie-agent/src/agent_loop.rs` — minor: confirm
  `AgentRunMachine` is reachable from outside the crate
  (already public per Plan 05 of REPL refactor) and that
  `AgentLoop::start_run_machine` is reachable.
- `crates/anie-cli/src/controller.rs` — wire up the
  `SubAgentFactory` + `ContextProvider` implementations,
  install `RecurseTool` in the tool registry per run.
- `crates/anie-cli/src/system_prompt.rs` (or wherever
  `build_system_prompt` lives) — add a `SystemPromptKind`
  enum with `Master` and `SubAgent` variants.

## Test plan

| # | Test | Asserts |
|---|------|---------|
| 1 | `recurse_tool_builds_subagent_with_scoped_messages` | Stub `SubAgentFactory` returns a recording sub-agent; assert sub-agent received only the messages selected by the scope. |
| 2 | `recurse_tool_returns_subagent_final_assistant_text` | Sub-agent yields a known final text; assert the tool result's text matches. |
| 3 | `recurse_tool_propagates_terminal_error` | Sub-agent returns `AgentRunResult { terminal_error: Some(...) }`; assert the tool result is an error result. |
| 4 | `recurse_tool_decrements_budget` | Build with budget=2; call recurse twice; third call returns an error result naming "recursion budget". |
| 5 | `recurse_tool_excluded_at_max_depth` | A sub-agent at depth=`max_depth` does not have `recurse` in its tool registry. |
| 6 | `recurse_scope_message_range_resolves_correctly` | Given a parent context of 10 messages, a scope of `{start: 2, end: 5}` resolves to messages 2..=4. |
| 7 | `recurse_scope_message_grep_matches_pattern` | Given a parent context with 5 messages and a regex pattern matching 2 of them, scope returns those 2. |
| 8 | `recurse_scope_file_reads_disk` | A scope of `{kind: "file", path: "..."}` reads the file from disk. |
| 9 | `recurse_tool_propagates_cancellation` | Cancel the parent's token; assert the in-flight sub-agent is also cancelled. |

Plus an end-to-end smoke (against Ollama) where the model
calls `recurse` to answer a question about an earlier turn.

## Risks

- **Cost.** Each sub-agent call is at least one full LLM
  round-trip plus any tool calls it makes. The budget
  (16/run default) caps it; eval data will tell us if the
  number's right.
- **Latency.** A `recurse` call blocks the master while the
  sub-agent runs to completion. This is fine — the master's
  next turn waits on the tool result anyway — but it does
  mean the user sees a longer pause between turns. Phase
  updates (`recurse: running`) help.
- **Sub-agent loops.** A sub-agent might fall into a
  multi-turn task instead of returning quickly. The sub-
  agent system prompt mitigates this; a hard cap on
  sub-agent steps (e.g. `sub_max_steps = 10`) is a
  belt-and-suspenders option for ship.
- **Context leakage.** If `ContextProvider` accidentally
  exposes more than the requested scope, recursion stops
  saving context. Tests #1, #6, #7 lock down the scope
  contract.

## Exit criteria

- [ ] `recurse` tool is registered when the run starts.
- [ ] All scope kinds in `RecurseScope` resolve correctly.
- [ ] Sub-agent uses the `SystemPromptKind::SubAgent` system prompt.
- [ ] Recursion budget is shared across sub-calls within a run.
- [ ] `recurse` not present in the sub-agent's tools at `max_depth`.
- [ ] All 9 unit tests pass.
- [ ] Manual smoke (Ollama qwen3.5:9b) where the model
      successfully uses `recurse` on a multi-turn session
      to answer a question about an earlier turn.
- [ ] Workspace tests + clippy + fmt clean.
- [ ] No new public AgentEvent variants.

## Deferred

- Streaming sub-agent events to the master's event channel.
  Initial version: opaque. Future: tag with depth and
  bubble up so the TUI can show a nested progress
  indicator.
- Fine-grained scope kinds (`SymbolName`, `RegexInFile`,
  embedding similarity, etc.). Add when concrete need
  appears.
- A `SymbolicLink` scope that lets the model construct a
  scope by name (`recent_python_code`) rather than
  programmatically. Useful but speculative; eval data will
  tell us.
- Caching of sub-call results. The same scope + query asked
  twice should hit a cache. Useful but adds correctness
  surface; defer.
