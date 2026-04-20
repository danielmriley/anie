# 02 — Anthropic `redacted_thinking` block support

> **Priority: P1.** No production failure yet, but the same silent-discard
> pattern that caused plan 01's bug is in place for `redacted_thinking`
> blocks. Enforces principles 1, 2, 4, 6.

## Background

When Anthropic's safety systems classify a model's internal reasoning
as sensitive, the API does not send the reasoning text — it sends a
`redacted_thinking` block with an opaque encrypted `data` field. The
block must be replayed verbatim on subsequent turns when thinking is
enabled, just like a signed `thinking` block. Dropping it silently
produces the same class of 400 that plan 01 addresses (and, in some
edge cases, produces "missing required block" errors).

## Current state

The Anthropic stream state machine at
`crates/anie-providers-builtin/src/anthropic.rs:356-391` matches on
`content_block.type` for `text`, `thinking`, and `tool_use`, with a
fallthrough `_ => {}` at line 390. `redacted_thinking` falls into that
arm and is silently discarded — in both its start event and any
subsequent deltas.

The protocol also has no way to represent a redacted thinking block:
`ContentBlock` in `crates/anie-protocol/src/content.rs` only knows
`Text`, `Image`, `Thinking`, `ToolCall`.

## Design outline

Add a `ContentBlock::RedactedThinking { data: String }` variant. Capture
the block during streaming. Never display it in the UI (render as an
elided placeholder). Replay it verbatim on Anthropic calls. Drop it on
providers that don't understand it (OpenAI + local).

## Phase 1 — Protocol variant

**Files:** `crates/anie-protocol/src/content.rs`,
`crates/anie-protocol/src/tests.rs`.

Add:

```rust
#[serde(rename = "redactedThinking")]
RedactedThinking { data: String },
```

Wire name is `redactedThinking` to match anie's existing camelCase
convention for internal tags (`toolCall`). On the Anthropic wire format
the type is `redacted_thinking` — translation is a per-provider concern,
not a protocol concern.

Add a roundtrip test.

## Phase 2 — Capture during streaming

**Files:** `crates/anie-providers-builtin/src/anthropic.rs`.

Extend the state-machine enum:

```rust
enum AnthropicBlockState {
    Text(String),
    Thinking(AnthropicThinkingState),
    RedactedThinking(String),   // opaque `data` payload
    ToolUse(AnthropicToolUseState),
}
```

In `content_block_start` (`anthropic.rs:359-391`):

```rust
Some("redacted_thinking") => {
    let data = block.get("data")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    self.blocks.insert(index, AnthropicBlockState::RedactedThinking(data));
}
```

In `content_block_delta`, add handling for any `redacted_thinking`
delta variant Anthropic may emit (docs suggest the block is atomic,
but defensive handling is cheap).

In `to_content_block`:

```rust
Self::RedactedThinking(data) => ContentBlock::RedactedThinking { data: data.clone() },
```

## Phase 3 — Replay on Anthropic

**Files:** `crates/anie-providers-builtin/src/anthropic.rs`.

Update `content_blocks_to_anthropic` (currently at
`anthropic.rs:248-272`):

```rust
ContentBlock::RedactedThinking { data } => json!({
    "type": "redacted_thinking",
    "data": data,
}),
```

## Phase 4 — Suppress on other providers

**Files:** `crates/anie-providers-builtin/src/openai/convert.rs`,
`crates/anie-agent/src/agent_loop.rs`.

- OpenAI's `assistant_message_to_openai_llm_message` (convert.rs:19)
  already filters out non-text content; `RedactedThinking` should also
  produce no text. Add explicit handling if needed.
- `sanitize_assistant_for_request` drops thinking blocks for providers
  where `includes_thinking_in_replay() == false`. Extend the same rule
  to `RedactedThinking`.

## Phase 5 — UI rendering

**Files:** `crates/anie-tui/src/app.rs`,
`crates/anie-tui/src/widgets/panel.rs`.

Render `RedactedThinking` as a one-line placeholder styled like a
disabled thinking block:

```
[reasoning redacted]
```

Do not show the `data` field — it is opaque and unreadable anyway.

## Phase 6 — Tests

- SSE-level unit test: feed a `redacted_thinking` `content_block_start`
  with a data field; assert the final `ContentBlock::RedactedThinking`
  carries that data.
- Serializer unit test: `content_blocks_to_anthropic` round-trips the
  block without mutation.
- Replay integration test (see plan 06): two-turn conversation where
  turn 1 returns a redacted thinking block; turn 2's outbound request
  body contains the block verbatim.

## Phase 7 — Rollout

Ship together with or immediately after plan 01. Same integration-test
scaffolding (plan 06) covers both. No model-catalog changes required —
any Anthropic model with thinking enabled can emit redacted blocks.

## Out of scope

- UI affordance for users to re-enable redacted reasoning
  (Anthropic-side decision, not ours).
- Attempting to decrypt or preview `data` (we can't; opaque by design).
