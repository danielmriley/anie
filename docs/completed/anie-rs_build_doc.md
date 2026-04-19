# anie-rs — Build Document

A Rust-based coding agent harness inspired by [pi](https://github.com/badlogic/pi-mono) and informed by [OpenAI Codex CLI](https://github.com/openai/codex). This document describes the architecture, crate layout, data flow, and implementation plan for building anie-rs from scratch.

**Companion docs:**
- `docs/v1_0_milestone_checklist.md` — distilled release blockers and sign-off checklist
- `docs/IMPLEMENTATION_ORDER.md` — concrete execution sequence for building the system
- `docs/notes.md` — planning issue tracker (resolved / open / deferred)

> **Historical note (2026-04-18).** Early versions of this build doc
> described an in-process extension crate. That crate was removed.
> Current state: there is no extension crate in the workspace; see
> `docs/arch/anie-rs_architecture.md` for the live architecture and
> `docs/refactor_plans/10_extension_system_pi_port.md` for the future
> out-of-process extension design.

---

## Design goals

1. **Four core tools only** — `read`, `write`, `edit`, `bash`. Same as pi. No bloat.
2. **Multi-provider LLM support** — OpenAI-compatible endpoints first (including Ollama and LM Studio), plus Anthropic. Google is optional for v1.0. OAuth flows, including GitHub Copilot, are post-v1.0.
3. **Simple TUI** — input frame at the bottom, output frame above. No overlays, no floating panels for v1.
4. **Extensibility** — trait-based tool and provider registries. New tools, providers, and hooks are addable without touching core code.
5. **Separation of concerns** — each crate has a single responsibility. The agent loop knows nothing about files; the TUI knows nothing about LLMs.
6. **Rust-native** — single static binary. No runtime dependencies. Cross-platform (Linux, macOS, Windows).

---

## Crate layout

```
anie-rs/
  Cargo.toml                  Workspace root
  crates/
    anie-protocol/            Wire types: messages, events, tools, config
    anie-provider/            LLM provider abstraction and registry
    anie-providers-builtin/   Built-in provider implementations (Anthropic, OpenAI, Google)
    anie-agent/               Generic agent loop (provider-agnostic, tool-agnostic)
    anie-tools/               Core tool implementations (read, write, edit, bash)
    anie-session/             Session persistence (append-only JSONL tree)
    anie-config/              TOML configuration loading and merging
    anie-auth/                API key and OAuth credential storage
    anie-tui/                 Terminal UI (ratatui-based)
    anie-cli/                 CLI entry point, arg parsing, run modes
```

### Dependency graph (simplified)

```
anie-cli
  ├── anie-tui
  │     └── anie-protocol
  ├── anie-agent
  │     ├── anie-provider
  │     └── anie-protocol
  ├── anie-tools
  │     ├── anie-agent
  │     └── anie-protocol
  ├── anie-providers-builtin
  │     ├── anie-provider
  │     └── anie-protocol
  ├── anie-session
  │     └── anie-protocol
  ├── anie-config
  │     └── anie-provider
  ├── anie-auth
  │     ├── anie-config
  │     ├── anie-provider
  │     └── anie-protocol
```

Each arrow means "depends on." Cycles are impossible by design.

---

## Crate: `anie-protocol`

Shared types used across all crates. No business logic.

### Messages

```rust
/// Role of a message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    /// Extension-defined messages that the agent loop preserves but does not
    /// interpret. Each variant carries an opaque `serde_json::Value` payload.
    Custom(CustomMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    pub content: Vec<ContentBlock>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
    pub provider: String,
    pub model: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomMessage {
    pub custom_type: String,
    pub content: serde_json::Value,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    Text { text: String },
    Image { media_type: String, data: String },
    Thinking { thinking: String },
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}
```

### Events

Modeled after Codex's SQ/EQ pattern and pi's `AgentEvent`:

```rust
#[derive(Debug, Clone)]
pub enum AgentEvent {
    // Lifecycle
    AgentStart,
    AgentEnd { messages: Vec<Message> },

    // Turn lifecycle
    TurnStart,
    TurnEnd { assistant: AssistantMessage, tool_results: Vec<ToolResultMessage> },

    // Streaming
    MessageStart { message: Message },
    MessageDelta { delta: StreamDelta },
    MessageEnd { message: Message },

    // Tool execution
    ToolExecStart { call_id: String, tool_name: String, args: serde_json::Value },
    ToolExecUpdate { call_id: String, partial: ToolResult },
    ToolExecEnd { call_id: String, result: ToolResult, is_error: bool },
}

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

### Tool schema

```rust
/// Tool definition registered with the agent.
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
}

/// Result returned by a tool execution.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    pub details: serde_json::Value,
}
```

### Usage and cost

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: Option<u64>,
    pub cost: Cost,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum StopReason {
    Stop,
    ToolUse,
    Error,
    Aborted,
}
```

---

## Crate: `anie-provider`

Trait abstraction for LLM providers. No concrete implementations live here.

```rust
/// A registered model with all metadata needed to make API calls.
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ApiKind {
    AnthropicMessages,
    OpenAICompletions,
    OpenAIResponses,
    GoogleGenerativeAI,
}

/// Reasoning / thinking level.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ThinkingLevel {
    Off,
    Low,
    Medium,
    High,
}

/// Context sent to the LLM for a single streaming request.
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

/// Streaming event from a provider.
pub enum ProviderEvent {
    Start,
    TextDelta(String),
    ThinkingDelta(String),
    ToolCallStart(ToolCall),
    ToolCallDelta { id: String, arguments_delta: String },
    ToolCallEnd { id: String },
    Done(AssistantMessage),
}

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderError>> + Send>>;

/// Trait that all LLM providers implement.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stream a completion from the model.
    fn stream(
        &self,
        model: &Model,
        context: LlmContext,
        options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError>;

    /// Convert generic messages to the provider's native format.
    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage>;

    /// Convert generic tool definitions to the provider's native format.
    fn convert_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value>;
}

/// Registry of providers keyed by ApiKind.
pub struct ProviderRegistry {
    providers: HashMap<ApiKind, Box<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn register(&mut self, api: ApiKind, provider: Box<dyn Provider>) { ... }
    pub fn get(&self, api: ApiKind) -> Option<&dyn Provider> { ... }

    /// Stream using the model's registered provider.
    pub fn stream(
        &self,
        model: &Model,
        context: LlmContext,
        options: StreamOptions,
    ) -> Result<ProviderStream> { ... }
}
```

### `LlmMessage`

The provider-specific message format. Each provider implementation converts from `Message` (protocol) to `LlmMessage` (native wire format) inside its `convert_messages` method. This keeps the agent loop and session manager working purely with `Message` while letting providers emit whatever JSON their API requires.

```rust
/// Provider-native message representation. Opaque to the agent loop.
pub struct LlmMessage {
    pub role: String,
    pub content: serde_json::Value,
}
```

---

## Crate: `anie-providers-builtin`

Concrete provider implementations. Each is a struct implementing `Provider`.

### Anthropic (`anthropic.rs`)

- Wire format: `POST /v1/messages` with `stream: true`.
- SSE parsing with `reqwest` + `eventsource-stream`.
- Maps `ThinkingLevel` to Anthropic's `thinking.budget_tokens`.
- Cache control: `cache_control: { type: "ephemeral" }` on system and tool definitions for prompt caching.
- Tool calls: `tool_use` content blocks → `ToolCall`, `tool_result` role for results.

### OpenAI (`openai.rs`)

- Wire format: `POST /v1/chat/completions` with `stream: true` (Completions API).
- Maps `ThinkingLevel` to `reasoning_effort` for o-series models.
- Tool calls: `tool_calls` array in assistant delta → `ToolCall`.
- Compatible with any OpenAI-compatible endpoint (Together, Groq, local vLLM, etc.) by changing `base_url`.

### Google (`google.rs`)

- Wire format: `POST /v1beta/models/{model}:streamGenerateContent`.
- Maps `ThinkingLevel` to `thinkingConfig.thinkingBudget`.
- Tool calls: `functionCall` parts → `ToolCall`, `functionResponse` for results.

### Registration

```rust
pub fn register_builtin_providers(registry: &mut ProviderRegistry) {
    registry.register(ApiKind::AnthropicMessages, Box::new(AnthropicProvider::new()));
    registry.register(ApiKind::OpenAICompletions, Box::new(OpenAIProvider::new()));
    registry.register(ApiKind::GoogleGenerativeAI, Box::new(GoogleProvider::new()));
}
```

---

## Crate: `anie-agent`

The generic agent loop. Knows about `Message`, `ToolDef`, `ToolResult`, and `ProviderEvent`. Knows nothing about files, shells, or sessions.

### Loop implementation

Modeled directly after pi's `agent-loop.ts` (the cleanest reference), adapted to async Rust:

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

impl AgentLoop {
    /// Run the agent loop for a single prompt using owned context.
    pub async fn run(
        &self,
        prompts: Vec<Message>,
        context: Vec<Message>,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> AgentRunResult { ... }
}
```

**Loop pseudocode:**

```
emit AgentStart
emit TurnStart
emit MessageStart for each prompt

loop {
    convert context to LlmMessage via provider.convert_messages()
    stream response via provider.stream()
    collect AssistantMessage from stream, emitting deltas

    if stop_reason == Error or Aborted:
        emit TurnEnd, AgentEnd
        return

    extract tool_calls from assistant content
    if no tool_calls:
        emit TurnEnd, AgentEnd
        return

    for each tool_call (parallel or sequential):
        validate arguments against tool schema
        run beforeToolCall hook (can block)
        execute tool
        run afterToolCall hook (can override result)
        emit ToolExecEnd
        push ToolResultMessage to context

    emit TurnEnd
    emit TurnStart  // next turn
}
```

### Tool registry

```rust
/// Trait that all tools implement.
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

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: Arc<dyn Tool>) { ... }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> { ... }
    pub fn definitions(&self) -> Vec<ToolDef> { ... }
}
```

### Hooks

Two traits for intercepting tool execution, following pi's `beforeToolCall` / `afterToolCall` pattern:

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

---

## Crate: `anie-tools`

The four core tools. Each is a struct implementing `Tool`.

### `ReadTool`

- Parameters: `path` (required), `offset` (optional, 1-indexed line), `limit` (optional).
- Reads file contents. Truncates at 2000 lines or 50 KB (whichever comes first).
- Detects image files (PNG, JPEG, GIF, WebP) and returns `ContentBlock::Image`.
- Returns `[remaining lines]` note when truncated.

### `WriteTool`

- Parameters: `path` (required), `content` (required).
- Creates parent directories automatically.
- Overwrites if the file exists.

### `EditTool`

- Parameters: `path` (required), `edits` (array of `{ old_text, new_text }`).
- Each edit is matched against the **original file content** (not incrementally).
- Edits must be unique and non-overlapping.
- On success, returns a unified diff of the changes.
- Fuzzy matching fallback: if exact match fails, try normalizing whitespace, smart quotes, and Unicode dashes (pi's `normalizeForFuzzyMatch` approach).

**Implementation detail:** Use the `similar` crate for diff generation, matching Codex's approach in `apply-patch`.

### `BashTool`

- Parameters: `command` (required), `timeout` (optional seconds).
- Spawns the user's shell (`$SHELL` or `/bin/bash`).
- Streams output to a temp file for large outputs.
- Truncates at 2000 lines or 50 KB (tail, keeping the end).
- Kills the process tree on timeout or abort.
- Returns exit code, stdout+stderr combined.

**Implementation detail:** Use `tokio::process::Command` with process group management. On Unix, spawn with `setsid` and kill the entire group on abort. Mirror Codex's `IO_DRAIN_TIMEOUT_MS` (2 seconds) for draining pipes after kill.

### File mutation queue

A per-path `tokio::sync::Mutex` to serialize concurrent `edit` and `write` calls to the same file. Prevents data races during parallel tool execution.

```rust
pub struct FileMutationQueue {
    locks: DashMap<PathBuf, Arc<Mutex<()>>>,
}
```

---

## Crate: `anie-session`

Append-only JSONL session persistence with tree structure. Modeled after pi's `SessionManager`.

### File format

Each session is a single `.jsonl` file in `~/.anie/sessions/`. One JSON object per line:

```jsonl
{"type":"session","version":1,"id":"abc123","timestamp":"2026-04-13T10:00:00Z","cwd":"/home/user/project"}
{"type":"message","id":"uuid1","parent_id":null,"timestamp":"...","message":{...}}
{"type":"message","id":"uuid2","parent_id":"uuid1","timestamp":"...","message":{...}}
{"type":"compaction","id":"uuid3","parent_id":"uuid2","summary":"...","tokens_before":45000,"first_kept_entry_id":"uuid2"}
{"type":"model_change","id":"uuid4","parent_id":"uuid3","model":"claude-sonnet-4-6","provider":"anthropic"}
```

### Entry types

| Type | Fields | Description |
|---|---|---|
| `session` | version, id, cwd, timestamp | File header |
| `message` | id, parent_id, message | Any `Message` variant |
| `compaction` | id, parent_id, summary, tokens_before, first_kept_entry_id | Compaction checkpoint |
| `model_change` | id, parent_id, model, provider | Model was switched |
| `thinking_change` | id, parent_id, level | Thinking level was changed |
| `label` | id, parent_id, label | Named bookmark |

### Tree operations

Every entry has `id` (UUID) and `parent_id` (UUID or null for root). The session is a tree, not a list. Forking creates a new branch from any entry. `get_branch(leaf_id)` walks `parent_id` pointers to reconstruct the path from root to leaf.

### Context compaction

Follows pi's algorithm:

1. Estimate tokens: `chars / 4` for text, 1200 for images.
2. Walk backwards from the newest entry accumulating token estimates.
3. Cut at `keep_recent_tokens` (default 20,000).
4. Summarize the discarded portion using the current model (structured prompt: Goal, Progress, Decisions, Next Steps).
5. If a previous summary exists, merge rather than replace.
6. Track files read and modified across compaction boundaries.
7. Replace discarded entries with a single `compaction` entry.
8. Trigger: `context_tokens > context_window - reserve_tokens` (default reserve: 16,384).

---

## Crate: `anie-config`

TOML-based configuration. File at `~/.anie/config.toml`.

```toml
# Default model
[model]
provider = "anthropic"
id = "claude-sonnet-4-6"
thinking = "medium"

# Provider credentials (prefer auth.json or env vars)
[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"

[providers.openai]
api_key_env = "OPENAI_API_KEY"

# Custom OpenAI-compatible provider
[providers.local-llm]
base_url = "http://localhost:8080/v1"
api = "openai-completions"
api_key_env = "LOCAL_LLM_KEY"

[[providers.local-llm.models]]
id = "qwen-72b"
name = "Qwen 72B"
context_window = 32768
max_tokens = 8192

# Compaction settings
[compaction]
enabled = true
reserve_tokens = 16384
keep_recent_tokens = 20000

# Project context
[context]
# Files walked from CWD upward, merged into system prompt
filenames = ["AGENTS.md", "CLAUDE.md"]
max_file_bytes = 32768
max_total_bytes = 65536
```

### Layer merging

Three layers, later overrides earlier:
1. **Global** — `~/.anie/config.toml`
2. **Project** — `.anie/config.toml` (in project root or CWD)
3. **CLI flags** — `--model`, `--provider`, `--api-key`, etc.

Use the `toml` crate for parsing, `serde` for deserialization into typed structs, and a manual merge function (field-by-field, later non-None wins).

---

## Crate: `anie-auth`

Credential storage and OAuth flows. File at `~/.anie/auth.json` (mode `0600`).

### Storage format

```json
{
  "anthropic": { "type": "api_key", "key": "sk-ant-..." },
  "openai": { "type": "oauth", "access": "...", "refresh": "...", "expires": 1744000000000 }
}
```

### Request resolution priority

The runtime resolves a `ResolvedRequestOptions` struct per request, not just an API-key string:
1. CLI `--api-key` flag (runtime override, not persisted)
2. `auth.json` API key entry
3. provider-specific environment variable (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.)
4. provider-specific extra headers / `base_url` overrides (for post-v1.0 OAuth-backed providers)

This keeps local OpenAI-compatible models first-class (`api_key: None`) while leaving room for future OAuth or proxy-backed providers without changing the agent loop.

### OAuth flows (post-v1.0)

Implement Anthropic / OpenAI / GitHub Copilot OAuth flows after v1.0 using the same patterns as pi:
- Local HTTP callback server (Authorization code + PKCE) or device flow.
- Manual paste fallback for remote/headless environments.
- Token refresh with file locking (`fd-lock` or `advisory-lock` crate) to prevent races between concurrent instances.

For v1.0, API keys and local models are sufficient.

---

## Crate: `anie-tui`

Terminal UI built on **ratatui** + **crossterm**. Two-pane layout: scrollable output above, input editor below.

### Layout

```
┌──────────────────────────────────────────────────────────┐
│  Output Frame (scrollable)                                │
│                                                           │
│  > You: Fix the bug in main.rs                            │
│                                                           │
│  Assistant: I'll read the file first.                     │
│                                                           │
│  ┌─ read ──────────────────────────────────────────────┐  │
│  │ path: src/main.rs                                   │  │
│  │ [42 lines, 1.2 KB]                                  │  │
│  └─────────────────────────────────────────────────────┘  │
│                                                           │
│  Assistant: Found the issue. Applying fix...              │
│                                                           │
│  ┌─ edit ──────────────────────────────────────────────┐  │
│  │ path: src/main.rs                                   │  │
│  │ -    let x = foo();                                 │  │
│  │ +    let x = foo().unwrap_or_default();             │  │
│  └─────────────────────────────────────────────────────┘  │
│                                                           │
├──────────────────────────────────────────────────────────┤
│  model: claude-sonnet-4-6 | thinking: medium | 12.4k/200k│
├──────────────────────────────────────────────────────────┤
│  > Type your message...                                   │
│                                                           │
└──────────────────────────────────────────────────────────┘
```

### Widgets

| Widget | Description |
|---|---|
| `OutputPane` | Scrollable viewport rendering the conversation transcript. Each message is rendered as a styled block. Tool calls are rendered with name, arguments, and result. |
| `StatusBar` | Single line: model name, thinking level, context token usage (`used/window`), CWD. |
| `InputPane` | Multi-line text editor for composing messages. Renders below the status bar. |
| `Spinner` | Inline animated indicator shown while the agent is streaming or executing tools. |

### Keybindings (input frame)

| Key | Action |
|---|---|
| `Enter` | Submit message |
| `Shift+Enter` or `Alt+Enter` | Insert newline |
| `Ctrl+C` | Interrupt current agent run (first press), quit (second press while idle) |
| `Ctrl+D` | Quit |
| `Up` / `Down` | Recall previous/next message from history |
| `Ctrl+A` | Move cursor to start of line |
| `Ctrl+E` | Move cursor to end of line |
| `Ctrl+W` | Delete word backward |
| `Ctrl+K` | Delete to end of line |
| `Ctrl+U` | Delete entire line |
| `Alt+Left` / `Alt+Right` | Move cursor by word |
| `Home` / `End` | Move to start/end of input |

### Keybindings (global)

| Key | Action |
|---|---|
| `Ctrl+L` | Clear screen and redraw |
| `Page Up` / `Page Down` | Scroll output pane |
| `Ctrl+O` | Open model selector (inline list, not overlay) |

### Rendering approach

Use ratatui's immediate-mode rendering with `Frame::render_widget`. The output pane is a custom widget that maintains a `Vec<RenderedBlock>` where each block is a message, tool call, or tool result. On each frame:

1. Compute visible blocks based on scroll offset and terminal height.
2. Render only visible blocks.
3. The input pane renders at a fixed position at the bottom.

Streaming deltas append to the last `RenderedBlock` in-place. Completed messages replace the streaming block with a finalized version.

### Alternate screen

Enter alternate screen on startup, leave on exit. This keeps the user's terminal history clean.

---

## Extensions (deferred)

There is no extension crate in the current workspace. The supported
future design is the out-of-process JSON-RPC system in
`docs/refactor_plans/10_extension_system_pi_port.md`.

---

## System prompt construction

Follows pi's layered approach:

```
1. Base role and identity
   "You are an expert coding assistant. You help users by reading files,
    executing commands, editing code, and writing new files."

2. Available tools (auto-generated from ToolRegistry)
   "Available tools:
    - read: Read file contents
    - write: Create or overwrite files
    - edit: Make precise text replacements
    - bash: Execute shell commands"

3. Tool guidelines
   "Guidelines:
    - Use bash for file operations like ls, grep, find
    - Use read to examine files (use offset + limit for large files)
    - Use edit for precise changes
    - Use write only for new files or complete rewrites
    - Be concise in your responses"

4. Project context (AGENTS.md / CLAUDE.md walked from CWD upward, capped)
   "# Project Context\n\n## /path/to/AGENTS.md\n\n<content up to max_file_bytes>"
   Total injected project-context bytes are capped by `context.max_total_bytes`.

5. Date and working directory
   "Current date: 2026-04-13"
   "Current working directory: /home/user/project"
```

`SYSTEM.md` in `.anie/` or `~/.anie/` replaces the base (items 1-3). `APPEND_SYSTEM.md` appends to the base.

---

## Onboarding workflow

### First run

1. `anie` is invoked with no arguments.
2. Check for `~/.anie/config.toml`. If absent, create it with commented defaults.
3. Check for any configured API key:
   - Scan environment variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`).
   - Check `~/.anie/auth.json`.
4. If no key found, prompt the user inline (not a separate screen):
   ```
   No API key configured. Choose a provider:
     1. Anthropic (ANTHROPIC_API_KEY)
     2. OpenAI (OPENAI_API_KEY)
     3. Google (GEMINI_API_KEY)
     4. Custom (OpenAI-compatible endpoint)

   Selection: _
   ```
5. After selection, prompt for the API key:
   ```
   Enter your Anthropic API key: sk-ant-...
   ```
6. Write to `~/.anie/auth.json`. Set default model in `~/.anie/config.toml`.
7. Enter the main TUI.

### Subsequent runs

1. Load config and auth.
2. Resolve the default model and verify auth.
3. Check for `AGENTS.md` in CWD hierarchy and load project context.
4. If a session ID is passed via `--resume`, load that session. Otherwise start fresh.
5. Enter the main TUI.

---

## Data flow: prompt to response

```
User types in InputPane, presses Enter
  │
  ▼
anie-tui: emit UiAction::SubmitPrompt(text)
  │
  ▼
anie-cli interactive controller: build UserMessage, persist it to anie-session
  │
  ▼
anie-agent: AgentLoop::run(prompts, owned_context, event_tx, cancel)
  │
  ├─► Check auto-compaction (is context near limit?)
  │     If yes: run compaction, persist CompactionEntry
  │
  ├─► Convert context to LlmMessage via provider.convert_messages()
  │
  ├─► ProviderRegistry::stream(model, llm_context, options)
  │     │
  │     ▼
  │   anie-providers-builtin: HTTP SSE stream to provider API
  │     │
  │     ▼
  │   Stream Result<ProviderEvent, ProviderError> back
  │
  ├─► Collect AssistantMessage from stream
  │     Emit MessageStart, MessageDelta, MessageEnd
  │     │
  │     ▼
  │   anie-tui: render streaming text in OutputPane
  │
  ├─► Extract ToolCalls from AssistantMessage
  │     For each tool call:
  │       ├─► Validate args against ToolDef schema
  │       ├─► ToolRegistry::get(name).execute(args)
  │       │     │
  │       │     ▼
  │       │   anie-tools: execute read/write/edit/bash
  │       │     │
  │       │     ▼
  │       │   Return ToolResult
  │       ├─► Emit ToolExecEnd
  │       └─► Push ToolResultMessage to context
  │
  ├─► anie-cli interactive controller: persist generated AssistantMessage + ToolResultMessages
  │
  └─► Loop back to stream next turn (if tool calls were made)

Agent finishes (no more tool calls)
  │
  ▼
anie-tui: show idle state, re-focus InputPane
```

---

## Implementation plan

### Phase 1: Foundation (weeks 1-2)

1. **`anie-protocol`** — define all types. Write exhaustive serde tests.
2. **`anie-provider`** — define traits and registry. No implementations yet.
3. **`anie-agent`** — implement the loop with a mock provider (returns canned responses). Test: prompt → assistant → tool call → tool result → assistant → stop.
4. **`anie-tools`** — implement `ReadTool`, `WriteTool`, and `BashTool` first (most useful for bootstrapping). Test in isolation.

### Phase 2: Providers (weeks 3-4)

5. **`anie-providers-builtin`** — OpenAI-compatible provider first. This unlocks OpenAI, Ollama, LM Studio, and local `vllm` with one implementation.
6. **Ollama / LM Studio integration** — manual config plus optional auto-detection so local testing is zero-cost from day one.
7. **Anthropic provider** — first hosted-provider target.
8. **`anie-auth`** — implement async request-option resolution backed by API keys / env vars. No OAuth in v1.0.
9. **`anie-config`** — implement TOML loading with layer merging and project-context size caps.

### Phase 3: TUI (weeks 5-6)

10. **`anie-tui`** — implement the two-pane layout. Start with static rendering (no streaming).
11. Add streaming: `MessageDelta` events update the output pane in real-time.
12. Add tool call rendering: bordered blocks for read/edit/bash results.
13. Add input history (up/down recall).
14. Add the status bar with model/thinking/context display.

### Phase 4: Sessions and compaction (weeks 7-8)

15. **`anie-session`** — JSONL persistence. Test write-read roundtrip.
16. Add tree structure (parent_id pointers). Test fork and branch traversal.
17. Add compaction. Test that compaction reduces token count while preserving context.
18. Wire session persistence into the interactive controller: prompts are persisted immediately, generated messages are persisted from `AgentRunResult`.

### Phase 5: EditTool, CLI, and post-v1.0 polish (weeks 9-10)

19. **`anie-cli`** — argument parsing with `clap`. Wire everything together.
20. Add `EditTool` (the last missing core tool; `WriteTool` already landed in Phase 1).
21. Add `--resume`, `--no-tools`, and session listing.
22. Add onboarding flow (prefer local providers, then hidden API-key prompt).
23. Add `/model`, `/thinking`, `/compact`, `/clear`, `/help` slash commands.
24. Add diff rendering for edit results (using `similar` crate output).
25. **Post-v1.0:** implement the out-of-process extension design from
    `docs/refactor_plans/10_extension_system_pi_port.md`.

### Phase 6: Hardening (weeks 11-12)

26. Error recovery: retry on 429/529/5xx with exponential backoff (mirror pi's auto-retry).
27. Context overflow handling: detect overflow errors, compact, retry.
28. Graceful shutdown: drain in-flight tool calls, persist session, restore terminal.
29. Cross-platform testing: Linux, macOS, Windows.
30. Release build optimization: `lto = "fat"`, `codegen-units = 1`, `strip = "symbols"`.

---

## Key dependencies

| Crate | Purpose |
|---|---|
| `tokio` | Async runtime |
| `reqwest` | HTTP client for provider APIs |
| `eventsource-stream` | SSE parsing for streaming responses |
| `ratatui` | TUI framework |
| `crossterm` | Terminal backend for ratatui |
| `serde` / `serde_json` | Serialization |
| `toml` | Config parsing |
| `clap` | CLI argument parsing |
| `similar` | Diff generation for edit tool |
| `tokio-util` | `CancellationToken` for abort |
| `tracing` | Structured logging |
| `uuid` | Entry IDs for session tree |
| `dashmap` | Concurrent map for file mutation queue |
| `anyhow` / `thiserror` | Error handling |

---

## What is deliberately omitted from v1

- **Sandboxing** — Codex has Landlock, bubblewrap, seatbelt, Windows restricted tokens. Anie-rs v1 runs tools unsandboxed (like pi). Sandboxing is a v2 concern.
- **MCP** — Model Context Protocol server support. Not needed for core functionality.
- **OAuth flows / GitHub Copilot** — API keys and local models are sufficient for v1.0. OAuth-backed providers are post-v1.0.
- **Web UI** — TUI only for v1.
- **Multi-agent** — Single agent, single thread. No subagent spawning.
- **Overlays and popups** — The TUI is two panes only. Model selection is via slash command or CLI flag, not a floating picker.
- **Branch summarization** — Session tree supports branching, but summarization of abandoned branches is a v2 feature.
- **Image input** — Text-only for v1. Image support in `read` output (for the model to see) but no image paste or attachment from the user.
- **Skills** — No SKILL.md discovery. Project context is via AGENTS.md only.
- **Prompt templates** — No `@template` expansion. Direct user input only.
- **Guaranteed Google provider support** — OpenAI-compatible + Anthropic are the required v1.0 provider set. Google can land in v1.1 if schedule slips.
