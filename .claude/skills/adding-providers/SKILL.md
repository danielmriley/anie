---
name: adding-providers
description: "Guide for adding a new LLM provider (Gemini, Bedrock, xAI, OpenAI Responses API, etc.) to the anie harness. Use this skill whenever the user asks to add, register, scaffold, or plan support for a new provider / backend / API family, or when extending the provider trait, catalog, streaming parser, or replay pipeline. Covers capability declaration on Model, ContentBlock extension, round-trip contract docs, error taxonomy, test harness invariants, and session schema bumps."
---

# Adding a New Provider to anie

This skill is the practical companion to
`docs/api_integrity_plans/` — the architecture plans that landed in
commits d968bf9 through 9d4b77e. Those plans established a
data-driven, capability-based pipeline so that adding a new provider
is a small, predictable set of edits rather than a refactor. Read the
plan files for *why*; read this skill for *how*.

## Before you start

1. Read the provider's streaming API docs end-to-end. Catalogue every
   event type, every content-block variant, and every field the
   provider mints that you don't produce (IDs, signatures, encrypted
   payloads, citations, cache tokens). Those opaque fields are what
   the round-trip contract is about.
2. Read `docs/api_integrity_plans/00_principles.md`. It's ten short
   invariants that govern everything below.
3. Scan `crates/anie-providers-builtin/src/anthropic.rs` as a
   reference implementation. Its top-of-file doc block is the shape
   you'll match.

## The six-step landing recipe

### 1. Declare capabilities on the `Model`, not the `Provider` trait

`ReplayCapabilities` at `crates/anie-provider/src/model.rs` is the
single place that drives replay policy. Add a catalog entry for each
of your provider's models and populate the relevant flags:

```rust
// in crates/anie-providers-builtin/src/models.rs
Model {
    id: "gemini-2.0-flash".into(),
    // ... existing fields ...
    replay_capabilities: Some(ReplayCapabilities {
        requires_thinking_signature: false,     // or true if applicable
        supports_redacted_thinking: false,
        supports_encrypted_reasoning: true,     // e.g. Responses API
    }),
}
```

If your provider has a new capability flag that isn't covered by the
current struct, extend `ReplayCapabilities` itself — don't add a
provider-trait method. The trait should stay thin; capability routing
is data.

**Why:** the agent-loop sanitizer (`crates/anie-agent/src/agent_loop.rs`
around the `sanitize_context_for_request` call) reads from
`model.effective_replay_capabilities()` — no provider edits are needed
when a new replay rule enters the picture.

### 2. Extend `ContentBlock` for new opaque state (if needed)

If the new provider mints a block type that has no analog in
`ContentBlock` (e.g. OpenAI Responses API's `encrypted_content`),
add a variant to `crates/anie-protocol/src/content.rs`:

```rust
#[serde(rename = "encryptedReasoning")]
EncryptedReasoning {
    id: String,
    encrypted_content: String,
},
```

Rules (from plan 05):
- Wire-tag in camelCase (protocol convention; providers translate to
  their own snake_case form on serialize).
- Every new field is `Option<T>` with
  `#[serde(default, skip_serializing_if = "Option::is_none")]` unless
  it's genuinely mandatory on every instance.
- Bump `CURRENT_SESSION_SCHEMA_VERSION` in `crates/anie-session/src/lib.rs`
  and add a row to the changelog table above the constant.
- Add a roundtrip test in `crates/anie-protocol/src/tests.rs`.

If you don't need a new variant — e.g., your provider only emits
text, tool calls, and thinking-with-signatures — skip this step.

### 3. Write the stream state machine with a round-trip contract block

Every new provider module lands with a top-of-file doc block in the
shape plan 03a introduced:

```rust
//! # Round-trip contract
//!
//! | Field                      | Source event            | Landing spot                             |
//! |----------------------------|-------------------------|------------------------------------------|
//! | `<opaque field name>`      | `<sse event name>`      | `<ContentBlock field or variant>`        |
//! ...
//!
//! Intentionally dropped on replay:
//!
//! | Event / field | Why safe to drop |
//! ...
//!
//! **Last verified against provider docs: YYYY-MM-DD.**
//! Re-audit quarterly.
```

Implementation rules:
- **No unannotated `_ => {}` arms.** Every silent fall-through needs
  a comment stating why the drop is safe.
- **Known-unsupported block types fail loud.** If the provider emits
  a block we can't round-trip (server tools, citations, whatever),
  return `ProviderError::UnsupportedStreamFeature(...)` from the
  parser. Pattern is in `anthropic.rs` under the `server_tool_use`
  arm.
- **Truly unknown block types fall through with a stderr log** so the
  field surfaces in logs before causing a downstream 400.
- **Capture opaque state into the block's own `AnthropicBlockState`-
  style variant.** Never store signatures / IDs in a side table
  keyed by index; they'll desync from their blocks under filtering
  or compaction.

### 4. Write the provider-specific HTTP error classifier

Each provider's 400s have their own shape. Mirror the pattern in
`crates/anie-providers-builtin/src/anthropic.rs`:

```rust
pub(crate) fn classify_<provider>_http_error(
    status: reqwest::StatusCode,
    body: &str,
    retry_after_ms: Option<u64>,
) -> ProviderError {
    if status.as_u16() == 400 && looks_like_replay_fidelity(body) {
        return ProviderError::ReplayFidelity {
            provider_hint: "<provider_name>",
            detail: body.chars().take(500).collect(),
        };
    }
    classify_http_error(status, body, retry_after_ms)
}

fn looks_like_replay_fidelity(body: &str) -> bool {
    // Provider-specific string patterns for "replay-broken 400".
    // Keep body-string detection confined to this one function.
}
```

Wire it into the HTTP send path of your provider. The retry policy in
`crates/anie-cli/src/retry_policy.rs` already treats
`ReplayFidelity` as Terminal — you get non-retryable behavior for
free.

### 5. Add test-utils exposure and invariant coverage

Two things in this step:

**(a) Expose `build_request_body_for_test`** so integration tests can
inspect outbound shape without hitting the network:

```rust
#[cfg(any(test, feature = "test-utils"))]
pub fn build_request_body_for_test(
    &self,
    model: &Model,
    context: &LlmContext,
    options: &StreamOptions,
) -> serde_json::Value {
    self.build_request_body(model, context, options /* any extra args */)
}
```

The `test-utils` feature is already defined on
`anie-providers-builtin/Cargo.toml` and pulled in by
`anie-integration-tests`.

**(b) Plug the provider into the invariant suite** at
`crates/anie-integration-tests/tests/provider_replay.rs`:

- Add `<provider>_model()` and `build_<provider>_body()` helpers.
- Extend each cross-provider invariant test (`cache_control_marker_count_bounded_across_providers`,
  `no_null_opaque_field_artifacts_in_serialized_body`,
  `required_opaque_fields_present_per_model_capabilities`,
  `body_is_valid_json_and_parses_back`,
  `conversation_shape_and_roles_preserved_across_providers`) to
  exercise your provider alongside Anthropic and OpenAI.
- Add at least one **scenario fixture** specific to your provider's
  opaque state. Patterns:
  - `<provider>_<opaque_field>_replay_emits_<field>_on_wire`
  - `<provider>_<opaque_field>_is_dropped_when_capability_absent`
  - `<provider>_tool_call_id_roundtrips`

A new provider is not "done" until it's in the invariant list. That's
the enforcement point that catches a future regression before it
ships.

### 6. Register the provider and ship

Wire the provider into the registry in `crates/anie-cli` where other
providers are registered (search for `AnthropicMessages` / `OpenAICompletions`
registrations), add it to the model picker in TUI onboarding if
appropriate, and confirm the full gate:

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Plus a two-turn manual smoke against the real API, following the
template in `docs/api_integrity_plans/01e_rollout_status.md`.

## What generalizes automatically

These you get for free; no provider work needed:

- **Non-retryable 4xx handling.** `ReplayPolicy::decide` already
  terminates on `Http { status: 400..=499, .. }` (except 429) plus
  `ReplayFidelity`, `FeatureUnsupported`, `UnsupportedStreamFeature`,
  `Auth`, `NativeReasoningUnsupported`, `RequestBuild`,
  `ToolCallMalformed`.
- **Sanitizer behavior for legacy sessions.** Thinking blocks with
  `signature: None` are dropped before replay when a model declares
  `requires_thinking_signature=true`. Redacted-thinking blocks are
  dropped for models that don't replay thinking at all.
- **Session schema forward-compat.** `open_session` refuses files
  with `version > CURRENT_SESSION_SCHEMA_VERSION`. Older-version
  files load through serde defaults.
- **Display rendering of errors.** `ProviderError`'s `thiserror::Error`
  derive gives human-readable strings; the UI layer shows them
  directly until/unless someone lands plan 04's dedicated UI branch.

## What does NOT generalize cleanly

Two things still take real work per provider:

1. **The SSE / streaming parser itself.** Each provider's wire format
   is genuinely different. The round-trip contract doc block tells
   you *what* to capture, but you still write a bespoke
   `process_event` tailored to its event names and field shapes.
2. **Model catalog curation.** Each model entry is hand-authored with
   the right pricing, context window, reasoning capabilities, and
   replay capabilities. There's no autodetection.

Every other concern — capability routing, retry classification,
invariant testing, error taxonomy, session compatibility — is
data-driven and flows from the catalog entry.

## Reference files

| What | Where |
|------|-------|
| `ReplayCapabilities` struct | `crates/anie-provider/src/model.rs` |
| Effective-capability helper | `Model::effective_replay_capabilities` |
| Sanitizer | `crates/anie-agent/src/agent_loop.rs` (`sanitize_assistant_for_request`) |
| Provider trait | `crates/anie-provider/src/provider.rs` |
| Anthropic reference impl | `crates/anie-providers-builtin/src/anthropic.rs` |
| OpenAI reference impl | `crates/anie-providers-builtin/src/openai/` |
| `ContentBlock` | `crates/anie-protocol/src/content.rs` |
| `ProviderError` | `crates/anie-provider/src/error.rs` |
| Retry policy | `crates/anie-cli/src/retry_policy.rs` |
| Catalog | `crates/anie-providers-builtin/src/models.rs` |
| Replay fixture tests | `crates/anie-integration-tests/tests/provider_replay.rs` |
| Session schema constant | `crates/anie-session/src/lib.rs` (`CURRENT_SESSION_SCHEMA_VERSION`) |
| Test-utils feature | `crates/anie-providers-builtin/Cargo.toml` |

## Anti-patterns (things to NOT do)

- **Don't add capability flags as methods on the `Provider` trait.**
  They belong on `ReplayCapabilities`. (The one time we did this —
  `Provider::requires_thinking_signature` in plan 01c — we moved it
  off the trait immediately in 03c.)
- **Don't pattern-match on `body` strings inside the generic
  `Http { status, body }` arm of `RetryPolicy::decide`.** If a 400
  carries semantic meaning, add a new `ProviderError` variant and
  classify it at the HTTP boundary.
- **Don't store opaque state in a side table keyed by index.** It
  desyncs from the block under filtering / compaction. Fields belong
  on their block variant.
- **Don't fabricate opaque fields you don't have.** If the provider
  mints a signature and you lost it, the right move is to drop the
  whole block from replay — not invent a placeholder.
- **Don't skip the contract doc block.** It's the breadcrumb future
  maintainers follow when this provider's API changes.

## When to call this skill

Trigger whenever the user wants to add, scaffold, plan, or register
a new LLM provider / API family / backend. Also trigger when
extending the provider trait, the model catalog, a streaming parser,
or anything that would land in one of the Reference files table
above.
