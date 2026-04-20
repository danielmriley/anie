# 01d — Legacy-session migration test

> Part of **plan 01** (Anthropic thinking-signature replay). Read
> [01_anthropic_thinking_signatures.md](01_anthropic_thinking_signatures.md)
> for symptom and root-cause context.
>
> **Dependencies:** 01a (protocol field), 01b (stream capture), 01c
> (sanitizer gate).
> **Unblocks:** 01e (final rollout gate).
> **Enforces principles:** 6 (unknown fields don't break old readers),
> 7 (replay-boundary tests), 8 (migration story is tested, not
> assumed).

## Goal

Prove that sessions written by pre-fix anie binaries (thinking blocks
without a `signature` field) do not produce 400s when replayed against
Anthropic after the fix lands. This is the backstop that confirms
01a's `#[serde(default)]` and 01c's sanitizer work together correctly
for real saved sessions.

## Why this is a separate sub-plan

The protocol (01a) change is forward-compatible by construction and
the sanitizer (01c) is unit-tested, but the full load → sanitize →
send pipeline is not exercised by either alone. A dedicated integration
test catches the case where `#[serde(default)]` is correct, the
sanitizer is correct, but the wiring between them skips a step.

## Files to change

| File | Change |
|------|--------|
| `crates/anie-integration-tests/tests/session_resume.rs` | Add a new test: write a legacy-shape session fixture, load it, run a fake second turn through an Anthropic stub provider, assert the outbound request body contains no thinking block |
| `crates/anie-integration-tests/src/helpers.rs` (if needed) | Add a stub Anthropic provider that records the last request body without hitting the network |

## Sub-step A — Legacy session fixture

Emit a session file with the pre-fix `ContentBlock::Thinking` shape
(no `signature` key). Either:

- **Inline literal:** construct the JSON string in the test body. Fast
  and self-contained.
- **Checked-in fixture:** `tests/fixtures/session_v1_thinking.json`.
  Cleaner if we anticipate more legacy fixtures under plan 05.

Recommendation: inline for 01d; move to a checked-in fixture when
plan 05's schema-migration CI test lands.

```rust
const LEGACY_SESSION_WITH_UNSIGNED_THINKING: &str = r#"{
  "messages": [
    {"role":"user","content":[{"type":"text","text":"first"}],"timestamp":1},
    {"role":"assistant","content":[
      {"type":"thinking","thinking":"prior reasoning"},
      {"type":"text","text":"prior answer"}
    ],"usage":{},"stopReason":"Stop","provider":"anthropic","model":"claude-sonnet-4-6","timestamp":2}
  ]
}"#;
```

(Adjust field naming to match the real session schema confirmed during
plan 05 phase 1.)

## Sub-step B — Stub Anthropic provider

We need to capture the serialized request body without hitting the
network. The cleanest option is a stub that wraps the real Anthropic
provider's `build_request_body` but short-circuits `stream`:

```rust
struct RequestBodyCapturingProvider {
    inner: AnthropicProvider,
    last_body: Arc<Mutex<Option<serde_json::Value>>>,
}

impl Provider for RequestBodyCapturingProvider {
    fn stream(&self, model: &Model, context: LlmContext, options: StreamOptions)
        -> Result<ProviderStream, ProviderError>
    {
        let body = self.inner.build_request_body(model, &context, &options);
        *self.last_body.lock().unwrap() = Some(body);
        // Emit a canned empty-stream so the caller thinks the request succeeded.
        Ok(Box::pin(futures::stream::empty()))
    }
    // Delegate the rest to inner.
    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
        self.inner.convert_messages(messages)
    }
    fn requires_thinking_signature(&self) -> bool {
        self.inner.requires_thinking_signature()
    }
    fn includes_thinking_in_replay(&self) -> bool {
        self.inner.includes_thinking_in_replay()
    }
}
```

Note: `build_request_body` is currently private (`fn` not `pub`). This
sub-plan needs it `pub(crate)` or exposed via a test-only accessor.
Minimal change; no behavior impact.

## Sub-step C — The test

```rust
#[test]
fn legacy_unsigned_thinking_is_dropped_before_replay_to_anthropic() {
    // 1. Deserialize the legacy session.
    let session: Session = serde_json::from_str(LEGACY_SESSION_WITH_UNSIGNED_THINKING)
        .expect("legacy session parses");

    // 2. Confirm the assistant turn has thinking with signature: None.
    let assistant = session.messages.iter().find_map(|m| match m {
        Message::Assistant(a) => Some(a),
        _ => None,
    }).unwrap();
    assert!(assistant.content.iter().any(|b| matches!(
        b,
        ContentBlock::Thinking { signature: None, .. }
    )));

    // 3. Add a new user turn on top.
    let mut messages = session.messages.clone();
    messages.push(Message::User(UserMessage {
        content: vec![ContentBlock::Text { text: "follow up".into() }],
        timestamp: 3,
    }));

    // 4. Run through sanitizer + provider.
    let capturing = make_capturing_anthropic_provider();
    let sanitized = sanitize_context_for_request(&messages, true, true);
    let llm_context = LlmContext {
        system_prompt: String::new(),
        messages: capturing.convert_messages(&sanitized),
        tools: Vec::new(),
    };
    let _ = capturing.stream(&sample_anthropic_model(), llm_context, StreamOptions::default());

    // 5. Assert: the captured request body contains no thinking block.
    let body = capturing.last_body.lock().unwrap().clone().unwrap();
    let messages = body["messages"].as_array().unwrap();
    for message in messages {
        if let Some(content) = message["content"].as_array() {
            for block in content {
                assert_ne!(block["type"], json!("thinking"),
                    "legacy unsigned thinking must not reach the wire");
            }
        }
    }
}
```

## Sub-step D — One-time log notice (optional)

When the sanitizer drops thinking blocks because signatures are
missing, emit a one-time INFO-level log:

```
Dropped N thinking block(s) from replay (session predates signature
capture). Conversation continues normally; earlier internal reasoning
will not be visible to the model.
```

No UI surfacing — log only. This is a breadcrumb for debugging; most
users will never see it.

Files: wherever the agent loop already logs (likely `agent_loop.rs`
or a helper). Keep the log rate-limited to once per session.

## Verification

- The new integration test passes on `refactor_branch` after 01a + 01b
  + 01c are applied.
- The same test, if run against the pre-01c sanitizer, would fail
  (sanity check: delete the `requires_thinking_signature` arm
  temporarily, confirm the test goes red, then restore).

## Exit criteria

- [ ] Legacy-session integration test lands in `session_resume.rs`.
- [ ] Test passes with the full 01a+01b+01c stack.
- [ ] Stub provider is reusable (not a test-local copy each time).
- [ ] `cargo test --workspace` passes.

## Out of scope (handled by later sub-plans)

- Full pre-release smoke test on real Anthropic API — see **01e**.
- Schema-version markers on session files — see plan **05**.
- The cross-provider fixture test suite — see plan **06**.
