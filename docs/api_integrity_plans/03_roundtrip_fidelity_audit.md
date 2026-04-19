# 03 — Round-trip fidelity audit

> ### ⚠️ Use the fine-grained sub-plans for implementation
>
> This file is the **overview** of plan 03 — motivation and the full
> cross-provider field inventory. For step-by-step implementation
> work, use the split-out sub-plans below.
>
> | Sub-plan | Scope |
> |----------|-------|
> | [03a_stream_field_audit.md](03a_stream_field_audit.md) | Walk every `_ => {}` arm in the stream state machines. Land top-of-file round-trip contract doc blocks. Preventive documentation. |
> | [03b_unsupported_block_rejection.md](03b_unsupported_block_rejection.md) | Fail loudly (typed error) when Anthropic emits a server-tool / web-search / citation block we don't support, instead of silently dropping. |
> | [03c_replay_capabilities.md](03c_replay_capabilities.md) | Refactor: move `requires_thinking_signature` off `Provider` and onto `Model::replay_capabilities`. Matches the existing `ReasoningCapabilities` pattern. |
> | [03d_cross_provider_invariants.md](03d_cross_provider_invariants.md) | Table-driven invariant tests that run against every registered provider (tool-call ID roundtrip, cache_control ≤ 4, no null artifacts, etc.). |
>
> Suggested order: **03a** any time; **03b** after 03a; **03c** after
> plan 01c has landed; **03d** after 03c and plan 06 (whichever fixture
> harness lands first).
>
> The rest of this file is retained as reference — the field inventory
> table is the most useful part and doesn't need rewriting.

---

> **Priority: P1.** Preventive. Defines the full set of invariants that
> every current and future provider impl must satisfy, and the audit
> checklist to prove it. Enforces principles 1, 4, 6, 7, 9.

## Why this plan exists

The thinking-signature bug (plan 01) was not a one-off. The stream
state machine dropped a field it didn't recognize, and nobody noticed
until it broke production. That pattern — silent discard in a
`_ => {}` arm — almost certainly recurs elsewhere. This plan catalogues
every field we know about across the two provider families we support,
identifies what must round-trip and what can safely be dropped, and
encodes those decisions as tests.

## Scope

- Anthropic Messages API (`anie-providers-builtin/src/anthropic.rs`).
- OpenAI chat-completions and local OpenAI-compatible targets
  (`anie-providers-builtin/src/openai/*`).
- Future: OpenAI Responses API (`/v1/responses`) — not yet implemented
  in anie, but included here because its `encrypted_content` is the
  direct analog of Anthropic signatures.

## Field inventory

### Anthropic Messages API

| Field | Block type | Required on replay? | Current handling | Plan |
|-------|-----------|---------------------|------------------|------|
| `thinking` text | `thinking` | Yes (with `signature`) | ✅ captured | plan 01 |
| `signature` | `thinking` | **Yes** | ❌ discarded | **plan 01** |
| `data` | `redacted_thinking` | **Yes** | ❌ discarded | **plan 02** |
| `text` | `text` | Yes | ✅ captured | — |
| `id` | `tool_use` | Yes | ✅ captured | — |
| `name` | `tool_use` | Yes | ✅ captured | — |
| `input` | `tool_use` | Yes | ✅ captured (via partial_json) | — |
| `cache_creation_input_tokens` | usage | No | ✅ tracked | — |
| `cache_read_input_tokens` | usage | No | ✅ tracked | — |
| `citations` | `text` | Unknown / unused today | ❌ not captured | **this plan, phase 2** |
| `server_tool_use` blocks | — | Unknown — feature beta | ❌ not captured | **this plan, phase 2** |
| `web_search_tool_result` blocks | — | Same | ❌ not captured | **this plan, phase 2** |

### OpenAI chat completions

| Field | Required on replay? | Current handling | Plan |
|-------|---------------------|------------------|------|
| `id` on tool_call | Yes | ✅ captured (`streaming.rs:116`) | — |
| `function.name` | Yes | ✅ captured | — |
| `function.arguments` | Yes | ✅ captured | — |
| Native `reasoning` / `reasoning_content` / `thinking` | **Omitted on replay** by design (`convert.rs:19` docstring) | ✅ intentionally dropped | — |
| `tool_call_id` on tool messages | Yes | ✅ captured (`convert.rs:98`) | — |

The OpenAI chat-completions path is in good shape: thinking is
explicitly dropped (correct — OpenAI doesn't round-trip reasoning on
chat-completions), and tool-call IDs round-trip. No immediate bugs.

### OpenAI Responses API (future)

When/if anie grows support for `/v1/responses`, the analog of
Anthropic signatures is:

| Field | Block type | Required on replay? |
|-------|-----------|---------------------|
| `encrypted_content` | `reasoning` | **Yes** (when `store=false`; otherwise `previous_response_id` replaces the entire chain) |
| `id` | `reasoning` | Yes |
| `response_id` chaining | request-level | Yes (with `previous_response_id`) |

This is explicitly out of scope for now. Documented so that whoever
implements the Responses API provider reads this plan first.

## Phase 1 — Audit existing stream state machines

**Goal:** Every `_ => {}` arm in the two provider state machines is
either (a) handled, or (b) explicitly annotated with a comment stating
why the field is safe to drop.

### Files to audit

- `crates/anie-providers-builtin/src/anthropic.rs:356-391`
  (`content_block_start` type match)
- `crates/anie-providers-builtin/src/anthropic.rs:396-437`
  (`content_block_delta` delta-type match)
- `crates/anie-providers-builtin/src/anthropic.rs:351-472`
  (top-level `event_type` match — already fine for `error`, `message_stop`, etc.)
- `crates/anie-providers-builtin/src/openai/streaming.rs:52-163`
  (`process_event` delta fields)

### Deliverable

Add a top-of-file doc block to each state machine listing every field
type it's known to see and the disposition of each. Something like:

```rust
//! Stream field inventory (verify against provider docs quarterly):
//!
//! | Event / field           | Handling                           |
//! |-------------------------|------------------------------------|
//! | content_block_start     | text / thinking / tool_use /       |
//! |                         | redacted_thinking (see plan 02)    |
//! | content_block_delta     | text_delta / thinking_delta /      |
//! |                         | signature_delta / input_json_delta |
//! | content_block_stop      | finalize partial tool args         |
//! | message_start           | seed usage                         |
//! | message_delta           | stop_reason / usage                |
//! | message_stop            | emit Done                          |
//! | error                   | propagate as stream error          |
//! | <anything else>         | ignore — never required on replay  |
//! |                         | per Anthropic docs as of YYYY-MM   |
```

This forces a human to re-check the docs when the table ages.

## Phase 2 — Citations and server-tool blocks

Anthropic's API can return `citations` on text blocks (when tools like
web_search are used) and `server_tool_use` / `web_search_tool_result`
content blocks. None of these are captured today. Risk: if anie ever
enables server-side tools, replay will 400.

**Recommendation:** until anie enables server-side Anthropic tools,
*explicitly reject* requests that return these block types with a
`ProviderError::UnsupportedFeature("server_tool_use")` rather than
silently dropping them. Better to fail loudly than silently lose
state.

**Files:** `crates/anie-providers-builtin/src/anthropic.rs`
(`content_block_start` match and the `_` fallthrough).

```rust
Some(other) if other.starts_with("server_tool_use")
            || other.starts_with("web_search")
            || other == "citations" => {
    return Err(ProviderError::MalformedStreamEvent(
        format!("unsupported Anthropic block type: {other} (see api_integrity_plans/03)"),
    ));
}
_ => {} // truly unknown — fall through; investigate if seen in logs
```

## Phase 3 — Model-declared replay capabilities

**Goal:** Move ad-hoc per-provider replay knowledge out of provider
code and onto the `Model` struct, matching the pattern
`ReasoningCapabilities` already uses (`anie-provider/src/model.rs`).

New struct:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayCapabilities {
    pub requires_thinking_signature: bool,
    pub supports_redacted_thinking: bool,
    pub supports_encrypted_reasoning: bool, // future
}
```

Wire through `Model`, set for Anthropic models in the model catalog,
default `false` for everything else.

Update:
- `Provider::requires_thinking_signature` (introduced in plan 01) can
  then read from the model rather than being a hard-coded impl-level
  `true`.
- `sanitize_assistant_for_request` routes through the model's
  capabilities, not `includes_thinking_in_replay()`.

This is the enforcement of **principle 9** (one source of truth for
capability flags).

## Phase 4 — Cross-provider invariant tests

**Goal:** A single table-driven test that loads a multi-turn session
fixture, runs it through each provider's `convert_messages` +
`build_request_body`, and asserts:

1. Tool-call IDs round-trip.
2. Required opaque fields (per `ReplayCapabilities`) are present.
3. Dropped fields are actually dropped (no accidental leaks).
4. No `cache_control` marker count exceeds 4 (covers the earlier bug
   in the same test).

Location: `crates/anie-integration-tests/tests/provider_roundtrip.rs`
(new file).

## Phase 5 — Documentation invariants

**Goal:** Each provider module carries an up-to-date "round-trip
contract" doc block that lists every opaque field it is responsible
for capturing. Reviewers check it during PR review when the stream
parser changes.

Add to the top of `anthropic.rs` and `openai/streaming.rs`:

```rust
//! # Round-trip contract
//!
//! This module guarantees that the following opaque provider-minted
//! fields are preserved through parse → store → replay:
//! - ...
//!
//! See docs/api_integrity_plans/03_roundtrip_fidelity_audit.md for
//! the full table. If you add a field to the parser, add it here.
```

## Phase 6 — Maintenance cadence

Quarterly (add a calendar reminder in whatever tracker the team uses):

1. Re-read the Anthropic Messages API and OpenAI chat-completions
   changelogs for new content-block types or stream event types.
2. Update the field inventory in this doc.
3. Decide disposition for any new field.
4. Ship handling or explicit rejection.

## Exit criteria

- [ ] Phase 1 audit complete; doc blocks in place.
- [ ] Phase 2 server-tool rejection shipped.
- [ ] Phase 3 `ReplayCapabilities` on `Model`, used by sanitizer.
- [ ] Phase 4 cross-provider test lands and is green.
- [ ] Phase 5 doc blocks reviewed in a real PR.
- [ ] Calendar reminder for phase 6 exists.
