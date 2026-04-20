# 00 — Principles for outbound API integrity

> Cross-cutting invariants that every plan in this folder must respect.
> If you're about to write provider code and you can't cite which of
> these principles applies, stop and check.

## 1. Provider-authoritative opaque state must round-trip verbatim

If a provider mints a field we did not produce — a cryptographic
signature, an encrypted reasoning payload, an opaque handle, a
provider-assigned ID — we **must** capture it on stream-in and emit it
byte-for-byte on replay. We never re-derive it, never regenerate it,
never omit it.

Current failure: `signature_delta` events on Anthropic thinking blocks
are dropped at `crates/anie-providers-builtin/src/anthropic.rs:435`.
This is the single principle that, had it been enforced, would have
prevented the production 400.

**Implementation rule.** Any `_ => {}` arm in a stream state machine
that handles a content-block-bearing event (`content_block_delta`,
`content_block_start`, `delta` in OpenAI chunks) is a bug candidate.
Either handle the case or log it and fail loudly. Silent discard is
banned.

## 2. Visible content and opaque state live on the same `ContentBlock`

The signature of a thinking block is not a sibling of the block — it
is *part of* the block. Storing it elsewhere (a side table keyed by
index, a separate `signatures: Vec<...>` field on `AssistantMessage`)
invites desynchronization between the two when messages are filtered,
merged, or compacted.

**Implementation rule.** Protocol extension is the right shape. Add
fields directly on the `ContentBlock` variant. See plan 05 for the
migration mechanics.

## 3. Replay is a hot code path — sanitize once, at the boundary

The agent loop calls `provider.convert_messages(&sanitized_context)`
(`crates/anie-agent/src/agent_loop.rs:406`). That is the one sanctioned
place to drop or reshape blocks before they hit the wire. Provider code
downstream must not further filter, because by that point the structure
is provider-native and tracing what got dropped is painful.

**Implementation rule.** Sanitization logic (drop, trim, filter) lives
in `sanitize_context_for_request` and its helpers in `agent_loop.rs`.
Provider `convert_messages` impls are pure translators.

## 4. `includes_thinking_in_replay` tells the truth

The Provider trait exposes `fn includes_thinking_in_replay() -> bool`
(`crates/anie-provider/src/provider.rs`, impl at `anthropic.rs:192`).
It's a hard contract: if a provider returns `true`, it commits to
replaying thinking blocks **including all opaque state required for
that replay to succeed.** A provider that returns `true` but silently
drops signatures is a bug.

**Implementation rule.** A provider returning `true` from this method
must have a round-trip test that proves the replay succeeds end-to-end
against the real API shape (fixture or live).

## 5. Non-retryable 4xx stays non-retryable

`RetryPolicy::decide` (`crates/anie-cli/src/retry_policy.rs:64`)
correctly treats generic 400s as terminal. Do not add ad-hoc retry
logic for "this particular 400 might be transient" — that is how
infinite loops ship. If a 400 is actually a signal to try a different
request shape (e.g., `NativeReasoningUnsupported`), it gets its own
error variant and its own explicit retry arm with a fallback strategy.

**Implementation rule.** New retry-worthy 4xx signals require a new
`ProviderError` variant and a dedicated arm in `RetryPolicy::decide`.
Not pattern-matching on `body` strings inside a generic `Http` arm.

## 6. Stream parsing is conservative; stream emission is exact

When parsing a stream, unknown fields are preserved (for later replay)
or explicitly rejected — never silently dropped. When emitting a
request, fields are present only if we have the real value; we never
invent, pad, or default opaque provider state.

**Implementation rule.** New `ContentBlock` variants with opaque
fields use `Option<T>` so sessions written before the field existed
still deserialize. Emission code serializes the field only when `Some`;
if the provider requires it and we have `None`, we drop the *entire
block* from replay rather than send a partial one.

## 7. The test harness lives at the replay boundary

Unit tests per provider have historically exercised only the first-turn
parse path. Multi-turn replay — where the opaque state actually
matters — was untested. Every plan in this folder that touches wire
format adds a corresponding replay test (see plan 06).

**Implementation rule.** A change to parser or serializer code must
come with a test that does: parse a fixture → serialize → diff against
an expected wire payload. "Unit test on parse" alone is insufficient.

## 8. Schema change requires a migration story

Protocol types (`anie-protocol`) are serialized into session files on
disk. A field added today will show up as `null` when loading a session
written yesterday. Design the field so that `None` is a legal state and
the replay path handles it safely (typically by dropping the block from
replay; see principle 6). See plan 05 for the full migration checklist.

**Implementation rule.** When adding a field to a `ContentBlock`
variant, include a `#[serde(default)]` test that deserializes an old
payload and round-trips it without loss.

## 9. One source of truth for "what provider supports what"

`ReasoningCapabilities` and the `effective_reasoning_capabilities`
helper in `openai/reasoning_strategy.rs:99` is the pattern: capabilities
are declared on the `Model`, with a fallback heuristic for local
targets. New capability flags (e.g., "requires signature replay",
"supports encrypted reasoning") extend that struct rather than living
as scattered `if provider == "anthropic"` checks.

**Implementation rule.** No new `match provider.as_str()` branches for
capability decisions. Extend `ReasoningCapabilities` (or a sibling
`ReplayCapabilities`) and route through the existing resolver.

## 10. Errors carry enough context for a human to fix the session

When a replay fails because opaque state is missing, the error surfaced
to the UI should say *what* is missing and *what to do* (e.g., "start
a new conversation; session contains thinking blocks without
signatures"). Not "HTTP 400 {\"type\":\"error\"...}".

**Implementation rule.** Plan 04 introduces `ProviderError::ReplayFidelity`.
The UI layer renders it as actionable text, not raw body.

---

These ten principles are the contract. Every plan below cites the
principle(s) it enforces.
