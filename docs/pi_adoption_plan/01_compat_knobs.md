# Plan 01 — compat knobs: `maxTokensField` + `"minimal"` thinking level

**Tier 1 — tiny, low risk, prevents future bugs.**

Two independent small changes bundled because they touch the same
files (`ModelCompat` + `ThinkingLevel`) and ship without any schema
cost.

## Rationale

### `maxTokensField`

pi has a compat flag on each model: `maxTokensField?:
"max_completion_tokens" | "max_tokens"`
(`packages/ai/src/types.ts:277`). Pi's auto-detect is **URL-based
rather than model-id-based** (`openai-completions.ts:~832`): the
default is `"max_completion_tokens"`, and only specific deployments
(currently `baseUrl.includes("chutes.ai")`) flip to
`"max_tokens"`. OpenAI's own `/v1/chat/completions` expects
`max_completion_tokens` for o-series and GPT-5 post-2024; pi's
default matches that.

OpenRouter's proxy *normalizes* this for us today — both fields
work, and OpenRouter translates. But:

- Direct OpenAI endpoints (no proxy) reject `max_tokens` for
  o-series / GPT-5 since the 2024 field rename.
- When we send `max_tokens: None` on the main agent path (post
  `32232b2`) the field is absent, so this doesn't bite today.
- The compaction path (`crates/anie-cli/src/compaction.rs:84`)
  *does* send it. That path currently uses the wire name
  `max_tokens` verbatim, which will 400 against a direct
  OpenAI o-series endpoint.

The fix is a compat-blob field that the outbound body-builder
reads and emits under the right name.

**anie-specific (not in pi):** we keep the default at
`"max_tokens"` rather than matching pi's `"max_completion_tokens"`
default. Reason: anie targets a broader set of endpoints than pi's
typical user, including local servers (llama.cpp variants,
older LM Studio builds) that haven't adopted the newer field
name. Opt-in-by-catalog-entry is the conservative path: the
built-in o4-mini entry and the OpenRouter `openai/o*` +
`openai/gpt-5*` capability mapping set
`MaxTokensField::MaxCompletionTokens` explicitly. Everything
else keeps the legacy wire name that predates the o-series
rename, matching current anie behavior. If a future problem
surfaces on a server that rejects `max_tokens`, flip its
compat entry.

### `"minimal"` thinking level

pi's `ThinkingLevel` is a union of 5 values: `"minimal" | "low" |
"medium" | "high" | "xhigh"` (`packages/ai/src/types.ts:45`).
`"minimal"` is a GPT-5-specific effort level — a way to request
reasoning-capable behavior without spending much token budget on
reasoning. We currently model four (`Off`, `Low`, `Medium`,
`High`); adding `"minimal"` now means we won't retrofit it across
every reasoning-strategy branch when it becomes load-bearing.

**Scope of effect.** Pi only applies reasoning-effort mapping
(including `"minimal"`) when a model's compat blob declares
`supportsReasoningEffort: true`. Providers without that flag —
Anthropic-over-OpenRouter, Groq, Zai — silently ignore the
`"minimal"` request. That's the correct behavior and we should
preserve it: adopting `ThinkingLevel::Minimal` in our enum
doesn't mean every provider respects it, it means the
*infrastructure* is in place so providers that do understand
`"minimal"` can consume it. On providers that don't, we fall
through to the existing no-native-reasoning path.

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-provider/src/model.rs` | Add `max_tokens_field: Option<MaxTokensField>` to `OpenAICompletionsCompat`. |
| `crates/anie-provider/src/thinking.rs` | Add `ThinkingLevel::Minimal` variant (or extend `Off < Low < Medium < High` to include it between `Off` and `Low`). |
| `crates/anie-providers-builtin/src/openai/mod.rs` | Body builder emits `max_tokens` vs `max_completion_tokens` based on compat flag. |
| `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs` | `reasoning_effort` maps `Minimal` to `"minimal"`. |
| `crates/anie-providers-builtin/src/openrouter.rs` | Capability-mapping sets `max_tokens_field = MaxTokensCompletion` for `openai/o*` and `openai/gpt-5*` upstreams. |
| `crates/anie-providers-builtin/src/models.rs` | Built-in o4-mini catalog entry gets the new compat flag. |
| `crates/anie-tui/src/commands.rs` + slash-command parsing | `/thinking minimal` accepted. |

## PRs

### PR A — `maxTokensField` compat flag

1. Add to `OpenAICompletionsCompat`:
   ```rust
   pub enum MaxTokensField {
       MaxTokens,
       MaxCompletionTokens,
   }

   pub struct OpenAICompletionsCompat {
       #[serde(skip_serializing_if = "Option::is_none")]
       pub openrouter_routing: Option<OpenRouterRouting>,
       /// Which outbound field name carries the output-token cap.
       /// Defaults to `"max_tokens"`; set to `"max_completion_tokens"`
       /// for OpenAI o-series and GPT-5 family models, which
       /// rejected the legacy `max_tokens` name post-2024.
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub max_tokens_field: Option<MaxTokensField>,
   }
   ```
2. In `OpenAIProvider::build_request_body_with_native_reasoning_strategy`,
   branch on the compat flag when emitting the output-cap field.
   Default stays `max_tokens` so existing behavior is unchanged.
3. In `openrouter::openrouter_capabilities_for`, set
   `MaxTokensCompletion` for upstream-id prefixes `openai/o` and
   `openai/gpt-5`.
4. Built-in `o4-mini` catalog entry (`models.rs`) gets the compat
   flag.
5. Tests:
   - `openai_request_uses_max_tokens_by_default`
   - `openai_request_uses_max_completion_tokens_when_compat_requests_it`
   - `openrouter_openai_o_series_gets_max_completion_tokens`
   - Existing `openrouter_request_uses_nested_reasoning_object` etc.
     stay green.

### PR B — `ThinkingLevel::Minimal`

1. Extend the enum. Order: `Off`, `Minimal`, `Low`, `Medium`,
   `High`.
2. `reasoning_effort` → `"minimal"` string for the Minimal level.
3. `parse_thinking_level` accepts `"minimal"`.
4. Slash-command validation and TUI display strings updated.
5. Tests: parse, map to effort, round-trip through `/thinking
   minimal`.

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | `compat_default_omits_max_tokens_field` | `anie-provider/src/model.rs` tests |
| 2 | `compat_max_completion_tokens_serializes_correctly` | same |
| 3 | `openai_body_uses_max_completion_tokens_when_compat_requests` | `anie-providers-builtin/src/openai/mod.rs` tests |
| 4 | `openrouter_mapping_sets_max_completion_tokens_for_openai_o_series` | `anie-providers-builtin/src/openrouter.rs` tests |
| 5 | `thinking_level_minimal_parses` | `anie-provider/src/thinking.rs` tests |
| 6 | `reasoning_effort_minimal_maps_to_minimal_string` | `anie-providers-builtin/src/openai/reasoning_strategy.rs` tests |
| 7 | `slash_thinking_accepts_minimal` | `anie-tui/src/commands.rs` tests |

## Risks

- **None significant.** Both changes are additive and gated by
  explicit compat flags / enum variants.
- Watch for a `Minimal` ThinkingLevel that accidentally gets
  treated as `Off` anywhere — run `cargo clippy` and fix any
  non-exhaustive match warnings.

## Exit criteria

- [ ] PR A merged; `max_tokens_field` flows from catalog → body
      builder → wire.
- [ ] PR B merged; `/thinking minimal` round-trips.
- [ ] Existing compaction path doesn't break (it currently sends
      `max_tokens`; with the compat flag set to default None, the
      behavior is identical).
- [ ] Added tests 1-7 pass; full suite green.

## Deferred

- `thinkingFormat: "zai" | "qwen-chat-template"` variants (pi-only)
  — add when a local-model use case lands that needs them. The
  compat-blob structure makes adding these trivial later.
- `supportsDeveloperRole` — related to GPT-5 `developer` role
  handling; bundle with the first GPT-5 integration PR.
