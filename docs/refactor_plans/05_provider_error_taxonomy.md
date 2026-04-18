# Plan 05 — Provider error taxonomy

> **Status (2026-04-18):** complete on `refactor_branch` (commit
> `3f8393c`). Landed as a single commit across 14 files, as the
> plan required — partial migration would have broken the build at
> the removed `Other` variant. Non-test call sites now use typed
> variants; `classify_openai_http_error` is the single place where
> error-body string detection still lives, and it upgrades 400
> responses whose body looks like native-reasoning rejection to
> `NativeReasoningUnsupported`. Test assertions use `matches!` on
> variants rather than `.contains(...)` on messages.

## Motivation

`ProviderError` (in `crates/anie-provider/src/error.rs`) currently
has variants `Auth`, `RateLimited`, `ContextOverflow`, `Http`,
`Request`, `Stream`, `Other`, `Response`. Call sites use them
inconsistently:

- `openai.rs:950` emits
  `ProviderError::Stream("empty assistant response")`, a sentinel
  string that callers pattern-match on.
- `ProviderError::Request` and `ProviderError::Other` absorb a wide
  range of unrelated causes.
- Retry-decision code in `controller.rs` inspects error message
  strings (`error.to_string().contains("empty")`) rather than
  variants.
- JSON parse errors inside the stream loop are flattened into
  `ProviderError::Stream` with the cause stringified.
- 55 `ProviderError::*` construction sites across 11 files — some
  consistency drift is inevitable.

Result: error handling is partly a string API. Callers can't
exhaustively match; tests assert on message text; new call sites
pick a variant by feel.

## Design principles

1. **Variants describe cause, not stringified detail.** If callers
   need to distinguish two situations, they should be two variants.
2. **Retryability is not in the error type.** It's a property
   derived by `RetryPolicy` (plan 03, phase 4). Errors stay
   descriptive; retry decisions stay in one place.
3. **HTTP errors carry status + body.** Enough to diagnose but not
   so much that tests pin brittle text.
4. **No `Other`.** If a situation genuinely doesn't fit any variant,
   add one. `Other` encourages "just pick this."
5. **Migration is mechanical.** This plan is largely call-site
   rewrites; the logic doesn't change.

## Preconditions

Plan 01 should land first (so the OpenAI call sites are localized
across several small files rather than concentrated in a 2000-LOC
file). It's not strictly required, but the diff is cleaner after 01.

---

## Phase 1 — Redesign `ProviderError`

**Goal:** New enum shape; deprecate `Other`; add specific variants.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-provider/src/error.rs` | Rewrite enum; derive `thiserror::Error` cleanly |
| `crates/anie-provider/src/lib.rs` | Update re-exports if needed |

(Just 2 files; trivial.)

### Sub-step A — Target shape

```rust
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// 401/403 from upstream, or auth missing.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// 429 from upstream. `retry_after` is the server-suggested
    /// wait, if it was present.
    #[error("rate limited (retry after {retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },

    /// Server-reported context-length exceeded. Distinct from other
    /// 400s because the retry strategy is compaction.
    #[error("context window exceeded")]
    ContextOverflow,

    /// Any other HTTP non-2xx. Body is captured verbatim (capped).
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },

    /// Transport failure (DNS, TLS, connect, I/O).
    #[error("transport error: {0}")]
    Transport(String),

    /// The stream ended with no visible assistant content. This is
    /// the case `reasoning_fix_plan.md` Phase 1 Sub-step B turns on.
    #[error("empty assistant response")]
    EmptyAssistantResponse,

    /// SSE frame could not be parsed as JSON.
    #[error("invalid stream JSON: {0}")]
    InvalidStreamJson(String),

    /// SSE frame parsed but was shaped wrong (missing required
    /// field, unknown event type we must honor, etc.).
    #[error("malformed stream event: {0}")]
    MalformedStreamEvent(String),

    /// Tool-call arguments were not valid JSON when the stream
    /// finished.
    #[error("tool call arguments not valid JSON: {0}")]
    ToolCallMalformed(String),

    /// The provider reported a native reasoning feature the model
    /// does not support. Used by OpenAI's compatibility-error
    /// detection (`is_native_reasoning_compatibility_error`).
    #[error("native reasoning not supported by target: {0}")]
    NativeReasoningUnsupported(String),

    /// Request body construction failed before sending.
    #[error("request build error: {0}")]
    RequestBuild(String),
}
```

Deleted / renamed:

- `Other` — gone.
- `Request` — split into `RequestBuild` (pre-send) and `Transport`
  (send-side).
- `Response` — renamed as appropriate; most call sites should become
  `Http { .. }` or `InvalidStreamJson(..)`.
- `Stream(String)` — split into `EmptyAssistantResponse`,
  `InvalidStreamJson`, `MalformedStreamEvent`,
  `ToolCallMalformed`, `NativeReasoningUnsupported`.

### Sub-step B — Keep a conversion shim if needed

If external crates pattern-match on `ProviderError` (unlikely — it's
internal), add `#[deprecated]` aliases. Otherwise just rewrite.

### Files that must NOT change yet

- Callers — they're the next phase.

### Exit criteria

- [ ] New enum compiles.
- [ ] `thiserror::Error` derives cleanly (no manual `Display`).
- [ ] No `Other` variant.
- [ ] `Stream(String)` is split.

---

## Phase 2 — Migrate OpenAI call sites

**Goal:** Every `ProviderError::*` construction in
`crates/anie-providers-builtin/src/openai*` maps to one of the new
variants.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/openai/streaming.rs` *(post-plan-01)* | Replace `Stream("empty...")` with `EmptyAssistantResponse`; replace JSON parse failures with `InvalidStreamJson`; tool-call issues with `ToolCallMalformed` |
| `crates/anie-providers-builtin/src/openai/mod.rs` | Replace HTTP classification results with the new variants |
| `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs` *(post-plan-01)* | `is_native_reasoning_compatibility_error` now checks for `NativeReasoningUnsupported` explicitly; emit that variant from the HTTP classification where the body matches the known pattern |
| `crates/anie-providers-builtin/src/util.rs` | Update `classify_http_error` to produce the new variants |

### Sub-step A — Catalogue current sites

Run `grep -rn 'ProviderError::' crates/anie-providers-builtin/src/openai*`
and list each site plus the intended new variant in the PR
description. This is an auditable record.

### Sub-step B — Migrate each site

For each site, pick the correct variant. Where a caller currently
reads `.to_string().contains("…")`, update both sides in the same
commit.

### Sub-step C — Native-reasoning compatibility error

Today `is_native_reasoning_compatibility_error` string-matches on
the error's `Display`. After this phase, it becomes:

```rust
pub fn is_native_reasoning_compatibility_error(error: &ProviderError) -> bool {
    matches!(error, ProviderError::NativeReasoningUnsupported(_))
}
```

The emission site — `classify_http_error` or the 400-response path
— is where the body text inspection moves. That's appropriate
because it's the boundary between raw upstream bytes and our typed
model.

### Test plan

| # | Test |
|---|------|
| 1 | Existing OpenAI unit tests (from plan 01) still pass; updated to match `matches!(err, ProviderError::EmptyAssistantResponse)` etc. |
| 2 | `classify_http_error_maps_401_to_auth` |
| 3 | `classify_http_error_maps_429_to_rate_limited_with_retry_after` |
| 4 | `classify_http_error_maps_context_length_body_to_context_overflow` (string detection in error bodies stays here, scoped to this one place) |
| 5 | `classify_http_error_maps_native_reasoning_body_to_unsupported` |

### Exit criteria

- [ ] Zero `ProviderError::Stream(_)` in `openai/*`.
- [ ] Zero `ProviderError::Other(_)` anywhere in the crate.
- [ ] `is_native_reasoning_compatibility_error` no longer strings.

---

## Phase 3 — Migrate Anthropic call sites

**Goal:** Same as phase 2, for `anthropic.rs`.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/anthropic.rs` | Migrate every `ProviderError::*` construction |

### Sub-step A — Catalogue and migrate

Same process as phase 2. Anthropic's surface is smaller (4 sites in
the file today), so the migration is quick.

### Exit criteria

- [ ] Zero `ProviderError::Stream(_)` in `anthropic.rs`.
- [ ] Anthropic-specific errors (e.g., `thinking_delta` parse
      failures) use `MalformedStreamEvent` or
      `InvalidStreamJson`.

---

## Phase 4 — Migrate `model_discovery.rs`

**Goal:** Discovery errors use the new taxonomy.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/model_discovery.rs` | Migrate 10 sites |

### Sub-step A — Rule set

- HTTP non-2xx → `Http` (discovery doesn't distinguish "this provider
  has a context window"; if it ever does, add variants).
- JSON parse failures → `InvalidStreamJson` is wrong; use a new
  `InvalidDiscoveryJson` variant, **or** extend
  `InvalidStreamJson` to `InvalidProviderJson { context: &'static
  str, detail: String }`. Pick whichever is cleaner. If you go with
  a generic variant, remove `InvalidStreamJson` in favor of it.

### Exit criteria

- [ ] No `ProviderError::Other(_)` in discovery code.
- [ ] Parse failures use a typed variant with enough context to
      diagnose.

---

## Phase 5 — Migrate callers in `anie-cli` and `anie-agent`

**Goal:** Retry and error-display sites consume the typed errors.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/retry_policy.rs` *(from plan 03 phase 4)* | `RetryPolicy::decide` pattern-matches on variants; no string `.contains(...)` |
| `crates/anie-cli/src/controller.rs` | Any inline retry/error-handling that remains uses `match` on `ProviderError` |
| `crates/anie-agent/src/agent_loop.rs` | Any error handling here uses variant match |
| `crates/anie-provider/src/mock.rs` | `MockProvider` produces typed errors in tests |
| `crates/anie-provider/src/tests.rs` | Update `assert!(matches!(..))` to use new variants |

### Sub-step A — Remove string matching

Every `.to_string().contains("…")` or `format!("{err}")`-then-search
is a bug after this phase; replace with a `match`.

### Sub-step B — Update mock

`MockProvider` should offer convenient constructors:

```rust
impl MockProvider {
    pub fn with_auth_error() -> Self;
    pub fn with_context_overflow() -> Self;
    pub fn with_empty_assistant_response() -> Self;
    // etc.
}
```

These let agent-level tests assert on retry behavior without
constructing full error strings.

### Test plan

| # | Test |
|---|------|
| 1 | `retry_policy_auth_gives_up` (using typed error) |
| 2 | `retry_policy_context_overflow_compacts` |
| 3 | `retry_policy_empty_assistant_response_retries_limited_times` |
| 4 | `agent_loop_handles_tool_call_malformed_as_terminal_assistant_error` |
| 5 | Existing integration tests pass. |

### Files that must NOT change

- `crates/anie-protocol/*` — wire format unchanged.
- `crates/anie-tui/*` — TUI consumes error *display* via
  `AgentEvent`, which is already string-valued for UI purposes.

### Exit criteria

- [ ] Zero `.to_string().contains(...)` over `ProviderError` in the
      workspace.
- [ ] All retry decisions come from typed matches.
- [ ] `MockProvider` exposes typed error constructors.

---

## Files that must NOT change in any phase

- `crates/anie-protocol/*` — errors are not serialized to session
  JSONL beyond their display form; no wire change.
- External `anyhow::Error` usage in CLI code — that's outside the
  `ProviderError` taxonomy.

## Dependency graph

```
Phase 1 (redesign enum)
  ├─► Phase 2 (OpenAI sites)
  ├─► Phase 3 (Anthropic sites)
  ├─► Phase 4 (discovery sites)
  └─► Phase 5 (CLI/agent consumers)
```

Phases 2–4 are independent; they just need phase 1's variants.
Phase 5 can land after any subset of 2–4, but is easiest after all
three so no mixed-variant error flows exist.

## Out of scope

- Error display / formatting for TUI — `AgentEvent::StatusUpdate`
  and friends keep their string payloads.
- Structured tracing attributes on errors — separate observability
  work.
- `thiserror` vs `anyhow` style debate in `anie-cli` — that crate
  keeps using `anyhow::Result<_>` for its own error surface.
