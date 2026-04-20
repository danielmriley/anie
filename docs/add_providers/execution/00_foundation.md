# Milestone 0 — Foundation

Cross-plan scaffolding that has to land before any provider
plan starts. Three PRs, each small, each independently testable.

## Why this is milestone 0

Three structural pieces surfaced across plans 01, 02, 03, and 05
during the pi comparison. If we don't land them first, each
provider plan re-opens the same design discussion and we lose
the consistency that makes the `adding-providers` skill work.

The pieces are:

1. **`Model` gets a compat blob** so provider-specific quirks
   (reasoning format, capability flags) live per-model instead
   of growing the `Provider` trait. This matches pi's
   `OpenAICompletionsCompat` on models.
2. **`ThinkingRequestMode::NestedReasoning` variant** so the
   shared OpenAI Chat Completions path can emit
   `reasoning: { effort }` when the model demands it (OpenRouter
   reasoning models). Direct follow-on from plan 01's wire-
   protocol finding.
3. **`ContentBlock::Thinking.thought_signature` preparation**
   so Gemini's `thoughtSignature` has a landing spot without a
   cross-crate refactor later. Narrow addition (not the full
   signature-on-any-block work flagged in plan 03) — just
   extending the existing signature field's semantics.

## Dependencies

- None. This is the first milestone.

## Files touched across this milestone (aggregate)

| File | PR |
|---|---|
| `crates/anie-provider/src/model.rs` | A |
| `crates/anie-provider/src/lib.rs` (re-exports) | A |
| `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs` | B |
| `crates/anie-providers-builtin/src/openai/streaming.rs` (strategy wire) | B |
| `crates/anie-protocol/src/content.rs` | C |
| `crates/anie-protocol/src/tests.rs` | C |
| `crates/anie-session/src/lib.rs` (`CURRENT_SESSION_SCHEMA_VERSION`) | C |

No changes to any existing provider module's behavior. Every PR
ships with existing tests green.

---

## PR A — Per-model compat blob

**Goal:** `Model` gains a typed compat blob so downstream plans
can attach provider-specific flags without touching the
`Provider` trait.

### Files
- `crates/anie-provider/src/model.rs`
- `crates/anie-provider/src/lib.rs`

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
    // Future plans can add more variants without a migration.
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAICompletionsCompat {
    /// If set, routing preferences sent as the top-level
    /// `provider` field in OpenRouter requests. Ignored when
    /// `base_url` is not OpenRouter.
    pub openrouter_routing: Option<OpenRouterRouting>,
    /// Reserved for future provider-family flags that don't
    /// warrant their own type today.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}
```

`OpenRouterRouting` itself is a subset of pi's shape — only
the fields our v1 plan 01 uses. More get added as-needed.

`Model` gains `pub compat: ModelCompat` with default
`ModelCompat::None`. Every existing catalog entry keeps the
default; nothing changes semantically.

### Test plan

| # | Test |
|---|---|
| 1 | `model_compat_defaults_to_none_and_is_skipped_on_serialize` — an existing model serializes without a `compat` key appearing. |
| 2 | `model_compat_with_openai_completions_roundtrips` — set `compat` to `OpenAICompletions(...)`, serialize, deserialize, assert equality. |
| 3 | `openai_completions_compat_default_has_no_routing` — `OpenAICompletionsCompat::default()` yields no routing preferences. |
| 4 | Every existing `anie-provider` test stays green. |

### Exit criteria

- [ ] `Model::compat: ModelCompat` exists with a default.
- [ ] `OpenAICompletionsCompat` and `OpenRouterRouting` types
      exist.
- [ ] Roundtrip tests pass.
- [ ] No existing catalog entry changed; no existing test
      regressed.

---

## PR B — `ThinkingRequestMode::NestedReasoning` variant

**Goal:** The OpenAI Chat Completions reasoning-request strategy
gets a new mode that emits `reasoning: { effort }` instead of
`reasoning_effort`. Wires the existing strategy infrastructure
so OpenRouter catalog entries in plan 01 can declare this.

### Files
- `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs`
- `crates/anie-providers-builtin/src/openai/streaming.rs` (only
  if the strategy is consumed here — otherwise the shared module
  that builds the request body)

### Shape

```rust
pub enum ThinkingRequestMode {
    PromptSteering,
    ReasoningEffort,      // existing: flat `reasoning_effort: "high"`
    NestedReasoning,      // new: `reasoning: { effort: "high" }`
    NestedReasoning,
}
```

Update `apply_reasoning_to_body` (or equivalent) to branch on
`NestedReasoning` and emit the nested object. When the user has
`ThinkingLevel::Off`, emit `{ "effort": "none" }` — pi's default
for OpenRouter, matches their docs.

### Test plan

| # | Test |
|---|---|
| 1 | `nested_reasoning_emits_reasoning_object_with_effort` — given a model with `request_mode = NestedReasoning` and level `High`, assert the body has `reasoning.effort = "high"` and does NOT have `reasoning_effort`. |
| 2 | `nested_reasoning_with_level_off_emits_effort_none` — assert `reasoning.effort = "none"`. |
| 3 | `existing_reasoning_effort_mode_unchanged` — regression: models with the existing `ReasoningEffort` mode still emit flat `reasoning_effort`. |
| 4 | All existing reasoning_strategy tests pass. |

### Exit criteria

- [ ] `ThinkingRequestMode::NestedReasoning` exists.
- [ ] Request body path emits the nested object for this mode.
- [ ] Tests 1–3 pass.

---

## PR C — `ContentBlock::Thinking.thought_signature`

**Goal:** Add the field that Gemini's `thoughtSignature` lands
in without breaking session schema compatibility. Narrow addition
— not the full "signature on any block" refactor flagged as
out-of-scope in plan 03.

### Files
- `crates/anie-protocol/src/content.rs`
- `crates/anie-protocol/src/tests.rs`
- `crates/anie-session/src/lib.rs`

### Shape

`ContentBlock::Thinking` currently has `thinking: String,
signature: Option<String>`. Add `thought_signature:
Option<String>` alongside — Anthropic populates
`signature`, Gemini populates `thought_signature`, and the two
are semantically distinct (Anthropic's is a replay-authentication
mechanism, Gemini's is encrypted reasoning state).

Why two fields: pi keeps them separate in TS too
(`types.ts`: `thinkingSignature`, `thoughtSignature`, and
`textSignature` — three different names). Trying to collapse
them into one field on our side means every serializer has to
know which kind its provider uses. Separate fields make the
serializers trivial.

Both fields are `#[serde(default, skip_serializing_if =
"Option::is_none")]` so older sessions load without migration
and newer sessions only carry the fields that were populated.

### Session schema version bump

Per `docs/completed/api_integrity_plans/05_session_schema_migration.md`,
bump `CURRENT_SESSION_SCHEMA_VERSION` in
`crates/anie-session/src/lib.rs`. Add a row to the changelog
table above the constant:

```
| N → N+1 | `thought_signature` added to `ContentBlock::Thinking`
           for Gemini reasoning replay. Pre-bump sessions
           deserialize with `thought_signature = None`, which
           is correct: they predate Gemini support. |
```

### Test plan

| # | Test |
|---|---|
| 1 | `thinking_block_with_thought_signature_roundtrips` — serialize + deserialize. |
| 2 | `thinking_block_without_thought_signature_omits_field_on_serialize` — assert the key is absent. |
| 3 | `legacy_session_loads_with_none_thought_signature` — deserialize a fixture-written old session, assert no errors. |
| 4 | `current_session_schema_version_bumped_by_one` — compile-time check via a test assertion. |

### Exit criteria

- [ ] `ContentBlock::Thinking` has both `signature` and
      `thought_signature`, both optional, both skip-serialized
      when None.
- [ ] Session schema version bumped with a changelog row.
- [ ] Legacy session fixtures still load.
- [ ] All tests green.

---

## Milestone exit criteria

- [ ] All three PRs merged in order (A → B → C).
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets` clean.
- [ ] No behavior change for any existing provider or catalog
      entry.
- [ ] Plans 01, 02, 03, 05 can now declare their compat-blob
      values and new-variant usage without further
      infrastructure work.
