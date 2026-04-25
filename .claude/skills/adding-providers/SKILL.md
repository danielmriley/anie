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

1. **Decide which path applies.** Two shapes of work, same goal, very
   different recipes:
   - **Native provider.** You're adding a new `ApiKind` — the provider
     has a wire protocol of its own (Anthropic Messages, Google
     Gemini, Bedrock Converse, AWS-signed variants). You'll write a
     streaming parser and probably touch `ContentBlock`. Follow the
     *Native provider path* below.
   - **Aggregator / OpenAI-compatible provider.** The provider speaks
     OpenAI chat-completions (xAI, Groq, Mistral, Together.ai,
     Fireworks, OpenRouter, Cerebras, etc.) and routes to any number
     of upstream models. You reuse the existing `OpenAIProvider` and
     extend via data — a preset, maybe a compat-blob variant, and
     per-vendor capability inference. Follow the *Aggregator provider
     path* below. `openrouter.rs` is the reference implementation.
   - Picking the wrong path will have you writing a streaming parser
     for a provider that didn't need one, or shoving wire quirks into
     shared code instead of a compat blob.
2. Read the provider's streaming API docs end-to-end. Catalogue every
   event type, every content-block variant, and every field the
   provider mints that you don't produce (IDs, signatures, encrypted
   payloads, citations, cache tokens). Those opaque fields are what
   the round-trip contract is about.
3. **Find a reference implementation.** Before writing anything, find
   one or two shipping SDKs / harnesses that already integrate this
   provider — pi's `packages/ai/src/providers/` is our go-to — and
   cross-check *exact wire shapes*: reasoning field names, request
   body structure (nested vs. flat `reasoning_effort`), per-upstream
   quirks, cache-marker conventions. Writing anie's integration first
   and fixing surprises later has cost us reruns. Doing this check up
   front for OpenRouter caught four behaviors we initially had wrong.
4. Read `docs/api_integrity_plans/00_principles.md`. It's ten short
   invariants that govern everything below.
5. Scan the reference implementation that matches your path:
   - Native: `crates/anie-providers-builtin/src/anthropic.rs`.
   - Aggregator: `crates/anie-providers-builtin/src/openrouter.rs`.
   The top-of-file doc block is the shape you'll match.

## Native provider path: six-step landing recipe

Use this when you're adding a new `ApiKind` — the provider has its
own wire protocol and needs its own streaming parser.

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

## Aggregator provider path: adapter recipe

Use this when the provider speaks OpenAI chat-completions and you're
reusing the existing `OpenAIProvider`. The aggregator provider may
front many upstream models (OpenRouter does), a single model family
(Groq), or just their own (xAI). The recipe is the same; the amount
of per-upstream logic varies.

Reference implementation: `crates/anie-providers-builtin/src/openrouter.rs`.

### A1. Register the onboarding preset

Add a `custom_openai_preset(...)` entry to `provider_presets()` in
`crates/anie-tui/src/overlays/onboarding.rs`. Placement matters — the
onboarding picker is the only place users can add the provider
(`/providers` lists *configured* providers only), so put it where
someone browsing will notice it:

```rust
custom_openai_preset(
    "xAI / Grok",
    "xai",
    "https://api.x.ai/v1",
    "grok-2-1212",      // placeholder; replaced by discovery after onboarding
),
```

The `env_var` defaults to `{PROVIDER_NAME_UPPER}_API_KEY`. If the
provider uses a different convention, set it explicitly on the
preset.

### A2. Extend the discovery parser (only if the provider ships new fields)

`crates/anie-providers-builtin/src/model_discovery.rs` already parses
pricing, `top_provider`, `supported_parameters`, and `architecture`
for OpenRouter — if the new aggregator uses the same field names,
you get them for free. Only extend `OpenAiModelEntry` if you hit a
field those don't cover, and follow the existing pattern: new fields
are `Option<T>` with `#[serde(default)]` so other endpoints keep
parsing.

If the provider *doesn't* expose `/v1/models`, skip this step — the
preset's hardcoded model id is what users get until you wire in a
per-vendor discovery branch. Document the limitation.

### A3. Write a per-vendor capability-mapping module

Mirror `openrouter.rs`. The module exposes three things:

```rust
// Target detection
pub fn is_<vendor>_target(base_url: &str) -> bool { ... }

// Per-model capability inference from id (and any other signal
// available — supports_reasoning flag, supported_parameters, etc.)
pub fn <vendor>_capabilities_for(
    model_id: &str,
    supports_reasoning: bool,
) -> (Option<ReplayCapabilities>, Option<ReasoningCapabilities>) { ... }

// In-place apply, called at discovery → Model conversion time
pub fn apply_<vendor>_capabilities(model: &mut Model) { ... }
```

For aggregators with upstream prefixes (OpenRouter's
`anthropic/claude-sonnet-4`, `openai/o3`), infer from the prefix:
`anthropic/*` + reasoning → `requires_thinking_signature`,
`openai/o*` + reasoning → `supports_reasoning_details_replay`, and
so on. For single-family providers (Groq serving
DeepSeek-R1 / Llama / Qwen), infer from the model id directly.

Wire the `apply_*_capabilities` call into both
`crates/anie-tui/src/overlays/onboarding.rs::configured_provider_from_context`
and `crates/anie-tui/src/overlays/providers.rs::model_info_to_provider_model`
so discovery produces fully-populated catalog entries whether the user
added the provider via the preset or via the `/providers` overlay.

### A4. Add per-vendor quirks as compat-blob data, not shared code

When the provider needs behavior that differs from baseline OpenAI
chat-completions — routing preferences, cache-control markers,
custom request fields — extend
`OpenAICompletionsCompat` in `crates/anie-provider/src/model.rs`
with an optional field and wire it through
`OpenAIProvider::build_request_body_with_native_reasoning_strategy`
guarded by `is_<vendor>_target(&model.base_url)`. Do *not* sprinkle
vendor branches through the shared code — each one becomes a
maintenance cost for every other vendor using the same path.

Patterns already in place:
- **Anthropic-upstream cache_control** (`needs_anthropic_cache_control`
  + `insert_anthropic_cache_control`): when the aggregator routes
  to Anthropic, walk the messages back-to-front and tag the last
  text part with `cache_control: ephemeral`. Idempotent; guarded
  by upstream-prefix detection.
- **Provider-routing preferences** (`OpenRouterRouting` in the
  compat blob): OpenRouter-specific. Serializes as the top-level
  `provider` field when populated, omitted otherwise.
- **`reasoning_details` replay** (per-model
  `supports_reasoning_details_replay`): streaming layer accumulates
  the opaque array; outbound converter emits it verbatim. Only
  stored when the model opts in — otherwise it'd bloat session
  files for nothing.

### A5. Filter the discovered catalog if the aggregator includes non-tool models

OpenRouter exposes completion-only and image-gen models; the picker
becomes unusable if you keep them. The filter goes in
`discover_openai_compatible_models` and is gated to the specific
aggregator (check `request.provider_name`) so direct-OpenAI
discovery — which doesn't return `supported_parameters` at all —
isn't wiped out. Single-family aggregators (xAI, Groq) probably
don't need this; confirm against their `/models` output.

### A6. Add tests and wire into invariant suite

Minimum tests for an aggregator:

- `<vendor>_capabilities_<upstream_family>_*` — one test per
  capability-mapping row. See `openrouter::tests` for the shape.
- `<vendor>_request_uses_nested_reasoning_object` (or whatever the
  request-side quirk is). Covers both populated and default compat
  blobs.
- `<vendor>_<quirk>_applies_only_when_gated`. e.g., Anthropic
  cache-control doesn't fire on `openai/*` routed through OpenRouter.
- `<vendor>_discovery_parses_<new_fields>` — if you extended the
  discovery parser in A2.
- If the aggregator fronts reasoning upstreams requiring replay:
  a `reasoning_details` capture + round-trip pair in
  `openai/streaming.rs` and `openai/convert.rs` tests.

Plug the aggregator into
`crates/anie-integration-tests/tests/provider_replay.rs` *if* it
fronts reasoning upstreams — the invariant suite covers the
multi-turn replay path and catches regressions in signature /
`reasoning_details` handling across turns.

### A7. Ship

Same gate as the native path:

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Plus a two-turn manual smoke against the real API. For aggregators
that front multiple upstreams, smoke at least one Anthropic-upstream
(exercises cache_control) and one OpenAI-reasoning-upstream
(exercises `reasoning_details`) if both are reachable.

## Shared concerns (both paths)

### Rate-limit retry policy

Free-tier endpoints on aggregators routinely 429 with no
`Retry-After` header. The retry policy already handles this
correctly (see `retry_policy.rs`: `MAX_RATE_LIMIT_RETRIES = 1`,
`RATE_LIMIT_FALLBACK_DELAY_MS = 15_000`) — but do not weaken these
floors when adding a provider. Burning 4 requests in 8 seconds on a
20-req/min budget only makes the next lockout arrive faster.

If a provider advertises its rate-limit window through custom
headers (OpenRouter sends `x-ratelimit-reset` as a unix-ms
timestamp), wire that into `parse_retry_after` at the HTTP layer.
Do *not* parse it inside `retry_policy.rs`.

### Reasoning field name variance

Three field names appear in the wild for the same payload:
`reasoning`, `reasoning_content`, `reasoning_text`. Any parser that
only checks the first one will silently drop reasoning on some
backends (DeepSeek's native, forwarded by OpenRouter, uses
`reasoning_text`). The ordered lookup in
`openai/reasoning_strategy.rs::native_reasoning_delta` is the
single point of truth — extend it if a new name surfaces.

### Session schema bumps for optional fields on any message type

The recipe in Native step 2 mentioned bumping
`CURRENT_SESSION_SCHEMA_VERSION` when adding a `ContentBlock`
variant. Also bump when adding an `Option<T>` field to
`AssistantMessage`, `Model`, or any other type that lands in a
session file — even if the field is serde-defaulted and strictly
backward compatible. The bump is the signal that *something
interpretable to newer binaries* exists in the file; older binaries
correctly reject v{N+1} files via the existing
`open_session_rejects_future_schema_versions` guard.

Add a row to the changelog comment above
`CURRENT_SESSION_SCHEMA_VERSION` and a forward-compat test like
`session_reopen_tolerates_pre_vN_files_without_<new_field>`.

### Test-fixture propagation

Adding a new field to `Model`, `AssistantMessage`, or
`ContentBlock` triggers `missing field` errors across dozens of
test fixtures. The pragmatic sweep is a Python one-liner against
the crates directory: find the literal (`Model {`, `AssistantMessage {`),
locate the last existing field line (`timestamp: ...`,
`replay_capabilities: ...`), and inject the new one with matching
indentation. Applied this pass three times during PR B; each took
~30 seconds once the pattern was worked out.

### Clippy `large_enum_variant` after growing `Model`

Each new field on `Model` or `AssistantMessage` grows the enum
variants that hold them. `UiAction::SetResolvedModel(Box<Model>)`
and `ModelPickerAction::Selected(Box<ModelInfo>)` are already
boxed. If a new enum appears that carries a `Model` by value,
expect clippy to fire and box it.

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

Native-provider-specific work:

1. **The SSE / streaming parser itself.** Each provider's wire format
   is genuinely different. The round-trip contract doc block tells
   you *what* to capture, but you still write a bespoke
   `process_event` tailored to its event names and field shapes.
2. **Model catalog curation** (for native providers without a
   `/v1/models` endpoint). Each model entry is hand-authored with
   the right pricing, context window, reasoning capabilities, and
   replay capabilities.

Aggregator-specific work:

1. **Per-vendor capability mapping.** `<vendor>_capabilities_for` is
   always bespoke — the upstream-id taxonomy is specific to each
   aggregator. Templated via the openrouter module but not
   auto-derivable.
2. **Wire quirks that aren't already modeled.** If the new aggregator
   needs a behavior that no compat-blob field covers — custom auth
   headers, non-standard streaming framing — that's a real code
   change in `OpenAIProvider`. Keep it guarded by
   `is_<vendor>_target`.

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
| Anthropic reference impl (native) | `crates/anie-providers-builtin/src/anthropic.rs` |
| OpenAI reference impl (native) | `crates/anie-providers-builtin/src/openai/` |
| OpenRouter reference impl (aggregator) | `crates/anie-providers-builtin/src/openrouter.rs` |
| `ModelCompat` / per-vendor compat blobs | `crates/anie-provider/src/model.rs` |
| Onboarding presets | `crates/anie-tui/src/overlays/onboarding.rs::provider_presets` |
| Discovery parser | `crates/anie-providers-builtin/src/model_discovery.rs` |
| Reasoning-field lookup | `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs::native_reasoning_delta` |
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
- **Don't write a streaming parser for an OpenAI-compatible
  aggregator.** Reuse `OpenAIProvider`. Per-vendor behavior goes in
  a capability-mapping module + compat-blob fields, not a second
  parser. Writing `xai_provider.rs` that looks 95% like `openai/mod.rs`
  is the first sign something is wrong.
- **Don't sprinkle `if provider == "<vendor>"` checks through shared
  code.** Gate with `is_<vendor>_target(&model.base_url)` and group
  the vendor logic in one module. Provider names are mutable
  config; base URLs are the stable routing signal.
- **Don't curate an aggregator catalog by hand.** If the aggregator
  has `/v1/models`, use discovery — 500+ entries go stale the day
  you ship them.
- **Don't apply a tool-supporting filter to direct-OpenAI
  discovery.** It'd return zero models — the OpenAI `/v1/models`
  endpoint doesn't populate `supported_parameters`. Gate the filter
  to the specific aggregator that needs it.

## When to call this skill

Trigger whenever the user wants to add, scaffold, plan, or register
a new LLM provider / API family / backend — native (Gemini, Bedrock,
a hypothetical new Anthropic endpoint) or OpenAI-compatible
aggregator (xAI, Groq, Mistral, Together, Fireworks, Cerebras, DeepInfra,
etc.). Also trigger when extending the provider trait, the model
catalog, a streaming parser, a compat blob, an upstream capability
mapping, the discovery parser, or anything that would land in one
of the Reference files table above.

First decision when the skill fires: *native or aggregator?* See
step 1 of "Before you start." Native means a new `ApiKind`.
Aggregator means reusing `OpenAICompletions` and extending via data.
Picking wrong doubles the work.
