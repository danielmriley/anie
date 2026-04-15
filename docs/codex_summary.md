# OpenAI Codex CLI — Implementation Summary

Source: https://github.com/openai/codex/tree/main

---

## Overview

Codex CLI is a terminal-based coding agent from OpenAI. The repository contains two distinct implementations:

- **`codex-cli/`** — The original Node.js/TypeScript CLI wrapper (now a thin launcher).
- **`codex-rs/`** — The current Rust implementation, comprising ~70 crates in a Cargo workspace. This is where essentially all the active development lives.

Distribution is via `npm install -g @openai/codex` (which bundles the compiled Rust binary), Homebrew cask, or direct download from GitHub Releases. Binaries are built for macOS (arm64, x86_64), Linux (musl static), and Windows. The build system supports both Cargo and Bazel.

---

## Repository layout

```
codex-rs/           Rust workspace (~70 crates)
  analytics/        Telemetry and analytics client
  app-server/       Central JSON-RPC server (the runtime host)
  app-server-protocol/  Shared wire types for app-server clients
  apply-patch/      File-patching tool implementation
  backend-client/   HTTP client for OpenAI backend APIs
  cli/              CLI entry point and argument parsing
  code-mode/        Structured code-only execution mode
  config/           TOML config loading and layer merging
  core/             Main agent runtime (turn loop, compaction, MCP, hooks)
  exec/             Non-interactive (headless) mode
  exec-server/      Sandboxed process execution server
  execpolicy/       Shell command allow/deny policy engine
  features/         Feature flag system
  feedback/         In-app feedback submission
  hooks/            Hook system (pre/post-tool-use, session-start, etc.)
  instructions/     System prompt / instructions management
  linux-sandbox/    Linux-specific sandbox (Landlock + bubblewrap)
  login/            Authentication (ChatGPT OAuth, API keys, keyring)
  mcp-server/       Built-in MCP server
  codex-mcp/        MCP connection manager and tool exposure layer
  models-manager/   Model catalog and collaboration-mode presets
  network-proxy/    Network proxy for sandboxed subprocesses
  protocol/         Core protocol types (events, submissions, permissions)
  realtime-webrtc/  Realtime voice/audio via WebRTC
  sandboxing/       Cross-platform sandbox abstraction
  skills/           Skill loading and rendering
  state/            SQLite-backed state DB (threads, history)
  tools/            Tool definitions for the Responses API
  tui/              Ratatui-based terminal UI
  windows-sandbox-rs/ Windows-specific restricted-token sandbox
  utils/*           Utility crates (PTY, strings, paths, images, etc.)
  vendor/bubblewrap  Vendored bubblewrap C source (Linux namespace sandbox)

codex-cli/          Legacy Node.js wrapper (thin launcher)
sdk/python/         Python SDK
sdk/typescript/     TypeScript SDK
docs/               Documentation
```

---

## Architecture: App-server pattern

All agent sessions are owned by the **app server** (`codex-app-server`). Both the TUI and the non-interactive `exec` mode connect to it as clients rather than running the agent directly. This decouples the UI from the model runtime and allows multiple clients to share one running server.

```
┌──────────────────────────────────────────────────────────┐
│  Client                                                   │
│  (TUI / exec CLI / TypeScript SDK / Python SDK)           │
└───────────────────────────┬──────────────────────────────┘
                            │  JSON-RPC (stdio / WebSocket /
                            │  in-process async channel)
┌───────────────────────────▼──────────────────────────────┐
│  codex-app-server                                         │
│  ┌──────────────────────────────────────────────────────┐ │
│  │  Session (one per thread)                            │ │
│  │  ┌──────────────────────────────────────────────┐   │ │
│  │  │  Codex (agent loop)                          │   │ │
│  │  │  ┌────────────────────────────────────────┐  │   │ │
│  │  │  │  ModelClientSession (per turn)         │  │   │ │
│  │  │  │  OpenAI Responses API (SSE / WS)       │  │   │ │
│  │  │  └────────────────────────────────────────┘  │   │ │
│  │  └──────────────────────────────────────────────┘   │ │
│  └──────────────────────────────────────────────────────┘ │
│  exec-server  │  state DB  │  hook runtime  │  MCP mgr    │
└──────────────────────────────────────────────────────────┘
```

The app server accepts three connection modes: stdio (standard in-process via a channel), WebSocket (remote), and a remote-control listener for multi-window setups. Each connection gets its own `ConnectionId`; outgoing events are routed per connection via `OutgoingEnvelope`.

---

## Agent loop (`codex-core`)

### `Codex` and `Session`

`Codex` is the main handle to a running agent thread. It owns a `Session` (per-thread state) and exposes two async queues:

- **Submission queue (SQ)** — client sends `Op`/`Submission` messages (user turn, steer, interrupt, settings change).
- **Event queue (EQ)** — agent emits `Event`/`EventMsg` back to the client.

A `Codex::spawn()` call creates a new thread, applies config and initial context, and starts the session loop.

### Turn lifecycle

Each user turn creates a `TurnContext` capturing:
- Model selection and context window size
- Sandbox permissions derived from config
- File-system sandbox context (cwd, allowed paths)
- Skills and app connectors active for this turn

The turn loop calls the OpenAI Responses API, streams deltas, dispatches tool calls, accumulates tool results, and loops until a terminal stop reason is reached.

### `ModelClientSession`

Created per turn. Manages:
- A reusable Responses API WebSocket connection (opened lazily, cached across retries).
- WebSocket **prewarm**: a `response.create` with `generate=false` is issued before the main stream request so the subsequent request can reuse the same connection and `previous_response_id` header.
- The `x-codex-turn-state` sticky routing token.
- Fallback from WebSocket to HTTP SSE on connection failure.

### Initial context and instructions

Before the first turn, `codex-instructions` constructs the system prompt from:
- The base instructions string from config.
- An `AGENTS.md` file discovered by walking from CWD upward.
- Skills injected as structured sections.
- App connector summaries.
- Turn-context items (local time, working directory, active tools, MCP tool list).

---

## Tools

### `apply_patch` (primary file-editing tool)

`codex-apply-patch` implements a structured diff format for file modifications. The model emits a heredoc-delimited patch body; the tool parses and applies it.

Patch format supports:
- `*** Add file: <path>` — create a new file.
- `*** Delete file: <path>` — remove a file.
- `*** Update file: <path>` — inline unified-diff-like hunks with `- ` / `+ ` / ` ` context lines.
- `*** Rename file: <from> -> <to>` — rename.

The parser produces `Hunk` structs with old/new content; the applier uses `similar::TextDiff` to locate context and compute replacements. Patch application is sandboxed through `ExecutorFileSystem` so write permissions are checked before touching disk.

The tool has two variants registered on the Responses API:
- `create_apply_patch_json_tool` — structured JSON parameters.
- `create_apply_patch_freeform_tool` — free-form text (model writes the patch directly).

### Shell command execution

Shell commands go through a multi-layer stack:

1. **ExecPolicy evaluation** (see below) — allow, deny, or request approval.
2. **Sandbox wrapping** — the command is transformed by the platform-specific `SandboxManager`.
3. **PTY/process spawning** — `codex-utils-pty` spawns the child inside a process group; stdout/stderr are streamed as `ExecCommandOutputDelta` events.
4. **Output truncation** — a hard byte cap (`DEFAULT_OUTPUT_BYTES_CAP`) and a delta event cap (`MAX_EXEC_OUTPUT_DELTAS_PER_CALL = 10,000`) prevent OOM from runaway commands.
5. **Timeout** — default 10 seconds (`DEFAULT_EXEC_COMMAND_TIMEOUT_MS`); configurable. On timeout, the child process group is killed with SIGKILL and exit code 124 is returned.

### Other tools

| Tool | Description |
|---|---|
| Web search | OpenAI web-search tool; result citations surfaced in the TUI |
| File search | Fuzzy file search using nucleo (tree-sitter indexing) |
| JS REPL | V8-backed JavaScript evaluator (`codex-v8-poc`) |
| Image generation | OpenAI image generation; gated on auth level |
| Plan | Structured plan update for plan-mode collaboration |
| Request user input | Prompts the user for a free-text response mid-turn |
| Agent spawn/send/wait | Multi-agent orchestration (see below) |
| MCP tools | All tools exposed by connected MCP servers |

---

## Shell execution policy (`codex-execpolicy`)

The exec-policy engine controls which shell commands the agent can run without prompting the user. Policies are loaded from `.rules` files (TOML-like DSL) stored under `~/.codex/rules/` (default: `default.rules`).

### Rule DSL

Each rule has a prefix pattern (executable name + optional argument tokens, with alternatives) and a decision:

```
cmd = ["git", "add", {alts: [".", "-A"]}]  # matches "git add ." or "git add -A"
decision = "allow"
```

`PatternToken` is either `Single(String)` or `Alts(Vec<String>)`. The policy is keyed by the first token (the executable). Evaluation walks all rules for that executable and returns the first match's decision.

**Decisions:** `Allow`, `Deny`, `AskForApproval`.

**Network rules** are separate: protocol (TCP/UDP/any) + host pattern.

### Approval flow

When a command requires approval, the agent emits an `ExecApprovalRequestEvent`. The client (TUI) shows a modal; the user can approve once, approve a session-scoped allow rule, or deny. Approval amends the in-memory policy by appending a new allow prefix rule (`blocking_append_allow_prefix_rule`). Similarly, network approval appends a network allow rule.

### Approval modes

`AskForApproval` has three variants:
- `Never` — all uncertain commands are rejected.
- `OnFailure` — only prompt after the command fails (for retry).
- `UnlessAllowed` — prompt unless the command matches an existing allow rule (the typical default).
- `Granular { sandbox_approval, rules }` — separate control for sandbox-level and rule-level approvals.

---

## Sandboxing

Platform-specific sandbox backends are abstracted behind `SandboxManager` and `SandboxCommand`.

### Linux: Landlock + bubblewrap

Two complementary mechanisms:

1. **Landlock** (in-process): Applied via `prctl(PR_SET_NO_NEW_PRIVS)` + the Landlock LSM API. Restricts filesystem access to a specified set of paths and access modes (read, write, execute). Applied inside the child before `exec` via `codex-linux-sandbox` (a helper executable).

2. **Bubblewrap** (`bwrap`): Namespace-based container. The vendored bubblewrap C source (`vendor/bubblewrap/`) is compiled into `codex-linux-sandbox`. It creates a new mount namespace, bind-mounts allowed paths read-only or read-write, and provides an isolated filesystem view. WSL1 is explicitly unsupported (namespace creation fails).

**Sandbox modes (Linux)**:
- `workspace-write`: workspace dir bind-mounted read-write; rest of filesystem read-only.
- `read-only`: entire filesystem read-only.
- `network-disabled`: no network access.
- `full-auto` / `disabled`: no sandbox.

### macOS: Seatbelt

Uses `sandbox-exec` with a Scheme-based policy profile. The profile is generated at runtime from the sandbox settings to grant or deny file-system and network access at the kernel level.

### Windows

Three levels of sandboxing, increasing in restriction:
- **None** — no sandbox.
- **Restricted token** — process runs with a restricted access token (no admin, limited privileges). Implemented in `windows-sandbox-rs` via Windows ACL, token manipulation, and a ConPTY for PTY emulation.
- **Elevated helper** — a separate elevated helper process (`setup_main`) configures ACLs and DPAPI encryption for the workspace; the agent process runs under a locked-down sandbox user account with a private Windows desktop.

The Windows sandbox crate (`codex-windows-sandbox`) includes firewall rule management, identity/SID manipulation, DPAPI secret storage, and a named-pipe IPC channel between the restricted process and the elevated helper.

---

## Configuration (`codex-config`)

Config is TOML at `~/.codex/config.toml` with a layered merge strategy (global → project-local → CLI overrides). A JSON Schema is generated from the Rust types and stored at `codex-rs/core/config.schema.json`.

Key config sections:

| Section | Description |
|---|---|
| `model` | Model name, provider, reasoning effort, reasoning summary |
| `approval_mode` | `AskForApproval` variant |
| `sandbox_mode` | `SandboxMode` (workspace-write, read-only, etc.) |
| `mcp_servers.*` | MCP server definitions (command, env, transport, tool approvals) |
| `collaboration_mode` | fast / review / plan + sub-options |
| `notify` | Hook command run when the agent finishes a turn |
| `hooks` | Pre/post-tool-use hook commands |
| `exec_policy_file` | Override path for `.rules` file |
| `sqlite_home` | Override path for SQLite state DB |
| `network_proxy` | Managed network proxy settings for sandboxed subprocesses |

Config is loaded with `ConfigBuilder`, which stacks layers and resolves constraints (`Constrained<T>` — can be locked by cloud or enterprise policy).

---

## Authentication (`codex-login`)

### ChatGPT OAuth (primary)

Codex uses the OpenAI `auth.openai.com` OAuth endpoint, the same flow used by the Codex desktop app. Login opens the browser and starts a local HTTP callback server on port 1455. The token is stored in the system keyring (`codex-keyring-store` wraps the `keyring` crate with platform keychain backends).

Tokens are refreshed automatically when expired. The `AuthManager` centralises credential state and provides `CodexAuth` handles to individual sessions. Unauthorized responses trigger a controlled recovery flow rather than a hard error.

### API key

Can be supplied via `OPENAI_API_KEY` or persisted with `/login --api-key`. API key auth runs through the same `CodexAuth` abstraction.

### OSS providers

`resolve_oss_provider()` detects local models (LM Studio, Ollama) and selects the appropriate provider. Enterprise setups can point to a custom base URL.

---

## MCP integration

`codex-mcp` manages connections to external MCP servers configured under `mcp_servers` in config. Each server runs as a child process over stdio (or another transport). Tools exposed by MCP servers are surfaced to the model via the Responses API `tools` array.

**Tool approval**: MCP tools default to asking for approval unless the tool is listed in the config with `approval_mode = "approve"`. Per-tool overrides are persisted in `config.toml` by the TUI's approval storage.

**Parallel calls**: Individual MCP servers can be marked `supports_parallel_tool_calls = true` to allow the model to call multiple tools on that server concurrently.

**MCP elicitation**: The `app-server-protocol` includes `McpServerElicitationRequest` — MCP servers can request structured user input mid-turn (a form with typed fields), surfaced as a modal in the TUI.

**Plugin marketplace**: MCP servers can be published as plugins. The TUI has a plugin browser (`/plugins`) backed by a marketplace API and a local plugin registry.

---

## Hooks (`codex-hooks`)

Hooks are external commands run at defined lifecycle points. They receive a JSON payload over stdin and can return a JSON response.

| Event | Trigger | Response fields |
|---|---|---|
| `session_start` | Session begins | `should_stop`, `additional_contexts` (injected into conversation) |
| `user_prompt_submit` | User submits a message | `should_stop`, `additional_contexts`, `decision` (accept/reject) |
| `pre_tool_use` | Before any tool call | `outcome` (allow/block), `additional_contexts` |
| `post_tool_use` | After any tool call | `additional_contexts`, feedback signal |
| `stop` | Agent finishes a turn | Notify payload |

The `notify` config key is a shorthand for a stop hook that receives `{ command, cwd, exitCode, duration, client }`.

Hooks are run with a configurable timeout. Output is captured and surfaced as `HookStartedEvent` / `HookCompletedEvent` protocol events, which the TUI renders as collapsible history cells.

---

## Context compaction

Compaction triggers when the cumulative token count approaches the model's context window. The core implements two strategies:

### Remote compaction

When the provider and model support it (detected via `should_use_remote_compact_task`), the history is sent to an OpenAI compaction API endpoint (`codex-api::CompactClient`). The API returns a replacement history. This is preferred because it preserves model-native token counts and does not consume additional user quota.

### Local compaction

Falls back to an LLM summarization call using the current model. The summarization prompt (`templates/compact/prompt.md`) asks for a structured handoff summary:
- Current progress and key decisions.
- Important context, constraints, or user preferences.
- What remains to be done.
- Any critical data or references needed to continue.

The older portion of history is replaced by a single compaction item containing the summary. `build_compacted_history()` constructs the replacement item list; `insert_initial_context_before_last_real_user_or_summary()` handles the `BeforeLastUserMessage` injection mode used for mid-turn compaction.

Compaction is tracked with analytics events (`CompactionPhase`, `CompactionStrategy`, `CompactionStatus`).

---

## Collaboration modes

Three built-in modes, selectable with `--mode` or `/mode` in the TUI:

- **Fast** — agent executes directly with no extra approval gates.
- **Review** — a "review" turn is injected before execution; the agent describes what it is about to do and waits for user confirmation before proceeding.
- **Plan** — the agent generates a structured plan (via the `plan` tool), displays it, and waits for the user to accept or edit it before executing.

**Guardian** is an optional external review service. When enabled, exec and patch approvals are sent to a Guardian endpoint for policy evaluation. The TUI renders Guardian decisions as `GuardianAssessmentEvent` cells with risk level, decision source, and user authorization state.

---

## Multi-agent

The `codex-tools` crate exports a family of sub-agent tools:

| Tool | Description |
|---|---|
| `spawn_agent` / `spawn_agent_v2` | Start a new agent thread with a task prompt |
| `send_message` / `send_input` | Send a user message to a running subagent |
| `wait_agent` / `wait_agent_v2` | Block until a subagent reports completion |
| `list_agents` | Enumerate currently running subagents |
| `close_agent` | Terminate a subagent |
| `followup_task` | Queue a follow-up task for a subagent |

The TUI's `multi_agents.rs` renders a summary of concurrently running subagents and their transcript output (`/ps` command).

---

## Terminal UI (`codex-tui`)

Built on **Ratatui** (a fork of `tui-rs`) with a patched crossterm for terminal color-query support.

### Layout

The TUI is split into two vertical panes:

- **Top (history viewport)**: Scrollable transcript of `HistoryCell` items — user messages, assistant text (streamed), exec calls, MCP calls, approval requests, branch summaries, compaction summaries.
- **Bottom (composer + status)**: Text input area, slash-command pop-ups, status line, mode indicator, keybind hints.

### Streaming rendering

Assistant text arrives as streaming deltas. The `markdown_stream` module accumulates chunks and emits `StreamCommit` signals when enough stable content has accumulated. The `streaming/controller.rs` manages the commit tick rate to balance latency against render cost. Committed Markdown is rendered by `markdown_render.rs` (using `pulldown-cmark`) with syntax highlighting via `syntect`.

### Diff rendering

`diff_render.rs` renders `apply_patch` diffs inline in the history. It produces a decorated view with:
- File path header.
- Syntax-highlighted inserted/deleted lines using tree-sitter grammars.
- Long lines wrapped with configurable indentation.
- Vertical ellipsis between non-adjacent hunks.
- Line-number gutters.

### Key widgets and overlays

| Component | Purpose |
|---|---|
| `ChatWidget` | Main controller: consumes events, manages cells, drives layout |
| `HistoryCell` | A single transcript item (user, assistant, exec, MCP, etc.) |
| `ExecCell` | Unified exec/tool group cell with streaming output |
| `DiffRender` | Inline patch diff display |
| `ApprovalModal` | Exec command or patch approval dialog |
| `ListSelectionView` | Generic scrollable picker (model, mode, theme, etc.) |
| `PagerOverlay` | Ctrl+T full-screen transcript overlay |
| `ResumePicker` | Session resume browser (searches SQLite state DB) |
| `StatusIndicatorWidget` | Top-of-viewport streaming status spinner |
| `BottomPane` | Composer + status line + queued messages + footer hints |

### Composer features

- History recall (up/down arrow).
- Multi-line input with Shift+Enter.
- Slash-command auto-complete popup (fuzzy matching via nucleo).
- `@`-mention auto-complete for file paths and MCP resources.
- `$`-connector mention for ChatGPT app connectors.
- Image attachment (local files or remote URLs).
- Draft message queue: messages typed while the agent is running are queued and sent after the current turn completes.

### Realtime voice

`codex-realtime-webrtc` provides a WebRTC-based realtime audio channel to OpenAI's realtime API. The TUI has a microphone picker and session controls for starting/stopping voice turns.

---

## State DB (`codex-state`)

Persistent session state is stored in a SQLite database (`codex-state` crate using `sqlx`). It records:
- Thread metadata (ID, name, title, model, created/updated timestamps).
- Message history per thread.
- Session compaction entries.
- Model availability NUX (new user experience) state.

The DB path defaults to `CODEX_HOME` but can be overridden with `CODEX_SQLITE_HOME`. In `workspace-write` sandbox sessions without an explicit override, a temp directory is used to avoid sandbox write conflicts.

---

## Protocol (`codex-protocol`)

The internal protocol uses a **Submission Queue / Event Queue** pattern. Types are defined with `serde` + `ts-rs` to generate matching TypeScript bindings for the SDKs.

**Key submission types (client → agent):**
- `TurnStart` — submit a user turn (text + optional images).
- `TurnInterrupt` — cancel the current turn.
- `ExecCommandApproval` — approve or deny a pending exec command.
- `ApplyPatchApproval` — approve or deny a pending patch.
- `SessionSettingsUpdate` — change model, mode, sandbox, etc.

**Key event types (agent → client):**
- `TurnStarted` / `TurnCompleted` — turn lifecycle.
- `AgentMessageDelta` / `AgentMessage` — streaming and final assistant text.
- `ExecCommandStart` / `ExecCommandOutputDelta` / `ExecCommandDone` — tool execution lifecycle.
- `ApplyPatchStart` / `ApplyPatchDone` — patch tool lifecycle.
- `ExecApprovalRequest` / `ApplyPatchApprovalRequest` — approval gates.
- `ContextCompactionStart` / `ContextCompactionDone` — compaction lifecycle.
- `HookStarted` / `HookCompleted` — hook execution events.
- `McpServerElicitationRequest` — MCP-initiated user input request.
- `GuardianAssessment` — Guardian review decision.
- `Error` — structured error event with `CodexErr` code.

The app-server protocol (`codex-app-server-protocol`) wraps these in a JSON-RPC envelope with `RequestId` for request/response correlation.

---

## SDKs

### TypeScript SDK (`sdk/typescript`)

Exposes `Codex`, `Thread`, and turn streaming primitives. Spawns the `codex` binary as a child process and communicates over stdio using the JSON-RPC app-server protocol. Supports:
- Streaming turn responses as async iterables.
- Structured output via JSON Schema or Zod.
- Thread resumption.

### Python SDK (`sdk/python`)

Similar wrapper approach. Documented with `sdk/python/docs/` and example notebooks.

---

## Build and distribution

- **Cargo** for local development; `cargo build --release` produces a single static binary (musl on Linux).
- **Bazel** for CI and release builds; Bazel rules generate per-platform release artifacts and handle cross-compilation.
- **Release profile**: `lto = "fat"`, `codegen-units = 1`, `strip = "symbols"` for minimum binary size.
- **npm package**: `codex-cli/scripts/build_npm_package.py` bundles the compiled Rust binary inside a Node.js package for `npm install -g @openai/codex`.
- **Homebrew cask**: separately maintained for macOS installation.
- **Custom CA**: `CODEX_CA_CERTIFICATE` or `SSL_CERT_FILE` env vars inject a custom PEM CA bundle into all outbound TLS connections (for enterprise proxies).
- **OTEL**: Optional OpenTelemetry tracing via `codex-otel`; trace context is propagated across subagent turns via W3C `traceparent` headers.
