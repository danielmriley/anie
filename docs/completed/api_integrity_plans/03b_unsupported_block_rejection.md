# 03b — Explicit rejection of unsupported Anthropic block types

> Part of **plan 03** (round-trip fidelity audit). Read
> [03_roundtrip_fidelity_audit.md](03_roundtrip_fidelity_audit.md) for
> the full field inventory and motivation.
>
> **Dependencies:** 03a (needs the audit so we know what's unhandled),
> though technically not a hard dependency.
> **Unblocks:** nothing; preventive measure.
> **Enforces principles:** 1 (fail loud, don't silently lose state),
> 6 (conservative stream parsing).

## Goal

If Anthropic emits a content block type we don't understand — server
tool calls, web search results, citations, anything else that could
appear if server-side features get enabled later — we **fail the
request** with a typed `ProviderError`, rather than silently dropping
the block and then hitting a less-diagnosable failure on replay.

"Fail loud" is better than "silently lose state" every time.

## Background

As of the current audit (see 03a), Anthropic can emit content blocks
with types we don't parse:

- `server_tool_use` — assistant invocations of server-side tools
  (web search, code interpreter, etc.).
- `web_search_tool_result` — the result payload from those tools.
- `citations` — structured citations on text blocks.

If a user or config enables any of those features, the current
`_ => {}` arm at `anthropic.rs:390` silently drops the block. The
assistant turn is stored without that content. On the next turn, the
replay is missing state the model expected to see — and Anthropic's
API will respond with 400.

We don't support those features today, but we should not fail
*quietly* if they're accidentally enabled.

## Files to change

| File | Change |
|------|--------|
| `crates/anie-provider/src/error.rs` | Add `ProviderError::UnsupportedStreamFeature(String)` variant |
| `crates/anie-providers-builtin/src/anthropic.rs` | In `content_block_start`, reject known-unsupported block types explicitly |
| `crates/anie-cli/src/retry_policy.rs` | Add `UnsupportedStreamFeature` as a `GiveUp` terminal arm |

## Sub-step A — New error variant

In `crates/anie-provider/src/error.rs`, add:

```rust
/// A provider stream contained a content-block or event type that
/// this client does not support. Usually indicates a server-side
/// feature (server tools, citations, web search) was enabled in
/// config but the client was not built to round-trip the resulting
/// blocks. Not retryable.
#[error("Unsupported provider stream feature: {0}")]
UnsupportedStreamFeature(String),
```

## Sub-step B — Explicit rejection

In `anthropic.rs`, inside the `content_block_start` type match:

```rust
match block["type"].as_str() {
    Some("text") => { /* existing */ }
    Some("thinking") => { /* existing (01b wires signature) */ }
    Some("redacted_thinking") => { /* existing (plan 02 wires this) */ }
    Some("tool_use") => { /* existing */ }
    Some(other)
        if other.starts_with("server_tool_use")
            || other.starts_with("web_search")
            || other == "citations" =>
    {
        return Err(ProviderError::UnsupportedStreamFeature(
            format!(
                "anthropic block type '{other}' — server-side tools and \
                 citations are not yet supported by anie. See \
                 docs/api_integrity_plans/03b."
            ),
        ));
    }
    Some(other) => {
        // Truly unknown type — fall through silently but log.
        eprintln!("anthropic: unknown content_block type {other:?} (ignoring)");
    }
    None => {}
}
```

Two policies here — explicit for the *known* server-feature blocks
(error), soft for truly unknown types (log and drop). The known list
is the high-risk set; the soft fallback avoids breaking users on a
brand-new block type we haven't seen yet.

## Sub-step C — Retry policy

In `crates/anie-cli/src/retry_policy.rs`, add an arm to
`RetryPolicy::decide`:

```rust
ProviderError::UnsupportedStreamFeature(_) => RetryDecision::GiveUp {
    reason: GiveUpReason::Terminal,
},
```

The error is terminal by definition — retrying produces the same
unsupported block.

## Sub-step D — UI surfacing

No UI-specific work in 03b. Plan **04** introduces
`ProviderError::ReplayFidelity` and an affordance for replay-related
errors; `UnsupportedStreamFeature` falls into the same UX bucket. When
04 lands, extend the special-case renderer to also handle this
variant.

Until then, it surfaces as "Unsupported provider stream feature: ..."
via `Display`, which is accurate if not pretty.

## Verification

Unit tests in `anthropic.rs`:

```rust
#[test]
fn rejects_server_tool_use_blocks_explicitly() {
    let mut state = AnthropicStreamState::new(sample_model());
    let err = state.process_event(
        "content_block_start",
        r#"{"index":0,"content_block":{"type":"server_tool_use","id":"x","name":"web_search"}}"#,
    ).expect_err("must reject");
    assert!(matches!(err, ProviderError::UnsupportedStreamFeature(_)));
}

#[test]
fn rejects_web_search_result_blocks_explicitly() {
    let mut state = AnthropicStreamState::new(sample_model());
    let err = state.process_event(
        "content_block_start",
        r#"{"index":0,"content_block":{"type":"web_search_tool_result"}}"#,
    ).expect_err("must reject");
    assert!(matches!(err, ProviderError::UnsupportedStreamFeature(_)));
}

#[test]
fn unknown_block_types_are_ignored_softly() {
    let mut state = AnthropicStreamState::new(sample_model());
    let events = state.process_event(
        "content_block_start",
        r#"{"index":0,"content_block":{"type":"futuristic_new_block"}}"#,
    ).expect("soft ignore");
    assert!(events.is_empty());
}
```

And a retry-policy test:

```rust
#[test]
fn unsupported_stream_feature_gives_up() {
    let policy = deterministic_policy(deterministic_config());
    assert_eq!(
        policy.decide(
            &ProviderError::UnsupportedStreamFeature("x".into()),
            0,
            false,
        ),
        RetryDecision::GiveUp { reason: GiveUpReason::Terminal },
    );
}
```

## Exit criteria

- [ ] `ProviderError::UnsupportedStreamFeature` exists.
- [ ] Anthropic parser rejects the three known server-feature block
      types with the typed error.
- [ ] Retry policy treats the error as terminal.
- [ ] Three new parser tests + one retry-policy test pass.
- [ ] `cargo test --workspace` and `cargo clippy --workspace
      --all-targets -- -D warnings` both pass.

## Out of scope

- Actually supporting server tools / citations / web search. That's
  a feature plan, not an integrity plan.
- OpenAI-side equivalent. No known analogous feature on chat-
  completions; Responses API is a future concern.
