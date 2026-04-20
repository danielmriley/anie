# 01 — Anthropic thinking-block signature replay

> ### ⚠️ Use the fine-grained sub-plans for implementation
>
> This file is the **overview** of plan 01 — symptom, root cause, and
> design outline. For step-by-step implementation work, use the
> split-out sub-plans below. Each is independently reviewable and
> ships as its own PR.
>
> | Sub-plan | Scope |
> |----------|-------|
> | [01a_protocol_field.md](01a_protocol_field.md) | Add `signature: Option<String>` to `ContentBlock::Thinking`. No wire behavior change. |
> | [01b_stream_capture.md](01b_stream_capture.md) | Capture `signature_delta` from the Anthropic SSE stream. |
> | [01c_serializer_and_sanitizer.md](01c_serializer_and_sanitizer.md) | Emit signatures on outbound requests; drop signature-less thinking on replay. **This is the phase that fixes the 400.** |
> | [01d_migration_test.md](01d_migration_test.md) | Integration test proving legacy sessions don't poison replay. |
> | [01e_rollout.md](01e_rollout.md) | Pre-merge automated + manual smoke checklist. |
>
> Land in order: **01a → 01b → 01c → 01d → 01e.**
>
> The rest of this file is retained as reference — symptom, root
> cause, and the full design rationale in one place.

---

> **Priority: P0.** This is the current production failure.
> Error shape: `HTTP 400 messages.N.content.M.thinking.signature: Field required`.
> Enforces principles 1, 2, 4, 6, 8.

## Symptom

On any second or later turn that includes a prior assistant message
with a `thinking` content block, Anthropic returns
```
HTTP 400
{"type":"error","error":{"type":"invalid_request_error",
 "message":"messages.1.content.0.thinking.signature: Field required"}}
```
The path `messages.1.content.0` points at a replayed assistant
thinking block that is missing the `signature` field that Anthropic
issued on the original turn.

## Root cause chain

| Step | Location | Behavior |
|------|----------|----------|
| 1 | `crates/anie-providers-builtin/src/anthropic.rs:435` | SSE event `content_block_delta` of type `signature_delta` is matched and **discarded**: `Some("signature_delta") => {}` |
| 2 | `crates/anie-providers-builtin/src/anthropic.rs:501-505` | `AnthropicBlockState::Thinking(String)` has no slot for the signature |
| 3 | `crates/anie-protocol/src/content.rs:14-15` | `ContentBlock::Thinking { thinking: String }` has no `signature` field at the protocol level |
| 4 | `crates/anie-providers-builtin/src/anthropic.rs:261-263` | `content_blocks_to_anthropic` emits `{"type":"thinking","thinking":"…"}` — no signature because none was retained |
| 5 | `crates/anie-providers-builtin/src/anthropic.rs:192-194` | `includes_thinking_in_replay() -> true` — so the agent loop **does** replay thinking blocks for Anthropic |
| 6 | Anthropic Messages API | Rejects every replayed thinking block missing `signature` → 400 |

## Design outline

Extend `ContentBlock::Thinking` with an optional `signature: Option<String>`.
Capture `signature_delta` events during Anthropic streaming and
accumulate them on the matching `AnthropicBlockState::Thinking`
variant. Serialize the signature on replay iff present. Drop thinking
blocks that lack a signature before they reach the wire (legacy
sessions + provider bugs).

This honors principles 1 (opaque state round-trips), 2 (state lives on
the block it belongs to), and 6 (sessions without the field still
deserialize; emission is exact).

---

## Phase 1 — Protocol change

**Goal:** `ContentBlock::Thinking` carries an optional signature field.
Sessions written by old binaries still deserialize cleanly.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-protocol/src/content.rs` | Add `signature: Option<String>` to the `Thinking` variant, with `#[serde(default, skip_serializing_if = "Option::is_none")]` |
| `crates/anie-protocol/src/tests.rs` | Add roundtrip test that deserializes the pre-change JSON shape (no `signature` key) and re-serializes without producing a `signature: null` key |

### Sub-step A — Variant shape

```rust
#[serde(rename = "thinking")]
Thinking {
    thinking: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
},
```

Rationale:
- `Option<String>` because older sessions have no signature; new
  first-turn responses may also have no signature if the model emitted
  no signed thinking (rare but possible).
- `skip_serializing_if = "Option::is_none"` keeps session files clean
  and keeps the JSON backward-compatible for anyone reading them with
  older parsers.

### Sub-step B — Fan-out call-site fixes

Every construction of `ContentBlock::Thinking { thinking }` must
become `ContentBlock::Thinking { thinking, signature: None }`. The
compiler drives this; expected touch-points from grep:

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

Non-Anthropic sites use `signature: None` unconditionally — only the
Anthropic stream state has a real signature to pass through.

### Verification

- `cargo check --workspace` compiles.
- New roundtrip test in `anie-protocol/src/tests.rs` passes:
  ```rust
  let old_json = r#"{"type":"thinking","thinking":"hmm"}"#;
  let block: ContentBlock = serde_json::from_str(old_json)?;
  assert_eq!(serde_json::to_string(&block)?, old_json);
  ```

---

## Phase 2 — Capture `signature_delta` during Anthropic streaming

**Goal:** Every thinking block we produce carries the signature
Anthropic sent us, without gaps if the signature arrives in multiple
deltas.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/anthropic.rs` | Extend `AnthropicBlockState::Thinking` to carry a signature string; handle `signature_delta`; pass through any `content_block.signature` at start; emit signature into `ContentBlock::Thinking` on collapse |

### Sub-step A — Struct change

Replace `Thinking(String)` with a struct-variant:

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

### Sub-step B — `content_block_start`

Currently `anthropic.rs:364-367`:
```rust
Some("thinking") => {
    self.blocks.insert(index, AnthropicBlockState::Thinking(String::new()));
}
```

New version: read `content_block.signature` if present (some SSE
implementations attach a seed signature at start) and seed the state:

```rust
Some("thinking") => {
    let signature = block.get("signature")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    self.blocks.insert(index, AnthropicBlockState::Thinking(AnthropicThinkingState {
        thinking: String::new(),
        signature,
    }));
}
```

### Sub-step C — `signature_delta`

Replace the current `Some("signature_delta") => {}` discard at
`anthropic.rs:435` with:

```rust
Some("signature_delta") => {
    let signature = delta.get("signature")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if let Some(AnthropicBlockState::Thinking(state)) = self.blocks.get_mut(&index) {
        state.signature.push_str(signature);
    }
}
```

Note: Anthropic has historically delivered signatures in a single delta,
but the SSE shape is delta-based; accumulating is the safe default.

### Sub-step D — `thinking_delta`

Update `anthropic.rs:406-419` to match the new struct variant:

```rust
if let Some(AnthropicBlockState::Thinking(state)) = self.blocks.get_mut(&index) {
    state.thinking.push_str(&thinking);
}
```

### Sub-step E — Collapse into `ContentBlock`

Update `AnthropicBlockState::to_content_block` at `anthropic.rs:508-520`:

```rust
Self::Thinking(state) => ContentBlock::Thinking {
    thinking: state.thinking.clone(),
    signature: (!state.signature.is_empty()).then(|| state.signature.clone()),
},
```

### Verification

- Extend the existing `parses_sse_events_into_provider_events` test
  (`anthropic.rs:677`) to inject a `signature_delta` between the
  thinking delta and the block stop, and assert the final
  `ContentBlock::Thinking { signature: Some(s), .. }` carries the
  value.
- Add a test that exercises split signature deltas (two `signature_delta`
  events with concatenated payload).

---

## Phase 3 — Emit signatures on the wire

**Goal:** Replayed thinking blocks serialize with their signature;
signature-less thinking blocks are dropped from replay rather than
sent as invalid payloads.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/anthropic.rs` | Update `content_blocks_to_anthropic` to emit `signature` when present; update sanitizer to reject signature-less thinking blocks before they reach the provider |
| `crates/anie-agent/src/agent_loop.rs` | Extend `sanitize_assistant_for_request` to drop thinking blocks lacking a signature when the provider requires one |

### Sub-step A — Serializer

`anthropic.rs:261-263` becomes:

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

### Sub-step B — Sanitizer gate

`sanitize_assistant_for_request` at `agent_loop.rs:940-946` currently
filters whitespace-only blocks and drops thinking entirely for
providers that don't replay it. Add a third rule: if the provider
**does** replay thinking and the block has no signature, drop it.

The cleanest shape is a provider-supplied predicate:

```rust
pub trait Provider {
    // existing methods...
    fn requires_thinking_signature(&self) -> bool { false }
}
```

Anthropic overrides to `true`. Sanitizer:

```rust
ContentBlock::Thinking { signature: None, .. }
    if provider.requires_thinking_signature() => None,
```

This is deliberately conservative: a signature-less thinking block
either came from an old session (pre-phase-1) or from a provider bug.
Dropping it keeps the conversation valid at the cost of losing one
turn's internal reasoning — a trade we should take every time.

### Sub-step C — Drop the beta-header dance?

`anthropic.rs:120-122` sends the `interleaved-thinking-2025-05-14`
beta header when any thinking level is on. This header is correct,
but worth re-reading the docs during this phase to confirm signatures
behave identically with and without it (they should).

### Verification

- New unit test in anthropic.rs: build a context with a signed thinking
  block and an unsigned one; `build_request_body` must contain the
  signed one (with its signature) and not the unsigned one.
- New unit test: `requires_thinking_signature() == true` for Anthropic,
  `false` for OpenAI.

---

## Phase 4 — Session-file migration

**Goal:** Existing sessions on disk (no signature field) load cleanly
and cannot poison subsequent turns.

### Files to change

No code changes beyond phases 1–3 if those are done correctly — the
`#[serde(default)]` + sanitizer combo handles migration on read.
**But:** add an explicit test.

### Sub-step A — Integration test

Add to `crates/anie-integration-tests/tests/session_resume.rs`:

1. Write a session file containing an assistant message with
   `{"type":"thinking","thinking":"..."}` (no signature).
2. Load the session. Confirm `ContentBlock::Thinking { signature: None, .. }`.
3. Run a fake second turn against a stub Anthropic provider.
4. Assert the outbound request body contains **no** thinking block
   (sanitizer dropped it).

This proves legacy sessions don't cause 400s.

### Sub-step B — User-visible notice (optional)

When the sanitizer drops thinking blocks on load, log a one-time INFO
message: `"Dropped N thinking blocks from replay (session predates signature capture)."`
Nothing in the UI — just enough signal in logs to explain reduced
replay fidelity.

---

## Phase 5 — Rollout and verification

**Pre-merge checklist:**

- [ ] `cargo test --workspace` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] The new `parses_sse_events_into_provider_events` variant
      captures and replays a signature.
- [ ] The new session-resume integration test passes.
- [ ] Manual smoke test: two-turn conversation with thinking enabled
      on `claude-opus-4-7`, `claude-sonnet-4-6`, and `claude-haiku-4-5`.
      All three models complete turn 2 without a 400.
- [ ] Manual smoke test: load a session created by the pre-fix binary,
      send another turn. No 400.

**Exit criterion:** the 400 from the original bug report
(`req_011CaCNC8FdNJLYZ2qp8qZsV`) does not reproduce.

## Out of scope

- Redacted thinking (plan 02).
- OpenAI Responses API encrypted reasoning (plan 03).
- UI rendering of signed vs. unsigned thinking (none; signatures are
  never shown to the user).
- Signature *verification* on our end (none; signatures are opaque to
  us and only Anthropic verifies them).
