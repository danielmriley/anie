# 06 — Multi-turn replay integration tests

> **Priority: P1 — must land with plan 01.** The signature bug shipped
> because our tests only covered first-turn parsing. No turn-2 replay,
> no 400. Enforces principle 7.

## Why the existing tests missed it

Every `process_event`/`convert_messages` test in
`anie-providers-builtin` feeds a single SSE stream into the state
machine and asserts the final `AssistantMessage`. That is the *first
half* of the lifecycle. The *second half* — serializing that message
back into a subsequent request — has no coverage. So a field that's
silently dropped during parse is invisible to tests because nothing
ever asks "did this field survive round-trip?"

## Target test shape

For each provider under test:

1. Start from a recorded SSE fixture (or hand-authored in the test
   body).
2. Feed it to `process_event` chunk by chunk, then collect the
   `AssistantMessage`.
3. Wrap that message into a `Message::Assistant(...)`, push it into a
   `Vec<Message>` along with a prior user turn and a new user turn.
4. Call `provider.convert_messages(...)` to get the replay-shaped
   `Vec<LlmMessage>`.
5. Call `provider.build_request_body(...)` to get the exact JSON we'd
   send on turn 2.
6. Assert on that JSON: required opaque fields present, dropped fields
   absent, shape valid.

This is the replay-fidelity contract made executable.

## Phase 1 — Harness scaffolding

**File:** `crates/anie-integration-tests/tests/provider_replay.rs` (new).

```rust
struct ReplayFixture {
    name: &'static str,
    sse_chunks: Vec<(&'static str, &'static str)>, // (event_type, data)
    follow_up_user: &'static str,
}

fn drive_stream<P: Provider>(provider: &P, fixture: &ReplayFixture) -> AssistantMessage { ... }
fn build_turn2_body<P: Provider>(provider: &P, ...) -> serde_json::Value { ... }
```

Keep the harness provider-agnostic so the same fixtures can be reused
across providers where it makes sense.

## Phase 2 — Anthropic fixtures

**Goal:** At least the following turn-2 replay scenarios pass.

### Fixture: `anthropic_thinking_signature_replay`

- Turn 1 SSE sequence:
  - `message_start` with input_tokens usage.
  - `content_block_start` index 0, type `thinking`.
  - `content_block_delta` index 0, `thinking_delta` "Let me consider…".
  - `content_block_delta` index 0, `signature_delta` "SIG_XYZ_abc123".
  - `content_block_stop` index 0.
  - `content_block_start` index 1, type `text`.
  - `content_block_delta` index 1, `text_delta` "Here's the answer.".
  - `content_block_stop` index 1.
  - `message_delta` with stop_reason `end_turn`.
  - `message_stop`.
- Assertions on the collected `AssistantMessage`:
  - First content block is `Thinking { signature: Some("SIG_XYZ_abc123"), .. }`.
  - Second is `Text { text: "Here's the answer." }`.
- Assertions on turn-2 request body:
  - `messages[1].content[0]` is `{"type":"thinking","thinking":"…","signature":"SIG_XYZ_abc123"}`.
  - `messages[1].content[1]` is `{"type":"text","text":"Here's the answer."}`.
  - No stray cache_control markers on assistant content.
  - Exactly 2 cache_control markers total (system + last tool).

### Fixture: `anthropic_signature_split_across_deltas`

Same as above but the signature arrives in two `signature_delta`
events. Asserts concatenation.

### Fixture: `anthropic_redacted_thinking_replay`

- Turn 1 SSE: a `content_block_start` with type `redacted_thinking`
  carrying a `data` field.
- Assertion: collected block is `ContentBlock::RedactedThinking { data: ... }`.
- Assertion: turn 2 body replays it verbatim.

### Fixture: `anthropic_legacy_unsigned_thinking_is_dropped`

- Construct an `AssistantMessage` directly (no SSE) with
  `ContentBlock::Thinking { thinking: "old", signature: None }`.
- Run it through the sanitizer + convert_messages + build_request_body.
- Assertion: turn 2 body has **no** thinking block in the replayed
  assistant turn, but still includes text/tool_use blocks.

## Phase 3 — OpenAI fixtures

**Goal:** Lock in the existing correct behavior so regressions are
caught. OpenAI chat-completions deliberately drops thinking on replay;
this must stay true.

### Fixture: `openai_thinking_stripped_on_replay`

- Turn 1 SSE: native `reasoning` deltas followed by visible content.
- Assertion: turn 2 body's assistant message has no reasoning field
  and no embedded `<think>` tags.

### Fixture: `openai_tool_call_id_roundtrip`

- Turn 1 SSE: a tool_call with `id: call_abc`.
- Turn 2 input: include a `ToolResult { tool_call_id: "call_abc", ... }`.
- Assertion: turn 2 body's `tool` message has `tool_call_id: "call_abc"`.

## Phase 4 — Cross-provider invariant test

Enumerate all providers and assert on each:

- Tool-call IDs round-trip.
- Cache-control marker count ≤ 4 (regression guard for the earlier
  cache_control fix).
- `build_request_body` output is valid JSON and parses back.
- No unexpected null fields (e.g., `signature: null`).

This is the test that catches the *next* bug in this family before it
ships.

## Phase 5 — Fixture format

Two options; pick one in phase 1:

### Option A — Inline fixtures

SSE chunks as `&'static str` in the test file. Easy to read; pairs
with assertions immediately.

**Pro:** No external files, self-contained test.
**Con:** Harder to generate from live recordings.

### Option B — JSON fixture files

`crates/anie-integration-tests/fixtures/anthropic/signature_replay.sse`
and `.expected.json` pairs.

**Pro:** Can be generated by a small capture tool that hits the real
API once and records the stream.
**Con:** More files to maintain; risk of fixture drift.

**Recommendation:** Option A for the initial round — it's faster to
land and easier to review. Option B if the fixture count grows past
~15.

## Phase 6 — Live-API smoke gate (optional, gated)

A separate test binary, behind `#[cfg(feature = "live-api")]` or an
env var:

- Reads `ANTHROPIC_API_KEY` from env.
- Sends a short two-turn conversation with thinking enabled.
- Asserts no 400.

Not run in CI by default (requires network + a valid key). Run
locally before release. This is the "does the abstract theory match
reality" gate that prevents plan 01's regression from shipping again.

## Phase 7 — Integration with existing test infrastructure

Hook the new test binary into the project's existing
`cargo test --workspace` pass. `crates/anie-integration-tests` already
contains live-provider integration tests
(`tests/session_resume.rs`, `tests/agent_tui.rs`) so the plumbing is
there.

## Exit criteria

- [ ] Phase 1 harness lands.
- [ ] All fixtures in phases 2 and 3 pass.
- [ ] Phase 4 cross-provider invariant test lands and is green.
- [ ] A regression test for the plan 01 bug specifically fails against
      the pre-fix code and passes against the post-fix code.
- [ ] CI runs the suite on every PR.

## Out of scope

- Fuzzing the SSE parser with malformed input. Separate concern; own
  plan if we want it.
- Load testing / benchmarks. Different kind of test.
- UI-layer rendering tests for assistant messages — already covered
  in `anie-tui` tests.
