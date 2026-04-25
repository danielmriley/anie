# anie vs pi: functional comparison

A thorough comparison of two coding-agent harnesses, surveying each
piece of functionality anie has today against pi's equivalent. Focused
on differences that matter for the anie roadmap — what pi does that we
don't, what we do that pi doesn't, and where our approaches diverge in
small ways worth knowing.

Commit baseline: anie on `feat/provider-compat-blob` through
`1207ccf` (max_tokens/PR3). pi on whatever is currently in
`/home/daniel/Projects/agents/pi` (JavaScript codebase, no pinned
commit in this doc).

## Top-level take

- **Language and runtime.** anie is Rust + tokio; pi is TypeScript on
  Node. Both single-threaded. Both async.
- **Layout philosophy.** anie cleanly separates
  protocol/agent/session/provider/cli as isolated crates with
  near-zero cross-crate leakage. pi co-locates types with providers
  (all of `ai/` is one package) and ships a separate
  `coding-agent` package that owns session management, tools, and
  extensions.
- **Breadth.** pi supports more wire protocols (15 Api values vs
  anie's 4 ApiKinds), more tools (7 vs 4 ships built-in), richer
  markdown/image rendering, and OAuth. anie ships live model
  discovery, stricter structured errors, richer replay fidelity
  (thinking signatures + reasoning_details), an onboarding preset
  system, and a cleaner session-replay contract.
- **Convergence.** Schema version 3 on both projects (accidental).
  Same compaction defaults (16 k reserve, 20 k keep-recent). Same
  chars/4 token heuristic. Same request-based render loop pattern
  (PR 1 of tui_responsiveness copied pi's design).

## Workspace layout + protocol types

### Crate / package structure

**anie** (Rust) — 12 crates in `/crates/`:

| Crate | Role |
|-------|------|
| `anie-protocol` | Pure types, serde only, no runtime deps |
| `anie-provider` | Provider trait, `ApiKind`, model metadata |
| `anie-providers-builtin` | Anthropic / OpenAI / OpenRouter impls |
| `anie-agent` | Agent loop, tool traits, hooks |
| `anie-tools` | Built-in tools (read/write/edit/bash) |
| `anie-session` | JSONL persistence, schema v3, compaction primitives |
| `anie-config` | TOML config, compaction settings |
| `anie-auth` | Credential store, env resolver |
| `anie-cli` | Main binary, controller, retry policy |
| `anie-tui` | ratatui TUI |
| `anie-integration-tests` | Cross-crate invariants |
| (build infra crates) | tooling |

**pi** (TypeScript) — 7 packages in `/packages/`:

| Package | Role |
|---------|------|
| `ai` (`@mariozechner/pi-ai`) | Protocol types **and** provider adapters |
| `agent` (`@mariozechner/pi-agent-core`) | Agent loop library |
| `coding-agent` (`@mariozechner/pi-coding-agent`) | Controller, session, tools, compaction, extensions, print-mode, TUI wiring |
| `tui` | Custom terminal UI framework |
| `mom`, `pods`, `web-ui` | Out-of-scope web concerns |

**Key difference.** anie separates the pure data layer (`anie-protocol`)
from runtime, provider-specific logic, session IO, etc. pi's `ai`
package co-locates types with provider code, which saves ceremony on
small changes but makes it harder to say "this file has no network
opinion."

### Core protocol types

*Message roles.* anie at `crates/anie-protocol/src/messages.rs:8`:
`User`, `Assistant`, `ToolResult`, `Custom`. pi at
`packages/ai/src/types.ts:193`: `UserMessage | AssistantMessage |
ToolResultMessage`, with apps extending via TypeScript declaration
merging (no explicit Custom variant).

*ContentBlock.* anie has five variants (`content.rs:6`): `Text`,
`Image`, `Thinking { thinking, signature? }`, `RedactedThinking`,
`ToolCall`. pi has four
(`packages/ai/src/types.ts:147`): `TextContent`, `ThinkingContent
{ thinking, thinkingSignature?, redacted? }`, `ImageContent`,
`ToolCall { ..., thoughtSignature? }`. pi folds redacted thinking
into a flag on `ThinkingContent` rather than a separate variant;
anie keeps them distinct.

*StopReason.* anie has four values (`Stop`, `ToolUse`, `Error`,
`Aborted`). pi has five — anie is missing `"length"` which we
route through the new `ProviderError::ResponseTruncated` at the
error layer instead.

*Usage.* Both structured. anie nests a `Cost` struct; pi flat.

*AgentEvent.* anie's (`events.rs:5`) is richer with 11 variants
including `StatusUpdate` (context tokens, cwd, model, session id),
`CompactionStart/End`, `RetryScheduled`, `TranscriptReplace`. pi's
(`packages/agent/src/types.ts:334`) has nine variants; no status
update, no compaction events surfaced to the UI layer.

### Schema versioning

Both projects are coincidentally on schema version 3.
anie at `crates/anie-session/src/lib.rs:83`
(`CURRENT_SESSION_SCHEMA_VERSION = 3`) documents the evolution in
comments and enforces a load-time forward-compat gate. pi at
`packages/coding-agent/src/core/session-manager.ts:28`
(`CURRENT_SESSION_VERSION = 3`) runs in-place migrations on load
but does not narrate why each version exists — the evolution rationale
isn't committed to the code.

## Provider abstraction + streaming

### Shape of the abstraction

anie uses a trait (`Provider` at
`crates/anie-provider/src/provider.rs:33`) with three methods:
`stream()`, `convert_messages()`, `convert_tools()`. Wire protocols
are enumerated by `ApiKind` (`AnthropicMessages`, `OpenAICompletions`,
`OpenAIResponses`, `GoogleGenerativeAI`). Impls register statically.

pi uses per-module function exports. Each provider in
`packages/ai/src/providers/` exports `streamXxx` and
`streamSimpleXxx` conforming to the `StreamFunction<Api>` type
(`types.ts:135`). Lazy registration via
`registerApiProvider()` in `providers/register-builtins.ts:366`. pi
supports 15 Api values vs anie's 4, including
`mistral-conversations`, `bedrock-converse-stream`,
`google-vertex`, `openai-codex-responses`, `azure-openai-responses`.

### Streaming state machine

anie centralizes SSE parsing in `OpenAiStreamState`
(`crates/anie-providers-builtin/src/openai/streaming.rs:47`) — a
dedicated state machine with `BTreeMap` tool-call tracking, opaque
`reasoning_details` accumulation for OpenRouter replay, and a
terminal `Done` event carrying the fully-assembled
`AssistantMessage`.

pi inlines SSE parsing into each provider's `stream` function. The
OpenAI completions parser (`openai-completions.ts:135`) buffers
`currentBlock` and finalizes on block-type change or
`finish_reason`. Tool arguments accumulate in a `partialArgs`
string parsed at `toolcall_end`. No centralized reusable state
machine, no `reasoning_details` preservation.

### Per-vendor compat knobs

anie has one compat variant (`ModelCompat::OpenAICompletions`
carrying `OpenAICompletionsCompat`) which currently holds only
`openrouter_routing: Option<OpenRouterRouting>` (5 fields:
`allow_fallbacks`, `order`, `only`, `ignore`, `zdr`). Upstream-aware
capability inference lives in a dedicated
`openrouter.rs` module that dispatches on id prefix.

pi has ~13 compat knobs inlined on the `Model` type
(`packages/ai/src/types.ts:407`): `supportsStore`,
`maxTokensField` ("max_tokens" vs "max_completion_tokens"),
`thinkingFormat` (4 variants: `"openai"`, `"openrouter"`, `"zai"`,
`"qwen-chat-template"`), `requiresToolResultName`,
`supportsDeveloperRole`, `supportsUsageInStreaming`,
`reasoningEffortMap`, two routing schemas (OpenRouter + Vercel
Gateway) with ~11 OpenRouter fields including `quantizations`,
`sort`, `max_price`, `preferred_min_latency`.

**Gap:** pi's compat is significantly more granular. Things we don't
model yet that could matter: `maxTokensField` (some OpenAI-compatible
servers want one or the other exclusively), `thinkingFormat`
variants (zai, qwen-chat-template — two local-model reasoning wire
formats we haven't hit yet), `supportsDeveloperRole` (OpenAI's
`developer` message role for reasoning models).

### Reasoning handling

anie has `ThinkingLevel` with four values (`Off`, `Low`, `Medium`,
`High`). Per-model request mode via `ThinkingRequestMode`
(`PromptSteering`, `ReasoningEffort`, `NestedReasoning`) chosen at
catalog time. Reasoning field normalization happens centrally in
`native_reasoning_delta` — checks `reasoning`,
`reasoning_content`, `reasoning_text`, `thinking`.

pi has `ThinkingLevel` with five values — the extra `"minimal"` is a
GPT-5-family nuance we may want later. Per-provider compat handles
reasoning shape (`thinkingFormat` flag + `reasoningEffortMap`).
Field normalization done inline in each provider. No
`reasoning_details` replay concept; the opaque OpenRouter
encrypted-reasoning round-trip is unique to anie.

### Error taxonomy

anie has 14 structured `ProviderError` variants
(`crates/anie-provider/src/error.rs:13`). Each variant routes through
`RetryPolicy::decide` unambiguously: `Auth`, `RateLimited
{ retry_after_ms }`, `ContextOverflow`, `ResponseTruncated`,
`EmptyAssistantResponse`, `ReplayFidelity { provider_hint, detail }`,
`NativeReasoningUnsupported`, `FeatureUnsupported`,
`UnsupportedStreamFeature`, `ToolCallMalformed`,
`InvalidStreamJson`, `MalformedStreamEvent`, `Http { status, body }`,
`Transport`, `RequestBuild`.

pi has no structured error type. Errors surface as `Error` objects
with `.message` strings. Retry logic uses a regex against those
strings (`agent-session.ts:2399`:
`overloaded|provider returned error|rate limit|429|500|502|503|504|
... |timeout|terminated|retry delay`). `stopReason: "error"` is
the only terminal signal.

**This is a large win for anie.** The structured taxonomy is why our
retry policy can do things like "rate-limit retries capped at 1
regardless of max_retries" and "ResponseTruncated is terminal but
`EmptyAssistantResponse` gets its own actionable message." Adding
that to pi would require introducing a typed error layer across
every provider; anie had it from day one.

## Agent loop + tools + retry

### Main agent loop

anie implements the loop explicitly in
`crates/anie-agent/src/agent_loop.rs::AgentLoop::run` (line 314).
Build request → stream → collect tool calls → execute tools →
emit events → repeat. Termination is local:
(1) model responds with no tool calls, (2) provider error reaches
terminal, or (3) `cancel.is_cancelled()` fires mid-turn (line 553).
Max-turns is not enforced — budget management happens via
compaction (input-shrinking), not output capping.

pi delegates to an external `@mariozechner/pi-agent-core` library.
The `AgentSession` in `packages/coding-agent/src/core/agent-session.ts`
wraps it and owns event emission, persistence, and retry. Flow:
`prompt()` queues → `agent.prompt(messages)` fires →
`waitForRetry()` blocks until retry completes or agent idles. The
Agent library itself decides termination.

**Architectural difference.** anie owns the loop inline and composes
from primitives (provider, tool, hooks, cancel token). pi builds on
a reusable `pi-agent-core` library so loop policy is somewhat
further from the app. Easier to swap the loop in pi; easier to
inspect / trace in anie.

### Tool system

anie ships four built-in tools: `read`, `write`, `edit`, `bash`
(`crates/anie-tools/src/`). `Tool` trait at
`crates/anie-agent/src/tool.rs:11` — `definition()` returns schema,
`execute()` takes call id, validated JSON args, a `CancellationToken`,
and an optional `mpsc::Sender<ToolResult>` for partial updates.
Sequential or parallel execution is configurable via
`AgentLoopConfig::tool_execution`.

pi ships seven: `read`, `bash`, `edit`, `write`, `grep`, `find`,
`ls` (`packages/coding-agent/src/core/tools/index.ts:110`).
Tools implement `AgentTool<T>` from pi-agent-core with signature
`execute(toolCallId, args, signal?, onUpdate?, ctx?)`. Partial
updates via `onUpdate()` callback (bash streams rolling-buffer
output — `bash.ts:328`).

**Gap for anie:** `grep`, `find`, `ls` are three missing search /
discovery tools. The agent can shell out via `bash` but a
first-class grep tool with structured output is faster to
iterate on.

### Hooks / middleware

anie has explicit `BeforeToolCallHook` / `AfterToolCallHook` traits
(`crates/anie-agent/src/hooks.rs`). Hooks can block or override
tool results.

pi has no built-in hooks. Control flows through an extension
system (`ExtensionRunner` at `agent-session.ts:52`) that wraps
tools and emits lifecycle events. More flexible (extensions are
arbitrary code); less typed (no explicit
`Block { reason }` / `Override { result }` surface).

### Retry policy

anie: `crates/anie-cli/src/retry_policy.rs::RetryPolicy::decide` is
a structured `match` on `ProviderError`. Transient errors retry up
to `max_retries` (default 3); rate limits are capped at 1 retry
regardless (15 s fallback delay if no `Retry-After`); context
overflow triggers compaction then retries once; terminal errors
give up.

pi: regex-based `_isRetryableError` (`agent-session.ts:2399`).
Exponential backoff `baseDelayMs * 2^(attempt - 1)`. No rate-limit-
specific cap. Context overflow excluded from regex because
compaction handles it separately.

**anie's structured approach is strictly more actionable.** pi's
regex string-matches risk false positives and can't distinguish
rate-limit-without-`Retry-After` from 429-with-advertised-delay.

### Cancellation

anie: `tokio_util::sync::CancellationToken`. Propagated via
`cancel.child_token()` into tool execution so a per-tool abort
doesn't kill siblings. Mid-stream abort reported as
`StopReason::Aborted`.

pi: standard `AbortSignal`/`AbortController`. Separate controllers
for compaction, auto-compaction, branch-summary, retry, and bash.
Functionally equivalent.

## Session persistence + compaction

### File format

Both JSONL, header-first, schema version 3.

anie (`crates/anie-session/src/lib.rs`): five entry types —
`Message`, `Compaction`, `ModelChange`, `ThinkingChange`, `Label`.
Each carries `id`, `parentId` (tree), `timestamp`.

pi (`packages/coding-agent/src/core/session-manager.ts`): nine
entry types including the above plus `custom`, `custom_message`,
`branch_summary`, `session_info`. Extensions can register custom
entries as first-class citizens.

**pi is more extensible; anie is stricter.** Both run forward-compat
gates on load; anie documents the schema evolution, pi doesn't.

### Context assembly

anie: `SessionManager::build_context()` walks root → leaf,
emitting `SessionContextMessage` tuples. Compaction entries
inject a synthetic summary message and emit kept messages from
`firstKeptEntryId`. Opaque fields (thinking signatures,
reasoning_details) survive because messages are cloned directly.

pi: `buildSessionContext()` similar but reconstructs messages via
factory helpers (`createCompactionSummaryMessage`,
`createCustomMessage`, `createBranchSummaryMessage`). Message
construction depends on factory semantics — opaque fields are
preserved only when the factory explicitly handles them.

**Replay fidelity: anie is more conservative by default.**
Direct cloning can't lose a field without changing the Message
definition itself; factory-based reconstruction can silently drop
new fields if the factory isn't updated.

### Compaction

Both projects ship near-identical compaction config defaults: 16 k
`reserve_tokens`, 20 k `keep_recent_tokens`, enabled-by-default.
Both trigger when `context_tokens > context_window - reserve_tokens`.

anie's compaction is linear: find a cut point walking backward from
leaf, accumulating tokens until `>= keep_recent_tokens`, avoiding
tool-result boundaries. The discarded prefix gets summarized; the
kept suffix is verbatim.

pi's compaction adds **split-turn handling**
(`packages/coding-agent/src/core/compaction/compaction.ts:715`): if
the cut lands mid-turn, pi generates two summaries in parallel — main
history + turn prefix — and concatenates with `---`. More recent
context is preserved across the cut.

pi also tracks **file operations** in compaction details (extension
output: which files were read/modified during the summarized
interval). anie does not preserve any structured details across
compaction.

**Gap for anie.** Split-turn summarization and file-operation
provenance are both worth copying. Neither is urgent but both
reduce information loss on long sessions.

### Token estimation

Both use chars/4 heuristic. pi goes one step further:
`calculateContextTokens` checks the assistant's `usage.totalTokens`
from the LLM response when available, falls back to heuristic.
anie always estimates.

**Small gap.** Using provider-reported usage is strictly more
accurate. One-line fix whenever an assistant message carries usage.

### Fork / branch

Both support leaf-pointer branching. pi adds:

- `branchWithSummary(branchFromId, summary)` — appends a
  `BranchSummaryEntry` summarizing the abandoned path with file
  operations preserved.
- `createBranchedSession(leafId)` — extracts one root-to-leaf path
  into a fresh session file with parent link.
- Branch-leaving triggers automatic summarization of the walk
  between leaves (`branch-summarization.ts`).

anie has `fork` and `fork_to_child_session` but no automatic
branch summarization.

## Model catalog, discovery, auth, config

### Catalog

anie: compile-time hardcoded `builtin_models()` (5 entries). Live
models come from discovery or user config. Discovery is the primary
growth surface.

pi: `packages/ai/src/models.generated.ts` is autogenerated from a
build-time script. Ships ~50+ models across 8+ providers. Updates
require regenerating and publishing a new `pi-ai` version.

### Discovery

**Asymmetry that matters.** anie has live `/v1/models` discovery
for OpenAI-compatible and Anthropic endpoints
(`crates/anie-providers-builtin/src/model_discovery.rs`). Parses
OpenRouter's `top_provider.max_completion_tokens`,
`supported_parameters`, `architecture`, pricing. TTL cache.
Tool-supporting filter on OpenRouter (drops ~half of 500+ entries
that are completion-only / image-gen).

pi: **no live discovery.** Relies on the generated catalog +
`models.json` overrides. Cannot dynamically expand to new
OpenRouter models without a regenerate.

This is one of the clearest pure-anie wins in this survey.

### Auth

Both use `auth.json` at mode 0600. Neither uses an OS keyring today
(anie has the `keyring` crate as a dep but does not invoke it;
pi has no keyring at all).

pi adds **OAuth** with automatic refresh, locking to prevent
concurrent-refresh races (`packages/coding-agent/src/core/auth-storage.ts:369`).
Relevant for Claude Code-style auth flows that anie has not shipped.

Priority order:

- anie: CLI flag > credential store > `api_key_env` > default env var.
- pi: runtime override > API key > OAuth (auto-refresh) > env var
  > fallback resolver.

### Config files

anie: TOML at `~/.anie/config.toml`. Sections for `model`,
`providers`, `compaction`, `context`. Human-first, TOML-style flat.

pi: JSON. `~/.pi/agent/models.json`, `auth.json`, `settings.json`.
Typebox-validated schemas. Machine-first, deep nesting (compat blobs
live directly in provider config).

### Provider presets

**anie-only.** `provider_presets()` in the onboarding overlay
hardcodes Anthropic, OpenAI, OpenRouter, xAI, Groq, Together,
Fireworks, Mistral. pi has no onboarding/preset system; users
configure `models.json` by hand.

## TUI + CLI UX

### Rendering architecture

Both use request-based rendering with frame-budget pacing. We
modeled anie's PR 1 (render scheduling) directly on pi's
`requestRender` / `scheduleRender`. anie caps at 30 fps (33 ms),
pi at 60 fps (16 ms) — pi can afford the higher rate because it
has finer-grained per-component caching.

anie caches lines per-block in a parallel `Vec<Option<LineCache>>`
keyed by `(width, lines)`
(`crates/anie-tui/src/output.rs`, PR 2 of tui_responsiveness). pi
caches on each individual component by `(text, width) -> lines`
(`packages/tui/src/components/markdown.ts:87`). pi's component-
local cache is marginally more fine-grained; anie's block-local is
simpler and covers the same hot path.

Both do differential rendering — pi explicitly (first/last-changed
detection in its own DOM diff); anie implicitly (ratatui's `Buffer`
cell-level diff). We considered porting pi's line-level diff in
PR 3 of tui_responsiveness and deliberately skipped because ratatui
already does it at the cell level.

### Component inventory

pi ships a meaningfully richer widget set:

| Component | anie | pi |
|-----------|------|----|
| Text / wrapped text | ✓ | ✓ |
| Select list | ✓ | ✓ |
| Text field / input | ✓ | ✓ |
| Fuzzy search | ✓ | ✓ |
| Overlay / panel | ✓ | ✓ |
| **Markdown renderer** | ✗ | ✓ |
| **Inline image (Kitty/iTerm2)** | ✗ | ✓ |
| **Settings list** | ✗ | ✓ |
| **Truncated text** | partial | ✓ |
| **Cancellable loader** | ✗ | ✓ |

### Markdown rendering

pi's `Markdown` component (`packages/tui/src/components/markdown.ts`)
uses `marked` to parse headings, lists, blockquotes, tables, code
blocks (with optional syntax highlighting via a theme-supplied
`highlightCode` function), and OSC 8 hyperlinks in compatible
terminals.

anie outputs plain text with ratatui span styling. Code blocks,
tables, inline links are all rendered as raw text.

**This is the biggest UX gap.** A full markdown renderer in anie
would bring parity and make the agent's output meaningfully more
readable.

### Input handling

anie (`crates/anie-tui/src/input.rs`): multiline editing, history,
saved-content-on-cancel, optional autocomplete. No undo, no
kill-ring, no paste-as-atomic-block.

pi (`packages/tui/src/editor-component.ts`,
`packages/tui/src/undo-stack.ts`, `packages/tui/src/kill-ring.ts`):
pluggable editor (vim/emacs mode possible), undo via structured-clone
snapshots, Emacs-style kill-ring with yank-pop, paste markers
(`[paste #1 +123 lines]`) that move atomically under cursor nav,
grapheme-cluster-aware segmentation.

### Slash commands

Both have declarative slash-command metadata + autocomplete. anie's
`SlashCommandInfo` (`crates/anie-tui/src/commands.rs`) explicitly
distinguishes Builtin / Extension / Prompt / Skill sources for
future extensibility. pi's is simpler but integrates through its
extension system (commands-as-extensions).

### Terminal capability detection

pi explicitly detects Kitty, Ghostty, WezTerm, iTerm2, VS Code,
Alacritty and disables image/hyperlink sequences under tmux/screen
(`packages/tui/src/terminal-image.ts:40`). anie has no capability
probing — we assume a generic xterm and render plain text.

### Print / non-interactive mode

Equivalent on both sides. anie: `crates/anie-cli/src/print_mode.rs`.
pi: `packages/coding-agent/src/modes/print-mode.ts`. Both filter
events to text (or JSON, in pi's case) and suppress TUI.

## What pi does that anie doesn't

Ordered by impact for the anie roadmap:

1. **Markdown rendering** in the TUI. Biggest visible UX gap.
2. **Inline image rendering** (Kitty/iTerm2 protocols). Smaller
   but useful for tools that produce images.
3. **Broader provider support.** 15 Api variants vs 4. Notables
   absent from anie: Bedrock, Vertex, Mistral native, Azure,
   google-gemini-cli, openai-codex-responses.
4. **OAuth token auth** with refresh locking. Needed for Claude
   Code style auth.
5. **More built-in tools** (`grep`, `find`, `ls`).
6. **Split-turn compaction summarization.** Less information loss
   on mid-turn cuts.
7. **File-operation tracking** in compaction entries.
8. **Branch summarization** when navigating off a branch.
9. **Richer compat knobs** per model (`maxTokensField`,
   `thinkingFormat` variants, `supportsDeveloperRole`,
   `supportsStore`).
10. **More thinking levels** (`"minimal"` on top of low/medium/high/xhigh).
11. **Sophisticated input editor** (undo, kill-ring, paste
    markers, grapheme-aware cursor).
12. **Terminal capability probing.**

## What anie does that pi doesn't

Ordered by how defensible / unique:

1. **Live `/v1/models` discovery** with OpenRouter-rich fields.
   pi is stuck on its generated catalog.
2. **Structured `ProviderError` taxonomy** — 14 typed variants
   routed through a `match`-based retry policy. pi uses regex on
   error messages.
3. **`ResponseTruncated` vs `EmptyAssistantResponse`** distinction.
   pi can't tell truncation from "model ran out of ideas."
4. **`ReplayFidelity` error variant** with `provider_hint`. No
   equivalent in pi.
5. **Rate-limit-specific retry cap** (1 attempt, 15 s fallback
   delay). pi applies the same retry policy to everything.
6. **Onboarding preset system** — 8 presets ship in the TUI.
7. **Explicit hook traits** (`BeforeToolCallHook`,
   `AfterToolCallHook`) with Block/Override/Continue outcomes.
8. **`ThinkingRequestMode` as a typed per-model attribute** with
   three variants (PromptSteering, ReasoningEffort,
   NestedReasoning). pi uses a string `thinkingFormat` flag.
9. **`reasoning_details` replay for OpenRouter encrypted
   reasoning.** Unique to anie today; required for
   `openai/o*` multi-turn on OpenRouter.
10. **Thinking signature + redacted-thinking preservation** as
    separate `ContentBlock` variants. Replay fidelity is strict.
11. **Cleaner protocol/runtime separation.** `anie-protocol` has
    no network dependencies; pi's `ai` package mixes them.
12. **Render-frame counter + `ANIE_DEBUG_REDRAW=1`** instrumentation.
    pi has `PI_DEBUG_REDRAW=1` but only logs full-redraws; anie
    logs every frame with elapsed ms.

## Convergences (same idea, small differences)

1. Schema version 3 on both (accidental).
2. Same compaction defaults: 16 k reserve, 20 k keep-recent.
3. Same chars/4 token estimation heuristic.
4. Same request-based TUI render scheduling pattern (anie copied
   from pi explicitly).
5. Both use JSONL for sessions.
6. Both use `auth.json` at mode 0600.
7. Both ignore `max_tokens` on the main agent path (anie does so
   as of commit `32232b2`).
8. Both support branching via leaf-pointer moves.
9. Both emit `TurnStart` / `TurnEnd`-shaped events for agent
   lifecycle.
10. Both expose slash commands with declarative metadata + autocomplete.

## Suggested next moves for anie

Prioritized by signal — these are differences above where adopting
pi's approach would pay off proportional to the cost:

- **Ship a markdown renderer** for the TUI. Visible immediately,
  one new widget.
- **Port pi's split-turn compaction** and file-operation tracking.
  Reduces information loss on long sessions.
- **Adopt `maxTokensField` compat flag.** Some OpenAI-compatible
  servers 400 on the "wrong" field — cheap to support before it
  becomes a user bug report.
- **Add `"minimal"` to `ThinkingLevel`.** GPT-5 family supports
  it; shipping now avoids a retrofit later.
- **Ship `grep` / `find` / `ls` as first-class tools.** Faster
  iteration than shelling through `bash`.
- **OAuth support for Claude Code-style login.** Required for any
  provider that doesn't offer an API key.
- **Provider-reported usage for token estimation.** One-line
  accuracy improvement on compaction triggering.
- **Terminal capability detection** before emitting image /
  hyperlink escape sequences.

Not worth copying:

- pi's 15 Api variants. anie gets most of them via OpenRouter
  routing; Bedrock / Vertex / Azure aren't priorities.
- pi's regex-based error classification — we have something
  strictly better.
- pi's component-local cache — our block-local cache is simpler
  and covers the same ground under ratatui.

## References

- anie: `/home/daniel/Projects/agents/anie`
- pi: `/home/daniel/Projects/agents/pi`
- anie branch at time of writing: `feat/provider-compat-blob`
- Prior comparison notes in `docs/add_providers/pi_comparison.md`
  (scope: OpenRouter-specific behaviors only).
- This document's companion plan: `docs/max_tokens_handling/README.md`
  (which is the template for how other ports-from-pi can be
  organized — evidence → plan → staged PRs).
