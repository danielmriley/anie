# 01e rollout — status

> Companion to [01e_rollout.md](01e_rollout.md). Records which items of
> the rollout checklist were verified automatically by the
> implementation pass, and which need manual user action (a valid
> Anthropic API key, live network, and human judgement on UX) before
> declaring the fix shipped.

## Automated — verified in this pass

- [x] `cargo check --workspace` compiles.
- [x] `cargo test --workspace` passes — 387 tests (vs. 370 baseline).
      The +17 delta covers every test added by 01a, 01b, 01c, 01d,
      and the 01e regression guard:
  - `thinking_content_block_with_signature_roundtrip`
  - `thinking_content_block_deserializes_without_signature_field`
  - `thinking_content_block_without_signature_reserializes_cleanly`
  - `thinking_content_block_with_signature_emits_signature_field`
  - `captures_signature_delta_on_thinking_block`
  - `concatenates_split_signature_deltas`
  - `uses_content_block_start_signature_when_present`
  - `unsigned_thinking_block_has_none_signature`
  - `thinking_block_serialization_includes_signature_when_present`
  - `thinking_block_serialization_omits_signature_when_absent`
  - `anthropic_provider_requires_thinking_signature`
  - `openai_provider_does_not_require_thinking_signature`
  - `sanitize_drops_unsigned_thinking_when_signature_required`
  - `sanitize_keeps_signed_thinking_when_signature_required`
  - `sanitize_drops_assistant_when_only_unsigned_thinking_remains`
  - `cache_control_marker_count_stays_bounded_with_many_tools`
  - `legacy_unsigned_thinking_is_dropped_before_replay`
- [x] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [x] Cache-control marker count regression guard: a 10-tool
      request still produces exactly 2 markers (one on system, one
      on the last tool). See
      `cache_control_marker_count_stays_bounded_with_many_tools`
      in `crates/anie-providers-builtin/src/anthropic.rs`.

## Needs manual action — requires network + API key

These smoke tests hit the real Anthropic API. They must be run from a
built binary with `ANTHROPIC_API_KEY` set, before the fix is declared
shipped. Each family emits slightly different thinking-block behavior,
which is why all three are listed explicitly.

- [ ] **`claude-opus-4-7`**, thinking High
  1. Ask: "What's 7 × 13? Think through it."
  2. Wait for turn-1 response with visible thinking + final answer.
  3. Ask: "Now what's 7 × 14?"
  4. Turn-2 response arrives without HTTP 400.

- [ ] **`claude-sonnet-4-6`**, thinking Medium
  1. Same two-turn flow.
  2. Turn-2 succeeds.

- [ ] **`claude-haiku-4-5`**, thinking Low
  1. Same two-turn flow.
  2. Turn-2 succeeds.

- [ ] **Tool-calling flow with thinking** (any model)
  1. Thinking enabled, tools registered.
  2. Ask something that triggers a tool call.
  3. Tool result returns.
  4. Assistant replies with a second turn (interleaved thinking).
  5. Third turn after a user follow-up succeeds.

- [ ] **Legacy session resume**
  1. Either use a session file created by a pre-plan-01 build, or
     temporarily hand-edit a session file to remove the `signature`
     field from a thinking block.
  2. Open the edited session with the post-fix build.
  3. Send a new user message.
  4. Request succeeds. No 400. Legacy thinking is silently dropped
     from replay (expected).

## Post-merge verification

- [ ] Within 24h of merge, check logs for any `ReplayFidelity` errors
      (once plan 04 lands) or raw `Http { status: 400 }` errors
      mentioning `signature` or `thinking`. Expected count: zero.

## If any manual smoke fails

Do not declare the fix shipped. Likely causes:
- 400 with `thinking.signature` message → 01c sanitizer/serializer
  regressed. Check `content_blocks_to_anthropic` and
  `sanitize_assistant_for_request`.
- 400 with `redacted_thinking` message → an encrypted reasoning
  block reached the wire without handling. Move plan **02** ahead
  of ship.
- 400 with any other message → escalate to plan **04** for
  classification before ship.
