# 01b — Capture Anthropic `signature_delta` during streaming

> Part of **plan 01** (Anthropic thinking-signature replay). Read
> [01_anthropic_thinking_signatures.md](01_anthropic_thinking_signatures.md)
> for symptom and root-cause context.
>
> **Dependencies:** 01a (needs the `signature` field on
> `ContentBlock::Thinking`).
> **Unblocks:** 01c (needs signatures populated on collected blocks so
> the serializer has something to emit).
> **Enforces principles:** 1 (opaque state round-trips), 6 (stream
> parsing is exact; no silent discard).

## Goal

Every `ContentBlock::Thinking` produced by the Anthropic stream state
machine carries the signature Anthropic sent us. No signature deltas
are silently dropped.

## Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/anthropic.rs` | Replace the `Thinking(String)` state variant with a struct carrying both text and signature; handle `signature_delta`; seed any `content_block.signature` at start; emit signature into `ContentBlock::Thinking` on collapse |

## Sub-step A — State-variant change

Current (`anthropic.rs:501-505`):

```rust
enum AnthropicBlockState {
    Text(String),
    Thinking(String),
    ToolUse(AnthropicToolUseState),
}
```

New:

```rust
enum AnthropicBlockState {
    Text(String),
    Thinking(AnthropicThinkingState),
    ToolUse(AnthropicToolUseState),
}

struct AnthropicThinkingState {
    thinking: String,
    signature: String,
}
```

`signature` is a `String` (not `Option<String>`) inside the state
machine because accumulation over deltas is cleaner with a plain
string. The `Option` only appears when we collapse the state into a
`ContentBlock` (sub-step E).

## Sub-step B — `content_block_start`

Currently at `anthropic.rs:364-367`:

```rust
Some("thinking") => {
    self.blocks.insert(index, AnthropicBlockState::Thinking(String::new()));
}
```

New: read `content_block.signature` if present (some SSE
implementations attach a seed signature at start) and seed the state:

```rust
Some("thinking") => {
    let signature = block
        .get("signature")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    self.blocks.insert(
        index,
        AnthropicBlockState::Thinking(AnthropicThinkingState {
            thinking: String::new(),
            signature,
        }),
    );
}
```

## Sub-step C — `signature_delta`

Replace the discard at `anthropic.rs:435` (`Some("signature_delta") => {}`):

```rust
Some("signature_delta") => {
    let signature = delta
        .get("signature")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if let Some(AnthropicBlockState::Thinking(state)) = self.blocks.get_mut(&index) {
        state.signature.push_str(signature);
    }
}
```

Notes:
- Anthropic historically delivers the signature in a single delta,
  but SSE is delta-based by contract — accumulating is the safe
  default and matches the `thinking_delta` handling pattern.
- If the indexed block is not `Thinking`, we silently ignore (can't
  happen in practice; defensive).

## Sub-step D — `thinking_delta`

Update to match the new struct variant. Currently at
`anthropic.rs:406-419`:

```rust
if let Some(AnthropicBlockState::Thinking(existing)) = self.blocks.get_mut(&index) {
    existing.push_str(&thinking);
}
```

becomes:

```rust
if let Some(AnthropicBlockState::Thinking(state)) = self.blocks.get_mut(&index) {
    state.thinking.push_str(&thinking);
}
```

## Sub-step E — Collapse into `ContentBlock`

Update `AnthropicBlockState::to_content_block` at `anthropic.rs:508-520`:

```rust
Self::Thinking(state) => ContentBlock::Thinking {
    thinking: state.thinking.clone(),
    signature: (!state.signature.is_empty()).then(|| state.signature.clone()),
},
```

`None` when the provider sent no signature (shouldn't happen with
thinking enabled, but defensive). `Some(sig)` when populated.

## Verification

### Extend existing SSE test

`anthropic.rs:677` has `parses_sse_events_into_provider_events`.
Extend (or add an adjacent test) to include a thinking block with a
signature:

```rust
#[test]
fn captures_signature_delta_on_thinking_block() {
    let mut state = AnthropicStreamState::new(sample_model());

    state.process_event("message_start",
        r#"{"message":{"usage":{"input_tokens":10}}}"#).unwrap();
    state.process_event("content_block_start",
        r#"{"index":0,"content_block":{"type":"thinking"}}"#).unwrap();
    state.process_event("content_block_delta",
        r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"reasoning"}}"#).unwrap();
    state.process_event("content_block_delta",
        r#"{"index":0,"delta":{"type":"signature_delta","signature":"SIG_abc"}}"#).unwrap();
    state.process_event("content_block_stop", r#"{"index":0}"#).unwrap();
    state.process_event("message_delta",
        r#"{"delta":{"stop_reason":"end_turn"}}"#).unwrap();
    let done = state.process_event("message_stop", "{}").unwrap();

    let ProviderEvent::Done(message) = done.last().expect("done event") else {
        panic!("expected done");
    };
    assert!(message.content.iter().any(|block| matches!(
        block,
        ContentBlock::Thinking { thinking, signature: Some(sig) }
            if thinking == "reasoning" && sig == "SIG_abc"
    )));
}
```

### Split-delta test

```rust
#[test]
fn concatenates_split_signature_deltas() {
    let mut state = AnthropicStreamState::new(sample_model());
    state.process_event("content_block_start",
        r#"{"index":0,"content_block":{"type":"thinking"}}"#).unwrap();
    state.process_event("content_block_delta",
        r#"{"index":0,"delta":{"type":"signature_delta","signature":"PART_A_"}}"#).unwrap();
    state.process_event("content_block_delta",
        r#"{"index":0,"delta":{"type":"signature_delta","signature":"PART_B"}}"#).unwrap();
    state.process_event("content_block_delta",
        r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"x"}}"#).unwrap();
    state.process_event("content_block_stop", r#"{"index":0}"#).unwrap();

    let message = state.into_message();
    assert!(message.content.iter().any(|block| matches!(
        block,
        ContentBlock::Thinking { signature: Some(sig), .. } if sig == "PART_A_PART_B"
    )));
}
```

### Start-seeded signature test

```rust
#[test]
fn uses_content_block_start_signature_when_present() {
    let mut state = AnthropicStreamState::new(sample_model());
    state.process_event("content_block_start",
        r#"{"index":0,"content_block":{"type":"thinking","signature":"SEED"}}"#).unwrap();
    state.process_event("content_block_delta",
        r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"x"}}"#).unwrap();
    state.process_event("content_block_stop", r#"{"index":0}"#).unwrap();

    let message = state.into_message();
    assert!(message.content.iter().any(|block| matches!(
        block,
        ContentBlock::Thinking { signature: Some(sig), .. } if sig == "SEED"
    )));
}
```

## Exit criteria

- [ ] `AnthropicBlockState::Thinking` is a struct variant with
      `thinking` and `signature` fields.
- [ ] `signature_delta` is no longer matched-and-discarded.
- [ ] `content_block_start` reads any seed signature.
- [ ] Three new tests (baseline, split-delta, start-seeded) pass.
- [ ] `cargo test --workspace` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.

## Out of scope (handled by later sub-plans)

- Emitting the signature on outbound requests — see **01c**.
- Sanitizing signature-less blocks — see **01c**.
- Session migration tests — see **01d**.
