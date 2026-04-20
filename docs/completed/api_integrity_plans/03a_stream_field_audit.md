# 03a — Stream field audit and round-trip contract doc blocks

> Part of **plan 03** (round-trip fidelity audit). Read
> [03_roundtrip_fidelity_audit.md](03_roundtrip_fidelity_audit.md) for
> the full field inventory and motivation.
>
> **Dependencies:** none. Can land before, alongside, or after 01.
> **Unblocks:** 03b (needs the audit to know which block types exist);
> nothing strictly blocks on this, but future changes to stream parsers
> will be safer once the contract doc blocks are in place.
> **Enforces principles:** 1 (no silent discard), 6 (stream parsing is
> conservative), 7 (replay invariants are discoverable).

## Goal

Every `_ => {}` arm in the stream state machines is either:
- explicitly handled, or
- annotated with a comment stating why the field is safe to drop.

Every provider module carries a top-of-file "round-trip contract"
doc block listing every opaque field it captures and its disposition.

This is preventive maintenance: the next person editing the parser
sees the contract and knows what to preserve.

## Why this is a separate sub-plan

The original plan 03 had phases 1 and 5 as separate items ("audit"
and "documentation"), but they're the same task: make the current
parser behavior self-documenting. Phase 6 (maintenance cadence) is
also folded in because it's a one-sentence calendar reminder, not a
plan of its own.

## Files to audit

- `crates/anie-providers-builtin/src/anthropic.rs`:
  - Top-level event-type match at `anthropic.rs:351-472`.
  - `content_block_start` inner type match at `anthropic.rs:356-391`.
  - `content_block_delta` inner delta-type match at
    `anthropic.rs:396-437`.
- `crates/anie-providers-builtin/src/openai/streaming.rs`:
  - `process_event` at `streaming.rs:52-163`.

## Sub-step A — Audit discipline

Walk through each `match` arm in the files above. For every arm that
is `_ => {}` or equivalent silent fall-through, answer:

1. **What field(s) can land here?** Consult the provider's published
   stream-event schema.
2. **Are any required on replay?** If yes, implement handling (not
   part of 03a — file a follow-up sub-plan).
3. **Why is silent drop safe for the rest?** Document it as a comment.

Example, in `anthropic.rs`'s current `_ => {}` at the tail of the
`content_block_delta` delta-type match (line 436):

```rust
// As of 2026-Q2, Anthropic's content_block_delta emits: text_delta,
// thinking_delta, signature_delta, input_json_delta. Any other
// delta type is either unrelated to content we need to reconstruct
// (e.g., internal telemetry) or a new API feature — if it shows up
// in logs, add handling explicitly rather than relying on this arm.
_ => {}
```

## Sub-step B — Top-of-file contract doc block

Add to the top of `anthropic.rs`:

```rust
//! Anthropic Messages API provider.
//!
//! # Round-trip contract
//!
//! This module guarantees that the following provider-minted opaque
//! fields are preserved through parse → store → replay:
//!
//! | Field                     | Source event            | Landing spot                              |
//! |---------------------------|-------------------------|-------------------------------------------|
//! | `thinking.signature`      | `signature_delta`       | `ContentBlock::Thinking::signature`       |
//! | `redacted_thinking.data`  | `content_block_start`   | `ContentBlock::RedactedThinking::data`    |
//! | `tool_use.id`             | `content_block_start`   | `ToolCall::id`                            |
//! | `tool_use.name`           | `content_block_start`   | `ToolCall::name`                          |
//! | `tool_use.input`          | `input_json_delta`      | `ToolCall::arguments`                     |
//!
//! Stream events we intentionally ignore (not required on replay as
//! of the last audit — see docs/api_integrity_plans/03a for the
//! audit cadence):
//!
//! | Event/field        | Why safe to drop                                    |
//! |--------------------|-----------------------------------------------------|
//! | `ping`             | Heartbeat; no payload.                              |
//! | Usage cache fields | Informational only; server re-derives from request. |
//!
//! If you add a field to this module's parser, add it to the table.
```

And to the top of `openai/streaming.rs`:

```rust
//! OpenAI-compatible streaming reassembly.
//!
//! # Round-trip contract
//!
//! | Field                         | Landing spot                       |
//! |-------------------------------|------------------------------------|
//! | `tool_calls[].id`             | `ToolCall::id`                     |
//! | `tool_calls[].function.name`  | `ToolCall::name`                   |
//! | `tool_calls[].function.args`  | `ToolCall::arguments` (accumulated)|
//!
//! Intentionally dropped on replay:
//!
//! | Field                                 | Why                           |
//! |---------------------------------------|-------------------------------|
//! | `reasoning` / `reasoning_content`     | OpenAI chat-completions does  |
//! | / `thinking` deltas                   | not round-trip reasoning as   |
//! |                                       | assistant content. Captured   |
//! |                                       | for display only.             |
```

## Sub-step C — Maintenance cadence

Add one line to each contract doc block:

```rust
//! Last verified against provider docs: 2026-04-19.
```

Add a calendar reminder (whatever tool the team uses — if none, note
in `docs/ROADMAP.md`) to re-audit quarterly. When the audit happens,
update the date, re-check the tables, move anything new into a
follow-up sub-plan.

## Verification

This phase is documentation + comments. Verify manually:

- [ ] Every `_ => {}` arm in the audited files has either handling
      or a comment explaining why silent drop is safe.
- [ ] Two contract doc blocks exist (one per provider module).
- [ ] Both doc blocks are dated.

No new tests — but if the audit turns up a field that should have
been captured, that's its own sub-plan.

## Exit criteria

- [ ] Audit pass complete on both provider modules.
- [ ] Contract doc blocks landed.
- [ ] Dates present and current.
- [ ] Any "should capture" findings filed as follow-up sub-plans
      (most likely already covered by 01b and 02).

## Out of scope

- Actually implementing capture for newly-identified fields — that's
  a per-field sub-plan (01b, 02, etc.).
- Explicit rejection of unsupported block types — see **03b**.
- Capability-driven routing — see **03c**.
