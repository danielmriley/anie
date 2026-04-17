# Plan 10 — Extension system (pi-shaped port)

## Motivation

`docs/refactor_plans/pi_mono_comparison.md` establishes that plan
07 option B (a compiled-in 4-hook trait) is too minimal to reach
pi-parity. pi's extension system (`~/Projects/agents/pi/packages/
coding-agent/src/core/extensions/`) is 3099 lines and exposes:

- 35+ event types (see pi's `ExtensionEvent` union).
- Tool registration: extensions declare LLM-callable tools with
  JSON-schema arguments.
- Command registration: `/custom-command` handlers.
- Keyboard shortcut registration.
- CLI flag registration.
- Message renderer registration (custom message types).
- Provider registration (including OAuth).
- A UI context for requesting interactive dialogs (`select`,
  `confirm`, `input`, `notify`, `setStatus`, `setWidget`,
  `setFooter`, `custom` overlay components).
- Error isolation — a crashing extension must not crash anie.

pi's own Rust port plan (`~/Projects/agents/pi/docs/rust-agent-
plan.md`) specifies the transport:

> External process plugins: any language (TypeScript, Python, Go,
> shell), JSON-RPC 2.0

This is important. anie's extension system will **not** be
compiled-in Rust traits — it will spawn subprocesses and speak
JSON-RPC over stdin/stdout. This matches pi's intent and makes
extensions language-agnostic.

This plan is a multi-week effort. It is sequenced in seven phases.
Phases 1–3 get a minimally useful system running. Phases 4–7 reach
feature parity with pi.

## Preconditions

- **Plan 07 option A** has landed (the stub `anie-extensions` crate
  has been deleted). This plan rebuilds the crate from scratch
  against a real contract.
- **Plan 03 phase 3** (slash-command registry with source tagging)
  is helpful but not strictly required for phase 1.

## Design principles

1. **Subprocess-based.** Extensions are executable files — any
   language — that speak JSON-RPC 2.0 over stdin/stdout. anie
   spawns them at startup and manages their lifecycle.
2. **Fault isolated.** A crashed or misbehaving extension gets
   logged and dropped. anie keeps running.
3. **Event-driven.** Extensions subscribe to named events; anie
   broadcasts. Subscription is declared in the extension's
   manifest, not inferred.
4. **Mutation through explicit RPC calls.** Tool registration,
   command registration, etc. are RPC requests the extension makes
   at initialize time (or later, if we support dynamic
   registration).
5. **Everything the extension sees is serializable.** No
   fire-and-forget Rust trait objects. Types that cross the
   boundary are protocol types.
6. **Pi event names and shapes, Rust-idiomatic types.** The wire
   protocol matches pi's `ExtensionEvent` union names
   (`before_agent_start`, `session_before_compact`, etc.). Rust
   types use snake_case enums + serde. This keeps pi extensions
   portable with thin shims.
7. **Bounded lifetime.** Every RPC has a timeout. Every event
   delivery has a deadline. Extensions cannot deadlock the agent.

## Transport overview

```
 anie process
 ┌─────────────────────────────────────────────────────────┐
 │   anie-agent / anie-cli                                  │
 │                                                          │
 │   ExtensionHost                                          │
 │     ├── ExtensionRunner (one per extension)             │
 │     │     ├── child: tokio::process::Child              │
 │     │     ├── stdin:  mpsc<JsonRpcMessage> → child      │
 │     │     ├── stdout: mpsc<JsonRpcMessage> ← child      │
 │     │     └── pending_calls: DashMap<RpcId, oneshot>    │
 │     │                                                    │
 │     └── Broadcaster                                      │
 │           emit(event) → fan out to subscribed runners   │
 │                                                          │
 └─────────────────────────────────────────────────────────┘
                       │
                       ▼  stdin/stdout pipes
 ┌─────────────────────────────────────────────────────────┐
 │   Extension subprocess (user-authored, any language)    │
 │     reads JSON-RPC messages from stdin                   │
 │     writes JSON-RPC messages to stdout                   │
 │     stderr flows to anie's log sink                      │
 └─────────────────────────────────────────────────────────┘
```

## Extension manifest

Each extension has a `manifest.json` alongside its executable:

```json
{
  "name": "my-extension",
  "version": "0.1.0",
  "entry": "./dist/index.js",
  "runtime": "node",
  "events": ["session_start", "before_tool_call", "message_end"],
  "timeout_ms": { "default": 5000, "tool_call": 30000 }
}
```

anie discovers manifests in:

- `~/.anie/extensions/<name>/manifest.json`
- `./.anie/extensions/<name>/manifest.json` (project-scoped)

(This matches pi's discovery shape; see pi's
`extensions/loader.ts:557`.)

---

## Phase 1 — Transport, discovery, lifecycle

**Goal:** Spawn extension subprocesses, exchange JSON-RPC
heartbeats, handle clean shutdown and crash isolation. No events,
no registrations yet — just the transport.

### Files to change

| File | Change |
|------|--------|
| `Cargo.toml` (workspace) | Add `jsonrpsee-types` (or hand-roll JSON-RPC envelope types); confirm `tokio = "1"` with `process` feature |
| `crates/anie-extensions/Cargo.toml` | Dependencies for tokio::process, serde, tracing, thiserror |
| `crates/anie-extensions/src/lib.rs` | Re-exports only |
| `crates/anie-extensions/src/manifest.rs` | New — `struct ExtensionManifest`, `parse_manifest`, `discover_extensions` |
| `crates/anie-extensions/src/runner.rs` | New — `struct ExtensionRunner` that owns one subprocess |

### Sub-step A — Manifest discovery

`discover_extensions(cwd: &Path) -> Vec<ExtensionManifest>` walks:

1. `~/.anie/extensions/*/manifest.json`
2. `<cwd>/.anie/extensions/*/manifest.json`
3. ancestors of `cwd` (stop at home)

Project-scoped manifests override global by `name`.

### Sub-step B — JSON-RPC envelope

Hand-roll minimal types:

```rust
#[derive(serde::Serialize, serde::Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str, // "2.0"
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: &'static str,
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub result: Option<serde_json::Value>,
    pub error: Option<JsonRpcError>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<serde_json::Value>,
}
```

Message framing: newline-delimited JSON (one object per line).

### Sub-step C — `ExtensionRunner`

```rust
pub struct ExtensionRunner {
    manifest: ExtensionManifest,
    child: Child,
    outgoing_tx: mpsc::Sender<OutgoingMessage>,
    pending_calls: Arc<DashMap<u64, oneshot::Sender<JsonRpcResponse>>>,
    next_id: AtomicU64,
}

impl ExtensionRunner {
    pub async fn spawn(manifest: ExtensionManifest) -> Result<Self, ExtensionError>;

    /// Fire a notification. Does not wait for a response.
    pub fn notify(&self, method: &str, params: serde_json::Value);

    /// Call a method, wait up to `timeout` for the response.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<T, ExtensionError>;

    pub async fn shutdown(self, grace: Duration) -> Result<(), ExtensionError>;
}
```

### Sub-step D — Stdin/stdout pumping

Two tasks per runner:

1. Reader task: `tokio::io::AsyncBufReadExt::lines` on child stdout.
   Parse each line as `JsonRpcResponse` or
   `JsonRpcRequest`/`Notification`. For responses, look up the
   `pending_calls` entry by `id` and send through the oneshot.
   For incoming requests, route to the host.
2. Writer task: consumes from `outgoing_rx`, writes to child
   stdin, flushes.

Stderr pipes through `tracing::warn!` with the extension name as a
span field.

### Sub-step E — Heartbeat

At initialize, anie calls `extension.initialize` with
`{ protocol_version: 1, anie_version: "x.y.z" }`. Extension
returns `{ name, version, capabilities: [...] }`. If the call
times out or fails, the runner is dropped and a warning logged;
anie continues without the extension.

### Sub-step F — `ExtensionHost`

```rust
pub struct ExtensionHost {
    runners: Vec<Arc<ExtensionRunner>>,
}

impl ExtensionHost {
    pub async fn load(cwd: &Path) -> Self;
    pub async fn shutdown_all(self, grace: Duration);
    pub fn runners(&self) -> &[Arc<ExtensionRunner>];
}
```

### Sub-step G — Wire into `anie-cli`

`ControllerState::new` loads the host. `ControllerState::drop` or
an explicit shutdown path tears it down.

### Test plan

| # | Test |
|---|------|
| 1 | `discover_finds_user_and_project_manifests` (tempdir-based) |
| 2 | `project_manifest_overrides_user_by_name` |
| 3 | `runner_spawns_and_initializes_handshake` (fixture extension: a shell script that echoes the initialize response) |
| 4 | `runner_times_out_on_missing_initialize` |
| 5 | `runner_survives_extension_crash_without_killing_anie` |
| 6 | `notify_writes_json_rpc_line_to_stdin` |
| 7 | `call_routes_response_by_id` |
| 8 | `call_returns_timeout_error` |
| 9 | `stderr_surfaces_via_tracing` |
| 10 | `shutdown_sends_sigterm_then_sigkill_after_grace` |

### Files that must NOT change

- `crates/anie-protocol/*`
- `crates/anie-provider/*`
- `crates/anie-agent/*` (until phase 2)

### Exit criteria

- [ ] `anie-extensions` crate exists with real code.
- [ ] A dummy echo-extension (fixture in tests dir) spawns,
      initializes, and shuts down cleanly.
- [ ] A deliberately-crashing extension does not crash anie.
- [ ] Ten transport tests above pass.

---

## Phase 2 — Core event broadcast

**Goal:** Extensions receive lifecycle events. No registrations
yet.

### Target events (this phase)

The minimum set to do anything useful:

- `session_start` — after session opens (pi equivalent:
  `SessionStartEvent`).
- `session_shutdown` — on clean exit.
- `agent_start` / `agent_end` — around each agent run.
- `turn_start` / `turn_end` — per loop iteration.
- `message_start` / `message_update` / `message_end` — around
  streamed messages.
- `tool_execution_start` / `tool_execution_update` /
  `tool_execution_end` — around tool calls.
- `model_select` — on model change.

(These are 12 of pi's 35+ events. Phases 3–6 add the rest.)

### Files to change

| File | Change |
|------|--------|
| `crates/anie-extensions/src/events.rs` | New — `enum ExtensionEvent` with serde-tagged variants, one per event |
| `crates/anie-extensions/src/host.rs` | `ExtensionHost::emit(&self, event: &ExtensionEvent)` broadcasts to subscribed runners |
| `crates/anie-extensions/src/manifest.rs` | Parse `events: [...]` field and expose subscription set |
| `crates/anie-agent/src/agent_loop.rs` | Emit events at lifecycle points |
| `crates/anie-cli/src/controller.rs` | Emit session events |

### Sub-step A — Event type

```rust
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum ExtensionEvent {
    #[serde(rename = "session_start")]
    SessionStart { reason: SessionStartReason, previous_session_file: Option<PathBuf> },

    #[serde(rename = "agent_start")]
    AgentStart,

    #[serde(rename = "agent_end")]
    AgentEnd { messages: Vec<AgentMessage> },

    // ...
}
```

Event names mirror pi's wire names so pi extensions can be ported
with a serde alias layer.

### Sub-step B — Broadcast semantics

- Each event is fire-and-forget by default (JSON-RPC notification).
- Some events (`before_*`) are **blocking** and wait for response
  (phase 3 onwards).
- `emit` returns immediately; delivery to each runner is
  best-effort with a per-runner deadline. A slow runner does not
  delay others.
- If a runner's outgoing queue is saturated, drop the event for
  that runner and log.

### Sub-step C — Wire into agent loop

At each event site in `agent_loop.rs`, after the existing
`AgentEvent` channel send, also call
`host.emit(&ExtensionEvent::AgentStart)`. Use a feature flag or an
optional `Option<Arc<ExtensionHost>>` so extension emission is zero
cost when extensions are disabled.

### Test plan

| # | Test |
|---|------|
| 1 | `extension_receives_subscribed_event` (fixture extension logs each event; assert via stderr capture) |
| 2 | `extension_does_not_receive_unsubscribed_event` |
| 3 | `slow_extension_does_not_block_emit` (extension sleeps; second extension still gets the event promptly) |
| 4 | `queue_saturation_drops_and_logs` |
| 5 | `serialized_event_round_trips_for_each_type` (unit tests on the enum — pure serde) |
| 6 | `agent_loop_emits_turn_start_turn_end_in_order` |
| 7 | `session_shutdown_fires_on_sigterm` |

### Exit criteria

- [ ] All 12 core events defined and serialize cleanly.
- [ ] Fixture extension observes events in the expected order.
- [ ] A slow extension does not regress user-visible latency.

---

## Phase 3 — Blocking events, tool registration

**Goal:** Extensions can register LLM-callable tools and can block
or modify key decision points.

### Target events (this phase)

- `before_agent_start` — extension can inject messages or replace
  system prompt (pi: `BeforeAgentStartEvent`).
- `context` — extension can mutate message list before the LLM
  call (pi: `ContextEvent`).
- `before_tool_call` — extension can block or mutate tool args
  (pi: `ToolCallEvent`).
- `after_tool_call` — extension can modify the result
  (pi: `ToolResultEvent`).
- `before_provider_request` — extension can replace the payload
  (pi: `BeforeProviderRequestEvent`).

### Files to change

| File | Change |
|------|--------|
| `crates/anie-extensions/src/tools.rs` | New — `struct RegisteredTool` that forwards `execute(args, ctx) -> Result<ToolResult, _>` to the extension via JSON-RPC |
| `crates/anie-extensions/src/host.rs` | `emit_blocking(event) -> Vec<ExtensionResponse>`; `collect_registered_tools() -> Vec<RegisteredTool>` |
| `crates/anie-extensions/src/events.rs` | Add blocking event variants with response types |
| `crates/anie-agent/src/agent_loop.rs` | At hook points, call `emit_blocking`; merge responses |
| `crates/anie-agent/src/tool.rs` | Accept extension-registered tools alongside built-ins |

### Sub-step A — Tool registration RPC

At initialize-response time (or later via a
`tools/register` RPC), the extension sends:

```json
{
  "method": "tools/register",
  "params": {
    "name": "my_tool",
    "label": "My Tool",
    "description": "...",
    "parameters": { "$schema": "...", ... }
  }
}
```

`ExtensionHost` collects these into a `Vec<RegisteredTool>`, each
holding an `Arc<ExtensionRunner>` to dispatch execution.

When the agent loop invokes the tool, it calls
`runner.call("tools/execute", { name, args, tool_call_id }, timeout)`.
Response:
```json
{ "result": { "content": [...], "isError": false } }
```

### Sub-step B — Blocking event responses

```rust
#[derive(serde::Deserialize)]
pub struct ToolCallEventResult {
    pub block: Option<bool>,
    pub reason: Option<String>,
}
```

The agent loop calls `host.emit_blocking(&event)` and gets a
`Vec<Response>` in registration order. First `block: true`
short-circuits.

For argument mutation: the event struct has a mutable `input`;
after each extension responds, anie re-reads the input and passes
the current value to the next extension.

### Sub-step C — Timeouts

Blocking events have per-event timeouts from the manifest
(`timeout_ms.tool_call` etc.). If an extension exceeds the budget,
it's skipped with a warn log; the agent proceeds.

### Test plan

| # | Test |
|---|------|
| 1 | `extension_can_register_tool_at_initialize` |
| 2 | `registered_tool_is_callable_by_llm` (mock provider emits tool call; assert extension receives `tools/execute`) |
| 3 | `tool_execution_timeout_returns_error_result` |
| 4 | `tool_execution_crash_does_not_crash_anie` |
| 5 | `before_tool_call_block_true_prevents_execution` |
| 6 | `before_tool_call_mutation_reaches_execution` |
| 7 | `after_tool_call_can_replace_content` |
| 8 | `before_agent_start_can_replace_system_prompt` |
| 9 | `context_event_can_strip_messages` |
| 10 | `blocking_events_respect_per_event_timeout` |

### Exit criteria

- [ ] A fixture extension registers one tool and the LLM can call
      it end-to-end.
- [ ] Blocking events behave as pi does.
- [ ] 10 tests pass.

---

## Phase 4 — Commands, shortcuts, flags

**Goal:** Extensions register slash commands, keyboard shortcuts,
and CLI flags.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-extensions/src/commands.rs` | New — `RegisteredCommand`, `RegisteredShortcut`, `RegisteredFlag` |
| `crates/anie-extensions/src/host.rs` | Aggregate registrations across runners |
| `crates/anie-cli/src/commands.rs` *(from plan 03 phase 3)* | `CommandRegistry` consumes extension-registered commands alongside builtins; `SlashCommandSource::Extension { extension_name }` tags them |
| `crates/anie-tui/src/input.rs` or `crates/anie-tui/src/app.rs` | Register extension shortcuts into the keybinding table at load time |
| `crates/anie-cli/src/lib.rs` | Inject extension-registered CLI flags into `clap` at startup |

### Sub-step A — Command registration RPC

```json
{
  "method": "commands/register",
  "params": {
    "name": "mycommand",
    "description": "Do a thing",
    "argument_completions": true
  }
}
```

Invocation: anie calls
`runner.call("commands/invoke", { name, args: "<text>" })`.

### Sub-step B — Shortcut registration

```json
{
  "method": "shortcuts/register",
  "params": { "keyid": "ctrl+g", "description": "Toggle git mode" }
}
```

Invocation: when the user presses the bound key, anie calls
`runner.call("shortcuts/invoke", { keyid })`.

### Sub-step C — Flag registration

Flags declared via `flags/register` at initialize time. anie
restarts `clap` parsing with the merged flag set. Because clap
needs flags at startup, this requires a two-pass parse: a first
pass collects extension manifests, a second pass with full flags.
(pi does this too — `scripts/*.ts` reads the manifest to inject
flags before clap parse.)

Alternative: defer flags to a `[flags]` section in
`manifest.json`, avoiding the two-pass need.

### Test plan

| # | Test |
|---|------|
| 1 | `extension_registered_command_appears_in_help` |
| 2 | `extension_command_invocation_passes_args` |
| 3 | `extension_shortcut_triggers_invocation` |
| 4 | `extension_flag_surfaces_in_getFlag_call` |
| 5 | `command_with_argument_completions_requests_completions_from_extension` |
| 6 | `multiple_extensions_cannot_register_same_command_name` (last-wins with warn, or error — match pi) |

### Exit criteria

- [ ] A fixture extension registers `/hello`, `ctrl+h`, and a
      `--greet` flag; all three work.
- [ ] Duplicate registrations handled per pi's behavior.

---

## Phase 5 — UI context primitives

**Goal:** Extensions can pop dialogs, set status, show
notifications. This is the most mode-dependent phase (interactive
has rich UI; print/RPC modes stub most of this).

### Target API (subset of pi's `ExtensionUIContext`)

- `select(title, options)` → blocking
- `confirm(title, message)` → blocking
- `input(title, placeholder)` → blocking
- `notify(message, type)` → fire-and-forget
- `setStatus(key, text)` → fire-and-forget
- `setWorkingMessage(message)`
- `setTitle(title)`

### Files to change

| File | Change |
|------|--------|
| `crates/anie-extensions/src/ui.rs` | New — RPC method handlers: `ui/select`, `ui/confirm`, `ui/input`, `ui/notify`, `ui/set_status` |
| `crates/anie-extensions/src/host.rs` | Route incoming `ui/*` RPC requests to the active UI backend |
| `crates/anie-tui/src/app.rs` or `crates/anie-tui/src/overlays/extension_dialog.rs` | New — small overlay that renders an extension-requested dialog (plan 02 overlays system makes this easy) |
| `crates/anie-cli/src/modes/*` (once modes are split — plan 03) | Per-mode `UiBackend` impls: interactive (real dialogs), print (stubbed), rpc (stubbed or proxied to client) |

### Sub-step A — Reverse RPC

Unlike phases 1–4 where anie calls extensions, here **extensions
call anie**. The `ExtensionRunner` reader loop must handle incoming
`JsonRpcRequest` as well as responses, and dispatch to host-side
handlers.

### Sub-step B — Per-mode backends

```rust
#[async_trait::async_trait]
pub trait ExtensionUiBackend: Send + Sync {
    async fn select(&self, title: &str, options: &[String]) -> Result<Option<String>, UiError>;
    async fn confirm(&self, title: &str, message: &str) -> Result<bool, UiError>;
    async fn input(&self, title: &str, placeholder: Option<&str>) -> Result<Option<String>, UiError>;
    fn notify(&self, message: &str, kind: NotifyKind);
    fn set_status(&self, key: &str, text: Option<&str>);
}
```

Interactive backend opens overlays. Print/RPC backends return
errors or no-ops with a warning.

### Test plan

| # | Test |
|---|------|
| 1 | `extension_can_call_ui_select_and_receive_choice` |
| 2 | `extension_ui_call_in_print_mode_returns_unsupported_error` |
| 3 | `set_status_updates_status_bar` (TUI integration) |
| 4 | `notify_surfaces_as_toast` |
| 5 | `concurrent_ui_requests_serialize_sanely` (two extensions both open selects — second waits or errors) |
| 6 | `ui_request_respects_abort_signal` |

### Exit criteria

- [ ] Extensions can open selects and inputs in interactive mode.
- [ ] Print/RPC modes gracefully decline UI calls.
- [ ] Status-bar extension use case works.

---

## Phase 6 — Message renderers and widgets

**Goal:** Extensions render custom message types and inject
widgets above/below the editor. This phase is TUI-deep.

### Target API

- `registerMessageRenderer(customType, renderer)`
- `setWidget(key, content, placement)` — content may be a static
  array or a component-factory callback.
- `setFooter(factory)` / `setHeader(factory)`
- `setEditorComponent(factory)` — override the input editor.

### Trade-off

pi's widget/footer/editor API takes **component factories** — in
TS that's a function returning a `Component`. In Rust, we can't
serialize a Rust closure across a JSON-RPC boundary. Options:

- **Option A (simpler):** Widgets are strings or structured data
  sent once via RPC; anie renders them. Extensions can update by
  sending a new `ui/set_widget` call. No callbacks.
- **Option B (harder):** Define a small markup language for
  widgets (rows, columns, text, button, fill, styled spans) and
  render on anie's side. Extensions emit markup; anie draws.

Pick **Option B** — it's closer to pi's flexibility and avoids
every widget change requiring anie code edits.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-extensions/src/markup.rs` | New — `enum WidgetNode { Row, Column, Text, ... }` with serde |
| `crates/anie-tui/src/widgets/extension_node.rs` | New — render a `WidgetNode` as a `ratatui` widget |
| `crates/anie-extensions/src/host.rs` | Handle `ui/set_widget`, `ui/set_footer`, `ui/set_header` |
| `crates/anie-tui/src/app.rs` | Accept extension widget state; place above/below editor or in footer |
| `crates/anie-protocol/src/messages.rs` | Add `CustomMessage { custom_type, content, display, details }` variant |

### Sub-step A — Custom message type

Add a `Custom` variant to `Message` carrying `custom_type: String`
+ `display: WidgetNode` + `details: serde_json::Value`. The
session manager persists it; the TUI renders `display` using the
extension's registered renderer or a fallback.

### Sub-step B — Renderer registration

```json
{
  "method": "renderers/register",
  "params": { "custom_type": "mytype" }
}
```

At render time for a custom message, anie calls
`runner.call("renderers/render", { message, options })`
and expects a `WidgetNode` back, synchronously. Timeout → fall
back to `display` as-stored.

### Test plan

| # | Test |
|---|------|
| 1 | `widget_node_row_renders_children_horizontally` |
| 2 | `widget_node_styled_text_applies_theme_color` |
| 3 | `extension_set_widget_updates_ui_above_editor` |
| 4 | `custom_message_renderer_produces_expected_output` |
| 5 | `renderer_timeout_falls_back_to_stored_display` |

### Exit criteria

- [ ] Extensions can paint widgets above and below the editor.
- [ ] Custom message types round-trip through session and
      redisplay with extension renderers.

---

## Phase 7 — Provider registration (OAuth-dependent)

**Goal:** Extensions can register new LLM providers, including
OAuth-authenticated ones.

### Prerequisite

OAuth support must exist in `anie-auth` (tracked in
`docs/ideas.md`). This phase is **deferred** until OAuth lands;
the plan is listed for completeness.

### Files to change (when unblocked)

| File | Change |
|------|--------|
| `crates/anie-extensions/src/providers.rs` | New — `RegisteredProvider` wraps an extension-backed `Provider` impl |
| `crates/anie-provider/src/registry.rs` | Accept external provider registrations at runtime |
| `crates/anie-extensions/src/host.rs` | Handle `providers/register` / `providers/unregister` RPCs |

### Sub-step A — Extension-backed provider

```rust
pub struct ExtensionProvider {
    runner: Arc<ExtensionRunner>,
    name: String,
}

#[async_trait::async_trait]
impl Provider for ExtensionProvider {
    async fn stream(&self, ...) -> ProviderStream {
        // Call runner.call("providers/stream", ...) and adapt the
        // response stream.
    }
    fn convert_messages(&self, ...) { ... }
    fn convert_tools(&self, ...) { ... }
}
```

### Sub-step B — OAuth hooks

If the extension manifest declares OAuth, anie handles the login
flow by dispatching `oauth/login` / `oauth/refresh` RPCs to the
extension.

### Exit criteria (deferred)

- [ ] Extensions can register OpenAI-compatible proxy providers.
- [ ] OAuth-authenticated providers work end-to-end.

---

## Files that must NOT change in any phase

- `crates/anie-protocol/*` — except phase 6 adds `Custom` message
  variant.
- `crates/anie-provider/*` — except phase 7.

## Dependency graph

```
Phase 1 (transport) ──► Phase 2 (events) ──► Phase 3 (blocking + tools)
                                                │
                                                ├─► Phase 4 (commands/shortcuts/flags)
                                                ├─► Phase 5 (UI primitives)
                                                └─► Phase 6 (renderers/widgets)
                                                        │
                                                        └─► Phase 7 (provider reg) — after OAuth
```

Phases 4, 5, 6 are independent of each other after phase 3. They
can land in any order. Phase 7 is blocked on independent OAuth
work.

## Out of scope

- **Rust-compiled extensions.** This plan is subprocess-only.
  Compiled-in extensions are a future consideration.
- **WASM extensions.** pi doesn't have them; neither will anie in
  this plan.
- **Hot-reload via filesystem watch.** Reload is `/reload` slash
  command only in pi; same here.
- **Cross-process extension sharing.** Each anie process has its
  own extension host.
- **Security sandboxing of extension subprocesses.** Extensions
  run with anie's privileges. Document clearly. Sandboxing is a
  separate future plan.

## Estimate

- Phase 1: ~1 week.
- Phase 2: ~0.5 week.
- Phase 3: ~1 week.
- Phase 4: ~0.5 week.
- Phase 5: ~1 week.
- Phase 6: ~1 week (widget markup is the hard part).
- Phase 7: ~1 week once OAuth exists.

Total: ~6 weeks of focused work to reach pi-feature-parity on
extensions.
