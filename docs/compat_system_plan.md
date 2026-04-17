# OpenAI-Compatible Backend Compat System

Inspired by pi's `compat` pattern, this plan adds per-provider and per-model
compatibility flags that replace hardcoded behaviors in the OpenAI-compatible
provider.

## Problem

The OpenAI provider currently makes decisions using broad predicates like
`is_local_openai_compatible_target(model)` and backend-guessing functions like
`openai_compatible_backend(model)`. These gate several behaviors:

| Behavior | Current decision method |
|----------|----------------------|
| Inject prompt-steering text | `is_local_openai_compatible_target()` — always on for all locals |
| Send `reasoning_effort` / nested reasoning fields | `ThinkingRequestMode` on `ReasoningCapabilities` |
| Reserve extra `max_tokens` headroom | `is_local_openai_compatible_target()` + `effective_reasoning_capabilities()` |
| Parse `<think>` / `<thinking>` / `<reasoning>` tags | Always on for all local models |
| Try `stream_options`, fall back on 400 | Always tried, detected by response body |
| Use `"system"` role for system prompt | Always `"system"` (never `"developer"`) |
| Field name: `max_tokens` | Always `max_tokens` (never `max_completion_tokens`) |
| Which native reasoning fields to read | Hardcoded list: `reasoning`, `reasoning_content`, `thinking` |
| Tagged reasoning tag set | Hardcoded: `<think>`, `<thinking>`, `<reasoning>` |

None of these can be overridden per model or per provider without code changes.

Pi solves this with a `compat` object on both providers and models. We should
adopt a similar pattern.

## Design

### New struct: `OpenAiCompat`

```rust
/// Compatibility flags for OpenAI-compatible backends.
///
/// Set at provider level for defaults, override at model level.
/// All fields are optional — absent means "use the existing default behavior."
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenAiCompat {
    /// How to request thinking/reasoning from this model.
    /// Replaces `ThinkingRequestMode` on `ReasoningCapabilities`.
    ///
    /// - `ReasoningEffort`     — top-level `reasoning_effort` field
    /// - `NestedReasoning`     — `reasoning: { effort: ... }`
    /// - `Qwen`                — top-level `enable_thinking: true`
    /// - `QwenChatTemplate`    — `chat_template_kwargs: { enable_thinking: true }`
    /// - `None`                — no native thinking control
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_format: Option<ThinkingFormat>,

    /// Whether to inject prompt-steering text into the system prompt
    /// based on the current thinking level.
    ///
    /// Default: `true` for local targets, `false` for hosted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_steering: Option<bool>,

    /// Whether the backend supports `stream_options: { include_usage: true }`.
    ///
    /// When `false`, the provider skips stream_options entirely instead of
    /// trying and falling back on a 400 error.
    ///
    /// Default: `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_stream_options: Option<bool>,

    /// Whether to use `"developer"` role instead of `"system"` for the
    /// system prompt. Some hosted OpenAI reasoning models prefer this.
    ///
    /// Default: `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_developer_role: Option<bool>,

    /// Which field name to use for max output tokens.
    ///
    /// Default: `MaxTokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<MaxTokensField>,

    /// Whether to reserve extra max_tokens headroom for reasoning output.
    ///
    /// Default: `true` for local models with reasoning output, `false` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_token_headroom: Option<bool>,

    /// Whether to parse `<think>`, `<thinking>`, `<reasoning>` tags
    /// in streamed content as hidden reasoning.
    ///
    /// Default: `true` for local targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse_tagged_reasoning: Option<bool>,

    /// Whether to convert thinking blocks to plain text when replaying
    /// assistant messages. Model-level override for `includes_thinking_in_replay`.
    ///
    /// Default: `false` (thinking is stripped from replay for OpenAI-compatible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_thinking_as_text: Option<bool>,
}
```

### New enum: `ThinkingFormat`

Replaces `ThinkingRequestMode` with richer variants:

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ThinkingFormat {
    /// Top-level `reasoning_effort` field (Ollama, most OpenAI-compatible).
    ReasoningEffort,
    /// Nested `reasoning: { effort: ... }` field (LM Studio).
    NestedReasoning,
    /// Top-level `enable_thinking: true` (DashScope / hosted Qwen).
    Qwen,
    /// `chat_template_kwargs: { enable_thinking: true }` (local Qwen servers).
    QwenChatTemplate,
}
```

### New enum: `MaxTokensField`

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MaxTokensField {
    /// Use `max_tokens` (most backends).
    MaxTokens,
    /// Use `max_completion_tokens` (newer OpenAI models).
    MaxCompletionTokens,
}
```

### Where it lives

`OpenAiCompat` goes on two levels:

1. **Provider config** — applies to all models under that provider
2. **Model config / `Model` struct** — overrides provider defaults for one model

```rust
// On Model (runtime)
pub struct Model {
    // ... existing fields ...
    /// OpenAI-compatible backend compat flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<OpenAiCompat>,
}
```

```toml
# In config — provider level
[providers.ollama]
base_url = "http://localhost:11434/v1"
api = "OpenAICompletions"

[providers.ollama.compat]
supports_developer_role = false
prompt_steering = true
parse_tagged_reasoning = true

# In config — model level overrides provider
[[providers.ollama.models]]
id = "qwen3.5:9b"
name = "Qwen 3.5 9B"
context_window = 32768
max_tokens = 8192
supports_reasoning = true
reasoning_control = "Native"
reasoning_output = "Tagged"
reasoning_tag_open = "<think>"
reasoning_tag_close = "</think>"

[providers.ollama.models.compat]
thinking_format = "ReasoningEffort"
# Inherits prompt_steering and parse_tagged_reasoning from provider
```

### Resolution order

When the OpenAI provider needs a compat flag, it resolves in this order:

1. **Model-level `compat`** — if the field is `Some`, use it
2. **Provider-level `compat`** — if the field is `Some`, use it
3. **Heuristic default** — existing behavior as fallback

This is the same merge pattern pi uses.

In code:

```rust
fn resolve_compat(model: &Model, provider_compat: Option<&OpenAiCompat>) -> ResolvedCompat {
    let model_compat = model.compat.as_ref();
    ResolvedCompat {
        thinking_format: model_compat
            .and_then(|c| c.thinking_format)
            .or_else(|| provider_compat.and_then(|c| c.thinking_format)),
        prompt_steering: model_compat
            .and_then(|c| c.prompt_steering)
            .or_else(|| provider_compat.and_then(|c| c.prompt_steering)),
        // ... etc for each field
    }
}
```

Where `ResolvedCompat` has non-optional fields with concrete defaults applied.

## What this replaces

| Current code | Replaced by |
|-------------|-------------|
| `ThinkingRequestMode` on `ReasoningCapabilities` | `compat.thinking_format` |
| `is_local_openai_compatible_target()` check for prompt steering | `compat.prompt_steering` |
| `stream_options` try/fallback logic | `compat.supports_stream_options` |
| Always `"system"` role | `compat.supports_developer_role` |
| Always `"max_tokens"` field | `compat.max_tokens_field` |
| Always parse tagged reasoning for locals | `compat.parse_tagged_reasoning` |
| `effective_max_tokens()` headroom heuristic | `compat.reasoning_token_headroom` |
| `includes_thinking_in_replay()` (provider-level only) | `compat.replay_thinking_as_text` (model-level) |

## What this does NOT replace

| Stays as-is | Why |
|-------------|-----|
| `ReasoningCapabilities.control` | Still useful: `Prompt` vs `Native` is a semantic distinction |
| `ReasoningCapabilities.output` | Still useful: `Tagged` vs `Separated` describes the model's output |
| `ReasoningCapabilities.tags` | Still useful: custom tag pairs for tagged output |
| `supports_reasoning` on Model | Still needed as a coarse capability flag |
| `includes_thinking_in_replay()` on Provider trait | Still the provider-level default; compat is the model-level override |
| Anthropic provider behavior | Anthropic has its own API, not OpenAI-compatible |
| `default_local_reasoning_capabilities()` | Becomes the heuristic fallback for compat too |

## Relationship to `ReasoningCapabilities`

After this change, the responsibilities split cleanly:

- **`ReasoningCapabilities`** describes *what the model can do*:
  - Can it reason? (`control`: Prompt / Native)
  - How does it emit reasoning? (`output`: Tagged / Separated)
  - What tags does it use? (`tags`)

- **`OpenAiCompat`** describes *how to talk to the backend*:
  - What request shape to use for thinking? (`thinking_format`)
  - Does it support stream_options? (`supports_stream_options`)
  - What role for system prompts? (`supports_developer_role`)
  - Should we inject prompt steering? (`prompt_steering`)

`request_mode` on `ReasoningCapabilities` gets removed — it was always a
backend concern, not a model capability.

## Native thinking in Qwen 3 / 3.5 models

Qwen 3 and Qwen 3.5 models have **genuine native thinking**. They were trained
with a structured chain-of-thought capability that produces `<think>...</think>`
blocks. This is not a hack or prompt trick — it is a real model architecture
feature.

On Ollama, when `reasoning_effort` is sent for a Qwen model, Ollama translates
it into the model's native `enable_thinking` parameter via the chat template.
So `reasoning_effort: "high"` on Ollama + Qwen 3.5 = the model's real thinking
mode is activated.

This means:

- Qwen 3/3.5 should always be configured with `supports_reasoning = true`
  and `reasoning_control = "Native"` — they are genuinely reasoning-capable.
- Their output format is `Tagged` with `<think>` / `</think>` tags.
- The correct `thinking_format` on Ollama is `ReasoningEffort`, because
  Ollama handles the translation to `enable_thinking` internally.
- The original bug (thinking-only responses accepted as valid) was not a
  sign that the model can't think. It was a sign that the model sometimes
  spends its entire output budget on thinking (especially smaller variants
  under `High` thinking) and produces no visible answer.
- The thinking-level question for these models is not "should we enable
  thinking" — it is "how much of the output budget should go to thinking
  vs the answer."

The compat system must reflect this: Qwen models on Ollama are natively
reasoning-capable with `ReasoningEffort` as the correct format, not
prompt-steering-only.

## Config examples

### Qwen 3.5 9B on Ollama — native reasoning, conservative thinking level

```toml
[[providers.ollama.models]]
id = "qwen3.5:9b"
name = "Qwen 3.5 9B"
context_window = 32768
max_tokens = 8192
supports_reasoning = true
reasoning_control = "Native"
reasoning_output = "Tagged"
reasoning_tag_open = "<think>"
reasoning_tag_close = "</think>"

[providers.ollama.models.compat]
thinking_format = "ReasoningEffort"
prompt_steering = true
parse_tagged_reasoning = true
```

The model has genuine native thinking. Ollama translates `reasoning_effort`
into the model's `enable_thinking`. Prompt steering is additive guidance on
top. For the 9B variant, use `Low` or `Medium` thinking level to avoid
spending the entire output budget on reasoning.

### Qwen 3.5 35B on Ollama — native reasoning, standard thinking level

```toml
[[providers.ollama.models]]
id = "qwen3.5:35b"
name = "Qwen 3.5 35B"
context_window = 32768
max_tokens = 8192
supports_reasoning = true
reasoning_control = "Native"
reasoning_output = "Tagged"
reasoning_tag_open = "<think>"
reasoning_tag_close = "</think>"

[providers.ollama.models.compat]
thinking_format = "ReasoningEffort"
prompt_steering = true
```

Same native thinking as the 9B, but the larger model handles `Medium` and
`High` thinking levels more reliably without running out of answer budget.

### Qwen 3 on a non-Ollama local server

If the local server does not translate `reasoning_effort` to
`enable_thinking` (i.e. it is not Ollama), use the Qwen-native format
directly:

```toml
[[providers.local.models]]
id = "qwen3:32b"
name = "Qwen 3 32B"
context_window = 32768
max_tokens = 8192
supports_reasoning = true
reasoning_control = "Native"
reasoning_output = "Tagged"
reasoning_tag_open = "<think>"
reasoning_tag_close = "</think>"

[providers.local.models.compat]
thinking_format = "QwenChatTemplate"
prompt_steering = false
parse_tagged_reasoning = true
```

Uses `chat_template_kwargs: { enable_thinking: true }` directly. No prompt
steering because the model's native thinking is sufficient.

### Non-reasoning local model (e.g. Llama, Mistral)

```toml
[[providers.ollama.models]]
id = "llama3.3:70b"
name = "Llama 3.3 70B"
context_window = 131072
max_tokens = 8192
supports_reasoning = false

[providers.ollama.models.compat]
prompt_steering = true
parse_tagged_reasoning = false
```

No native thinking. Prompt steering provides soft behavioral guidance.
Tagged reasoning parsing is off because this model does not produce
`<think>` blocks.

### LM Studio model

```toml
[[providers.lmstudio.models]]
id = "deepseek-r1:14b"
name = "DeepSeek R1 14B"
context_window = 32768
max_tokens = 8192
supports_reasoning = true
reasoning_control = "Native"
reasoning_output = "Separated"

[providers.lmstudio.models.compat]
thinking_format = "NestedReasoning"
supports_stream_options = false
```

### Provider-level defaults with model overrides

```toml
[providers.ollama]
base_url = "http://localhost:11434/v1"
api = "OpenAICompletions"

[providers.ollama.compat]
supports_developer_role = false
prompt_steering = true
parse_tagged_reasoning = true

[[providers.ollama.models]]
id = "llama3.3:70b"
name = "Llama 3.3 70B"
context_window = 131072
max_tokens = 8192
supports_reasoning = false

[providers.ollama.models.compat]
parse_tagged_reasoning = false
# Inherits prompt_steering = true from provider

[[providers.ollama.models]]
id = "qwen3.5:35b"
name = "Qwen 3.5 35B"
context_window = 32768
max_tokens = 8192
supports_reasoning = true
reasoning_control = "Native"
reasoning_output = "Tagged"
reasoning_tag_open = "<think>"
reasoning_tag_close = "</think>"

[providers.ollama.models.compat]
thinking_format = "ReasoningEffort"
# Inherits prompt_steering = true from provider
# Inherits parse_tagged_reasoning = true from provider
```

### Hosted OpenAI model that needs developer role

```toml
[[providers.openai.models]]
id = "o4-mini"
name = "o4-mini"
context_window = 200000
max_tokens = 100000
supports_reasoning = true

[providers.openai.models.compat]
supports_developer_role = true
max_tokens_field = "MaxCompletionTokens"
thinking_format = "ReasoningEffort"
prompt_steering = false
parse_tagged_reasoning = false
```

## Heuristic defaults

When compat fields are absent (both model and provider level), the existing
behavior continues as the fallback. This means:

| Flag | Heuristic default |
|------|------------------|
| `thinking_format` | `ReasoningEffort` for known native reasoning families (Qwen 3/3.5, QwQ, DeepSeek-R1) on Ollama/vLLM; `NestedReasoning` for LM Studio; absent for all others |
| `prompt_steering` | `true` for `is_local_openai_compatible_target()`, else `false` |
| `supports_stream_options` | `true` (try, fall back on 400) |
| `supports_developer_role` | `false` |
| `max_tokens_field` | `MaxTokens` |
| `reasoning_token_headroom` | `true` for local models with reasoning output |
| `parse_tagged_reasoning` | `true` for local targets |
| `replay_thinking_as_text` | `false` |

So a user who configures nothing gets exactly the current behavior.

## Request body examples for each ThinkingFormat

When thinking is enabled (e.g. `Medium` or `High`), each format produces:

### `ReasoningEffort`
```json
{ "reasoning_effort": "high", ... }
```

### `NestedReasoning`
```json
{ "reasoning": { "effort": "high" }, ... }
```

### `Qwen`
```json
{ "enable_thinking": true, ... }
```

### `QwenChatTemplate`
```json
{ "chat_template_kwargs": { "enable_thinking": true }, ... }
```

When thinking is `Off`, no native thinking fields are sent for any format.

For `Qwen` and `QwenChatTemplate`, the value is boolean — thinking is on or
off. Anie maps any non-`Off` thinking level to `true`.

For `ReasoningEffort` and `NestedReasoning`, the value is a string level.
Anie maps `Low` → `"low"`, `Medium` → `"medium"`, `High` → `"high"`.

## Interaction between `thinking_format` and `ReasoningCapabilities.control`

`ReasoningCapabilities.control` describes *whether* the model can reason
natively (`Native`) or only via prompt guidance (`Prompt`).

`compat.thinking_format` describes *how* to send reasoning controls to the
backend.

These are independent:

- A model with `control = Native` and no `thinking_format` will use
  heuristic detection for the wire format.
- A model with `thinking_format = ReasoningEffort` implicitly has native
  reasoning control — there is no need to also set `control = Native`,
  though it is not wrong to do so.
- A model with `control = Prompt` and `thinking_format = ReasoningEffort`
  means: "this model has prompt-level reasoning only, but the backend
  happens to accept a `reasoning_effort` field." This is an unusual
  combination but not invalid.

In practice, if `thinking_format` is set, `control` is informational.
The compat flag wins for wire-format decisions.

## `replay_thinking_as_text` and agent-loop access

The `replay_thinking_as_text` flag is a model-level override for the
provider-level `includes_thinking_in_replay()` trait method.

Currently, `sanitize_context_for_request()` in the agent loop takes a
bool from the provider. To support model-level override, the agent loop
would need access to the model's compat flags.

Two options:

1. **Pass the resolved replay policy as a bool** — the controller resolves
   `model.compat.replay_thinking_as_text` OR `provider.includes_thinking_in_replay()`
   and passes the final bool to the agent loop. Simple, no trait change.

2. **Extend the Provider trait** — add a method like
   `fn includes_thinking_in_replay_for_model(&self, model: &Model) -> bool`
   that checks model compat then falls back to the provider default.

Option 1 is simpler and keeps the agent loop unaware of compat details.

## Migration from `ThinkingRequestMode`

`ThinkingRequestMode` was added in Phase 3 of the reasoning fix plan and
lives on `ReasoningCapabilities.request_mode`. It is being replaced by
`compat.thinking_format`.

The migration:

| `ThinkingRequestMode` | `ThinkingFormat` equivalent |
|-----------------------|----------------------------|
| `PromptSteering` | (no `thinking_format`, use `prompt_steering = true`) |
| `ReasoningEffort` | `ThinkingFormat::ReasoningEffort` |
| `NestedReasoning` | `ThinkingFormat::NestedReasoning` |

New variants not in `ThinkingRequestMode`:
- `ThinkingFormat::Qwen`
- `ThinkingFormat::QwenChatTemplate`

The `request_mode` field on `ReasoningCapabilities` is removed in Phase C.
Any existing config using `thinking_request_mode` will need migration to
`[*.compat] thinking_format = ...`. During the transition, both can be
supported with a deprecation warning.

## Internal cleanup: `NativeReasoningRequestStrategy`

The internal `NativeReasoningRequestStrategy` enum in the OpenAI provider:

```rust
enum NativeReasoningRequestStrategy {
    NoNativeFields,
    TopLevelReasoningEffort,
    LmStudioNestedReasoning,
}
```

becomes redundant. `ThinkingFormat` maps directly to what gets emitted:

- `ReasoningEffort` → top-level field
- `NestedReasoning` → nested field
- `Qwen` → enable_thinking
- `QwenChatTemplate` → chat_template_kwargs
- absent → no native fields

The internal enum and the `native_reasoning_request_strategies()` method
are removed. The request body builder reads the resolved `thinking_format`
directly.

## Implementation phases

### Phase A — Add the types

- Add `OpenAiCompat`, `ThinkingFormat`, `MaxTokensField` to `anie-provider`
- Add `compat: Option<OpenAiCompat>` to `Model`
- Add `compat` to `ProviderConfig` and `CustomModelConfig` in `anie-config`
- Add compat merging to `ConfigMutator`
- Wire serde, ensure backward compatibility
- Keep `ThinkingRequestMode` temporarily for backward compat

### Phase B — Wire compat into the OpenAI provider

Replace each hardcoded behavior one at a time. These are mostly
independent — each can be done and tested individually:

1. `thinking_format` → replaces `ThinkingRequestMode` and
   `NativeReasoningRequestStrategy` and
   `native_reasoning_request_strategies()`
2. `prompt_steering` → replaces `is_local_openai_compatible_target()` gate
   in `effective_system_prompt()`
3. `supports_stream_options` → replaces the try/fallback logic in
   `send_stream_request_once()`
4. `supports_developer_role` → replaces hardcoded `"system"` role
5. `max_tokens_field` → replaces hardcoded `"max_tokens"` key
6. `parse_tagged_reasoning` → gates `TaggedReasoningSplitter` usage
7. `reasoning_token_headroom` → replaces the `is_local_openai_compatible_target()`
   gate in `effective_max_tokens()`
8. `replay_thinking_as_text` → model-level override for the provider trait
   method, resolved by the controller before passing to the agent loop

### Phase C — Heuristic compat defaults and cleanup

- Add `default_local_compat()` function alongside
  `default_local_reasoning_capabilities()` to produce heuristic
  `OpenAiCompat` for unconfigured local models
- Ensure the resolution chain works: model compat → provider compat → heuristic
- Remove `ThinkingRequestMode` from `ReasoningCapabilities`
- Remove `request_mode` field
- Remove `NativeReasoningRequestStrategy` enum
- Remove `openai_compatible_backend()` function (backend type is now
  expressed through compat flags, not inferred from provider name/URL)
- Migrate `thinking_request_mode` config key to `[compat] thinking_format`
  with deprecation support

### Phase D — Config template and docs

- Update default config template with compat examples
- Document all compat fields
- Add a `docs/local_models.md` guide with recommended configs for common
  local model families (Qwen, DeepSeek, Llama, Mistral)
- Update `docs/ideas.md` to mark the compat system as implemented

## What this gives Anie that it doesn't have today

1. **Per-model backend quirk control** without code changes
2. **Provider-level defaults** that apply to all models under a provider
3. **Qwen thinking support** (`enable_thinking` / `chat_template_kwargs`) for
   non-Ollama servers that don't translate `reasoning_effort` automatically
4. **Correct Qwen native reasoning** — Qwen 3/3.5 are always treated as
   genuinely reasoning-capable, not prompt-steering-only
5. **developer role support** for hosted OpenAI reasoning models
6. **stream_options control** without relying on error-based fallback
7. **Tagged reasoning opt-out** for models that use `<think>` literally
8. **max_completion_tokens** for newer OpenAI models
9. **Model-level thinking replay control** beyond the provider-level default
10. **Clean separation** between model capabilities and backend wire format

## What this gives Anie relative to pi

| Feature | pi | Anie (after this) |
|---------|----|--------------------|
| Compat flags | ✅ JSON | ✅ TOML |
| Provider-level defaults | ✅ | ✅ |
| Model-level overrides | ✅ | ✅ |
| Heuristic fallbacks | ❌ | ✅ |
| Prompt steering | ❌ | ✅ |
| Completion validity checks | ❌ | ✅ |
| Thinking replay policy | partial (`requiresThinkingAsText`) | ✅ (provider + model level) |
| Auto local-server detection | ❌ | ✅ |
| TUI model picker | ✅ | ✅ |
| Qwen thinking formats | ✅ `qwen`, `qwen-chat-template` | ✅ `Qwen`, `QwenChatTemplate` |
| Custom reasoning effort mapping | ✅ `reasoningEffortMap` | future (not in this plan) |
