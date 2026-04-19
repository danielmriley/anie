# anie-rs — Architecture Diagram

Equivalent of the Codex CLI architecture diagram, adapted for anie-rs.

---

## High-Level Architecture

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                              ENTRY BINARY                                    │
│   anie-cli: clap arg parsing, config loading, auth resolution                │
│   Dispatches to: interactive (TUI) · print (one-shot) · rpc (JSONL stdio)   │
└───────────────────────────────────┬──────────────────────────────────────────┘
                                    │
              ┌─────────────────────┼──────────────────────┐
              ▼                     ▼                      ▼
     ┌──────────────┐      ┌──────────────┐       ┌────────────────┐
     │  anie-tui    │      │  print mode  │       │  rpc mode      │
     │  ratatui +   │      │  (in cli)    │       │  JSONL over    │
     │  crossterm   │      │  stdout only │       │  stdin/stdout  │
     └──────┬───────┘      └──────┬───────┘       └────────┬───────┘
            │                     │                        │
            └──────────── AgentEvent channel ───────────────┘
                       (tokio::sync::mpsc sender)
                                    │
                                    ▼
        ┌────────────────────────────────────────────────────────────────┐
        │                        anie-agent                              │
        │   AgentLoop::run(prompts, owned_context, event_tx, cancel)     │
        │   -> AgentRunResult                                             │
        │   ToolRegistry (trait objects)                                  │
        │   Internal before/after-tool-call seams                         │
        │   Prompt → Stream → ToolExec → Loop → AgentEnd                 │
        └───┬───────────┬─────────────┬─────────────────────────────┘
            │           │             │
            ▼           ▼             ▼
     ┌───────────┐ ┌──────────┐ ┌───────────┐
     │ anie-     │ │ anie-    │ │ anie-     │
     │ protocol  │ │ provider │ │ tools     │
     │           │ │          │ │           │
     │ Message   │ │ Provider │ │ ReadTool  │
     │ (User,    │ │ trait    │ │ WriteTool │
     │  Asst,    │ │          │ │ EditTool  │
     │  ToolRes, │ │ Provider │ │ BashTool  │
     │  Custom)  │ │ Registry │ │           │
     │           │ │          │ │ FileMut-  │
     │ AgentEvent│ │ Model    │ │ ationQueue│
     │ StreamΔ   │ │ ApiKind  │ │           │
     │ ToolDef   │ │ Thinking │ │ Tool trait│
     │ ToolResult│ │ Level    │ │ impl per  │
     │ Usage     │ │          │ │ tool      │
     │ Cost      │ │ LlmCtx   │ └───────────┘
     │ StopReason│ │ StreamOpt│
     └───────────┘ │ Provider │
                   │ Event    │
                   └─────┬────┘
                         │
           ┌─────────────┼─────────────────────┐
           ▼             ▼                     ▼
    ┌────────────┐┌────────────┐ ┌──────────────┐
    │ Anthropic  ││  OpenAI    │ │   Google     │
    │            ││            │ │              │
    │ POST /v1/  ││ POST /v1/  │ │ POST /v1beta│
    │ messages   ││ chat/      │ │ /models/    │
    │ stream:true││ completions│ │ :stream-    │
    │            ││ stream:true│ │ Generate-   │
    │ SSE via    ││ SSE via    │ │ Content     │
    │ reqwest +  ││ reqwest +  │ │ SSE via     │
    │ eventsource││ eventsource│ │ reqwest     │
    └────────────┘└────────────┘ └──────────────┘

    anie-providers-builtin
    (register_builtin_providers populates ProviderRegistry at startup)

   ┌──────────────────────────────────────────────────────────────────────┐
   │  CROSS-CUTTING (no business logic depends upward)                    │
   │                                                                      │
   │   anie-config     ~/.anie/config.toml                                │
   │                   Layer merge: global → project (.anie/) → CLI flags │
   │                   Model defaults, compaction settings, custom provs  │
   │                                                                      │
   │   anie-auth       ~/.anie/auth.json (mode 0600)                      │
   │                   API key storage + async request-option resolver    │
   │                   Priority: CLI → auth.json → env var                │
   │                   OAuth / Copilot refresh is post-v1.0               │
   │                                                                      │
   │   anie-session    ~/.anie/sessions/*.jsonl                           │
   │                   Append-only tree (id + parent_id per entry)        │
   │                   Entry types: message, compaction, model_change,    │
   │                                thinking_change, label               │
   │                   Context compaction (LLM summarization)             │
   │                   Branch/fork, session listing, search               │
   └──────────────────────────────────────────────────────────────────────┘
```

**Concurrent writers.** Session files are opened with an exclusive
advisory lock. A second process attempting to open the same session
gets `SessionError::AlreadyOpen`, which the CLI surfaces as an
actionable non-zero-exit error. On filesystems without advisory-lock
support, the lock attempt degrades to a warning rather than a hard
failure.

---

## Data Flow

```
User keystroke
      │
      ▼
anie-tui: InputPane captures text, user presses Enter
      │
      ▼
anie-cli interactive controller: builds UserMessage, persists to anie-session
      │
      ▼
anie-agent: AgentLoop::run(prompts, owned_context, event_tx, cancel)
      │
      ├─► anie-session: check auto-compaction threshold
      │     if context_tokens > context_window - reserve:
      │       summarize old messages, persist CompactionEntry
      │
      ├─► anie-provider: Provider::convert_messages(context)
      │     Message[] → LlmMessage[] (provider-native wire format)
      │
      ├─► anie-provider: ProviderRegistry::stream(model, llm_ctx, opts)
      │     │
      │     ▼
      │   anie-providers-builtin: HTTP SSE to Anthropic/OpenAI/Google
      │     │
      │     ▼
      │   Stream<Result<ProviderEvent, ProviderError>> back to agent loop
      │
      ├─► anie-agent: collect AssistantMessage from stream
      │     emit MessageStart, MessageDelta×N, MessageEnd
      │     │
      │     ▼
      │   anie-tui: render streaming text in OutputPane
      │
      ├─► anie-agent: extract ToolCalls from AssistantMessage.content
      │     │
      │     for each ToolCall (parallel or sequential):
      │       │
      │       ├─► validate args against ToolDef JSON Schema
      │       ├─► anie-tools: Tool::execute(call_id, args, cancel)
      │       │     │
      │       │     ├── ReadTool: read file, truncate, return content
      │       │     ├── WriteTool: create dirs, write file
      │       │     ├── EditTool: match oldText, apply edits, return diff
      │       │     └── BashTool: spawn shell, stream output, return result
      │       │
      │       ├─► emit ToolExecEnd
      │       └─► push ToolResultMessage to context
      │
      ├─► anie-cli interactive controller: persist generated Assistant/ToolResult messages
      │
      └─► loop back (if tool calls were made) or emit AgentEnd

anie-tui: receives AgentEnd, shows idle state, refocuses InputPane
```

---

## Dependency Graph

```
anie-cli
  ├── anie-agent
  ├── anie-auth
  ├── anie-config
  ├── anie-providers-builtin
  ├── anie-session
  ├── anie-tools
  └── anie-tui

anie-agent
  ├── anie-provider
  └── anie-protocol

anie-auth
  ├── anie-config
  ├── anie-provider
  └── anie-protocol

anie-config
  └── anie-provider

anie-providers-builtin
  ├── anie-provider
  └── anie-protocol

anie-session
  └── anie-protocol

anie-tools
  ├── anie-agent
  └── anie-protocol

anie-tui
  └── anie-protocol

Legend:
  A → B means A depends on B
  This is the compile-time crate graph, not the runtime event flow
  No cycles exist
```

**Extensions.** A future out-of-process JSON-RPC extension system is
tracked in
[`docs/refactor_plans/10_extension_system_pi_port.md`](../refactor_plans/10_extension_system_pi_port.md).
Today there is no extension crate in the workspace; the hook traits in
`anie-agent/src/hooks.rs` are internal-only seams reserved for that
future host.

---

## Contrast with Codex CLI

| Aspect | Codex CLI (92 crates) | anie-rs (11 crates) |
|---|---|---|
| Entry dispatch | Multi-call binary (`arg0`), routes by `argv[0]` to sandbox helper, patch applier, execve wrapper | Single binary, `clap` subcommand dispatch |
| Client↔Agent coupling | App-server (JSON-RPC over stdio/WS), SQ/EQ channels, remote clients | Direct `mpsc` channel, in-process only |
| Tools | Shell, apply_patch, JS REPL, image gen, MCP tools, plan, user-input, agent-spawn | `read`, `write`, `edit`, `bash` |
| Sandboxing | 3 native backends (Landlock+bwrap, Seatbelt, Windows restricted token) + Starlark exec policy | None (v1). Tools run unsandboxed. |
| MCP | Full MCP client + server, plugin marketplace, tool approval storage | None (v1) |
| Provider abstraction | `codex-api` (Responses API, SSE+WS, zstd, CompactClient, MemoriesClient) + local model bridges (ollama, lmstudio) | `Provider` trait + 3 built-in impls (Anthropic, OpenAI, Google) |
| TUI | 6000+ LoC ratatui app with overlays, diff rendering, approval modals, voice, multi-agent transcript | Two-pane layout: scrollable output + input editor |
| Session persistence | SQLite state DB + in-memory rollout | Append-only JSONL files with tree structure |
| Extensions | Dynamic tool spec, hooks (Starlark), connectors API, feature flags (60+) | Compiled `Extension` trait, hook dispatch |
| Observability | OpenTelemetry, analytics, sentry, tracing-appender | `tracing` only |
| Security | process-hardening, deny.toml, keyring, escalation protocol | `auth.json` file permissions (0600) |
