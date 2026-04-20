# 01a — `ContentBlock::Thinking.signature` protocol field

> Part of **plan 01** (Anthropic thinking-signature replay). Read
> [01_anthropic_thinking_signatures.md](01_anthropic_thinking_signatures.md)
> for symptom and root-cause context.
>
> **Dependencies:** none. This lands first.
> **Unblocks:** 01b (stream capture needs the field to populate),
> 01c (serializer needs the field to emit).
> **Enforces principles:** 2 (state lives on the block), 8 (schema
> migration is forward-compatible).

## Goal

`ContentBlock::Thinking` carries an optional `signature: Option<String>`.
Sessions written by pre-fix binaries still deserialize cleanly. Nothing
about the wire behavior changes yet — all call sites just pass
`signature: None`.

This is deliberately a **no-op on the wire** for turn-1 responses: the
field exists, is default-`None`, and is skipped in serialization when
`None`. The whole point is to land the protocol shape as its own PR
so 01b and 01c can follow without a giant cross-cutting diff.

## Files to change

| File | Change |
|------|--------|
| `crates/anie-protocol/src/content.rs` | Add `signature: Option<String>` to the `Thinking` variant with `#[serde(default, skip_serializing_if = "Option::is_none")]` |
| `crates/anie-protocol/src/tests.rs` | Add forward/backward compatibility roundtrip test |
| (fan-out) — many files | Update every construction of `ContentBlock::Thinking { thinking }` to `ContentBlock::Thinking { thinking, signature: None }` |

## Sub-step A — Variant shape

```rust
#[serde(rename = "thinking")]
Thinking {
    thinking: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
},
```

Rationale:
- `Option<String>`: older sessions have no signature; first-turn
  responses that emit no signed thinking also have `None`.
- `skip_serializing_if = "Option::is_none"`: session JSON stays clean;
  no `signature: null` noise; older readers see an unchanged shape.

## Sub-step B — Fan-out call-site fixes

The compiler drives this. Expected touch-points (grep-confirmed
against `refactor_branch`):

- `crates/anie-cli/src/compaction.rs:127`
- `crates/anie-providers-builtin/src/openai/convert.rs:118`
- `crates/anie-providers-builtin/src/openai/convert.rs:215`
- `crates/anie-providers-builtin/src/openai/convert.rs:234`
- `crates/anie-providers-builtin/src/openai/streaming.rs:227`
- `crates/anie-providers-builtin/src/openai/streaming.rs:334`
- `crates/anie-providers-builtin/src/anthropic.rs:261`
- `crates/anie-providers-builtin/src/anthropic.rs:511`
- `crates/anie-protocol/src/tests.rs:115`
- `crates/anie-integration-tests/tests/session_resume.rs:57, 89`
- `crates/anie-integration-tests/tests/agent_tui.rs:82, 207`
- `crates/anie-tui/src/tests.rs:105, 1273, 1311, 1333`
- `crates/anie-tui/src/app.rs:1288`
- `crates/anie-session/src/lib.rs:918, 1011, 1104, 1182`
- `crates/anie-agent/src/agent_loop.rs:942, 943, 951, 1123, 1163, 1292, 1310, 1336`

All non-Anthropic sites pass `signature: None` unconditionally. The
Anthropic stream state machine also passes `None` at this phase —
actual capture happens in 01b.

Use pattern matching exhaustively where possible; this catches future
additions of the field automatically:

```rust
// Prefer this shape in match arms that don't need the signature:
ContentBlock::Thinking { thinking, signature: _ } => { ... }
```

## Verification

### Compilation

`cargo check --workspace` builds cleanly — any missed call site is a
compile error, so this is self-policing.

### Forward-compatibility test

Add to `crates/anie-protocol/src/tests.rs`:

```rust
#[test]
fn thinking_block_deserializes_without_signature_field() {
    let old_json = r#"{"type":"thinking","thinking":"hmm"}"#;
    let block: ContentBlock = serde_json::from_str(old_json).unwrap();
    assert!(matches!(
        &block,
        ContentBlock::Thinking { thinking, signature: None } if thinking == "hmm"
    ));
}

#[test]
fn thinking_block_without_signature_reserializes_cleanly() {
    let block = ContentBlock::Thinking {
        thinking: "hmm".into(),
        signature: None,
    };
    let serialized = serde_json::to_string(&block).unwrap();
    assert_eq!(serialized, r#"{"type":"thinking","thinking":"hmm"}"#);
    assert!(!serialized.contains("signature"));
}

#[test]
fn thinking_block_with_signature_roundtrips() {
    let block = ContentBlock::Thinking {
        thinking: "hmm".into(),
        signature: Some("SIG".into()),
    };
    let serialized = serde_json::to_string(&block).unwrap();
    assert!(serialized.contains("\"signature\":\"SIG\""));
    let parsed: ContentBlock = serde_json::from_str(&serialized).unwrap();
    assert_eq!(parsed, block);
}
```

### Lint

`cargo clippy --workspace --all-targets -- -D warnings` passes.

## Exit criteria

- [ ] `ContentBlock::Thinking` has the new optional field.
- [ ] `cargo check --workspace` compiles.
- [ ] `cargo test --workspace` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] Three new roundtrip tests exist and pass.

## Out of scope (handled by later sub-plans)

- Capturing the signature from the Anthropic SSE stream — see **01b**.
- Emitting the signature on outbound requests — see **01c**.
- Session migration tests — see **01d**.
- Manual smoke testing — see **01e**.
