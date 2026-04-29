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
| `anie-cli` | CLI parsing, mode dispatch, onboarding commands, controller state machine, retry policy, runtime-state persistence, slash-command registry, provider/tool/session wiring | Rendering internals, provider wire formats, tool implementation details |
| `anie-tui` | Ratatui/crossterm UI state, active-draft input, overlays, slash-command UX, autocomplete, transcript rendering, model/provider picker surfaces | Agent orchestration, session mutation, provider calls |
| `anie-agent` | Provider/tool-agnostic agent loop, REPL step machine (`AgentRunMachine`), `BeforeModelPolicy` hook, tool-call validation/execution, streaming event normalization into `AgentEvent` | Persistence, retry policy, auth storage, UI rendering |
| `anie-provider` | Provider traits, model metadata, request options, normalized provider events, structured provider error taxonomy | Built-in HTTP implementations, credential lookup |
| `anie-providers-builtin` | Built-in provider implementations, SSE/HTTP helpers, local-server probing, model discovery, built-in model catalog helpers | Controller policy, session persistence, credential storage |
| `anie-tools` | Built-in tools and file-mutation serialization (`read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`) | Agent loop policy, UI rendering, sandbox policy beyond its path behavior |
| `anie-tools-web` | Default-feature web tools (`web_read`, `web_search`), HTTP fetch/search boundaries, robots/rate-limit/SSRF handling, optional headless rendering | Core filesystem tools, controller orchestration, provider calls |
| `anie-protocol` | Shared message/content/tool/event/usage/stop-reason types | Persistence policy, provider-specific HTTP conversion |
| `anie-session` | Append-only session file schema, file locking, context reconstruction, branch/fork support, compaction records | Config merging, auth, provider calls |
| `anie-config` | Config schema, global/project config loading and merging, context-file discovery, web/Ollama/UI/tool config, comment-preserving config mutation | Secret storage, session history |
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
  -> anie-tools-web (default `web` feature)
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

anie-tools-web
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
  -> AgentLoop::start_run_machine -> AgentRunMachine
       (emits AgentStart, initial TurnStart, prompt MessageStart/End)
  -> machine.next_step() per REPL iteration:
       Read   — BeforeModelPolicy hook; resolve request options;
                look up provider; sanitize replay context; build LlmContext
       Eval   — Provider::stream  -> collect_stream emits MessageStart/Delta/End live
                or execute_tool_calls -> ToolExec* emitted live
       Print  — commit observation to AgentRunState; emit boundary events
                (TurnEnd, follow-up TurnStart) not already emitted live
       Decide — produce next AgentIntent: ModelTurn | ExecuteTools |
                AppendFollowUps | RunCompactionGate | Finish
  -> machine.finish() -> AgentRunResult (tail AgentEnd emission)
  -> controller persists generated messages to anie-session
  -> anie-tui renders AgentEvent updates and returns to idle
```

The agent loop is a Read → Eval → Print → Decide REPL driver, exposed
publicly as `AgentRunMachine`. Each step evaluates one bounded
`AgentIntent` and produces one `AgentObservation`; the dispatcher then
calls `decide_next_step` synchronously to choose the next intent.
`AgentLoop::run` is a thin run-to-completion wrapper that print mode,
RPC mode, and integration tests still use; the interactive controller
drives the same machine through `run_via_step_machine`, so the seam is
in place for future PRs that want to interpose policy at step
boundaries (queued-prompt folding, proactive compaction, verifier
loops). Streaming events stay live: `MessageStart/Delta/End` are emitted
during `Eval` from inside `collect_stream`, not buffered until `Print`.

The first cross-step extension point is `BeforeModelPolicy`, called in
the `Read` phase of every `ModelTurn`. The default install is
`NoopBeforeModelPolicy`; a custom policy can return
`AppendMessages(Vec<Message>)` to inject *context-only* messages
(`AgentRunState::append_policy_context`) that do not appear in
`AgentRunResult::generated_messages` and are therefore not persisted by
the controller. Future hooks (after-model, on-tool-error, etc.) get
their own traits when real consumers materialize. Plan series:
`docs/repl_agent_loop/`.

Each run is wrapped in a tracing `agent_run` span keyed by a UUID
`run_id`; each REPL iteration gets a child `agent_repl_step` span with
`run_step` (monotonic), `intent`, `context_messages`, and
`generated_messages` fields; phase methods carry `agent_eval`,
`agent_print`, and `agent_decide` spans. None of this changes the
public `AgentEvent` protocol — observability lives entirely in tracing.

The controller owns run-level retries. The agent loop returns
structured terminal provider errors in `AgentRunResult`;
`anie-cli::retry_policy::RetryPolicy` decides whether to retry, compact,
or give up. Step-level policy lives behind the `AgentRunMachine`
boundary; run-level policy stays in the controller.

The current interactive model is still single-run: an active provider/tool loop
must finish or be aborted before another provider request starts. The TUI can,
however, keep an editable draft while the agent is running. Pressing Enter in an
active state sends `UiAction::QueuePrompt`; the controller appends that prompt to
an in-memory FIFO and starts it after the current run's assistant/tool messages
have been persisted. Queued prompts are not written to the session until they
actually start, preserving transcript order.

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
  thinking level, native Ollama context-length overrides, and CLI API-key
  override.
- `SessionHandle` owns the active `SessionManager`, sessions directory, and
  session cwd.
- `SystemPromptCache` owns loaded system/context prompt text and context-file
  staleness tracking.
- `CommandRegistry` owns slash-command metadata for `/help`, validation,
  autocomplete, and dispatch coupling.

New long-lived controller state should usually be introduced as another focused
handle, not as a loose field on `ControllerState`.

### Retry state invariant

The controller uses a `PendingRetry` state machine rather than sleeping inline.
That lets the main loop keep polling `ui_action_rx` while a retry backoff is
armed. Ctrl+C, quit, and other actions remain responsive during backoff.

Run-affecting changes now have explicit retry policy:

- model and thinking changes are rejected while a run is active;
- model and thinking changes cancel an armed pending retry before the next
  continuation can start;
- `/context-length` mutation is rejected while a run is active or a retry is
  armed, because changing native Ollama `num_ctx` would alter the retry's
  request shape;
- a queued follow-up prompt supersedes a stale armed retry and starts a fresh
  run instead.

These rules keep retries responsive without silently changing the failed
request's settings under the user.

## Agent loop contract

`anie-agent::AgentLoop` is provider/tool agnostic. Each run receives owned
prompt and context vectors, immutable `AgentLoopConfig`, a sender for
`AgentEvent`, and a cancellation token. Internally the loop is a Read →
Eval → Print → Decide REPL driver — see `AgentRunMachine` and
"End-to-end control flow" above.

Run state is held in a private `AgentRunState`:

- `context: Vec<Message>` — prompts plus every generated, follow-up,
  steering, and policy-injected message. This is what the next provider
  request sees.
- `generated_messages: Vec<Message>` — assistants and tool results
  only. Excludes prompts, follow-ups, steering, and policy injections.
  This is the controller's session-persistence input.
- `terminal_error: Option<ProviderError>`, `finished: bool`,
  `step_index: u64`, `suppress_tail_agent_end: bool` — driver
  bookkeeping.

Helpers enforce the dual-append rule for content the controller
persists (`append_assistant`, `append_tool_results`) versus the
context-only rule for runtime injections (`extend_context`,
`append_policy_context`).

Agent-loop invariants:

- Context is mutable only inside `AgentRunMachine`/`AgentLoop::run`;
  callers persist the returned generated messages after the run.
- Each REPL step evaluates exactly one bounded `AgentIntent`
  (`ModelTurn`, `ExecuteTools`, `AppendFollowUps`, `RunCompactionGate`,
  `Finish`) and produces one `AgentObservation`.
- Streaming events (`MessageStart`/`Delta`/`End`, `ToolExec*`) are
  emitted live during `Eval`, not buffered until `Print`.
- Provider lookup is by `Model.api`; absence of a registered provider is a
  request/build failure.
- Per-request auth/routing is resolved immediately before the provider call
  through `RequestOptionsResolver`.
- `BeforeModelPolicy` is consulted in the `Read` phase of every
  `ModelTurn` step; the default `NoopBeforeModelPolicy` always returns
  `Continue`. Policy-injected messages are context-only and are never
  persisted as agent output.
- Per-run native Ollama `num_ctx` overrides are snapshotted from
  `ConfigState` into `AgentLoopConfig`, then copied to
  `StreamOptions::num_ctx_override` for the provider call. The provider remains
  stateless.
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
- `AgentEnd` is emitted exactly once per run: at the tail of
  `AgentRunMachine::finish` for the common path, or inline via
  `finish_with_assistant` for preflight failures (resolver error,
  missing provider, stream-init error) which set
  `state.suppress_tail_agent_end` so the tail no-ops.
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
- `ApiKind::OllamaChatApi` -> `OllamaChatProvider`

`ApiKind::OpenAIResponses` and `ApiKind::GoogleGenerativeAI` exist as protocol
enum variants for planned providers but do not have registered providers today.

OpenRouter, LM Studio, and custom OpenAI-compatible endpoints are routed through
model catalog/config/base-url/compat behavior on top of
`OpenAICompletions`. Ollama has two paths:

- legacy OpenAI-compatible configs (`api = "OpenAICompletions"`, usually
  `base_url = ".../v1"`) continue to work, but cannot honor Ollama-native
  `think` or `options.num_ctx`;
- native configs/discovery (`api = "OllamaChatApi"`, root base URL) use
  `/api/chat`, stream Ollama NDJSON, send `think` for reasoning-capable models,
  and send `options.num_ctx` from `StreamOptions::num_ctx_override` or
  `Model.context_window`.

Native Ollama support is anie-specific; pi uses Ollama's OpenAI-compatible
endpoint.

### Model metadata and compat knobs

`Model` is the routing and request-parameter source of truth. It carries the
provider id, model id, `ApiKind`, base URL, context window, max output tokens,
image/reasoning support, replay requirements, pricing, and per-family compat
metadata.

Important current compat behavior:

- OpenAI-compatible user messages keep the legacy string `content` shape for
  text-only turns, but switch to ordered `text` / `image_url` content parts
  when an image block is present. User-side thinking blocks become text parts in
  that mixed-content shape; redacted thinking remains provider-opaque and is
  dropped for OpenAI-compatible backends.
- OpenRouter catalog/discovery entries can attach routing preferences and
  `max_tokens` wire-name selection through `ModelCompat::OpenAICompletions`.
- Native Ollama discovery reads `/api/tags` + `/api/show`, maps capabilities
  such as `thinking` and `vision`, and propagates discovered context lengths
  into `Model.context_window`.
- `[ollama] default_max_num_ctx` is an optional workspace-wide cap applied to
  native Ollama context windows at catalog-load time. `/context-length <N>` is a
  per-model runtime override that wins over this cap until reset.

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

The built-in tool registry is assembled in `anie-cli::bootstrap`. The core
tool set is always available unless `--no-tools` is passed:

- `read`
- `write`
- `edit`
- `bash`
- `grep`
- `find`
- `ls`

`write` and `edit` share a `FileMutationQueue` so file mutations are serialized.

The default `anie-cli` build enables the `web` feature and also registers:

- `web_read`
- `web_search`

Lean builds can compile those out with `--no-default-features`. The
`web-headless` feature additionally enables `web_read(javascript=true)` through
Chrome/Chromium.

### Tool resource boundaries

Tool limits are guardrails, not an isolation layer:

- `read` caps text output by lines/bytes and caps image reads by bytes.
- `edit` caps edit count, per-edit text sizes, total edit argument bytes, input
  file bytes, and output file bytes before writing.
- `grep` and `bash` share line/byte truncation helpers for large outputs.
- `bash` has timeout/cancellation support plus the optional deny policy below.
- Web tools enforce configurable fetch timeouts, page byte caps, redirect caps,
  bounded error/robots bodies, cancellation checks, and per-host rate limiting.

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

### Web tool network boundary

`anie-tools-web` is a separate crate because network fetch/search has a
different risk profile from local filesystem tools. It exposes typed
`WebToolError` variants rather than stringly errors and registers through the
same `ToolRegistry` as the core tools.

Default non-headless `web_read` / `web_search` behavior:

- allows only `http` and `https` URLs;
- rejects private, loopback, link-local, mDNS/local, and internal-looking
  destinations unless `[tools.web].allow_private_ips = true`;
- resolves hostnames before requests and rejects private resolved IPs;
- disables automatic redirects and manually validates every redirect target
  before following it;
- honors `robots.txt` for `web_read`;
- streams bodies with configured byte caps instead of buffering unbounded
  responses;
- shares a per-host rate limiter across `web_read` and `web_search`.

Known limitation: the resolved-IP check has a small DNS TOCTOU between
validation and reqwest's actual connection. The headless Chrome path validates
the initial URL but cannot currently intercept every browser redirect or
subresource, so it is intentionally documented as a weaker boundary.

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

Session files are the audit trail. Every run-affecting change that must be
reconstructable during resume/fork, such as user prompts, generated messages,
model changes, thinking changes, and compactions, belongs in the session log
rather than only in runtime state.

## Config and runtime state

`anie-config` owns TOML config:

- global config: `~/.anie/config.toml`
- project config: nearest `.anie/config.toml` found by walking upward
- context files: defaults to `AGENTS.md` and `CLAUDE.md`, also found by walking
  upward with per-file and total-byte caps
- UI preferences: slash-command popup, finalized-message markdown rendering,
  and successful `bash`/`read` tool-output display mode
- tool preferences: bash deny policy and web fetch budgets/network policy
- Ollama preferences: optional workspace-wide native `num_ctx` cap

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
non-secret selections such as provider, model, thinking level, last session, and
per-model native Ollama `num_ctx` overrides keyed by `"{provider}:{model_id}"`.

Runtime state is convenience state. The session log remains the stronger audit
record for run-affecting history. User-visible settings such as model and
thinking changes are also appended to the session log; `/context-length`
overrides are runtime preferences that affect future requests but are not a
session entry today. Persistence failures on user-command mutation paths are
surfaced as non-fatal warnings where the command can keep the setting active for
the current session; bootstrap/session-maintenance persistence failures are
logged.

Config, auth, and runtime-state writes use an atomic temp-file-plus-rename
helper with PID + in-process counter temp names. This is crash safe on POSIX,
but it is not a multi-writer lock. Windows builds are explicitly compile-gated
until the helper grows `ReplaceFileW`-style replacement semantics. Config
mutation assumes a single writer.

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

Lock acquisition is isolated with `tokio::task::spawn_blocking`; the blocking
fs4 polling loop does not occupy a Tokio worker thread.

## TUI rendering and responsiveness

The TUI is a hot path during streaming. The current design uses:

- `RenderDirty` to separate full redraws from urgent input-only redraws;
- streaming text accumulation and line caching in `OutputPane`;
- markdown rendering for finalized blocks, not for actively streaming blocks;
- configurable markdown rendering for finalized assistant messages;
- compact/verbose successful tool-output display modes for noisy `bash` and
  `read` results;
- inline slash-command autocomplete and controller-sourced command metadata;
- editable active drafts and FIFO queued follow-up prompts while a run is
  active;
- spinner tick debouncing;
- stall-aware redraw suppression when no streaming delta has arrived recently;
- transcript replacement events for session switches/resumes instead of
  piecemeal replay.
- bounded per-frame agent event draining (`MAX_AGENT_EVENTS_PER_FRAME = 256`)
  with adjacent text/thinking delta coalescing.

Responsiveness invariant: keyboard input should not wait behind expensive
transcript rebuilds when only the composer changed.

The TUI no longer drains an unbounded number of agent events per frame. The cap
is set to the observed saturated channel burst size, and coalescing preserves
streaming efficiency without starving terminal input indefinitely.

## Performance-sensitive seams

| Seam | Current strategy | Risk to watch |
|---|---|---|
| Tool definitions | Sorted/cached at registration in `ToolRegistry` | Do not re-sort on every provider request. |
| Tool schemas | JSON Schema validators precompiled at registration | Invalid schemas stay registered and fail on first use with preserved error text. |
| Provider streaming | Normalized `ProviderEvent` stream | Preserve opaque replay fields; do not flatten unsupported provider blocks. |
| TUI output | Cached wrapped lines by width/markdown state | Streaming blocks should avoid per-frame markdown parsing. |
| Retry backoff | `PendingRetry` in controller select loop | Keep run-affecting changes explicit: cancel, reject, or supersede according to command semantics. |
| Session persistence | Append-only JSONL with advisory lock | Avoid multi-writer use of one session; fork for branches. |
| Config/auth/state writes | Atomic temp file and POSIX rename | Not a concurrent-write lock; Windows replacement semantics are compile-gated until implemented. |
| Web fetches | Manual redirect validation, DNS private-IP checks, byte caps | Headless Chrome path has weaker network controls; DNS validation has a small TOCTOU. |

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
| Windows compile gate for atomic writes | Config/auth/state | `anie-config` intentionally refuses Windows builds until replace-over-existing semantics are implemented safely. | Add a `cfg(windows)` `ReplaceFileW`-style implementation before claiming Windows support. |
| Runtime state is not an audit log | CLI/config | `/context-length` and last-used selections persist as convenience state, not session entries; crashes before persistence can lose unlogged preferences. | Keep run-affecting audit data in sessions; add session entries only when a setting must be replayable as history. |
| Headless web fetch boundary is weaker than non-headless | Web tools | Chrome owns redirects/subresources, so anie cannot currently validate every network hop under `javascript=true`. | Prefer non-headless fetch; add browser request interception before treating headless as equivalent. |
| DNS validation TOCTOU | Web tools | A hostname can theoretically resolve differently between anie's preflight check and reqwest's connection. | Use a custom reqwest resolver/connector if this becomes a product security boundary. |
| External Defuddle dependency | Web tools | `web_read` extraction depends on `defuddle` or `npx defuddle` being available at runtime. | Keep the error typed/actionable; consider vendoring or a pure-Rust extractor only if deployment friction warrants it. |
| Active drafts are not persisted | TUI/controller | Drafts and queued prompts are in-memory only; a crash loses unstarted follow-ups. | Persist drafts only if product requirements grow beyond the current single-run queue. |
| Provider enum has reserved unimplemented variants | Providers | `OpenAIResponses` and `GoogleGenerativeAI` are protocol variants without registered providers, so selecting them fails provider lookup. | Add providers through the provider-addition process, with explicit replay/wire-shape tests. |
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
cargo build --release
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo +1.85 check --workspace --all-targets
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
- [`../ollama_native_chat_api/`](../ollama_native_chat_api/) - native Ollama
  `/api/chat` implementation plan.
- [`../ollama_context_length_override/`](../ollama_context_length_override/) -
  runtime `/context-length` design.
- [`../ollama_default_num_ctx_cap/`](../ollama_default_num_ctx_cap/) -
  workspace-wide native Ollama context cap.
- [`../web_tool_2026-04-26/`](../web_tool_2026-04-26/) and
  [`../code_review_2026-04-27/`](../code_review_2026-04-27/) - web tool design
  and hardening plans.
- [`../active_input_2026-04-27/`](../active_input_2026-04-27/) - active-draft
  and queued-follow-up design.
