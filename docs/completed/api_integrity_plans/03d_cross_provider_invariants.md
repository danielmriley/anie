# 03d — Cross-provider invariant tests

> Part of **plan 03** (round-trip fidelity audit). Read
> [03_roundtrip_fidelity_audit.md](03_roundtrip_fidelity_audit.md) for
> the full field inventory and motivation.
>
> **Dependencies:** 03a (audit defines what invariants to check).
> **Unblocks:** nothing; regression guard.
> **Enforces principles:** 1, 6, 7 (invariants are executable, not
> prose).
>
> **⚠️ Overlap with plan 06.** Plan 06 builds a replay-fidelity test
> harness fixture-by-fixture. 03d adds a *cross-provider, invariant-
> driven* suite that runs the same assertions against every provider.
> The two are complementary:
>
> - Plan 06 answers: "did this provider correctly round-trip this
>   fixture scenario?"
> - Plan 03d answers: "do all providers satisfy the universal
>   invariants simultaneously?"
>
> If in doubt, land 06 first (its fixtures are more concrete) and
> then 03d (its tests are table-driven over all providers).

## Goal

A single test binary that enumerates every registered provider and,
for each, asserts a fixed list of invariants on the output of
`convert_messages` + `build_request_body` over a common multi-turn
conversation fixture.

## Invariants under test

Each applies to every provider:

1. **Tool-call IDs round-trip.** A `ToolCall { id: "call_xyz", ... }`
   in the input `Vec<Message>` appears as `call_xyz` in the serialized
   request body.
2. **`cache_control` marker count ≤ 4.** For providers that use them
   (Anthropic); zero for others. Regression guard for the earlier
   cache-control fix.
3. **Valid JSON.** The output of `build_request_body` serializes and
   re-parses without error.
4. **No accidental `null` fields.** Serialized body does not contain
   `"signature":null`, `"data":null`, or similar — fields are either
   present with a real value or absent.
5. **Round-trip of required opaque fields.** For each capability
   declared in the model's `ReplayCapabilities` (see 03c), the
   corresponding opaque field appears on replay.
6. **Dropped-on-purpose fields stay dropped.** For OpenAI chat-
   completions, replayed assistant content has no `reasoning`,
   `reasoning_content`, `thinking`, or `<think>` tags. (Explicit
   design choice; see `openai/convert.rs:19` docstring.)

## Files to change

| File | Change |
|------|--------|
| `crates/anie-integration-tests/tests/provider_invariants.rs` | New file: table-driven test over all providers |
| `crates/anie-integration-tests/src/helpers.rs` | Add shared fixture builders (multi-turn conversation with thinking + tool call) |

## Sub-step A — Shared fixture

One `Vec<Message>` fixture is enough to exercise the invariants.
Minimal shape:

```rust
pub fn multi_turn_fixture() -> Vec<Message> {
    vec![
        Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: "compute 2+2".into() }],
            timestamp: 1,
        }),
        Message::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    thinking: "addition is trivial".into(),
                    signature: Some("SIG_1".into()),
                },
                ContentBlock::ToolCall(ToolCall {
                    id: "call_xyz".into(),
                    name: "calculator".into(),
                    arguments: json!({ "op": "add", "a": 2, "b": 2 }),
                }),
            ],
            stop_reason: StopReason::ToolUse,
            // ... other fields
        }),
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "call_xyz".into(),
            tool_name: "calculator".into(),
            content: vec![ContentBlock::Text { text: "4".into() }],
            is_error: false,
            // ... other fields
        }),
        Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: "now try 3+3".into() }],
            timestamp: 5,
        }),
    ]
}
```

## Sub-step B — Provider enumeration

```rust
fn all_providers_with_models() -> Vec<(Box<dyn Provider>, Model)> {
    vec![
        (Box::new(AnthropicProvider::new()), sample_anthropic_model_with_signatures()),
        (Box::new(OpenAIProvider::new()),    sample_openai_model()),
        // add others as they join the tree
    ]
}
```

Keeping this list in one place forces contributors adding a new
provider to think about the invariants.

## Sub-step C — The invariant tests

```rust
#[test]
fn tool_call_id_roundtrips_across_providers() {
    for (provider, model) in all_providers_with_models() {
        let sanitized = sanitize_context_for_request(
            &multi_turn_fixture(),
            provider.includes_thinking_in_replay(),
            model.effective_replay_capabilities().requires_thinking_signature,
        );
        let llm_messages = provider.convert_messages(&sanitized);
        let ctx = LlmContext {
            system_prompt: String::new(),
            messages: llm_messages,
            tools: Vec::new(),
        };
        let body = provider.build_request_body(&model, &ctx, &StreamOptions::default());
        let serialized = serde_json::to_string(&body).unwrap();
        assert!(
            serialized.contains("call_xyz"),
            "provider {} dropped tool_call_id",
            model.provider,
        );
    }
}

#[test]
fn cache_control_marker_count_bounded_across_providers() {
    for (provider, model) in all_providers_with_models() {
        // ... build body as above ...
        let count = count_cache_control_markers(&body);
        assert!(count <= 4, "provider {} has {count} cache_control markers", model.provider);
    }
}

#[test]
fn no_null_artifacts_in_serialized_body() {
    for (provider, model) in all_providers_with_models() {
        // ... build body as above ...
        let serialized = serde_json::to_string(&body).unwrap();
        assert!(
            !serialized.contains("\"signature\":null"),
            "provider {} emitted a null signature", model.provider,
        );
        assert!(
            !serialized.contains("\"data\":null"),
            "provider {} emitted a null data field", model.provider,
        );
    }
}

#[test]
fn required_opaque_fields_present_per_model_capabilities() {
    for (provider, model) in all_providers_with_models() {
        let caps = model.effective_replay_capabilities();
        // ... build body ...
        if caps.requires_thinking_signature {
            // The fixture's thinking block had signature: Some("SIG_1").
            let serialized = serde_json::to_string(&body).unwrap();
            assert!(
                serialized.contains("SIG_1"),
                "provider {} (requires_thinking_signature=true) dropped signature",
                model.provider,
            );
        }
    }
}

#[test]
fn openai_strips_reasoning_on_replay() {
    let provider = OpenAIProvider::new();
    let model = sample_openai_model();
    // ... build body ...
    let serialized = serde_json::to_string(&body).unwrap().to_lowercase();
    assert!(!serialized.contains("reasoning"));
    assert!(!serialized.contains("<think>"));
    assert!(!serialized.contains("addition is trivial"));
}
```

`count_cache_control_markers` is a small helper that walks a
`serde_json::Value` recursively.

## Sub-step D — Make `build_request_body` accessible

Both `AnthropicProvider::build_request_body` and its OpenAI equivalent
are currently private/pub(crate). For this test binary to reach them,
either:

- Promote to `pub(crate)` and expose via a trait method on `Provider`
  (cleanest — also useful for plan 06), or
- Export a test-only helper from each provider crate that wraps the
  call.

Recommendation: add `fn build_request_body(&self, ...) -> serde_json::Value`
to the `Provider` trait with a default implementation that panics
("unimplemented for this provider"). Each provider overrides; the
default keeps the trait additive.

## Verification

- [ ] Each invariant test fails with a clear message when violated.
- [ ] Deliberately introduce a regression (e.g., drop the
      `signature` field in 01c's serializer) and confirm the
      "required_opaque_fields_present" test goes red.
- [ ] Restore and confirm green.

## Exit criteria

- [ ] `tests/provider_invariants.rs` lands.
- [ ] All five invariant tests pass for both Anthropic and OpenAI.
- [ ] A contributor adding a new provider must update the enumeration
      list (force via a comment / test at enumeration point).
- [ ] `cargo test --workspace` green.

## Out of scope

- Fixture files per scenario (lives in plan **06**).
- Live-API smoke tests (lives in plan **06** phase 6).
- Property-based / fuzz testing. Separate concern.
