# Phase 0 — Test Crate and Shared Infrastructure

## Why this phase exists

Integration tests need a place to live and a set of reusable helpers. This phase creates the `anie-integration-tests` crate, wires it into the workspace, and builds the shared test infrastructure that all subsequent phases depend on.

No actual test cases are written in this phase. The exit criteria is that the crate compiles, runs an empty test suite, and the helper modules are available.

---

## Files to create

### `crates/anie-integration-tests/Cargo.toml`

```toml
[package]
name = "anie-integration-tests"
version.workspace = true
edition.workspace = true
publish = false

# This crate exists only for integration tests.
# It produces no library or binary.

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

### `crates/anie-integration-tests/src/lib.rs`

```rust
//! Integration test helpers for anie.
//!
//! This crate contains no production code. It exists to provide shared
//! infrastructure for the integration test files in `tests/`.

pub mod helpers;
```

### `crates/anie-integration-tests/src/helpers.rs`

This module provides the reusable building blocks for all integration tests.

---

## Shared helpers to implement

### 1. `sample_model() -> Model`

Returns a minimal `Model` pointing at `ApiKind::OpenAICompletions` with mock provider settings. Reused by every test that creates an `AgentLoop`.

```rust
pub fn sample_model() -> Model {
    Model {
        id: "mock-model".into(),
        name: "Mock Model".into(),
        provider: "mock".into(),
        api: ApiKind::OpenAICompletions,
        base_url: "http://localhost".into(),
        context_window: 128_000,
        max_tokens: 8_192,
        supports_reasoning: false,
        reasoning_capabilities: None,
        supports_images: false,
        cost_per_million: CostPerMillion::zero(),
    }
}
```

### 2. `static_resolver() -> Arc<dyn RequestOptionsResolver>`

Returns a resolver that always succeeds with default (no-auth) options.

```rust
pub fn static_resolver() -> Arc<dyn RequestOptionsResolver> {
    Arc::new(StaticResolver)
}

struct StaticResolver;

#[async_trait]
impl RequestOptionsResolver for StaticResolver {
    async fn resolve(
        &self,
        _model: &Model,
        _context: &[Message],
    ) -> Result<ResolvedRequestOptions, ProviderError> {
        Ok(ResolvedRequestOptions::default())
    }
}
```

### 3. `build_agent(provider, tool_registry) -> AgentLoop`

Creates an `AgentLoop` with the given provider and tool registry, using `sample_model()` and `static_resolver()`. Provides sensible defaults for all other config fields.

```rust
pub fn build_agent(
    provider: Box<dyn Provider>,
    tool_registry: Arc<ToolRegistry>,
) -> AgentLoop {
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(ApiKind::OpenAICompletions, provider);

    AgentLoop::new(
        Arc::new(provider_registry),
        tool_registry,
        AgentLoopConfig {
            model: sample_model(),
            system_prompt: "You are a test agent.".into(),
            thinking: ThinkingLevel::Off,
            tool_execution: ToolExecutionMode::Parallel,
            request_options_resolver: static_resolver(),
            get_steering_messages: None,
            get_follow_up_messages: None,
            before_tool_call_hook: None,
            after_tool_call_hook: None,
        },
    )
}
```

### 4. `run_agent_collecting_events(...) -> (AgentRunResult, Vec<AgentEvent>)`

Runs the agent loop to completion and collects all emitted events.

```rust
pub async fn run_agent_collecting_events(
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
        if is_end {
            break;
        }
    }

    (handle.await.expect("agent task"), events)
}
```

### 5. `create_temp_session() -> (TempDir, SessionManager)`

Creates a fresh session backed by a temp directory.

```rust
pub fn create_temp_session() -> (tempfile::TempDir, SessionManager) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();
    let session = SessionManager::new_session(dir.path(), &cwd)
        .expect("new session");
    (dir, session)
}
```

### 6. `real_tool_registry(cwd) -> Arc<ToolRegistry>`

Creates a tool registry containing the four real tools, all rooted at the given working directory. Edit and write tools share a `FileMutationQueue`.

```rust
pub fn real_tool_registry(cwd: &Path) -> Arc<ToolRegistry> {
    let queue = Arc::new(FileMutationQueue::new());
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ReadTool::new(cwd)));
    registry.register(Arc::new(WriteTool::with_queue(cwd, Arc::clone(&queue))));
    registry.register(Arc::new(EditTool::with_queue(cwd, Arc::clone(&queue))));
    registry.register(Arc::new(BashTool::new(cwd)));
    Arc::new(registry)
}
```

### 7. Message construction helpers

```rust
pub fn user_prompt(text: &str) -> Message {
    Message::User(UserMessage {
        content: vec![ContentBlock::Text { text: text.into() }],
        timestamp: 1,
    })
}

pub fn final_assistant(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text { text: text.into() }],
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        provider: "mock".into(),
        model: "mock-model".into(),
        timestamp: 1,
    }
}

pub fn assistant_with_tool_calls(tool_calls: Vec<ToolCall>) -> AssistantMessage {
    let mut content = Vec::new();
    content.extend(tool_calls.into_iter().map(ContentBlock::ToolCall));
    AssistantMessage {
        content,
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        provider: "mock".into(),
        model: "mock-model".into(),
        timestamp: 1,
    }
}

pub fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}
```

### 8. `event_kinds(events) -> Vec<&str>`

Extracts event type names for sequence assertions.

```rust
pub fn event_kinds(events: &[AgentEvent]) -> Vec<&'static str> {
    events.iter().map(|event| match event {
        AgentEvent::AgentStart => "AgentStart",
        AgentEvent::AgentEnd { .. } => "AgentEnd",
        AgentEvent::TurnStart => "TurnStart",
        AgentEvent::TurnEnd { .. } => "TurnEnd",
        AgentEvent::MessageStart { .. } => "MessageStart",
        AgentEvent::MessageDelta { .. } => "MessageDelta",
        AgentEvent::MessageEnd { .. } => "MessageEnd",
        AgentEvent::ToolExecStart { .. } => "ToolExecStart",
        AgentEvent::ToolExecUpdate { .. } => "ToolExecUpdate",
        AgentEvent::ToolExecEnd { .. } => "ToolExecEnd",
        AgentEvent::TranscriptReplace { .. } => "TranscriptReplace",
        AgentEvent::SystemMessage { .. } => "SystemMessage",
        AgentEvent::StatusUpdate { .. } => "StatusUpdate",
        AgentEvent::CompactionStart => "CompactionStart",
        AgentEvent::CompactionEnd { .. } => "CompactionEnd",
        AgentEvent::RetryScheduled { .. } => "RetryScheduled",
    }).collect()
}
```

---

## Workspace change

Add `anie-integration-tests` to the workspace members list in the root `Cargo.toml`:

```toml
members = [
    # ... existing members ...
    "crates/anie-integration-tests",
]
```

Do **not** add it to `default-members`. It should only run with `cargo test --workspace` or `cargo test -p anie-integration-tests`.

---

## Exit criteria

- [ ] `crates/anie-integration-tests/` exists with `Cargo.toml`, `src/lib.rs`, and `src/helpers.rs`
- [ ] the crate is listed in workspace `members` but not in `default-members`
- [ ] `cargo check -p anie-integration-tests` passes
- [ ] `cargo test -p anie-integration-tests` passes (0 tests, but compiles)
- [ ] all helper functions compile and are importable from test files
- [ ] `cargo test --workspace` still passes with the new crate included
