# Phase 1: Foundation (Weeks 1–2)

**Goal:** Stand up the workspace, define all shared types, implement the agent loop with a mock provider, and ship the three core bootstrapping tools (`read`, `write`, `bash`). By the end of Phase 1 you should be able to run a trivial integration test: prompt → mock assistant → tool call → tool result → assistant → stop, using owned agent context and structured provider errors from day one.

---

## Sub-phase 1.1: Workspace Scaffolding

**Duration:** Day 1

### Tasks

1. **Create the Cargo workspace.**

   ```
   anie-rs/
     Cargo.toml          # [workspace] with members list
     rust-toolchain.toml  # pin stable channel (e.g. 1.85)
     .cargo/config.toml   # optional target-dir, linker settings
     crates/
       anie-protocol/
       anie-provider/
       anie-providers-builtin/
       anie-agent/
       anie-tools/
       anie-session/
       anie-config/
       anie-auth/
       anie-tui/
       anie-cli/
       anie-extensions/
   ```

2. **Workspace-level dependency pinning.** Every third-party crate is declared once in `[workspace.dependencies]` with a version. Members use `foo = { workspace = true }`. Internal crates are listed the same way, keyed by path.

   ```toml
   [workspace.dependencies]
   # External
   tokio = { version = "1", features = ["full"] }
   serde = { version = "1", features = ["derive"] }
   serde_json = "1"
   anyhow = "1"
   thiserror = "2"
   tracing = "0.1"
   tracing-subscriber = { version = "0.3", features = ["env-filter"] }
   uuid = { version = "1", features = ["v4"] }
   async-trait = "0.1"
   tokio-util = "0.7"
   futures = "0.3"

   # Internal
   anie-protocol = { path = "crates/anie-protocol" }
   anie-provider = { path = "crates/anie-provider" }
   # ... etc
   ```

3. **Workspace-level lint configuration.** Set up clippy lints in `[workspace.lints.clippy]`. Take inspiration from Codex's strict configuration but start with a practical subset:

   ```toml
   [workspace.lints.clippy]
   unwrap_used = "warn"       # warn, not deny — we're bootstrapping
   expect_used = "warn"
   needless_borrow = "deny"
   redundant_clone = "deny"
   uninlined_format_args = "deny"
   manual_let_else = "deny"
   ```

4. **Create stub `lib.rs` for every crate.** Each crate should compile with `cargo check` even if it's empty. This validates the dependency graph from day one.

5. **Set up basic CI.** A Justfile or Makefile with:
   - `just check` — `cargo check --workspace`
   - `just test` — `cargo test --workspace`
   - `just clippy` — `cargo clippy --workspace -- -D warnings`
   - `just fmt` — `cargo fmt --all -- --check`

### Acceptance Criteria

- `cargo check --workspace` passes.
- `cargo test --workspace` passes (trivially — no tests yet).
- Dependency graph matches the architecture document (no cycles).

---

## Sub-phase 1.2: `anie-protocol` — Shared Types

**Duration:** Days 2–4

This is the leaf crate that every other crate depends on. It must be complete and correct before anything else can begin. No business logic lives here — only type definitions, serde implementations, and exhaustive tests.

### Types to Define

#### Messages (`messages.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role")]
pub enum Message {
    #[serde(rename = "user")]
    User(UserMessage),
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    #[serde(rename = "toolResult")]
    ToolResult(ToolResultMessage),
    #[serde(rename = "custom")]
    Custom(CustomMessage),
}
```

Implementation notes:
- Use `#[serde(tag = "role")]` for discriminated union serialization. This matches the JSONL session format and wire formats.
- `timestamp` fields should be `u64` (milliseconds since epoch), not `chrono::DateTime`. Chrono is a heavy dependency for what is just an integer. Use a helper: `fn now_millis() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 }`.
- `ContentBlock` uses `#[serde(tag = "type")]` for its discriminant. The `ToolCall` variant contains `id`, `name`, `arguments: serde_json::Value`.

**Critical detail — `arguments` field:**
The `ToolCall.arguments` field is `serde_json::Value`, not `String`. Some providers (Anthropic) send arguments as a JSON object; others (OpenAI) stream them as a string that must be parsed. The provider layer is responsible for ensuring `arguments` is always a parsed JSON value by the time it reaches the agent loop.

#### Content Blocks (`content.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { media_type: String, data: String },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    #[serde(rename = "toolCall")]
    ToolCall(ToolCall),
}
```

#### Events (`events.rs`)

```rust
#[derive(Debug, Clone)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd { messages: Vec<Message> },
    TurnStart,
    TurnEnd { assistant: AssistantMessage, tool_results: Vec<ToolResultMessage> },
    MessageStart { message: Message },
    MessageDelta { delta: StreamDelta },
    MessageEnd { message: Message },
    ToolExecStart { call_id: String, tool_name: String, args: serde_json::Value },
    ToolExecUpdate { call_id: String, partial: ToolResult },
    ToolExecEnd { call_id: String, result: ToolResult, is_error: bool },
}
```

`AgentEvent` does **not** need `Serialize/Deserialize` — it is only used in-process over mpsc channels. The RPC mode (Phase 5) will define its own serializable event types that map from `AgentEvent`.

#### Stream Deltas (`stream.rs`)

```rust
#[derive(Debug, Clone)]
pub enum StreamDelta {
    TextStart,
    TextDelta(String),
    TextEnd,
    ThinkingStart,
    ThinkingDelta(String),
    ThinkingEnd,
    ToolCallStart(ToolCall),
    ToolCallDelta { id: String, arguments_delta: String },
    ToolCallEnd { id: String },
}
```

#### Tool Schema (`tools.rs`)

```rust
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema object
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    pub details: serde_json::Value,
}
```

#### Usage and Cost (`usage.rs`)

All numeric fields `u64`, cost fields `f64`. Derive `Default` so the agent loop can accumulate costs across turns.

#### StopReason (`stop_reason.rs`)

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum StopReason {
    Stop,
    ToolUse,
    Error,
    Aborted,
}
```

### Tests

Write **exhaustive serde roundtrip tests** for every type. This is non-negotiable — serde regressions in the protocol crate will cascade everywhere.

```rust
#[test]
fn user_message_roundtrip() {
    let msg = Message::User(UserMessage {
        content: vec![ContentBlock::Text { text: "hello".into() }],
        timestamp: 1000,
    });
    let json = serde_json::to_string(&msg).unwrap();
    let deserialized: Message = serde_json::from_str(&json).unwrap();
    // assert fields match
}
```

Test each `ContentBlock` variant, each `Message` variant, `ToolCall` with nested arguments, and edge cases (empty content arrays, null arguments, Unicode text).

### Acceptance Criteria

- All types compile with correct serde derives.
- 20+ unit tests covering serialization roundtrips.
- `cargo doc --package anie-protocol --no-deps` produces clean documentation.

---

## Sub-phase 1.3: `anie-provider` — Trait Abstraction

**Duration:** Days 4–5

Define the provider trait and registry. No concrete implementations yet — those come in Phase 2.

### Types to Define

#### Model (`model.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub api: ApiKind,
    pub base_url: String,
    pub context_window: u64,
    pub max_tokens: u64,
    pub supports_reasoning: bool,
    pub supports_images: bool,
    pub cost_per_million: CostPerMillion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostPerMillion {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}
```

#### ApiKind (`api_kind.rs`)

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ApiKind {
    AnthropicMessages,
    OpenAICompletions,
    OpenAIResponses,
    GoogleGenerativeAI,
}
```

**Design decision:** Include `OpenAIResponses` even though v1 only implements `OpenAICompletions`. The enum is part of the public API and adding variants later is a breaking change. Better to reserve the slot now.

#### ThinkingLevel (`thinking.rs`)

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ThinkingLevel {
    Off,
    Low,
    Medium,
    High,
}
```

#### Provider Trait (`provider.rs`)

```rust
use futures::Stream;
use std::pin::Pin;

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderError>> + Send>>;

#[async_trait]
pub trait Provider: Send + Sync {
    fn stream(
        &self,
        model: &Model,
        context: LlmContext,
        options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError>;

    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage>;

    fn convert_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value>;
}
```

**Important:** `stream()` returns `Result<ProviderStream>`, not a bare stream, and `ProviderStream` itself yields `Result<ProviderEvent, ProviderError>`. Connection failures, auth errors, and mid-stream failures must stay structured so Phase 6 retry logic can switch on `ProviderError` instead of parsing strings.

#### ProviderError (`error.rs`)

```rust
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("HTTP error: {status} {body}")]
    Http { status: u16, body: String },
    #[error("Authentication failed: {0}")]
    Auth(String),
    #[error("Request building error: {0}")]
    Request(String),
    #[error("Stream error: {0}")]
    Stream(String),
    #[error("Rate limited (retry after {retry_after_ms:?}ms)")]
    RateLimited { retry_after_ms: Option<u64> },
    #[error("Context overflow: {0}")]
    ContextOverflow(String),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}
```

Categorize errors from the start. Phase 6 (hardening) will add retry logic that switches on these variants. Having them defined now prevents a painful refactor later.

#### ProviderRegistry (`registry.rs`)

```rust
pub struct ProviderRegistry {
    providers: HashMap<ApiKind, Box<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self { ... }
    pub fn register(&mut self, api: ApiKind, provider: Box<dyn Provider>) { ... }
    pub fn get(&self, api: &ApiKind) -> Option<&dyn Provider> { ... }

    pub fn stream(
        &self,
        model: &Model,
        context: LlmContext,
        options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> {
        let provider = self.get(&model.api)
            .ok_or_else(|| ProviderError::Request(
                format!("No provider registered for {:?}", model.api)
            ))?;
        provider.stream(model, context, options)
    }
}
```

#### LlmMessage and LlmContext

```rust
/// Provider-native message representation. Opaque to the agent loop.
#[derive(Debug, Clone)]
pub struct LlmMessage {
    pub role: String,
    pub content: serde_json::Value,
}

/// Full context for a streaming LLM request.
pub struct LlmContext {
    pub system_prompt: String,
    pub messages: Vec<LlmMessage>,
    pub tools: Vec<ToolDef>,
}

/// Options for a streaming request.
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    pub api_key: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u64>,
    pub thinking: ThinkingLevel,
    pub headers: HashMap<String, String>,
}

/// Request-specific auth and routing resolved just before a provider call.
#[derive(Debug, Clone, Default)]
pub struct ResolvedRequestOptions {
    pub api_key: Option<String>,
    pub headers: HashMap<String, String>,
    pub base_url_override: Option<String>,
}
```

This keeps local OpenAI-compatible servers (`ollama`, `lmstudio`, local `vllm`) first-class: they can run with `api_key: None`, while future OAuth-backed providers can override headers or `base_url` per request.

### Mock Provider (for testing)

Create `MockProvider` in the test module of `anie-provider`. This provider returns canned responses and is essential for testing the agent loop in Sub-phase 1.4.

```rust
#[cfg(test)]
pub mod mock {
    pub struct MockProvider {
        responses: Vec<AssistantMessage>,
    }

    impl MockProvider {
        pub fn new(responses: Vec<AssistantMessage>) -> Self { ... }
    }

    impl Provider for MockProvider {
        fn stream(&self, ...) -> Result<ProviderStream> {
            // Pop the next canned response, convert to a stream of Result<ProviderEvent, ProviderError>
        }
        // ...
    }
}
```

**Make the mock available to other crates** by putting it behind a `#[cfg(feature = "mock")]` feature flag rather than `#[cfg(test)]`. This way `anie-agent` tests can depend on `anie-provider = { workspace = true, features = ["mock"] }`.

### Acceptance Criteria

- `Provider` trait compiles.
- `ProviderRegistry` stores and retrieves providers by `ApiKind`.
- `MockProvider` can return a canned `AssistantMessage` as a stream.

---

## Sub-phase 1.4: `anie-agent` — The Agent Loop

**Duration:** Days 5–8

This is the most architecturally significant code in the project. The loop must be correct, well-tested, and resilient. Model it directly after pi's `agent-loop.ts`, adapted to async Rust with `tokio::sync::mpsc` for event emission.

### Core Structures

```rust
pub struct AgentLoop {
    provider_registry: Arc<ProviderRegistry>,
    tool_registry: Arc<ToolRegistry>,
    config: AgentLoopConfig,
}

pub struct AgentLoopConfig {
    pub model: Model,
    pub system_prompt: String,
    pub thinking: ThinkingLevel,
    pub tool_execution: ToolExecutionMode,
    pub request_options_resolver: Arc<dyn anie_provider::RequestOptionsResolver>,
    pub get_steering_messages: Option<Arc<dyn Fn() -> Vec<Message> + Send + Sync>>,
    pub get_follow_up_messages: Option<Arc<dyn Fn() -> Vec<Message> + Send + Sync>>,
}

pub struct AgentRunResult {
    pub generated_messages: Vec<Message>,
    pub final_context: Vec<Message>,
}

#[derive(Debug, Clone, Copy)]
pub enum ToolExecutionMode {
    Sequential,
    Parallel,
}
```

`get_steering_messages` and `get_follow_up_messages` can return empty vectors in Phase 1, but the hooks should exist from the start so the TUI and RPC modes do not need an architectural refactor later.

### The `run` Method

```rust
impl AgentLoop {
    pub async fn run(
        &self,
        prompts: Vec<Message>,
        context: Vec<Message>,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> AgentRunResult {
        // Implementation follows
    }
}
```

### Loop Pseudocode (Rust)

```rust
async fn run(&self, prompts, context, event_tx, cancel) -> AgentRunResult {
    let mut context = context;
    let mut generated_messages: Vec<Message> = Vec::new();
    context.extend(prompts.iter().cloned());

    let _ = event_tx.send(AgentEvent::AgentStart).await;
    let _ = event_tx.send(AgentEvent::TurnStart).await;

    // Emit MessageStart/End for each prompt. Prompts are caller-owned and are
    // not returned in generated_messages.
    for prompt in &prompts {
        let _ = event_tx.send(AgentEvent::MessageStart { message: prompt.clone() }).await;
        let _ = event_tx.send(AgentEvent::MessageEnd { message: prompt.clone() }).await;
    }

    loop {
        // 1. Resolve request-specific auth/headers/base_url
        let request = match self.config.request_options_resolver
            .resolve(&self.config.model, &context)
            .await {
            Ok(request) => request,
            Err(error) => {
                // Emit error and return
                // ...
                return AgentRunResult { generated_messages, final_context: context };
            }
        };

        // 2. Convert context to LLM format
        let provider = self.provider_registry.get(&self.config.model.api).unwrap();
        let llm_messages = provider.convert_messages(&context);
        let llm_tools = provider.convert_tools(&self.tool_registry.definitions());

        let llm_context = LlmContext {
            system_prompt: self.config.system_prompt.clone(),
            messages: llm_messages,
            tools: llm_tools,
        };

        let mut model = self.config.model.clone();
        if let Some(base_url) = request.base_url_override {
            model.base_url = base_url;
        }

        let options = StreamOptions {
            api_key: request.api_key,
            temperature: None,
            max_tokens: Some(model.max_tokens),
            thinking: self.config.thinking,
            headers: request.headers,
        };

        let stream = match provider.stream(&model, llm_context, options) {
            Ok(s) => s,
            Err(error) => {
                // Emit error and return
                // ...
                return AgentRunResult { generated_messages, final_context: context };
            }
        };

        // 3. Collect assistant message from stream
        let assistant_message = self.collect_stream(stream, &event_tx, &cancel).await;
        context.push(Message::Assistant(assistant_message.clone()));
        generated_messages.push(Message::Assistant(assistant_message.clone()));

        // 4. Check stop condition
        if assistant_message.stop_reason == StopReason::Error
            || assistant_message.stop_reason == StopReason::Aborted
        {
            let _ = event_tx.send(AgentEvent::TurnEnd { ... }).await;
            let _ = event_tx.send(AgentEvent::AgentEnd { messages: generated_messages.clone() }).await;
            return AgentRunResult { generated_messages, final_context: context };
        }

        // 5. Extract tool calls
        let tool_calls: Vec<&ToolCall> = assistant_message.content.iter()
            .filter_map(|c| match c {
                ContentBlock::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .collect();

        if tool_calls.is_empty() {
            // Give steering/follow-up hooks a chance before stopping
            if let Some(get_follow_up_messages) = &self.config.get_follow_up_messages {
                let follow_up = get_follow_up_messages();
                if !follow_up.is_empty() {
                    context.extend(follow_up);
                    let _ = event_tx.send(AgentEvent::TurnEnd { ... }).await;
                    let _ = event_tx.send(AgentEvent::TurnStart).await;
                    continue;
                }
            }

            let _ = event_tx.send(AgentEvent::TurnEnd { ... }).await;
            let _ = event_tx.send(AgentEvent::AgentEnd { messages: generated_messages.clone() }).await;
            return AgentRunResult { generated_messages, final_context: context };
        }

        // 6. Execute tool calls
        let tool_results = self.execute_tool_calls(
            &tool_calls, &context, &event_tx, &cancel
        ).await;

        for result in &tool_results {
            context.push(Message::ToolResult(result.clone()));
            generated_messages.push(Message::ToolResult(result.clone()));
        }

        if let Some(get_steering_messages) = &self.config.get_steering_messages {
            context.extend(get_steering_messages());
        }

        let _ = event_tx.send(AgentEvent::TurnEnd { ... }).await;
        let _ = event_tx.send(AgentEvent::TurnStart).await;
        // Loop back for next turn
    }
}
```

### Stream Collection

The `collect_stream` method pins the provider stream, iterates `ProviderEvent` values, emits `AgentEvent::MessageStart`, `AgentEvent::MessageDelta` (mapped from `ProviderEvent` to `StreamDelta`), and `AgentEvent::MessageEnd`.

**Critical detail — cancellation during streaming:**
Use `tokio::select!` to race the stream against the `CancellationToken`. If cancelled mid-stream:
1. Drop the stream (this closes the HTTP connection).
2. Construct a partial `AssistantMessage` with `stop_reason: Aborted`.
3. Emit `MessageEnd` with the partial message.
4. Return immediately.

```rust
async fn collect_stream(
    &self,
    stream: ProviderStream,
    event_tx: &mpsc::Sender<AgentEvent>,
    cancel: &CancellationToken,
) -> AssistantMessage {
    tokio::pin!(stream);
    let mut builder = AssistantMessageBuilder::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                return builder.finish_aborted();
            }
            event = stream.next() => {
                match event {
                    Some(Ok(ProviderEvent::Done(msg))) => {
                        let _ = event_tx.send(AgentEvent::MessageEnd { message: Message::Assistant(msg.clone()) }).await;
                        return msg;
                    }
                    Some(Ok(provider_event)) => {
                        builder.apply(&provider_event);
                        let delta = StreamDelta::from(provider_event);
                        let _ = event_tx.send(AgentEvent::MessageDelta { delta }).await;
                    }
                    Some(Err(error)) => {
                        return builder.finish_error(error.to_string());
                    }
                    None => {
                        return builder.finish_error("Stream ended unexpectedly");
                    }
                }
            }
        }
    }
}
```

### Tool Execution

Two modes: sequential and parallel. Both follow the same prepare → execute → finalize pattern from pi.

**Sequential:**
```rust
for tool_call in tool_calls {
    let result = self.execute_single_tool(tool_call, context, event_tx, cancel).await;
    results.push(result);
}
```

**Parallel:**
```rust
let futures: Vec<_> = tool_calls.iter()
    .map(|tc| self.execute_single_tool(tc, context, event_tx, cancel))
    .collect();
let results = futures::future::join_all(futures).await;
```

Each `execute_single_tool`:
1. Look up tool in `ToolRegistry`.
2. Validate arguments against `ToolDef.parameters` JSON Schema (use `jsonschema` crate).
3. Call `beforeToolCall` hooks (if any). If blocked, return error result.
4. Call `tool.execute(call_id, args, cancel, update_tx)`.
5. Call `afterToolCall` hooks (if any). May override result.
6. Emit `ToolExecEnd`.
7. Construct `ToolResultMessage`.

**Argument validation detail:** Use the `jsonschema` crate to validate `tool_call.arguments` against `tool_def.parameters`. If validation fails, skip execution and return the validation error as a `ToolResultMessage` with `is_error: true`. This prevents invalid arguments from reaching tool implementations.

### ToolRegistry

```rust
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self { ... }
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.definition().name.clone(), tool);
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }
    pub fn definitions(&self) -> Vec<ToolDef> {
        self.tools.values().map(|t| t.definition()).collect()
    }
}
```

### Tool Trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDef;

    async fn execute(
        &self,
        call_id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
        update_tx: Option<mpsc::Sender<ToolResult>>,
    ) -> Result<ToolResult, ToolError>;
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    ExecutionFailed(String),
    #[error("Tool execution aborted")]
    Aborted,
    #[error("Timeout after {0} seconds")]
    Timeout(u64),
}
```

### Hook Traits

Define the `BeforeToolCallHook` and `AfterToolCallHook` traits in `anie-agent`. They are called by the agent loop but implemented by `anie-extensions` (Phase 5).

```rust
#[async_trait]
pub trait BeforeToolCallHook: Send + Sync {
    async fn before_tool_call(
        &self,
        tool_call: &ToolCall,
        args: &serde_json::Value,
        context: &[Message],
    ) -> BeforeToolCallResult;
}

pub enum BeforeToolCallResult {
    Allow,
    Block { reason: String },
}

#[async_trait]
pub trait AfterToolCallHook: Send + Sync {
    async fn after_tool_call(
        &self,
        tool_call: &ToolCall,
        result: &ToolResult,
        is_error: bool,
    ) -> Option<ToolResultOverride>;
}
```

For Phase 1, the agent loop accepts `Option<Arc<dyn BeforeToolCallHook>>` and `Option<Arc<dyn AfterToolCallHook>>`. Both default to `None` (no hooks).

### Tests

This is the most important test suite in the project.

1. **Basic flow:** Prompt → assistant (no tool calls) → agent end.
2. **Single tool call:** Prompt → assistant with tool call → tool result → assistant → agent end.
3. **Multiple sequential tool calls:** Assistant calls tool A, then tool B in the same response.
4. **Parallel tool calls:** Two tool calls in one assistant message, executed in parallel.
5. **Cancellation during streaming:** Cancel token fires mid-stream. Assert `stop_reason == Aborted`.
6. **Cancellation during tool execution:** Cancel token fires during tool execution.
7. **Tool not found:** Assistant calls a tool that isn't registered. Assert error result.
8. **Tool argument validation failure:** Invalid arguments against JSON Schema. Assert error result.
9. **beforeToolCall blocks:** Hook returns `Block`. Assert error result, tool not executed.
10. **afterToolCall overrides:** Hook overrides tool result. Assert the overridden result is used.
11. **Multiple turns:** Assistant calls tool → result → assistant calls another tool → result → assistant stops.
12. **Provider stream error:** Provider stream yields `Err(ProviderError::Stream(...))`. Assert structured error handling.

### Acceptance Criteria

- `AgentLoop::run` completes all 12 test scenarios correctly.
- Events are emitted in the correct order for every scenario.
- Cancellation is responsive (no dangling tasks).

---

## Sub-phase 1.5: `anie-tools` — ReadTool, WriteTool, and BashTool

**Duration:** Days 8–10

Implement the three most useful bootstrapping tools first. `WriteTool` moves into Phase 1 because it is simple, unblocks real file-modifying workflows, and lets you validate end-to-end coding behavior before `EditTool` exists. `EditTool` remains deferred to Phase 5 because fuzzy matching, diff generation, BOM handling, and CRLF preservation make it substantially more complex.

### ReadTool (`read.rs`)

**Parameters (JSON Schema):**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path to the file to read" },
    "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed)" },
    "limit": { "type": "integer", "description": "Maximum number of lines to read" }
  },
  "required": ["path"],
  "additionalProperties": false
}
```

**Implementation:**

1. Resolve `path` relative to CWD (configurable, stored in the tool struct).
2. Detect image files by extension (`.png`, `.jpg`, `.jpeg`, `.gif`, `.webp`). For images:
   - Read the file as bytes.
   - Base64-encode.
   - Return `ContentBlock::Image { media_type, data }`.
3. For text files:
   - Read the file as UTF-8 (handle encoding errors with `from_utf8_lossy`).
   - Apply offset/limit if provided. Offset is 1-indexed.
   - Truncate at `MAX_LINES` (2000) or `MAX_BYTES` (50 KB), whichever is hit first.
   - If truncated, append `\n[remaining X lines not shown. Use offset to read more.]`.
   - Return `ContentBlock::Text { text }`.

**Edge cases to handle:**
- File not found → `ToolError::ExecutionFailed`.
- Binary file detection (non-UTF-8) → return a message saying the file appears to be binary.
- Symlinks → follow them (default `fs::read` behavior).
- Very large images → cap at some reasonable size (10 MB?). Return an error if the image is too large.

**Tests:**
- Read a small text file.
- Read with offset and limit.
- Truncation at line limit.
- Truncation at byte limit.
- Image file detection and base64 encoding.
- File not found error.

### WriteTool (`write.rs`)

Implement in Phase 1, not Phase 5.

**Parameters (JSON Schema):**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path to the file to write" },
    "content": { "type": "string", "description": "Content to write to the file" }
  },
  "required": ["path", "content"],
  "additionalProperties": false
}
```

**Implementation:**
1. Resolve `path` relative to CWD.
2. Acquire the per-path mutation lock from `FileMutationQueue`.
3. Create parent directories automatically.
4. Overwrite the file atomically if practical (`write` to temp + rename is a good follow-up improvement; plain overwrite is acceptable for Phase 1).
5. Return structured details (`path`, `lines`, `bytes`) so later `/diff` and session tooling do not need to parse human-readable text.

**Tests:**
- Write a new file.
- Overwrite an existing file.
- Auto-create parent directories.
- Cancellation before the write occurs.

### BashTool (`bash.rs`)

**Parameters (JSON Schema):**
```json
{
  "type": "object",
  "properties": {
    "command": { "type": "string", "description": "Bash command to execute" },
    "timeout": { "type": "number", "description": "Timeout in seconds (optional)" }
  },
  "required": ["command"],
  "additionalProperties": false
}
```

**Implementation:**

1. Determine the shell: `$SHELL` env var, falling back to `/bin/bash` on Unix and `cmd.exe` on Windows.
2. Spawn the command with `tokio::process::Command`:
   ```rust
   let mut child = Command::new(&shell)
       .args(&["-c", &command])  // Unix
       .current_dir(&cwd)
       .stdout(Stdio::piped())
       .stderr(Stdio::piped())
       .process_group(0)  // New process group for clean kill
       .spawn()?;
   ```
3. **Stream output** by merging stdout and stderr into a single byte buffer. Use `tokio::io::AsyncReadExt` to read chunks.
4. **Write to temp file** if output exceeds `MAX_BYTES` (50 KB). Use `tokio::fs::File` for async writes.
5. **Send partial updates** via `update_tx` if provided.
6. **Truncation:** Keep a rolling buffer of the last `MAX_BYTES` (tail truncation). Apply line-level truncation at `MAX_LINES` (2000).
7. **Timeout handling:**
   - Use `tokio::time::timeout` wrapping the process wait.
   - On timeout, kill the entire process group: `kill(-pgid, SIGKILL)`.
   - Wait 2 seconds for pipe draining (Codex's `IO_DRAIN_TIMEOUT_MS` pattern).
   - Return timeout error with whatever output was captured.
8. **Cancellation:**
   - Use `tokio::select!` to race the process against `cancel.cancelled()`.
   - On cancel, kill the process group, drain, return `ToolError::Aborted`.
9. **Exit code:**
   - Non-zero exit → `ToolError::ExecutionFailed` with output + exit code.
   - Zero exit → success.

**Process group management (Unix):**
```rust
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

fn kill_process_tree(pid: u32) {
    let pgid = Pid::from_raw(-(pid as i32));
    let _ = kill(pgid, Signal::SIGKILL);
}
```

On Windows, use `taskkill /F /T /PID`.

**Tests:**
- Simple command (`echo hello`).
- Multi-line output.
- Exit code propagation.
- Timeout enforcement.
- Large output truncation.
- Stderr capture.
- Cancellation via `CancellationToken`.

### FileMutationQueue (`file_mutation_queue.rs`)

Even though `WriteTool` and `EditTool` aren't implemented yet, define the queue now. It will be needed in Phase 5.

```rust
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct FileMutationQueue {
    locks: DashMap<PathBuf, Arc<Mutex<()>>>,
}

impl FileMutationQueue {
    pub fn new() -> Self {
        Self { locks: DashMap::new() }
    }

    pub async fn with_lock<F, T>(&self, path: &Path, f: F) -> T
    where
        F: Future<Output = T>,
    {
        let canonical = std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf());
        let lock = self.locks.entry(canonical)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;
        f.await
    }
}
```

### Acceptance Criteria

- `ReadTool` passes all tests including image detection and truncation.
- `WriteTool` passes all tests including overwrite, mkdirs, and cancellation.
- `BashTool` passes all tests including timeout, cancellation, and truncation.
- All three tools integrate cleanly with the `ToolRegistry`.
- End-to-end test: `AgentLoop` + `MockProvider` + `ReadTool` + `WriteTool` + `BashTool` → prompt triggers a tool call, tool executes, result flows back.

---

## Phase 1 Milestones Checklist

| # | Milestone | Verified By |
|---|---|---|
| 1 | Workspace compiles with `cargo check --workspace` | CI |
| 2 | `anie-protocol` types serialize/deserialize correctly | 20+ unit tests |
| 3 | `anie-provider` trait compiles with `MockProvider` | Compile + mock tests |
| 4 | `anie-agent` loop handles prompt → tool → response cycle | 12 integration tests |
| 5 | `ReadTool` reads files with truncation | 6+ unit tests |
| 6 | `WriteTool` writes files safely with mkdirs + locking | 4+ unit tests |
| 7 | `BashTool` executes commands with timeout/cancel | 7+ unit tests |
| 8 | End-to-end: MockProvider + tools in AgentLoop | 1 integration test |
