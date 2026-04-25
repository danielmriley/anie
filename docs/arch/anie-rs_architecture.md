# anie architecture

This document is the source of truth for anie's current architecture. It
describes the design patterns, ownership boundaries, persistence formats, hot
paths, and known structural risks that should guide future code review and
refactor planning.

It describes the code as it exists today, not a target architecture. When the
implementation changes, update this document in the same PR.

## Architectural goals

anie is a local Rust coding-agent harness. Its core design is deliberately
small:

- keep provider, tool, protocol, persistence, UI, and controller concerns in
  separate crates;
- route all model interaction through a normalized streaming provider contract;
- route all model-requested actions through a small tool contract;
- persist conversation state as append-only JSONL so sessions can be resumed,
  forked, compacted, and audited;
- keep the TUI responsive while provider streams, tool execution, compaction,
  and retry backoff are active;
- avoid stringly typed provider recovery logic by using the structured
  `ProviderError` taxonomy.

The project currently chooses simplicity over isolation: tools intentionally run
in the current process user's security context without sandboxing or approval
prompts. Relative paths resolve from the session cwd, but absolute paths and
parent traversal are allowed. WASM/containerized tool execution is a future
isolation direction, not current behavior.

## Workspace map

| Crate | Responsibility | Must not own |
|---|---|---|
| `anie-cli` | CLI parsing, mode dispatch, onboarding commands, controller state machine, retry policy, runtime-state persistence, provider/tool/session wiring | Rendering internals, provider wire formats, tool implementation details |
| `anie-tui` | Ratatui/crossterm UI state, input handling, overlays, slash-command UX, transcript rendering, model/provider picker surfaces | Agent orchestration, session mutation, provider calls |
| `anie-agent` | Provider/tool-agnostic agent loop, tool-call validation/execution, streaming event normalization into `AgentEvent` | Persistence, retry policy, auth storage, UI rendering |
| `anie-provider` | Provider traits, model metadata, request options, normalized provider events, structured provider error taxonomy | Built-in HTTP implementations, credential lookup |
| `anie-providers-builtin` | Built-in provider implementations, SSE/HTTP helpers, local-server probing, model discovery, built-in model catalog helpers | Controller policy, session persistence, credential storage |
| `anie-tools` | Built-in tools and file-mutation serialization (`read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`) | Agent loop policy, UI rendering, sandbox policy beyond its path behavior |
| `anie-protocol` | Shared message/content/tool/event/usage/stop-reason types | Persistence policy, provider-specific HTTP conversion |
| `anie-session` | Append-only session file schema, file locking, context reconstruction, branch/fork support, compaction records | Config merging, auth, provider calls |
| `anie-config` | Config schema, global/project config loading and merging, context-file discovery, comment-preserving config mutation | Secret storage, session history |
| `anie-auth` | Credential storage/resolution, OAuth provider clients, OAuth refresh locking, request-option resolution | Provider streaming, model selection policy |
| `anie-integration-tests` | Cross-crate behavior tests | Runtime code |

The compile-time graph is intentionally layered:

```text
anie-cli
  -> anie-agent
  -> anie-auth
  -> anie-config
  -> anie-provider
  -> anie-providers-builtin
  -> anie-session
  -> anie-tools
  -> anie-tui

anie-agent
  -> anie-provider
  -> anie-protocol

anie-auth
  -> anie-config
  -> anie-provider
  -> anie-protocol

anie-config
  -> anie-provider

anie-providers-builtin
  -> anie-provider
  -> anie-protocol

anie-session
  -> anie-protocol

anie-tools
  -> anie-agent
  -> anie-protocol

anie-tui
  -> anie-protocol
```

No lower-level crate should depend upward on `anie-cli` or `anie-tui`.

## Runtime modes

`anie-cli` dispatches into three runtime modes:

| Mode | Entry | Shape |
|---|---|---|
| Interactive TUI | default when no prompt is supplied | Spawns the `InteractiveController` and `anie-tui::App`; user input flows to the controller through an unbounded UI-action channel, and agent/controller events flow back through an `mpsc::Sender<AgentEvent>`. |
| Print mode | `--print` or a prompt argument | Runs a single prompt, streams text to stdout, and exits. It uses the same provider/tool/session building blocks without the ratatui event loop. |
| RPC mode | `--rpc` | JSONL over stdin/stdout for editor/tool integrations. It is an integration surface, not a plugin host. |

First-run onboarding and top-level commands (`onboard`, `models`, `login`,
`logout`) are handled before the mode dispatch.

## End-to-end control flow

```text
User input
  -> anie-tui InputPane / slash-command handling
  -> UiAction sent to anie-cli InteractiveController
  -> session/config/runtime state mutation, if needed
  -> AgentLoop::run(prompts, context, event_tx, cancel)
  -> AuthResolver resolves per-request options
  -> ProviderRegistry selects a Provider from Model.api
  -> Provider::stream returns normalized ProviderEvent stream
  -> AgentLoop collects AssistantMessage and emits AgentEvent deltas
  -> tool calls are validated through ToolRegistry and executed
  -> generated assistant/tool messages return in AgentRunResult
  -> controller persists generated messages to anie-session
  -> anie-tui renders AgentEvent updates and returns to idle
```

The controller owns retries. The agent loop returns structured terminal
provider errors in `AgentRunResult`; `anie-cli::retry_policy::RetryPolicy`
decides whether to retry, compact, or give up.

## Controller and TUI boundary

`anie-cli::InteractiveController` is the sole owner of agent orchestration in
interactive mode. It is responsible for:

- spawning and cancelling agent runs;
- receiving `UiAction` values;
- appending user prompts, generated messages, model changes, thinking changes,
  compactions, forks, and labels to the session log;
- applying session overrides when sessions are resumed, switched, or forked;
- managing retry backoff and context-overflow compaction retries;
- emitting `AgentEvent` values to the TUI.

`anie-tui::App` owns UI-only state:

- `OutputPane`, `InputPane`, status bar, overlays, picker state, spinner state,
  terminal rendering, and clipboard integration;
- slash-command presentation and pre-dispatch validation;
- conversion of terminal events into `UiAction`;
- conversion of `AgentEvent` into visible transcript/status changes.

The TUI must not mutate sessions directly or call providers/tools directly. The
controller must not know how a transcript block is rendered.

### Focused controller handles

`ControllerState` is intentionally decomposed into focused handles:

- `ConfigState` owns loaded config, runtime defaults, current model, current
  thinking level, and CLI API-key override.
- `SessionHandle` owns the active `SessionManager`, sessions directory, and
  session cwd.
- `SystemPromptCache` owns loaded system/context prompt text and context-file
  staleness tracking.

New long-lived controller state should usually be introduced as another focused
handle, not as a loose field on `ControllerState`.

### Retry state invariant

The controller uses a `PendingRetry` state machine rather than sleeping inline.
That lets the main loop keep polling `ui_action_rx` while a retry backoff is
armed. Ctrl+C, quit, and other actions remain responsive during backoff.

Current review risk: model/thinking changes are accepted while a retry is
armed, and the continuation run is built from the latest controller state. A
future fix should either cancel the retry when run-affecting settings change or
reject those changes until the retry resolves.

## Agent loop contract

`anie-agent::AgentLoop` is provider/tool agnostic. Each run receives owned
prompt and context vectors, immutable `AgentLoopConfig`, a sender for
`AgentEvent`, and a cancellation token.

Agent-loop invariants:

- Context is mutable only inside `AgentLoop::run`; callers persist the returned
  generated messages after the run.
- Provider lookup is by `Model.api`; absence of a registered provider is a
  request/build failure.
- Per-request auth/routing is resolved immediately before the provider call
  through `RequestOptionsResolver`.
- The loop sanitizes replay context based on provider/model replay
  capabilities before converting messages.
- Tool definitions are provided to the model from `ToolRegistry` in
  deterministic name order.
- Tool-call arguments are validated against precompiled JSON Schema validators
  before `Tool::execute`.
- Tool execution may be sequential or parallel according to `ToolExecutionMode`.
- Cancellation is propagated by `CancellationToken`; tools receive child tokens
  and should return `ToolError::Aborted` or an equivalent terminal result when
  cancelled.
- The loop does not decide retry policy. It returns terminal provider errors to
  the caller.

`AgentEvent` is the in-process event protocol between agent/controller and UI.
It carries run/turn/message lifecycle events, streaming deltas, tool execution
events, transcript replacement, status updates, compaction notifications, and
retry scheduling notices.

## Provider contract

Providers implement `anie_provider::Provider`:

```rust
fn stream(
    &self,
    model: &Model,
    context: LlmContext,
    options: StreamOptions,
) -> Result<ProviderStream, ProviderError>;

fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage>;
fn includes_thinking_in_replay(&self) -> bool;
fn convert_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value>;
```

Provider implementations must:

- convert protocol messages and tools into the provider-native request shape;
- return a normalized `ProviderStream` of `ProviderEvent` values;
- preserve opaque replay fields required by the provider (thinking signatures,
  redacted reasoning, encrypted reasoning details);
- classify failures into `ProviderError` variants instead of relying on string
  matching in callers;
- avoid blocking the async runtime in streaming paths;
- never own long-lived credentials. Secrets enter through `StreamOptions` and
  request-specific headers returned by `RequestOptionsResolver`.

`ProviderError` is a core architectural boundary. Retry/recovery decisions must
match on variants such as `RateLimited`, `Transport`, `ContextOverflow`,
`ReplayFidelity`, `ResponseTruncated`, or `Auth`; callers should not parse
human-readable error strings.

### Built-in provider registration

`anie-providers-builtin::register_builtin_providers` currently registers:

- `ApiKind::AnthropicMessages` -> `AnthropicProvider`
- `ApiKind::OpenAICompletions` -> `OpenAIProvider`

OpenRouter, Ollama, LM Studio, and custom OpenAI-compatible endpoints are routed
through model catalog/config/base-url behavior on top of those provider shapes,
not through separate registered provider trait objects.

## Tool contract

Tools implement `anie_agent::Tool`:

```rust
fn definition(&self) -> ToolDef;

async fn execute(
    &self,
    call_id: &str,
    args: serde_json::Value,
    cancel: CancellationToken,
    update_tx: Option<mpsc::Sender<ToolResult>>,
) -> Result<ToolResult, ToolError>;
```

Tool responsibilities:

- expose a JSON Schema in `ToolDef`;
- accept already-validated JSON arguments;
- honor cancellation;
- return a structured `ToolResult` or `ToolError`;
- send bounded partial updates through `update_tx` when useful;
- implement its own resource/time limits where the operation can grow large.

The built-in tool registry is assembled in `anie-cli::bootstrap` with:

- `read`
- `write`
- `edit`
- `bash`
- `grep`
- `find`
- `ls`

`write` and `edit` share a `FileMutationQueue` so file mutations are serialized.

### Bash deny policy

`BashTool` supports a pre-spawn deny policy configured under
`[tools.bash.policy]`:

- `enabled` toggles the guardrail.
- `deny_commands` blocks simple command names and basenames such as `rm` or
  `/bin/rm`.
- `deny_patterns` blocks regular expressions matched against the raw command
  string before the shell process is spawned.

This is an accidental-risk guardrail, not a security boundary. It is useful for
user preferences like "never run `rm`" or "never force-push", but it is
bypassable via shell indirection, scripts, interpreters, or future non-bash
tools. Do not use it as a substitute for the future WASM/containerized
tool-execution layer.

### Filesystem safety boundary

Tools currently run without sandboxing by design. Path helpers join relative
paths to the session cwd but preserve absolute paths, and lexical `..`
traversal is not confined to cwd. The README warning is therefore the
authoritative safety boundary: anie can read, write, edit, list, search, and
execute commands anywhere the process user has access.

Future isolation work should be designed as a separate tool-execution layer.
The preferred direction is WASM/containerized tool execution rather than
quietly changing today's path resolver into a partial sandbox.

## Protocol and replay fidelity

`anie-protocol` defines the stable in-process shapes:

- `Message`: user, assistant, tool-result, and custom messages;
- `ContentBlock`: text, image, thinking, redacted thinking, and tool calls;
- `ToolDef` / `ToolResult`;
- `AgentEvent` and streaming deltas;
- usage/cost/stop-reason types.

Replay fidelity is a design requirement. Provider-emitted opaque blocks that are
required on the next turn must be persisted and replayed verbatim. Examples:

- Anthropic thinking signatures on thinking blocks;
- Anthropic `redacted_thinking` blocks;
- OpenRouter encrypted reasoning details on assistant messages.

If a provider stream contains a block anie cannot round-trip safely, the correct
behavior is to fail with `ProviderError::UnsupportedStreamFeature` or
`ProviderError::ReplayFidelity`, not to silently drop or stringify the block.

Adding protocol fields or variants that are persisted in sessions requires:

- serde defaults for optional new fields;
- `skip_serializing_if = "Option::is_none"` for optional fields;
- a `CURRENT_SESSION_SCHEMA_VERSION` bump when session semantics change;
- a migration/forward-compat test for older sessions.

## Session persistence

`anie-session` stores sessions as append-only JSONL files under
`~/.anie/sessions/*.jsonl`.

The first line is a `SessionHeader`:

```json
{"type":"session","version":4,"id":"...","timestamp":"...","cwd":"...","parent_session":null}
```

Subsequent lines are `SessionEntry` values:

- `message`
- `compaction`
- `model_change`
- `thinking_change`
- `label`

Each entry has an id, optional parent id, and timestamp. The parent links form
the active branch used for resume/fork/history reconstruction.

Current schema version:

| Version | Change |
|---|---|
| 1 | Baseline session format |
| 2 | Optional thinking signatures and redacted thinking blocks |
| 3 | Optional assistant `reasoning_details` for OpenRouter encrypted reasoning replay |
| 4 | Optional `CompactionDetails` on compaction entries |

Session invariants:

- Session files are append-only; history is not rewritten for normal operation.
- `SessionManager` takes an exclusive advisory lock for the lifetime of an open
  session.
- If the same session is already open, opening it again returns
  `SessionError::AlreadyOpen`; the CLI tells the user to close the other
  process, fork, or start a new session.
- Filesystems without advisory-lock support degrade to a warning.
- Context reconstruction walks the active branch, applies the latest compaction
  cutoff, and materializes the messages needed for the next request.
- Compaction entries may carry `CompactionDetails` with deduplicated read and
  modified file lists from the discarded interval.

Session files are the audit trail. Every user-visible run-affecting state
change should be represented in the session log, not only in runtime state.

## Config and runtime state

`anie-config` owns TOML config:

- global config: `~/.anie/config.toml`
- project config: nearest `.anie/config.toml` found by walking upward
- context files: defaults to `AGENTS.md` and `CLAUDE.md`, also found by walking
  upward with per-file and total-byte caps

Config load order:

1. built-in defaults;
2. global config;
3. project config;
4. CLI overrides.

Merge rules:

- partial config structs use `Option` so absent fields do not overwrite earlier
  layers;
- any explicit `[model]` file section sets `model_explicitly_set`, preventing
  last-used runtime state from overriding a declared default;
- provider sections overlay by provider name;
- a present provider `models` array replaces that provider's earlier custom
  model list;
- CLI overrides are unconditional and applied last.

`~/.anie/state.json` is runtime state, not config. It stores last-used
non-secret selections such as provider, model, thinking level, and last session.
Failures to persist runtime state are currently logged but not surfaced to the
user; the session log remains the stronger audit record.

Config and auth writes use an atomic temp-file-plus-rename helper. This is crash
safe on POSIX, but it is not a multi-writer lock and has known Windows
replacement caveats. Config mutation assumes a single writer.

## Authentication and request options

`anie-auth` owns credential resolution and OAuth refresh.

Resolution order:

1. CLI `--api-key`;
2. structured stored credential from the JSON store, including OAuth refresh
   when needed;
3. flat API key from native keyring/credential storage;
4. configured provider `api_key_env`;
5. built-in provider environment variable;
6. no credential.

`AuthResolver` implements `RequestOptionsResolver`, so the agent loop can ask
for request-specific API keys, headers, and base-url overrides immediately
before each provider call.

Credential storage is hybrid:

- native keyring is used for API-key style secrets when available;
- `~/.anie/auth.json` remains the structured JSON fallback and is required for
  OAuth credentials because refresh tokens and provider metadata must round-trip;
- migrated legacy JSON files are preserved as `~/.anie/auth.json.migrated`.

OAuth refresh uses a per-provider fs4 lock under an auth lock directory. The
refresh path follows a double-check pattern: read credential, check expiry,
acquire lock, re-check, refresh only if still needed, persist, unlock.

Current review risk: refresh lock acquisition currently polls with blocking
sleep from an async request path. Move it to `spawn_blocking` or an async sleep
loop before increasing concurrent OAuth usage.

## TUI rendering and responsiveness

The TUI is a hot path during streaming. The current design uses:

- `RenderDirty` to separate full redraws from urgent input-only redraws;
- streaming text accumulation and line caching in `OutputPane`;
- markdown rendering for finalized blocks, not for actively streaming blocks;
- spinner tick debouncing;
- stall-aware redraw suppression when no streaming delta has arrived recently;
- transcript replacement events for session switches/resumes instead of
  piecemeal replay.

Responsiveness invariant: keyboard input should not wait behind expensive
transcript rebuilds when only the composer changed.

Current review risk: the TUI drains all queued agent events into a batch before
returning to terminal input. Under a very large stream/tool burst, this can
starve input. A future refactor should bound event draining by event count or
time budget per frame while preserving delta coalescing.

## Performance-sensitive seams

| Seam | Current strategy | Risk to watch |
|---|---|---|
| Tool definitions | Sorted/cached at registration in `ToolRegistry` | Do not re-sort on every provider request. |
| Tool schemas | JSON Schema validators precompiled at registration | Invalid schemas stay registered and fail on first use with preserved error text. |
| Provider streaming | Normalized `ProviderEvent` stream | Preserve opaque replay fields; do not flatten unsupported provider blocks. |
| TUI output | Cached wrapped lines by width/markdown state | Streaming blocks should avoid per-frame markdown parsing. |
| Retry backoff | `PendingRetry` in controller select loop | Run-affecting UI changes during backoff need explicit policy. |
| Session persistence | Append-only JSONL with advisory lock | Avoid multi-writer use of one session; fork for branches. |
| Config/auth writes | Atomic temp file and rename | Not a concurrent-write lock; Windows replacement semantics need hardening. |

## Error handling patterns

- Provider failures use `ProviderError`; retry policy lives in `anie-cli`, not
  provider implementations.
- Tool failures use `ToolError` and are normally converted into tool-result
  messages for the model unless cancellation or loop termination applies.
- User-command failures should be classified and surfaced as system messages so
  typos do not terminate the session.
- Fatal internal failures should terminate the controller rather than being
  hidden behind success-shaped fallbacks.
- Do not add broad catches or string-matching recovery paths where a typed error
  variant should exist.

## Known architectural risks

These risks are intentionally listed here so future refactors and reviews can
check whether they are still true.

| Risk | Area | Why it matters | Preferred direction |
|---|---|---|---|
| Full filesystem access is intentional | Tools | Agents can access any path the process user can access. | Keep tool docs explicit until a dedicated WASM/containerized execution layer exists. |
| Blocking OAuth refresh lock in async path | Auth | Lock contention can occupy Tokio worker threads. | Move polling to `spawn_blocking` or use async sleep around short lock attempts. |
| OpenAI-compatible image support can be advertised while images are flattened to text | Providers/protocol | Image-capable models may silently receive placeholders instead of image content. | Implement OpenAI multimodal content arrays or mark those models unsupported for images. |
| Model/thinking changes during pending retry | Controller | Retry continuation may run with settings different from the failed attempt. | Cancel retry on run-affecting changes or reject those changes while armed. |
| OAuth callback accepted-socket read lacks timeout | Auth/login | Local idle connection can hang login beyond overall deadline. | Apply deadline to read/write after accept. |
| Runtime-state persistence failure only logs | CLI/config | UI can report success while next launch reverts last-used settings. | Return a warning/result and surface it non-fatally. |
| POSIX-oriented atomic write | Config/auth | Windows CI exists; rename-over-existing and temp-name collision behavior need hardening. | Add platform-specific replace semantics and unique temp names. |
| Unbounded edit batch arguments | Tools | Malformed model output can allocate large vectors/strings. | Add schema and runtime caps. |
| Unbounded TUI event drain | TUI | Large bursts can starve input processing. | Bound per-frame event count or elapsed processing time. |
| Large monolithic session module | Session | Schema, storage, context, compaction, listing, and tests collide in one file. | Split into schema/storage/context/compaction/listing modules when session work resumes. |

## Refactor rules of thumb

Use these rules during future cleanup:

1. Keep ownership local. If a crate owns persistence, no other crate should
   mutate that file format directly.
2. Add typed variants before adding string parsing. This is especially important
   for provider recovery behavior.
3. Preserve replay fidelity over convenience. Opaque provider fields should be
   stored and replayed verbatim or rejected explicitly.
4. Keep TUI hot paths allocation-aware. Avoid full transcript rebuilds for input
   edits or streaming ticks.
5. Treat session entries as audit records. If a setting affects a run, decide
   whether it belongs in the session log.
6. Prefer focused handles over expanding controller state with unrelated fields.
7. Do not add a new dependency when an existing workspace dependency covers the
   same role.
8. When adding persisted optional fields, add serde defaults and update session
   schema/version tests.
9. Validate tool resource bounds at both schema and runtime layers.
10. Keep docs and README summaries aligned with actual registrations in
    `bootstrap.rs` and `register_builtin_providers`.

## Validation gates

The normal workspace validation gate is:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Documentation-only changes do not need the full gate unless they modify tested
examples or code snippets, but architecture changes should still be reviewed
against the current code before being treated as authoritative.

## Related docs

- [`credential_resolution.md`](credential_resolution.md) - deeper auth-flow
  notes.
- [`onboarding_flow.md`](onboarding_flow.md) - first-run and provider-management
  TUI flow.
- [`../anie_vs_pi_comparison.md`](../anie_vs_pi_comparison.md) - comparison with
  pi.
- [`../pi_adoption_plan/`](../pi_adoption_plan/) - prioritized pi-adoption
  plans.
- [`../code_review_performance_2026-04-21/`](../code_review_performance_2026-04-21/)
  performance review plans.
