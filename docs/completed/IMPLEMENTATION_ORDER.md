# anie-rs Implementation Order

This document turns the phase plans into a single execution sequence.

It is intentionally practical:
- do these items in this order,
- stop at each gate,
- do not move on while the previous layer is still unstable.

If this document ever conflicts with the phase plans, update the phase plans and then update this file.

---

## Global guardrails

Before starting implementation, preserve these decisions:

1. **v1.0 is local-first**
   - OpenAI-compatible provider support is the primary path.
   - Ollama / LM Studio are required so development/testing can be zero-cost.
   - Anthropic is strongly desired.
   - Google is optional stretch.
   - GitHub Copilot OAuth is post-v1.0.

2. **Owned context only**
   - `AgentLoop::run(...)` takes owned context and returns `AgentRunResult`.
   - No shared mutable transcript between TUI and agent loop.

3. **Structured provider errors only**
   - Provider streams yield `Result<ProviderEvent, ProviderError>`.
   - Do not reintroduce stringly-typed provider failure paths.

4. **UI/orchestration split**
   - `anie-tui` renders and emits UI actions.
   - `anie-cli` / interactive controller owns config, auth, sessions, compaction, and agent runs.

5. **Session persistence comes from controller/run results**
   - Not from render events.

---

## Step-by-step execution sequence

## Step 0 — Workspace skeleton

Implement:
- Cargo workspace
- crate stubs
- shared dependency versions
- fmt/clippy/test/check commands

Gate:
- `cargo check --workspace` passes
- crate graph matches the architecture docs

Do not continue until this is green.

---

## Step 1 — `anie-protocol`

Implement:
- `Message`
- `ContentBlock`
- `ToolCall`
- `AgentEvent`
- `StreamDelta`
- `ToolDef`
- `ToolResult`
- `Usage`, `Cost`, `StopReason`

Gate:
- exhaustive serde roundtrip tests pass
- message/content/tool result shapes are stable enough that other crates can depend on them

---

## Step 2 — `anie-provider` core contracts

Implement:
- `Model`
- `ApiKind`
- `ThinkingLevel`
- `ProviderError`
- `ProviderEvent`
- `ProviderStream = Stream<Item = Result<ProviderEvent, ProviderError>>`
- `StreamOptions`
- `ResolvedRequestOptions`
- `RequestOptionsResolver` trait
- `Provider` trait
- `ProviderRegistry`
- feature-gated `MockProvider`

Gate:
- the provider trait supports:
  - structured mid-stream errors
  - optional API keys
  - future headers / base URL overrides
- mock provider can simulate text streaming, tool calls, and stream errors

---

## Step 3 — `anie-agent`

Implement:
- `AgentLoop`
- `AgentLoopConfig`
- `AgentRunResult`
- `ToolRegistry`
- `Tool` trait
- hook traits
- `get_steering_messages` / `get_follow_up_messages` hooks (can initially return empty)

Critical rules:
- input context is owned
- output is `AgentRunResult`
- prompts are not silently lost
- provider stream errors stay structured

Gate:
- all core agent-loop tests pass
- prompt → assistant → tool → tool result → assistant loop works with `MockProvider`

This is the first real architectural checkpoint.

---

## Step 4 — Bootstrapping tools (`anie-tools`)

Implement now:
- `ReadTool`
- `WriteTool`
- `BashTool`
- `FileMutationQueue`

Do **not** wait until later for `WriteTool`.

Gate:
- file read/write/bash behavior is test-covered
- mutation queue canonicalizes paths
- end-to-end mock-provider test can read and write files

At this point you have the first real vertical slice.

---

## Step 5 — OpenAI-compatible provider first

Implement:
- shared HTTP client
- SSE parsing helper
- OpenAI-compatible request/response conversion
- streamed text/tool-call handling
- missing-usage tolerance
- optional-auth behavior

Why first:
- one implementation unlocks OpenAI, Ollama, LM Studio, local `vllm`, and other compatible backends

Gate:
- provider tests pass
- tool call streaming works
- argument-fragment accumulation works

---

## Step 6 — Local-model path (`ollama` / `lmstudio`)

Implement:
- manual config path for local models
- local models with no API key
- optional auto-detection (`/v1/models`)
- local-model quirks handling (`stream_options`, missing usage, etc.)

Gate:
- at least one zero-cost local path works end-to-end
- you can use the harness without paying provider API costs

**This is the v1.0 development gate.**
If this is not working, do not move deeper into TUI/session work.

---

## Step 7 — Auth and config

Implement:
- `auth.json`
- async `AuthResolver` / request option resolution
- config loading + layer merging
- custom provider config
- project-context caps (`max_file_bytes`, `max_total_bytes`)

Gate:
- CLI override → auth file → env var resolution works
- local models can still resolve with `api_key: None`
- large `AGENTS.md` files are capped

---

## Step 8 — CLI harness

Implement:
- simple non-TUI harness for real end-to-end testing
- local model default path if available
- streaming transcript to stdout

Gate:
- prompt → tools → response works against a local provider
- this becomes the main debug surface while TUI is being built

---

## Step 9 — Anthropic provider

Implement:
- Anthropic message conversion
- tool-result batching
- SSE parsing
- thinking support
- usage extraction

Gate:
- Anthropic end-to-end runs succeed
- cloud-provider path works in addition to local path

If schedule slips, Anthropic is still higher priority than Google.

---

## Step 10 — TUI rendering layer only

Implement in `anie-tui`:
- app shell
- panes
- input editor
- status bar
- transcript rendering
- tool block rendering
- streaming updates
- scrolling/history/spinner
- snapshot tests with `ratatui::TestBackend`

Do **not** load config/auth or spawn providers from the TUI crate.

Gate:
- TUI works with fake/mocked events
- rendering is stable enough without live orchestration glued in yet

---

## Step 11 — Interactive controller

Implement in `anie-cli` (or equivalent interactive controller module):
- provider/tool setup
- model resolution
- request resolver wiring
- system prompt construction
- UI action handling
- spawning agent runs
- owned canonical context

Gate:
- TUI sends actions to the controller
- controller sends `AgentEvent`s back to the TUI
- there is no shared mutable transcript between UI and agent loop

---

## Step 12 — Sessions

Implement:
- JSONL format
- `SessionManager`
- append-only writes
- active leaf semantics
- `append_message` / `append_messages`
- resume by session ID

Gate:
- user prompts persist immediately
- generated messages persist from `AgentRunResult`
- `--resume <session_id>` reopens the latest leaf cleanly

---

## Step 13 — Compaction

Implement:
- token estimation
- `SessionContextMessage`
- cut-point logic using preserved entry IDs
- summarization prompt + provider call
- proactive compaction
- overflow-triggered compaction/retry

Gate:
- compaction preserves `first_kept_entry_id`
- context overflow recovery works
- summaries are good enough to continue work coherently

---

## Step 14 — `EditTool`

Implement after the rest of the system is already usable.

Implement:
- exact replacement
- overlap/duplicate/not-found errors
- BOM handling
- CRLF preservation
- diff generation
- fuzzy matching for locating spans only

Gate:
- EditTool tests pass
- file edits render correctly in transcript and session flow

---

## Step 15 — Full CLI / RPC / onboarding

Implement:
- default interactive mode
- print mode
- minimal versioned RPC v1
- `--version`
- `--no-tools`
- onboarding that prefers local providers first
- hidden API-key input
- core slash commands

Gate:
- all entry modes work
- onboarding does not expose secrets in plaintext
- RPC protocol is stable enough for editor integration later

---

## Step 16 — Hardening

Implement:
- retry/backoff
- graceful shutdown
- process cleanup
- structured logging
- cross-platform validation
- release profiles

Gate:
- Linux/macOS/Windows builds are validated
- terminal restoration is reliable
- provider failures do not crash the app

---

## Step 17 — Only after v1.0

Then consider:
- Google provider if it slipped
- GitHub Copilot OAuth
- extensions
- `memory_write`
- richer RPC surface
- branch-selection UX

---

## Recommended first coding slice

If you want the shortest path to “something real works”, do this exact mini-sequence first:

1. workspace
2. protocol
3. provider traits + mock provider
4. agent loop
5. read/write/bash
6. OpenAI-compatible provider
7. Ollama manual config
8. CLI harness

That gets you to a functioning local coding harness as fast as possible.

---

## Stop conditions

Pause implementation and re-plan if any of these happen:

- the provider trait starts needing incompatible exceptions for one provider
- the TUI starts owning config/auth/session logic again
- session persistence drifts back toward render-event-based writes
- compaction starts guessing message-to-entry identity without preserved entry IDs
- local-model support becomes second-class compared to cloud-only flows

If any of those reappear, fix the design before writing more code.
