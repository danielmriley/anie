# Milestone 0 — Foundation

Cross-plan scaffolding that has to land before OpenRouter
(milestone 1) starts. **Two PRs** for the OpenRouter-only scope.

A third PR (`ContentBlock::Thinking.thought_signature` for
Gemini) was previously planned here but is deferred with the
rest of the Gemini work; it lands alongside plan 03 whenever
that's prioritized.

## Why this is milestone 0

Two structural pieces matter for OpenRouter and belong in
`anie-provider` / `anie-providers-builtin` rather than
OpenRouter-specific code:

1. **`Model` gets a compat blob** so provider-specific quirks
   (routing preferences, reasoning format) live per-model
   instead of growing the `Provider` trait. Mirrors pi's
   `OpenAICompletionsCompat` on models.
2. **`ThinkingRequestMode::NestedReasoning` variant** so the
   shared OpenAI Chat Completions path can emit
   `reasoning: { effort }` when a catalog entry declares it.
   Required for reasoning-capable OpenRouter models
   (Anthropic-, OpenAI-, Google-upstreams via OR all use the
   nested form).

Landing both in a dedicated foundation PR pair means plan 01
(OpenRouter) is pure catalog/discovery work with no
infrastructure changes in it.

## Dependencies

- None. First milestone.

## PR A — Per-model compat blob

**Goal:** `Model` gains a typed compat blob for
provider-family-specific flags.

### Files
- `crates/anie-provider/src/model.rs`
- `crates/anie-provider/src/lib.rs` (re-exports)

### Shape

```rust
/// Provider-family compat knobs attached per model.
///
/// Each variant collects the flags that are semantically
/// meaningful for one `ApiKind` family. Variants are open —
/// fields inside a variant are `Option<T>` so adding one later
/// doesn't break serde round-trips.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ModelCompat {
    #[default]
    None,
    #[serde(rename = "openai-completions")]
    OpenAICompletions(OpenAICompletionsCompat),
    // Future plans add variants here without a migration.
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAICompletionsCompat {
    /// If set, routing preferences sent as the top-level
    /// `provider` field in OpenRouter requests. Ignored when
    /// `base_url` is not OpenRouter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openrouter_routing: Option<OpenRouterRouting>,
}

/// OpenRouter provider-routing preferences.
///
/// Subset of the shape OpenRouter accepts
/// (https://openrouter.ai/docs/provider-routing) — only the
/// fields we use in v1. Additional fields added as-needed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenRouterRouting {
    /// Whether OpenRouter may fall back to other providers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_fallbacks: Option<bool>,
    /// Ordered provider slugs OpenRouter should try in sequence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
    /// Exclusive upstream provider allowlist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    /// Upstream providers to skip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
    /// Restrict to Zero-Data-Retention providers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zdr: Option<bool>,
}
```

`Model` gains `pub compat: ModelCompat` with default
`ModelCompat::None`. Every existing catalog entry keeps the
default; nothing changes semantically.

### Test plan

| # | Test |
|---|---|
| 1 | `model_compat_defaults_to_none_and_is_skipped_on_serialize` — an existing model serializes without a `compat` key. |
| 2 | `model_compat_with_openai_completions_roundtrips` — populate `compat`, serialize, deserialize, assert equality. |
| 3 | `openrouter_routing_default_has_no_preferences` — the type's `default()` is an all-None struct. |
| 4 | `openrouter_routing_roundtrips_with_only_populated_fields` — serialize a partially-populated routing value, assert serde `skip_serializing_if` elided the None fields. |
| 5 | Every existing `anie-provider` test stays green. |

### Exit criteria

- [ ] `ModelCompat`, `OpenAICompletionsCompat`, `OpenRouterRouting`
      exist and are re-exported from `anie-provider`.
- [ ] `Model.compat: ModelCompat` field present with default.
- [ ] Tests 1–4 pass.
- [ ] No existing catalog entry changed; no existing test
      regressed.

## PR B — `ThinkingRequestMode::NestedReasoning` variant

**Goal:** Shared OpenAI Chat Completions reasoning-request
strategy gets a mode that emits `reasoning: { effort }` (nested)
instead of `reasoning_effort` (flat). Wires through the existing
`ReasoningCapabilities.request_mode` so OpenRouter catalog
entries declare it and everything else stays the same.

### Files
- `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs`
- Whichever module uses the strategy to build the request body
  (likely `openai/mod.rs` or `openai/convert.rs`)

### Shape

```rust
// existing:
pub enum ThinkingRequestMode {
    PromptSteering,
    ReasoningEffort,       // flat `reasoning_effort: "high"`
    NestedReasoning,       // new: `reasoning: { effort: "high" }`
    // ... other existing variants
}
```

The strategy that translates `ReasoningCapabilities +
ThinkingLevel` into request-body mutations branches on the new
variant:

```rust
match capabilities.request_mode {
    ThinkingRequestMode::NestedReasoning => {
        let effort = match level {
            ThinkingLevel::Off => "none",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
        };
        body["reasoning"] = json!({ "effort": effort });
    }
    ThinkingRequestMode::ReasoningEffort => {
        // existing flat path
    }
    // ... others
}
```

The `Off` → `"none"` mapping matches pi's default for
OpenRouter (`packages/ai/src/providers/openai-completions.ts:437`).

### Test plan

| # | Test |
|---|---|
| 6 | `nested_reasoning_emits_reasoning_object_with_effort` — given a model with `request_mode = NestedReasoning` and `thinking = High`, the body has `reasoning.effort = "high"` and NO top-level `reasoning_effort`. |
| 7 | `nested_reasoning_off_level_emits_effort_none` — `thinking = Off` sets `reasoning.effort = "none"`. |
| 8 | `nested_reasoning_maps_every_thinking_level` — table test covering Off / Low / Medium / High. |
| 9 | `existing_reasoning_effort_mode_unchanged` — regression: flat `reasoning_effort` still works for non-OpenRouter reasoning models. |
| 10 | All existing reasoning-strategy tests pass. |

### Exit criteria

- [ ] `ThinkingRequestMode::NestedReasoning` exists.
- [ ] Request-body path emits the nested object exactly when
      the variant is set.
- [ ] Tests 6–9 pass.
- [ ] No regression in existing reasoning model tests.

## Milestone exit criteria

- [ ] Both PRs merged in order (A → B).
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets` clean.
- [ ] No user-visible behavior change on any configured
      provider or any existing catalog entry.
- [ ] Milestone 1 (OpenRouter) can proceed with pure
      catalog/discovery work — no more core-type changes.

## Deferred to Gemini work

`ContentBlock::Thinking.thought_signature` field + associated
session-schema bump. Documented in plan 03
(`../03_google_gemini.md`) under the "Replay capabilities"
section. Lands when that plan starts, not here.
