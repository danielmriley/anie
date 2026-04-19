# 01c — Emit signatures on the wire; drop unsigned thinking on replay

> Part of **plan 01** (Anthropic thinking-signature replay). Read
> [01_anthropic_thinking_signatures.md](01_anthropic_thinking_signatures.md)
> for symptom and root-cause context.
>
> **Dependencies:** 01a (protocol field), 01b (stream capture
> populates signatures).
> **Unblocks:** 01d (migration test relies on the sanitizer gate).
> **Enforces principles:** 1 (opaque state round-trips), 3 (sanitize
> at the boundary), 4 (`includes_thinking_in_replay` honors its
> contract), 6 (emission is exact; partial blocks are dropped).

## Goal

- Replayed thinking blocks serialize with their signature when the
  block has one.
- Thinking blocks without a signature are **dropped** from replay
  before they reach the Anthropic wire (they would be rejected with
  400 anyway).
- The policy is driven by a typed provider capability, not a hard-
  coded per-provider `if`.

This is the phase that actually fixes the 400. 01a and 01b were
groundwork; 01c flips the behavior.

## Files to change

| File | Change |
|------|--------|
| `crates/anie-provider/src/provider.rs` | Add `fn requires_thinking_signature(&self) -> bool { false }` on the `Provider` trait (default `false`) |
| `crates/anie-providers-builtin/src/anthropic.rs` | Override to `true`; update `content_blocks_to_anthropic` to emit `signature` when present |
| `crates/anie-agent/src/agent_loop.rs` | Extend `sanitize_assistant_for_request` to drop signature-less thinking blocks when the provider requires one |

## Sub-step A — Provider trait addition

In `crates/anie-provider/src/provider.rs`, add to the trait:

```rust
/// True if the provider's wire format requires an opaque signature
/// on every replayed thinking block. When true, the sanitizer drops
/// thinking blocks that have no signature rather than sending
/// invalid payloads.
fn requires_thinking_signature(&self) -> bool {
    false
}
```

Default `false` — no existing provider breaks.

In `AnthropicProvider` (`crates/anie-providers-builtin/src/anthropic.rs`):

```rust
fn requires_thinking_signature(&self) -> bool {
    true
}
```

> **Note.** Plan **03c** later moves this decision onto the `Model`
> via a `ReplayCapabilities` struct, so the provider can ask the
> model rather than hard-coding. That's a refactor, not a behavior
> change. For 01c we keep it on the provider impl; 03c updates the
> routing.

## Sub-step B — Serializer

Update `content_blocks_to_anthropic` at `anthropic.rs:248-272`.
Currently:

```rust
ContentBlock::Thinking { thinking } => {
    json!({ "type": "thinking", "thinking": thinking })
}
```

Becomes:

```rust
ContentBlock::Thinking { thinking, signature } => {
    let mut block = serde_json::Map::new();
    block.insert("type".into(), json!("thinking"));
    block.insert("thinking".into(), json!(thinking));
    if let Some(signature) = signature {
        block.insert("signature".into(), json!(signature));
    }
    serde_json::Value::Object(block)
}
```

(The sanitizer will have dropped signature-less thinking blocks
before they reach this point when `requires_thinking_signature` is
true; the `if let Some` is defense-in-depth.)

## Sub-step C — Sanitizer gate

`sanitize_assistant_for_request` at `agent_loop.rs:926-960` currently
takes `includes_thinking_in_replay: bool` as a positional arg. Extend
to also take `requires_thinking_signature: bool` (or, cleaner, pass
the provider reference so both are accessible).

Minimal shape — keep positional-bool style to match the existing
function, since the caller is also in `agent_loop.rs`:

```rust
fn sanitize_assistant_for_request(
    assistant: &AssistantMessage,
    includes_thinking_in_replay: bool,
    requires_thinking_signature: bool,
) -> Option<AssistantMessage> {
    if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) {
        return None;
    }

    let content = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if text.trim().is_empty() => None,
            ContentBlock::Thinking { thinking, .. } if thinking.trim().is_empty() => None,
            ContentBlock::Thinking { .. } if !includes_thinking_in_replay => None,
            ContentBlock::Thinking { signature: None, .. }
                if requires_thinking_signature => None,
            _ => Some(block.clone()),
        })
        .collect::<Vec<_>>();

    if content.is_empty()
        || !content
            .iter()
            .any(|block| !matches!(block, ContentBlock::Thinking { .. }))
    {
        return None;
    }

    Some(AssistantMessage {
        content,
        ..assistant.clone()
    })
}
```

Update the caller at `agent_loop.rs:402-403`:

```rust
let sanitized_context = sanitize_context_for_request(
    &context,
    provider.includes_thinking_in_replay(),
    provider.requires_thinking_signature(),
);
```

Propagate the new parameter through `sanitize_context_for_request` and
any intermediate helpers.

## Sub-step D — Beta header re-check

`anthropic.rs:120-122` sends the `interleaved-thinking-2025-05-14`
beta header when `ThinkingLevel != Off`. During this phase, re-read
the Anthropic docs to confirm signature behavior is identical with
and without the header. If there's any divergence, document it in a
comment at the header-insertion site.

No behavior change expected — this is a sanity check, not a code
change.

## Verification

### Serializer test

In `anthropic.rs` tests:

```rust
#[test]
fn thinking_block_serialization_includes_signature_when_present() {
    let signed = content_blocks_to_anthropic(&[ContentBlock::Thinking {
        thinking: "r".into(),
        signature: Some("SIG".into()),
    }]);
    assert_eq!(signed[0]["signature"], json!("SIG"));

    let unsigned = content_blocks_to_anthropic(&[ContentBlock::Thinking {
        thinking: "r".into(),
        signature: None,
    }]);
    assert!(unsigned[0].get("signature").is_none());
}
```

### Sanitizer test

In `agent_loop.rs` tests:

```rust
#[test]
fn drops_unsigned_thinking_when_provider_requires_signature() {
    let assistant = AssistantMessage {
        content: vec![
            ContentBlock::Thinking { thinking: "r".into(), signature: None },
            ContentBlock::Text { text: "answer".into() },
        ],
        // ... other fields
    };
    let sanitized = sanitize_assistant_for_request(&assistant, true, true).unwrap();
    assert_eq!(sanitized.content.len(), 1);
    assert!(matches!(sanitized.content[0], ContentBlock::Text { .. }));
}

#[test]
fn keeps_signed_thinking_when_provider_requires_signature() {
    let assistant = AssistantMessage {
        content: vec![
            ContentBlock::Thinking {
                thinking: "r".into(),
                signature: Some("SIG".into()),
            },
            ContentBlock::Text { text: "answer".into() },
        ],
        // ... other fields
    };
    let sanitized = sanitize_assistant_for_request(&assistant, true, true).unwrap();
    assert_eq!(sanitized.content.len(), 2);
}

#[test]
fn requires_thinking_signature_defaults_false_for_openai() {
    let provider = OpenAIProvider::new();
    assert!(!provider.requires_thinking_signature());
}

#[test]
fn requires_thinking_signature_true_for_anthropic() {
    let provider = AnthropicProvider::new();
    assert!(provider.requires_thinking_signature());
}
```

### End-to-end body test

In `anthropic.rs` tests, construct an `LlmContext` containing an
assistant turn with a signed thinking block, call `build_request_body`,
and assert the serialized `messages[1].content[0].signature` is
present.

## Exit criteria

- [ ] `Provider::requires_thinking_signature` exists with a `false`
      default.
- [ ] `AnthropicProvider` overrides to `true`.
- [ ] `content_blocks_to_anthropic` emits `signature` when present.
- [ ] `sanitize_assistant_for_request` drops signature-less thinking
      blocks when the provider requires them.
- [ ] Four new unit tests (serializer + 3 sanitizer) pass.
- [ ] `cargo test --workspace` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] The original 400 error from Anthropic no longer reproduces in
      a local two-turn conversation with thinking enabled (confirm in
      01e).

## Out of scope (handled by later sub-plans)

- Migrating existing sessions without signatures — see **01d**.
- Pre-release smoke testing — see **01e**.
- Routing the capability through `ReplayCapabilities` on `Model` —
  see **03c**.
