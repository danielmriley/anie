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
        │   BeforeToolCall / AfterToolCall hooks                         │
        │   Prompt → Stream → ToolExec → Loop → AgentEnd                 │
        └───┬───────────┬─────────────┬─────────────────────────────┬───┘
            │           │             │                             │
            ▼           ▼             ▼                             ▼
     ┌───────────┐ ┌──────────┐ ┌───────────┐              ┌─────────────┐
     │ anie-     │ │ anie-    │ │ anie-     │              │ anie-       │
     │ protocol  │ │ provider │ │ tools     │              │ extensions  │
     │           │ │          │ │           │              │             │
     │ Message   │ │ Provider │ │ ReadTool  │              │ Extension   │
     │ (User,    │ │ trait    │ │ WriteTool │              │ trait       │
     │  Asst,    │ │          │ │ EditTool  │              │             │
     │  ToolRes, │ │ Provider │ │ BashTool  │              │ Extension   │
     │  Custom)  │ │ Registry │ │           │              │ Runner      │
     │           │ │          │ │ FileMut-  │              │             │
     │ AgentEvent│ │ Model    │ │ ationQueue│              │ Hooks:      │
     │ StreamΔ   │ │ ApiKind  │ │           │              │ before_     │
     │ ToolDef   │ │ Thinking │ │ Tool trait│              │ agent_start │
     │ ToolResult│ │ Level    │ │ impl per  │              │ session_    │
     │ Usage     │ │          │ │ tool      │              │ start       │
     │ Cost      │ │ LlmCtx   │ └───────────┘              │ before/     │
     │ StopReason│ │ StreamOpt│                             │ after_      │
     └───────────┘ │ Provider │                             │ tool_call   │
                   │ Event    │                             └─────────────┘
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
      ├─► anie-extensions: before_agent_start hook
      │     may modify system_prompt, inject messages
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
      │       ├─► anie-extensions: before_tool_call (can block)
      │       ├─► anie-tools: Tool::execute(call_id, args, cancel)
      │       │     │
      │       │     ├── ReadTool: read file, truncate, return content
      │       │     ├── WriteTool: create dirs, write file
      │       │     ├── EditTool: match oldText, apply edits, return diff
      │       │     └── BashTool: spawn shell, stream output, return result
      │       │
      │       ├─► anie-extensions: after_tool_call (can override result)
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
  ├── anie-extensions
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

anie-extensions
  ├── anie-agent
  └── anie-protocol

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
