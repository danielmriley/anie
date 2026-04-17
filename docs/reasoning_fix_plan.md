# Reasoning Completion Semantics — Phased Fix Plan

This plan fixes the bug where local reasoning models can produce thinking-only assistant turns that are accepted as successful completions and then poison subsequent context.

## Root cause summary

Three independent behaviors combine to produce the failure:

1. **Completion validity**: `OpenAiStreamState::has_meaningful_content()` treats a `Thinking`-only response as meaningful content, so a turn with hidden reasoning but no visible answer is accepted as a successful `StopReason::Stop` completion.

2. **Context replay**: `assistant_message_to_openai_llm_message()` joins `Thinking` blocks into the `content` field alongside `Text` blocks, so prior hidden reasoning is replayed back to the model as if it were the assistant's visible answer.

3. **Agent-loop sanitization**: `sanitize_assistant_for_request()` preserves thinking-only assistant messages (the existing test `sanitize_assistant_for_request_preserves_meaningful_thinking` explicitly asserts this behavior).

Together, a local model that emits only hidden reasoning produces a "successful" turn that poisons every subsequent request.

## Design principles (decided in prior discussion)

1. **Hidden reasoning is not completion content.** A successful assistant turn must contain at least one user-visible or actionable block (text, tool call). Thinking-only turns are incomplete.

2. **Reasoning replay is provider-policy-driven.** Whether historical `Thinking` blocks are sent back in future requests is decided per provider, not as an accidental side effect of message conversion.

3. **Local thinking support is model-profile-driven.** Heuristics are acceptable as fallbacks, but the long-term path is explicit model metadata / config.

---

## Phase 1 — Completion validity and context hygiene

**Goal**: fix the bug. No new types, no new config. Targeted behavioral changes only.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-agent/src/agent_loop.rs` | `sanitize_assistant_for_request()` — drop thinking-only assistant messages |
| `crates/anie-providers-builtin/src/openai.rs` | `has_meaningful_content()` — require visible text or tool calls |
| `crates/anie-providers-builtin/src/openai.rs` | `assistant_message_to_openai_llm_message()` — omit `Thinking` blocks from replay content |

### Sub-step A — Agent-loop: drop thinking-only assistant messages from context

In `sanitize_assistant_for_request()`, after filtering empty text and empty thinking blocks, check whether the remaining content contains any **non-thinking** block. If not, return `None`.

Current behavior (test asserts this):
```
Thinking("plan first") + Text("   ") → keeps Thinking("plan first")
```

New behavior:
```
Thinking("plan first") + Text("   ") → None (dropped from context)
Thinking("plan") + Text("real answer") → keeps both
Thinking("plan") + ToolCall(...) → keeps both
```

This ensures that incomplete turns never participate in future requests regardless of provider.

**Update the existing test** `sanitize_assistant_for_request_preserves_meaningful_thinking`:
- Rename to `sanitize_assistant_for_request_drops_thinking_only_messages`
- Assert that a thinking-only message returns `None`
- Add a new test `sanitize_assistant_for_request_preserves_thinking_alongside_visible_content` that asserts thinking is kept when paired with real text or tool calls

### Sub-step B — OpenAI provider: require visible content for successful completion

In `has_meaningful_content()`, change the condition:

Current:
```rust
fn has_meaningful_content(&self) -> bool {
    !self.text.is_empty()
        || !self.thinking.is_empty()
        || self.tool_calls.values().any(|state| !state.id.is_empty())
}
```

New:
```rust
fn has_meaningful_content(&self) -> bool {
    !self.text.is_empty()
        || self.tool_calls.values().any(|state| !state.id.is_empty())
}
```

This means a stream that produces only reasoning and ends with `finish_reason: stop` will return `Err(ProviderError::Stream("empty assistant response"))` from `finish_stream()`.

The existing retry machinery in the controller will then handle this as a transient error.

**Update the existing test** `reasoning_only_stream_with_empty_content_finishes_as_thinking_not_empty_message`:
- Rename to `reasoning_only_stream_without_visible_content_is_an_error`
- Assert that `finish_stream()` returns `Err(ProviderError::Stream(...))` when the only content is reasoning

**Add a new test** `reasoning_with_visible_text_still_succeeds`:
- Assert that a stream with both reasoning and text completes normally

### Sub-step C — OpenAI provider: omit Thinking blocks from history replay

In `assistant_message_to_openai_llm_message()`, change the text-joining filter:

Current:
```rust
let text = assistant_message
    .content
    .iter()
    .filter_map(|block| match block {
        ContentBlock::Text { text } => Some(text.as_str()),
        ContentBlock::Thinking { thinking } => Some(thinking.as_str()),
        _ => None,
    })
    .collect::<Vec<_>>()
    .join("\n");
```

New:
```rust
let text = assistant_message
    .content
    .iter()
    .filter_map(|block| match block {
        ContentBlock::Text { text } => Some(text.as_str()),
        _ => None,
    })
    .collect::<Vec<_>>()
    .join("\n");
```

This means prior hidden reasoning is never replayed to OpenAI-compatible backends as assistant content.

Note: the Anthropic provider already handles `Thinking` blocks correctly via `content_blocks_to_anthropic()`, which serializes them as `{"type": "thinking", "thinking": ...}` — the native Anthropic thinking format. No change needed there.

**Update the existing test** `converts_reasoning_only_assistant_messages_when_converting_messages`:
- Rename to `reasoning_only_assistant_messages_are_omitted_from_openai_replay`
- Assert that `convert_messages()` returns an **empty** Vec (or skips the message) when the assistant message contains only `Thinking`

**Add a new test** `thinking_is_omitted_but_text_and_tools_preserved_in_openai_replay`:
- Assert that for an assistant message with Thinking + Text + ToolCall, the converted message contains text and tool calls but not thinking content

### Files that must NOT change

- `crates/anie-providers-builtin/src/anthropic.rs` — Anthropic's native thinking replay is correct
- `crates/anie-tui/` — no TUI changes; thinking still renders in the transcript
- `crates/anie-session/` — sessions still persist thinking blocks for display on resume
- `crates/anie-protocol/` — `ContentBlock::Thinking` stays as-is

### Test plan

| # | Test | File |
|---|------|------|
| 1 | thinking-only assistant is dropped from sanitized context | `anie-agent` |
| 2 | thinking + visible text is preserved in sanitized context | `anie-agent` |
| 3 | thinking + tool call is preserved in sanitized context | `anie-agent` |
| 4 | reasoning-only OpenAI stream → `ProviderError::Stream` | `anie-providers-builtin` |
| 5 | reasoning + text OpenAI stream → success | `anie-providers-builtin` |
| 6 | reasoning-only assistant message → omitted from OpenAI replay | `anie-providers-builtin` |
| 7 | thinking omitted but text/tools preserved in OpenAI replay | `anie-providers-builtin` |
| 8 | Anthropic thinking replay unchanged (existing tests pass) | `anie-providers-builtin` |

### Exit criteria

- [ ] A local model that returns only hidden reasoning triggers a stream error, not a successful turn
- [ ] Thinking-only assistant messages are dropped from future request context
- [ ] OpenAI-compatible history replay does not include `Thinking` content
- [ ] Anthropic thinking replay is unchanged
- [ ] All existing tests pass (updated as described above)
- [ ] TUI still renders thinking blocks in the transcript
- [ ] Sessions still persist thinking blocks

---

## Phase 2 — Thinking replay policy as an explicit provider contract

**Goal**: make the replay decision explicit rather than implicit in message conversion code.

### Motivation

Phase 1 hard-codes "omit thinking from OpenAI replay" and "include thinking in Anthropic replay" inside each provider's `convert_messages()`. That works, but the decision is implicit.

Making it explicit:
- documents the contract clearly
- lets future providers declare their policy
- makes the agent loop aware of the decision

### Changes

Add a method to the `Provider` trait:

```rust
/// Whether historical thinking blocks should be included when replaying
/// assistant messages back to this provider.
fn includes_thinking_in_replay(&self) -> bool {
    false // safe default
}
```

- `AnthropicProvider` → returns `true` (Anthropic expects thinking blocks back)
- `OpenAIProvider` → returns `false` (default)
- `MockProvider` → returns `false` (default)

Then in `sanitize_context_for_request()` (or a new provider-aware sanitization path), strip `Thinking` blocks from assistant messages **before** calling `convert_messages()` when the provider returns `false`.

This moves the thinking-strip logic out of each provider's `convert_messages()` and into one shared place.

### Test plan

| # | Test |
|---|------|
| 1 | OpenAI provider `includes_thinking_in_replay()` returns false |
| 2 | Anthropic provider `includes_thinking_in_replay()` returns true |
| 3 | Context sanitization strips thinking when provider says false |
| 4 | Context sanitization preserves thinking when provider says true |

### Exit criteria

- [ ] Replay policy is an explicit, documented provider contract
- [ ] No behavior change from Phase 1 — this is a refactor, not a feature

---

## Phase 3 — Richer local thinking profiles

**Goal**: replace heuristic-based local reasoning detection with model-profile-driven configuration.

### Motivation

The current approach uses `is_reasoning_capable_family()` to guess whether a model supports native reasoning based on substrings in the model ID (`qwen3`, `qwq`, `deepseek-r1`, `gpt-oss`). This:

- matches too broadly (e.g. `qwen3.5` is not `qwen3`)
- cannot express per-model differences in request format or output format
- cannot be overridden by users
- cannot capture future models without code changes

### Changes

Extend `ReasoningCapabilities` (or a sibling struct on `Model`) to include:

```rust
/// How thinking should be requested for this model.
pub request_mode: Option<ThinkingRequestMode>,
```

Where:
```rust
pub enum ThinkingRequestMode {
    /// Use prompt-steering text to encourage/discourage reasoning.
    PromptSteering,
    /// Use `reasoning_effort` top-level field (Ollama, most OpenAI-compatible).
    ReasoningEffort,
    /// Use nested `reasoning.effort` field (LM Studio).
    NestedReasoning,
}
```

This replaces the dynamic `NativeReasoningRequestStrategy` detection and caching with a declarative model property.

Allow this to be set via:
1. `builtin_models()` entries
2. `[[providers.*.models]]` config entries (new optional field)
3. `default_local_reasoning_capabilities()` heuristic as fallback
4. Model discovery metadata (future)

### Config example
```toml
[[providers.ollama.models]]
id = "qwen3.5:9b"
name = "Qwen 3.5 9B"
context_window = 32768
max_tokens = 8192
supports_reasoning = true
reasoning_control = "Native"
reasoning_output = "Separated"
thinking_request_mode = "ReasoningEffort"
```

### Test plan

| # | Test |
|---|------|
| 1 | Model with explicit `thinking_request_mode` uses it |
| 2 | Model without `thinking_request_mode` falls back to heuristic |
| 3 | Config-declared `thinking_request_mode` is loaded and applied |
| 4 | Builtin models have correct default profiles |

### Exit criteria

- [ ] Local reasoning request mode is declarative, not guessed at runtime
- [ ] Existing heuristics continue to work as fallbacks
- [ ] Users can override reasoning behavior per model in config
- [ ] No behavior change for correctly-configured models

---

## Dependency graph

```
Phase 1 ──► Phase 2 ──► Phase 3
(fix bug)   (refactor)   (architecture)
```

- **Phase 1** is the correctness fix. Ship it immediately.
- **Phase 2** is a refactor that makes the policy explicit. Can ship with Phase 1 or shortly after.
- **Phase 3** is the long-term architecture improvement from `docs/ideas.md`. Can be planned independently.

---

## What this plan does NOT include

- TUI changes for incomplete-response messaging (can be added later as UX polish)
- Automatic retry with degraded thinking level (recovery policy — separate feature)
- Changes to session persistence format (thinking blocks stay in JSONL for display)
- Changes to Anthropic thinking behavior (already correct)
- New `ThinkingProfile` struct (Phase 3 extends the existing `ReasoningCapabilities` instead)
