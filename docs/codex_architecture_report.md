# Codex CLI — Architecture Report

*Source: `/home/daniel/Projects/agents/codex/` · Generated 2026-04-13*

OpenAI's Codex CLI is a local coding agent distributed through three surfaces: a Rust workspace (`codex-rs/`, 92 crates) that holds the agent, a thin npm wrapper (`codex-cli/`) that ships the compiled binaries, and language SDKs (`sdk/python`, `sdk/typescript`) that drive the agent programmatically. The entire runtime — model client, tool execution, sandboxing, TUI, MCP bridge, telemetry — is pure Rust, edition 2024.

---

## 1. High-Level Architecture

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                            DISTRIBUTION SURFACES                             │
│   codex-cli (npm)        sdk/python · sdk/typescript       IDE extensions    │
└──────────────┬────────────────────┬───────────────────────────┬──────────────┘
               │                    │                           │
               ▼                    ▼                           ▼
        ┌────────────────────────────────────────────────────────────────┐
        │                 ENTRY BINARY  (cli + arg0)                     │
        │  MultitoolCli dispatches: exec · tui · login · mcp · app …    │
        │  arg0_dispatch_or_else reroutes argv[0] aliases                │
        │  (apply_patch, codex-linux-sandbox, fs_helper, execve-wrapper) │
        └───────────────────────────┬────────────────────────────────────┘
                                    │
              ┌─────────────────────┼──────────────────────┐
              ▼                     ▼                      ▼
     ┌──────────────┐      ┌──────────────┐       ┌────────────────┐
     │  codex-tui   │      │  codex-exec  │       │  app-server    │
     │  Ratatui UI  │      │  one-shot    │       │  JSON-RPC svc  │
     └──────┬───────┘      └──────┬───────┘       └────────┬───────┘
            │                     │                        │
            └─────── app-server-client (in-proc | WS) ─────┘
                                    │
                                    ▼
        ┌────────────────────────────────────────────────────────────────┐
        │                       codex-core                               │
        │   Session · submission_loop · Op/Event queues (async_channel)  │
        │   Conversation state · turn orchestration · approval gate      │
        └───┬───────────┬─────────────┬─────────────┬──────────────┬─────┘
            │           │             │             │              │
            ▼           ▼             ▼             ▼              ▼
     ┌───────────┐ ┌──────────┐ ┌──────────┐ ┌────────────┐ ┌───────────┐
     │ protocol  │ │ codex-api│ │  tools   │ │  sandbox   │ │   mcp     │
     │ Op/Event  │ │ Responses│ │  shell / │ │ bwrap /    │ │ rmcp-     │
     │ wire types│ │ SSE / WS │ │ patch /  │ │ landlock / │ │ client +  │
     │ ts-rs gen │ │          │ │ MCP / FS │ │ seatbelt / │ │ mcp-server│
     └─────┬─────┘ └─────┬────┘ │ jsrepl / │ │ windows SB │ └─────┬─────┘
           │             │      │ skills   │ │ execpolicy │       │
           │             ▼      └──────────┘ │ (starlark) │       ▼
           │       ┌──────────┐              └────────────┘  external MCP
           │       │providers │                                    servers
           │       │ ollama / │
           │       │ lmstudio │
           │       │ chatgpt  │
           │       └──────────┘
           ▼
   ┌──────────────────────────────────────────────────────────────────────┐
   │  CROSS-CUTTING:  config · login + keyring-store · otel + analytics   │
   │   rollout · state · hooks · plugins · features · instructions        │
   │   utils/* (absolute-path · pty · rustls-provider · fuzzy-match …)    │
   └──────────────────────────────────────────────────────────────────────┘
```

The key architectural idea: **Submission Queue / Event Queue (SQ/EQ) pattern**. Clients (tui, exec, remote app-server) push `Op` submissions into a channel; `codex-core`'s `submission_loop` consumes them, drives a turn, and emits `Event` messages back over a second channel. All clients share the same `AppServerClient` abstraction, which can run the agent in-process or connect to a remote app-server over WebSocket.

---

## 2. Crate Inventory

The workspace declares 92 members under `codex-rs/Cargo.toml`, all at `version = "0.0.0"` and `edition = "2024"`. They group into the layers below.

### 2.1 Entry points and orchestration

| Crate | Purpose |
|---|---|
| `cli` (`codex-cli`) | Main binary. `MultitoolCli` (clap) routes to `exec`, `tui`, `login`, `mcp`, `app`, and other subcommands; merges config overrides. |
| `arg0` | Multi-call binary dispatcher. `arg0_dispatch_or_else` inspects `argv[0]`; if the process was invoked as `codex-linux-sandbox`, `apply_patch`, `codex-execve-wrapper`, or `fs_helper`, it runs the specialized handler and exits. Lets one binary act as many tools without subprocess overhead. |
| `exec` (`codex-exec`) | Non-interactive runner. Parses CLI args, opens an `InProcessAppServerClient`, submits a `UserTurn`, drains events, emits JSONL or human-readable output until `TurnComplete`. |
| `tui` (`codex-tui`) | Interactive Ratatui UI. `App` state machine holds conversation + approval queue; `AppServerSession` submits Ops and streams events into render layers. |
| `app-server` | JSON-RPC server that exposes the agent. Dual-loop processor (incoming requests) + outbound router; transports are `Stdio` or WebSocket. Used by IDE extensions and remote clients. |
| `app-server-protocol` | Wire types for the JSON-RPC API (v1 frozen, v2 active). Emits TypeScript via `ts-rs`; strict camelCase conventions, cursor pagination, experimental-field gating. |
| `app-server-client` / `app-server-test-client` | Clients of `app-server`. The test client drives it over WebSocket/tungstenite for diagnostics. |
| `exec-server` | Process sandbox RPC server. Exposes `ExecServerClient`, `ExecutorFileSystem` (local / remote / sandboxed), and PTY allocation. |
| `stdio-to-uds` | Transport adapter that tunnels stdio over a Unix domain socket (uses `uds_windows` on Windows). |

### 2.2 Agent core

| Crate | Purpose |
|---|---|
| `core` (`codex-core`) | The agent. `Codex::spawn()` creates a session bound to `Sender<Submission>` / `Receiver<Event>`. `submission_loop` dispatches `Op` variants (`UserTurn`, `ExecApproval`, `PatchApproval`, `Interrupt`, …) to handlers that call the model, run tools, emit events. State protected with `tokio::sync::{Mutex, watch, RwLock}`. `AGENTS.md` explicitly **resists growth of `codex-core`** — new concepts get new crates. |
| `protocol` (`codex-protocol`) | Wire schema shared by all clients. Defines `Submission { id, op, trace }`, `Event { id, msg: EventMsg }`, `SandboxPolicy`, `AskForApproval`, `ReviewDecision`. `EventMsg` covers lifecycle (`TurnStarted`, `TurnComplete`, `ContextCompacted`), streaming content (`AgentMessageDelta`, `AgentReasoningDelta`), tools (`ExecCommandBegin/End`, `McpToolCallBegin/End`), and approvals. |
| `state` | Persistent session/conversation state. |
| `rollout` | Conversation transcript rollout/replay format. |

### 2.3 Model clients and provider abstraction

| Crate | Purpose |
|---|---|
| `codex-client` | Low-level HTTP transport. `ReqwestTransport`, `RetryPolicy`, SSE streaming via `sse_stream()`, custom CA support. |
| `codex-api` | High-level ChatGPT/OpenAI API: `ResponsesClient`, `CompactClient`, `MemoriesClient`, `ModelsClient`, `RealtimeWebsocketClient`. Wraps SSE + WebSocket; zstd compression. |
| `backend-client` + `codex-backend-openapi-models` | REST client to the Codex backend, schema-typed via OpenAPI. |
| `chatgpt` | ChatGPT auth-flow wrapper around `connectors`. |
| `responses-api-proxy` | Thin `tiny_http` proxy that forwards Responses API calls with credential injection. |
| `ollama`, `lmstudio` | Local OSS model bridges; `ensure_oss_ready()` installs/loads models. |
| `model-provider-info`, `models-manager` | Provider metadata registry, roster, rate limiting, token counting, pricing. |
| `realtime-webrtc` | macOS-only WebRTC bridge for voice mode. |

### 2.4 Tools, skills, and extensibility

| Crate | Purpose |
|---|---|
| `tools` | Unified tool registry: shell, apply_patch, JS REPL, image viewing, MCP resources, plan tools, user-input requests, agent jobs. |
| `skills`, `core-skills` | Built-in skills + fingerprinted installer that caches compiled skills to `$CODEX_HOME/skills/.system`. |
| `code-mode` | Code-execution mode configuration. |
| `instructions` | Prompt instruction assets. |
| `collaboration-mode-templates` | Multi-agent handoff templates. |
| `codex-mcp` | Central MCP manager. `McpManager`, `McpConnectionManager`, tool name resolution, OAuth scope tracking. `AGENTS.md` instructs all MCP mutations to go through `mcp_connection_manager.rs`. |
| `mcp-server` | Serves Codex as an MCP server to external MCP clients. |
| `rmcp-client` | Client for external MCP servers over HTTP/stdio/OAuth via `rmcp`. |
| `hooks` | Lifecycle hooks (regex-triggered shell commands). |
| `plugin`, `utils/plugins` | Plugin identity, capability summary, discovery, mention sigils. |
| `features` | Feature-flag registry with a central `Feature` enum (60+ flags) and `Stage` lifecycle (UnderDevelopment → Experimental → Stable → Deprecated → Removed). |
| `connectors` | App-connector discovery/caching against the ChatGPT directory API. |

### 2.5 Sandboxing and execution safety

| Crate | Purpose |
|---|---|
| `sandboxing` | Platform-agnostic sandbox orchestration, trait-based policy transforms. |
| `linux-sandbox` | Bubblewrap + landlock + seccomp sandbox; WSL1 detection. |
| `windows-sandbox-rs` | Windows native sandbox via IPC, setup binary, and command runner. |
| `execpolicy` (modern) | Starlark-based command policy DSL with positional source tracking (`TextPosition`, `ErrorLocation`). |
| `execpolicy-legacy` | Older argument-matcher policy; retained for compatibility. |
| `shell-command`, `shell-escalation` | Command execution wrapper + Unix privilege-escalation server/wrapper with decision protocol. |
| `process-hardening` | Pre-main hardening via `#[ctor]`: disables core dumps, ptrace attach, strips `LD_PRELOAD` / `DYLD_*` / malloc-control env vars. |
| `network-proxy` | Network isolation layer. |
| `apply-patch` | Safe patch application (`thiserror` errors). |
| `secrets`, `keyring-store`, `login` | Credential handling: OS-native keychains (macOS Keychain, Windows, libsecret), OAuth device flow, token refresh. |

### 2.6 Cloud and remote surfaces

| Crate | Purpose |
|---|---|
| `cloud-tasks`, `cloud-tasks-client`, `cloud-tasks-mock-client` | Cloud task execution API + mock for tests. |
| `cloud-requirements` | Startup loader for billing / feature entitlements; TOML-backed cache. |

### 2.7 Observability

| Crate | Purpose |
|---|---|
| `otel` | OpenTelemetry 0.31 stack: OTLP/HTTP exporters, TLS, `OtelProvider`, W3C traceparent propagation, sanitized metric tagging. |
| `analytics` | Session telemetry; tool-decision source tracking (`AutomatedReviewer` / `Config` / `User`). |
| `feedback` | User feedback collection/routing. |
| `response-debug-context`, `debug-client` | Debug artifacts attached to responses. |

### 2.8 Utilities (`utils/*`, 22 crates)

`absolute-path`, `approval-presets`, `cache`, `cargo-bin`, `cli`, `elapsed`, `fuzzy-match` (nucleo-matcher), `home-dir`, `image`, `json-to-toml`, `oss`, `output-truncation`, `path-utils` (atomic writes, symlink resolution), `plugins`, `pty`, `readiness` (token-based async readiness), `rustls-provider`, `sandbox-summary`, `sleep-inhibitor` (blocks idle sleep during turns), `stream-parser`, `string`, `template`. Plus top-level `async-utils`, `file-search`, `git-utils`, `terminal-detection`, `test-binary-support`, `codex-experimental-api-macros`.

---

## 3. Request / Response Flow

```
User keystroke in TUI
      │
      ▼
Op::UserTurn { items, cwd, approval_policy, sandbox_policy, model, effort, … }
      │  async_channel::Sender<Submission>
      ▼
codex-core::submission_loop  ──▶  user_input_or_turn handler
      │                                │
      │                                ▼
      │                     codex-api Responses (SSE stream)
      │                                │   ResponseItem chunks
      │                                ▼
      │                     Event::AgentMessageDelta   ─────┐
      │                                │                    │
      │                     model emits a tool call         │
      │                                ▼                    │
      │                     Event::ExecCommandBegin         │
      │                                │                    │
      │                     needs approval?                 │
      │                                ▼                    │
      │                     Event::ExecApprovalRequest ─────┤
      │                                │                    │ (rx_event)
      │                     Op::ExecApproval { decision } ◀─┘
      │                                │
      │                                ▼
      │                     sandbox dispatch:
      │                       linux-sandbox (bwrap+landlock)
      │                       windows-sandbox-rs
      │                       seatbelt (.sbpl)
      │                     gated by execpolicy (Starlark)
      │                                │
      │                     Event::ExecCommandOutputDelta ×N
      │                     Event::ExecCommandEnd
      │                                │
      │                     tool result fed back into model
      │                                ▼
      │                     Event::AgentMessage
      │                     Event::TurnComplete
      ▼
Client drains Receiver<Event>, renders / logs
```

Primary async primitives: `async_channel` (unbounded SQ/EQ, cap 512), `tokio::spawn` for background loops, `tokio::sync::{Mutex, watch, RwLock}` for session state, `futures::Shared<BoxFuture>` for coordinated shutdown (`SessionLoopTermination`).

---

## 4. Rust Best Practices Employed

**Edition and toolchain.** Rust edition 2024 set once in `[workspace.package]` so `cargo new -w` inherits it. Pinned toolchain via `rust-toolchain.toml`. Release profile uses `lto = "fat"`, `codegen-units = 1`, `strip = "symbols"`, `split-debuginfo = "off"` for minimum-size deterministic binaries. A dedicated `ci-test` profile (`debug = 1`, `opt-level = 0`) keeps CI fast.

**Workspace-level dependency pinning.** Every third-party crate is declared once in `[workspace.dependencies]` with an exact version; members then use `foo = { workspace = true }`. Internal crates are listed the same way, keyed by path. `cargo-shear` is used to detect unused deps (with documented exceptions in `[workspace.metadata.cargo-shear]`). Forked forks (crossterm, ratatui, tokio-tungstenite) are pinned by git rev in `[patch.crates-io]`.

**Aggressive clippy enforcement.** `[workspace.lints.clippy]` denies 33 lints, including `unwrap_used`, `expect_used`, `uninlined_format_args`, `needless_borrow`, `redundant_clone`, `manual_*` family, `trivially_copy_pass_by_ref`. `clippy.toml` relaxes `expect`/`unwrap` in tests only. TUI has additional lints disallowing raw `Color::Rgb` and hardcoded colors to keep themes coherent.

**Error handling.** Split between `thiserror` for structured domain errors (protocol, execpolicy, `codex-api`, `apply-patch`, image utils) and `anyhow` where context-chain flexibility matters. Most crates expose a crate-local `Result<T>` alias; execpolicy attaches source `TextPosition`/`TextRange` to errors for DSL diagnostics.

**API ergonomics.** `AGENTS.md` codifies rules that show up in the code:
- No boolean or ambiguous `Option` parameters at call sites; prefer enums, newtypes, or named methods. When unavoidable, use `/*param_name*/` comments enforced by an `argument_comment_lint`.
- Exhaustive `match` over wildcard arms.
- Inline `format!` args.
- Private modules with explicit `pub` API.
- Modules capped at ~500 LoC (hard limit ~800); extract rather than extend high-touch files.
- Don't add one-call helper methods.

**Testing.** `insta` snapshot tests are a first-class requirement for any UI change (`cargo insta pending-snapshots`, `cargo insta accept`). `wiremock` backs HTTP integration tests; a shared `core_test_support::responses` helper offers `mount_sse_once`, `ev_*` SSE builders, `ResponseMock::single_request()` for body assertions. `pretty_assertions` is the default; tests compare whole objects rather than field-by-field. `serial_test` handles process-env tests, `tracing-test` asserts on spans, `test-log` structures test logs. Tests avoid mutating the process environment and resolve workspace binaries through `codex_utils_cargo_bin::cargo_bin` so they work under both Cargo and Bazel runfiles.

**Dual build (Cargo + Bazel).** The project builds under both. Each crate has a sibling `BUILD.bazel`. A `justfile` wraps common tasks (`just fmt`, `just fix -p <crate>`, `just test`, `just argument-comment-lint`, `just bazel-lock-update`, `just write-config-schema`, `just write-app-server-schema`). Dependency changes must refresh `MODULE.bazel.lock`. `include_str!` / `include_bytes!` / `sqlx::migrate!` additions require matching `compile_data` entries in `BUILD.bazel`.

**Observability.** Structured `tracing` everywhere, with `tracing-subscriber`, `tracing-appender` for log rotation, and `tracing-opentelemetry` bridging spans to the OpenTelemetry pipeline. Metrics include runtime totals and turn timers; trace context is W3C traceparent.

**Security hygiene.** `deny.toml` whitelists registries/git sources (OpenAI forks only), licenses (Apache-2.0 primary, plus MIT/BSD/MPL-2.0/etc.), and enumerates advisory ignores with justification. `process-hardening` runs before `main`. Sandboxing has three native implementations plus an escalation server, and the Starlark-based `execpolicy` DSL gates every spawned command. The cross-cutting env-var `CODEX_SANDBOX_*` contract (documented in `AGENTS.md`) is considered untouchable.

**Type-safe wire protocols.** `ts-rs` exports protocol types to TypeScript so SDK and IDE extensions share the schema. `app-server-protocol` v2 enforces `#[serde(rename_all = "camelCase")]`, `*Params` / `*Response` / `*Notification` naming, cursor pagination defaults, and `#[experimental(...)]` markers for pre-stable fields. Schemas are regenerated by `just write-app-server-schema` on API changes.

**Cross-platform discipline.** Windows, macOS, and Linux all have first-class sandbox implementations. `#[cfg]` guards are pervasive but narrow; platform-specific code lives in its own crate (`linux-sandbox`, `windows-sandbox-rs`, macOS Seatbelt policies in `sandboxing`) rather than in ad-hoc `cfg` blocks inside `core`.

**Small-crate discipline.** The guiding design principle — stated explicitly in `AGENTS.md` — is to **resist adding code to `codex-core`**. Each new feature gets its own crate (`skills`, `hooks`, `plugin`, `features`, `connectors`, `realtime-webrtc`, `cloud-tasks`, …). This is why the workspace has 92 members: the crate graph, not a god-module, is the unit of encapsulation.

---

## 5. Distribution Layers

- **`codex-cli/`** — npm package `@openai/codex` (`bin/codex.js` → vendored Rust binary + bundled `rg`). Ships what `cargo build --release` produces.
- **`sdk/python/`** — Python SDK (pyproject, examples, notebooks, runtime setup).
- **`sdk/typescript/`** — TypeScript SDK using the ts-rs-generated types from `app-server-protocol`. Tested via jest, bundled with tsup.
- **`.devcontainer/`, `.github/`, `Dockerfile`** — Reproducible build and CI infrastructure.
- **`docs/codex_mcp_interface.md`, `docs/protocol_v1.md`, `docs/bazel.md`** — Protocol and build docs that `AGENTS.md` requires be kept in sync with API changes.

---

## 6. Takeaways

Codex CLI is a strong reference for production Rust agent design. The load-bearing decisions are:

1. **Channels, not callbacks.** The SQ/EQ pattern keeps the agent loop and its clients loosely coupled and makes interruption, approval gating, and remote execution all the same shape.
2. **One protocol crate everyone speaks.** `codex-protocol` + `app-server-protocol` (with ts-rs) give Rust, TypeScript SDK, Python SDK, and IDE extensions a single source of truth.
3. **Aggressive de-monolithing.** The workspace punishes god-crates: `codex-core` is gated, features and platforms get their own crates, utilities live in `utils/*`.
4. **Security designed in, not bolted on.** Three native sandboxes, a Starlark policy DSL, pre-main process hardening, OS keyrings, and an escalation protocol — all as separate crates with clear seams.
5. **Strict tooling baseline.** Edition 2024, 33 denied clippy lints, workspace-pinned deps, snapshot tests, OpenTelemetry, dual Cargo+Bazel build, and an `AGENTS.md` that encodes house rules the linter can't catch.
