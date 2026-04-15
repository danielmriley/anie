# Integration Testing Plan for anie

**Date:** 2026-04-15

---

## Context

The status report (`docs/status_report_2026-04-15.md`) identified that all 165 existing tests are unit tests living inside their respective crates. There are no integration tests that exercise multi-crate flows end-to-end.

This report evaluates whether integration tests are needed, what they should cover, and how they should be structured.

---

## Why integration tests matter for this project

The anie workspace has clean crate boundaries. Each crate is well unit-tested in isolation. But the system's real value emerges from the **interactions between crates**, and those interactions are currently tested only indirectly.

The key integration seams are:

1. **Controller → Agent loop → Mock provider → Tool execution → Session persistence**
   - The controller builds an `AgentLoop`, runs it with owned context, receives an `AgentRunResult`, persists messages to the session, and emits `AgentEvent`s to the TUI.
   - No single crate's unit tests exercise this full chain.

2. **Session persistence → Context rebuild → Agent loop replay**
   - Messages are persisted to JSONL. On resume, they are read back, the context is rebuilt, and the agent loop continues from that state.
   - Session unit tests verify JSONL roundtrips. Agent unit tests verify the loop. But no test verifies that a persisted session produces a valid agent-loop input on resume.

3. **Provider response → Agent loop → TUI rendering**
   - The TUI tests use `AgentEvent` sequences to verify rendering. The agent tests use `MockProvider` to verify event emission. But no test connects a mock provider response through the agent loop and into a TUI render.

4. **Tool execution → File system → Session persistence of tool results**
   - Tool unit tests verify file operations against real temp directories. Agent tests verify tool dispatch with `TestTool`. But no test verifies that a real `ReadTool`/`EditTool` result flows through the agent loop and lands in a session file.

5. **Config + Auth → Provider resolution → Request options**
   - Config unit tests verify TOML parsing. Auth unit tests verify key resolution. But no test verifies that a loaded config produces a correctly wired provider registry with working request-option resolution.

### What could go wrong without integration tests

- A `SessionContextMessage` shape change in `anie-session` could produce context that `anie-agent` accepts but the provider rejects.
- A `ToolResult` detail field added by `anie-tools` could fail to serialize when `anie-session` persists it.
- A new `AgentEvent` variant could be emitted by the agent loop but not handled by the TUI, causing a silent rendering gap.
- A config schema change could produce a valid `AnieConfig` that wires up an invalid provider registry.

These are exactly the class of bugs that unit tests miss and integration tests catch.

---

## What should NOT be integration-tested

- **Real network calls to providers.** Integration tests should use `MockProvider`, not live APIs.
- **Real terminal rendering.** The TUI's `TestBackend` is already used in unit tests and works well.
- **Exhaustive tool behavior.** Tool edge cases (BOM, CRLF, fuzzy matching) belong in `anie-tools` unit tests.
- **Provider-specific SSE parsing.** That belongs in `anie-providers-builtin` unit tests.

The goal is to test the **wiring between crates**, not to re-test each crate's internals.

---

## Proposed test categories

### Category 1: Agent loop → real tools → session persistence

These tests verify the full vertical slice that matters most: a user prompt flows through the agent loop, triggers real tool execution against a temp directory, produces an `AgentRunResult`, and the generated messages are persisted to a real session file.

**Test cases:**

1. **Prompt → assistant response → session persistence**
   - Create a `MockProvider` that returns a text-only assistant message.
   - Run the agent loop with a real `SessionManager` writing to a temp directory.
   - Verify the session file contains the user prompt and assistant response.
   - Reopen the session and verify `build_context()` reconstructs the same messages.

2. **Prompt → tool call → tool result → assistant response → session persistence**
   - Create a `MockProvider` scripted to: request a `read` tool call, then return a final answer.
   - Register a real `ReadTool` pointed at a temp directory with a known file.
   - Run the agent loop and persist the result.
   - Verify the session contains: user prompt, assistant (with tool call), tool result (with file contents), final assistant.
   - Reopen the session and verify context reconstruction.

3. **Prompt → edit tool → diff in session**
   - Write a known file to a temp directory.
   - Script the mock provider to request an `edit` tool call.
   - Verify the tool result in the session contains a diff in its `details`.
   - Verify the file on disk was actually modified.

4. **Prompt → bash tool → session persistence with elapsed time**
   - Script the mock provider to request a `bash` tool call (`echo hello`).
   - Verify the tool result in the session contains `elapsed_ms` in details.

5. **Multi-turn tool loop → session has correct message count and order**
   - Script the mock provider for two tool-call rounds before a final answer.
   - Verify the session message sequence: user, assistant, tool_result, assistant, tool_result, assistant.
   - Verify `build_context()` returns them in the correct order.

### Category 2: Session resume and context continuity

These tests verify that persisted sessions can be reopened and produce valid agent-loop inputs.

**Test cases:**

6. **Persist a session → reopen → build context → run agent loop**
   - Write a session with a user prompt and assistant response.
   - Reopen the session and build context.
   - Create a new agent loop with the rebuilt context and a new user prompt.
   - Verify the agent loop completes successfully with the prior context visible.

7. **Persist a session with thinking content → reopen → verify thinking blocks survive**
   - Write an assistant message with `ContentBlock::Thinking` to a session.
   - Reopen and verify thinking blocks are present in the rebuilt context.

8. **Compaction → resume → agent loop still works**
   - Write enough messages to trigger compaction.
   - Run compaction with a mock provider (summary = "prior work summary").
   - Verify the reopened session's context starts with the compaction summary.
   - Run the agent loop with the compacted context and verify it completes.

### Category 3: Agent events → TUI rendering consistency

These tests verify that the `AgentEvent` stream produced by the agent loop renders correctly when fed into the TUI's `App`.

**Test cases:**

9. **Agent loop events → TUI renders user prompt, assistant text, and tool blocks**
   - Run an agent loop with a mock provider and a real tool.
   - Collect all `AgentEvent`s.
   - Replay them into a TUI `App` instance.
   - Render to a `TestBackend` and verify the screen contains the expected text.

10. **Agent loop with thinking → TUI renders thinking section above answer**
    - Script the mock provider to emit `ThinkingDelta` and `TextDelta` events.
    - Collect and replay into the TUI.
    - Verify the rendered screen has the thinking section above the answer.

11. **Agent loop error → TUI renders error message**
    - Script the mock provider to emit a stream error.
    - Collect and replay into the TUI.
    - Verify the rendered screen contains the error text.

### Category 4: Config → provider registry wiring

These tests verify that configuration produces a correctly wired system.

**Test cases:**

12. **Default config → provider registry has OpenAI and Anthropic**
    - Load the default config.
    - Build the provider registry with `register_builtin_providers`.
    - Verify both `ApiKind::OpenAICompletions` and `ApiKind::AnthropicMessages` are registered.

13. **Custom local provider config → model catalog includes custom models**
    - Write a config TOML with a custom Ollama provider and model.
    - Load the config and call `configured_models()`.
    - Verify the custom model appears with the correct base URL and API kind.

14. **Auth resolver with config env var → resolves key from environment**
    - Write a config with `api_key_env = "TEST_KEY"`.
    - Set the environment variable.
    - Verify the resolver produces the expected key.

---

## Where the tests should live

### Recommended structure

```
tests/
├── integration/
│   ├── mod.rs
│   ├── agent_session.rs      ← Category 1 + 2
│   ├── agent_tui.rs           ← Category 3
│   └── config_wiring.rs       ← Category 4
```

These would be Rust integration tests in the workspace root, not inside any single crate. They would depend on multiple crates simultaneously.

### Why workspace-level integration tests

- They naturally cross crate boundaries.
- They don't pollute any single crate's test surface.
- They can depend on `anie-agent`, `anie-tools`, `anie-session`, `anie-tui`, `anie-config`, `anie-auth`, and `anie-provider` simultaneously.
- They run with `cargo test --workspace` alongside the existing unit tests.

### Alternative: a dedicated `anie-integration-tests` crate

If workspace-root `tests/` causes issues with dependency resolution (since `tests/` at the root applies to the default workspace member, which is `anie-cli`), a cleaner option is a dedicated test-only crate:

```
crates/anie-integration-tests/
├── Cargo.toml
├── src/
│   └── lib.rs               ← empty, just enables the test harness
└── tests/
    ├── agent_session.rs
    ├── agent_tui.rs
    └── config_wiring.rs
```

This crate would:
- list all needed workspace crates as `[dev-dependencies]`
- not produce a binary or library
- only contain `#[test]` functions
- be included in `cargo test --workspace`

This is the **recommended approach** for anie because the workspace has a `default-members` configuration that would make root-level `tests/` only apply to `anie-cli`.

### Cargo.toml for the test crate

```toml
[package]
name = "anie-integration-tests"
version.workspace = true
edition.workspace = true
publish = false

[dependencies]
anie-agent.workspace = true
anie-auth.workspace = true
anie-config.workspace = true
anie-protocol.workspace = true
anie-provider.workspace = true
anie-providers-builtin.workspace = true
anie-session.workspace = true
anie-tools.workspace = true
anie-tui.workspace = true

anyhow.workspace = true
ratatui.workspace = true
serde_json.workspace = true
tempfile.workspace = true
tokio.workspace = true
tokio-util.workspace = true
```

---

## Shared test infrastructure

The integration tests will need some shared helpers. These should live in a `common` module within the test crate.

### Mock provider helpers

The existing `MockProvider` and `MockStreamScript` in `anie-provider/src/mock.rs` are already public and suitable for integration tests. No duplication needed.

### Agent loop runner

The agent tests already have a `collect_run` helper that runs the agent loop and collects events. A similar helper should be available in the integration tests:

```rust
async fn run_agent_collecting_events(
    agent: AgentLoop,
    prompts: Vec<Message>,
    context: Vec<Message>,
) -> (AgentRunResult, Vec<AgentEvent>) {
    let (event_tx, mut event_rx) = mpsc::channel(128);
    let cancel = CancellationToken::new();
    let handle = tokio::spawn(async move {
        agent.run(prompts, context, event_tx, cancel).await
    });
    let mut events = Vec::new();
    while let Some(event) = event_rx.recv().await {
        let is_end = matches!(event, AgentEvent::AgentEnd { .. });
        events.push(event);
        if is_end { break; }
    }
    (handle.await.expect("agent task"), events)
}
```

### Session helpers

```rust
fn create_temp_session() -> (tempfile::TempDir, SessionManager) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();
    let session = SessionManager::new_session(dir.path(), &cwd)
        .expect("new session");
    (dir, session)
}
```

### Tool registry with real tools

```rust
fn real_tool_registry(cwd: &Path) -> Arc<ToolRegistry> {
    let queue = Arc::new(FileMutationQueue::new());
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ReadTool::new(cwd)));
    registry.register(Arc::new(WriteTool::with_queue(cwd, Arc::clone(&queue))));
    registry.register(Arc::new(EditTool::with_queue(cwd, Arc::clone(&queue))));
    registry.register(Arc::new(BashTool::new(cwd)));
    Arc::new(registry)
}
```

### TUI render helper

```rust
fn render_events_to_screen(events: &[AgentEvent], width: u16, height: u16) -> String {
    let (event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::channel(8);
    let mut app = App::new(event_rx, action_tx);
    for event in events {
        app.handle_agent_event(event.clone()).expect("handle event");
    }
    let mut terminal = Terminal::new(TestBackend::new(width, height))
        .expect("test terminal");
    terminal.draw(|frame| app.render(frame)).expect("draw");
    // extract screen text from backend buffer
}
```

---

## Implementation order

1. **Create the `anie-integration-tests` crate** with an empty lib and Cargo.toml.
2. **Add it to the workspace** members list.
3. **Implement Category 1 tests first** (agent → tools → session) — these have the highest value.
4. **Implement Category 2 tests** (session resume) — validates persistence continuity.
5. **Implement Category 3 tests** (agent → TUI) — validates the event contract.
6. **Implement Category 4 tests** (config → wiring) — validates startup correctness.

### Estimated scope

- ~14 test functions across 3 files
- ~1 shared helpers module
- No new production code changes required
- The `MockProvider` and `MockStreamScript` are already public and ready to use

---

## Risks and constraints

### Test speed
Integration tests that use real file I/O and async agent loops will be slower than pure unit tests. Keep each test focused and avoid unnecessary provider round-trips.

### Temp directory cleanup
All file-system tests should use `tempfile::TempDir` for automatic cleanup.

### Event ordering sensitivity
Tests that assert on `AgentEvent` sequences should use the `event_kinds` pattern from the agent tests — match on event type order, not exact content — unless specific content assertions are needed.

### Mock provider script exhaustion
If the mock provider runs out of scripted responses, it returns a `ProviderError`. Tests must script exactly the right number of responses for their scenario.

### Session file format coupling
Integration tests that inspect JSONL content directly will be fragile if the session format changes. Prefer asserting through `build_context()` rather than parsing raw JSONL.

---

## Recommendation

Integration tests should be added. The crate boundaries are clean and well-tested individually, but the wiring between them — especially the controller → agent → tools → session chain — is a real gap.

The highest-value starting point is Category 1 (agent loop with real tools and session persistence). These tests would catch the most likely class of cross-crate bugs with the least setup overhead.

Category 3 (agent → TUI rendering) is also valuable because it locks in the `AgentEvent` contract between the agent loop and the TUI, which is the primary integration surface that users see.

Categories 2 and 4 are lower priority but still worth adding for completeness.

The dedicated `anie-integration-tests` crate approach is recommended over workspace-root `tests/` because the workspace uses `default-members` scoping.
