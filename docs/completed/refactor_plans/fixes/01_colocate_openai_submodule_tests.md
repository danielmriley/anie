# Fix 01 — Colocate openai submodule tests

Finishes the work plan 01 started: tests for `streaming`,
`convert`, and `reasoning_strategy` were supposed to live in the
same file as the code they exercise, but they were left in the
shared `mod tests` block at the bottom of
`crates/anie-providers-builtin/src/openai/mod.rs`.

## Motivation

Plan 01 phases 3–5 each had an exit criterion of the form:

> New — unit tests for <submodule>

`tagged_reasoning.rs` correctly has its own 12 inline tests
(`tagged_reasoning.rs:190–321`). The other three submodules do not:

| File | Inline tests today |
|---|---|
| `openai/streaming.rs` | 0 |
| `openai/convert.rs` | 0 |
| `openai/reasoning_strategy.rs` | 0 |

All tests that should live next to that code are instead inside
`openai/mod.rs`'s shared `mod tests` block
(`mod.rs:380–1449`). The file is 1449 LOC because of this. Phase 6's
exit criterion ("`openai/mod.rs` ≤ 800 LOC") is therefore only met
for production code; reviewers seeing the file size cannot tell.

**Why this matters:**

- A 50-line edit to the `Provider` impl surfaces as a change to a
  1449-line file in diffs and `git blame`.
- When we eventually land plan 04 phase 2 (`ToolCallAssembler`), the
  streaming tests migrate alongside the streaming code naturally
  only if they're colocated first.
- The reasoning-fix plan (`docs/reasoning_fix_plan.md`) references
  specific tests by name; finding them is slower when they live
  adjacent to unrelated tests.

## Design principles

1. **Tests live next to the code they test.** The workspace already
   follows this idiom (e.g., `tagged_reasoning.rs`, `retry_policy`,
   `commands.rs`).
2. **Zero behavior change.** This plan renames files — no logic is
   touched.
3. **One commit per submodule.** Each of the three moves is
   independently reviewable.
4. **Helpers follow their primary consumer.** Test-support fns
   (`sample_model`, `assistant_text`, `final_message`,
   `sample_heuristic_local_model`, etc.) move with the tests that
   use them most. If a helper is used by tests in multiple
   submodules, duplicate it or hoist it to a `#[cfg(test)] mod
   test_support;` sibling — decide per-helper.

## Phase 1 — Inventory

**Goal:** Know exactly which test belongs with which submodule
before moving anything.

### Sub-step A — Classify each test

For each `#[test]` function in `openai/mod.rs:381–1449`, assign one
of: `convert`, `streaming`, `reasoning_strategy`,
`tagged_reasoning` (should be zero — they already moved), or
`stays` (integration-shaped, belongs at the `mod.rs` level because
it drives the full `Provider` impl).

Record the classification in a scratch doc (NOT committed) — e.g.:

| Test | Target |
|---|---|
| `converts_messages_for_openai_chat_completions` | `convert` |
| `openai_provider_does_not_replay_thinking_blocks` | `convert` |
| `skips_empty_assistant_messages_when_converting_messages` | `convert` |
| `accumulates_argument_fragments_into_tool_calls` | `streaming` |
| `handles_missing_usage_fields` | `streaming` |
| `parses_native_reasoning_fields_into_thinking_deltas` | `streaming` |
| `reasoning_only_stream_without_visible_content_is_an_error` | `streaming` |
| `reasoning_with_visible_text_still_succeeds` | `streaming` |
| `same_event_native_reasoning_and_text_are_both_preserved` | `streaming` |
| `tagged_parsing_remains_a_fallback_when_native_reasoning_fields_are_absent` | `streaming` |
| `reasoning_only_assistant_messages_are_omitted_from_openai_replay` | `convert` |
| `thinking_is_omitted_but_text_and_tools_preserved_in_openai_replay` | `convert` |
| `parses_tagged_reasoning_when_opening_tag_is_split_across_chunks` | `streaming` |
| `parses_tagged_reasoning_when_closing_tag_is_split_across_chunks` | `streaming` |
| `parses_multiple_tagged_reasoning_spans_in_one_response` | `streaming` |
| `tagged_reasoning_aliases_all_emit_thinking` | `streaming` |
| `malformed_or_unclosed_tag_sequences_do_not_lose_content` | `streaming` |
| `truly_empty_successful_stop_becomes_stream_error` | `streaming` |
| `reasoning_effort_maps_from_thinking_level` | `reasoning_strategy` |
| `request_body_prepends_system_prompt_and_preserves_message_order` | `stays` (drives `build_request_body`) |
| `request_body_omits_blank_system_prompt` | `stays` |
| `local_effective_system_prompt_varies_by_thinking_level` | `stays` |
| `local_request_body_adds_prompt_steering_without_native_reasoning_fields` | `stays` |
| `hosted_blank_system_prompt_stays_omitted_without_local_prompt_steering` | `stays` |
| `ollama_native_reasoning_profile_emits_top_level_reasoning_effort` | `stays` |
| `vllm_native_reasoning_profile_emits_top_level_reasoning_effort` | `stays` |
| `lmstudio_native_reasoning_profile_uses_nested_reasoning_effort` | `stays` |
| `thinking_off_does_not_force_native_reasoning_fields_for_local_native_models` | `stays` |
| `is_native_reasoning_compatibility_error_only_matches_typed_variant` | `reasoning_strategy` |
| `classify_openai_http_error_upgrades_reasoning_compat_bodies` | `reasoning_strategy` |
| `backend_defaults_resolve_conservatively_for_local_models` | `reasoning_strategy` |
| `local_reasoning_token_headroom_changes_predictably_with_thinking_level` | `reasoning_strategy` |

This table is a starting point — revisit each row when you get to
its move to confirm the test's actual imports match.

### Sub-step B — Classify the helpers

| Helper in `openai/mod.rs` tests | Called by |
|---|---|
| `sample_model()` | most tests |
| `sample_heuristic_local_model(...)` | local / backend tests |
| `sample_local_model()` | local tests |
| `sample_native_local_model(...)` | native-reasoning tests |
| `assistant_text(...)` | streaming tests |
| `assistant_thinking(...)` | streaming tests |
| `final_message(...)` | streaming tests |

These are pure functions that construct `Model` / read
`AssistantMessage` — no state. Duplicate rather than extract a
shared support module; they're ~30 LOC total.

### Exit criteria

- [ ] Every test in `openai/mod.rs` has an assigned destination.
- [ ] Every helper in `openai/mod.rs`'s `tests` module has an
      assigned destination (may be "multiple — duplicate").

---

## Phase 2 — Move `convert` tests

**Goal:** The four tests that exercise
`assistant_message_to_openai_llm_message`,
`llm_message_to_openai_message`, and `join_text_content` live in
`openai/convert.rs`.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/openai/convert.rs` | Add `#[cfg(test)] mod tests { ... }` with the migrated tests + helpers |
| `crates/anie-providers-builtin/src/openai/mod.rs` | Remove the migrated tests + any helpers no longer referenced |

### Sub-step A — Move

Cut each test listed as `convert` in phase 1 from `mod.rs` into
`convert.rs`'s new `mod tests` block. Copy `sample_model` and any
other helpers the migrated tests use; use `use super::*;` inside the
tests module.

### Sub-step B — Clean up the source

Delete every helper in `mod.rs`'s `tests` module that has zero
remaining callers. `cargo test -p anie-providers-builtin` verifies
locally.

### Test plan

| # | Test |
|---|------|
| 1 | `cargo test -p anie-providers-builtin openai::convert::tests` runs the migrated cases |
| 2 | Counts: all previously-passing tests still pass; no test was renamed |
| 3 | Clippy clean |

### Exit criteria

- [ ] `convert.rs` contains tests for all the conversion helpers.
- [ ] `mod.rs` no longer has convert-scoped tests.
- [ ] Net test count is unchanged.

---

## Phase 3 — Move `streaming` tests

**Goal:** Tests exercising `OpenAiStreamState` / `OpenAiToolCallState`
land in `openai/streaming.rs`.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/openai/streaming.rs` | Add `#[cfg(test)] mod tests { ... }` with migrated tests + helpers |
| `crates/anie-providers-builtin/src/openai/mod.rs` | Remove migrated tests + now-dead helpers |

### Sub-step A — Move

Move every test marked `streaming` in phase 1. These tests tend to
construct a `Value` representing an SSE frame, feed it into
`OpenAiStreamState::process_event`, then assert on `ProviderEvent`
emissions or the result of `finish_stream()`.

Duplicate `assistant_text`, `assistant_thinking`, `final_message`
helpers. These are ~15 LOC each and only used here after this
phase.

### Sub-step B — Clean up the source

Remove helpers no longer referenced. `cargo test` verifies.

### Test plan

| # | Test |
|---|------|
| 1 | `cargo test -p anie-providers-builtin openai::streaming::tests` runs every migrated case |
| 2 | Previously-passing tests still pass; names preserved |
| 3 | Clippy clean |

### Exit criteria

- [ ] `streaming.rs` hosts the stream-state tests.
- [ ] `mod.rs` no longer has streaming-scoped tests.
- [ ] Net test count is unchanged.

---

## Phase 4 — Move `reasoning_strategy` tests

**Goal:** Tests exercising `reasoning_effort`,
`classify_openai_http_error`, `openai_compatible_backend`, etc.
live in `reasoning_strategy.rs`.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs` | Add `#[cfg(test)] mod tests` with migrated tests |
| `crates/anie-providers-builtin/src/openai/mod.rs` | Remove migrated tests + now-dead helpers |

### Sub-step A — Move

Move each test marked `reasoning_strategy` in phase 1.

### Sub-step B — Visibility

If any migrated test needs a currently `pub(super)`-or-tighter
item, bump to `pub(crate)` only when necessary. Prefer
`#[cfg(test)] pub(crate)` shims if the production visibility must
stay narrow.

### Test plan

| # | Test |
|---|------|
| 1 | `cargo test -p anie-providers-builtin openai::reasoning_strategy::tests` runs the migrated cases |
| 2 | Previously-passing tests still pass; names preserved |
| 3 | Clippy clean |

### Exit criteria

- [ ] `reasoning_strategy.rs` hosts reasoning-strategy tests.
- [ ] `mod.rs` no longer has reasoning-strategy tests.
- [ ] Net test count is unchanged.

---

## Phase 5 — Tighten `openai/mod.rs`

**Goal:** What's left in `mod.rs`'s `tests` module is the
integration-shaped tests that drive the full `Provider` impl
(`build_request_body*`, `effective_system_prompt`,
`convert_messages` end-to-end).

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/openai/mod.rs` | Remove the dead helpers; confirm remaining tests still compile; update top-of-file doc comment if needed |

### Sub-step A — Verify file size

Run `wc -l crates/anie-providers-builtin/src/openai/mod.rs`. Target
≤ 900 LOC. Plan 01 Phase 6's ≤ 800 target is a stretch — the
remaining `OpenAIProvider` impl is ~370 LOC of production code,
plus ~400 LOC of integration tests. If it ends up over 900,
revisit the classification — a few `stays` tests may belong with
`reasoning_strategy` after all.

### Sub-step B — Module doc comment

Update the top-of-file doc comment on `openai/mod.rs` to say:

```rust
//! OpenAI-compatible chat-completions provider.
//!
//! Composed of four submodules:
//! - `tagged_reasoning` — <think>…</think> extraction
//! - `streaming`        — SSE → `ProviderEvent` reassembly
//! - `convert`          — protocol `Message` ↔ OpenAI wire format
//! - `reasoning_strategy` — native-reasoning request-side policy
//!
//! This file hosts the `Provider` impl itself and the small retry
//! loop around `send_stream_request`. Per-submodule unit tests
//! live alongside their submodule.
```

### Test plan

| # | Test |
|---|------|
| 1 | `cargo test --workspace` passes |
| 2 | `cargo clippy --workspace --all-targets -- -D warnings` passes |
| 3 | `wc -l openai/mod.rs` ≤ 900 |
| 4 | `wc -l openai/mod.rs` for production-only code ≤ 400 |

### Exit criteria

- [ ] `openai/mod.rs` is clean of submodule-scoped tests.
- [ ] Total file size ≤ 900 LOC.
- [ ] Module doc comment reflects the current layout.

---

## Files that must NOT change

- `openai/tagged_reasoning.rs` — already correctly structured.
- Any production code in `openai/*.rs`. This plan is test-move-only.
- `anie-provider/src/error.rs`, `anie-provider/src/provider.rs` —
  no error- or trait-shape changes here.

## Dependency graph

```
Phase 1 (inventory)
  └── Phase 2 (convert) ─┐
  └── Phase 3 (streaming) ─┼── Phase 5 (tighten mod.rs)
  └── Phase 4 (reasoning) ─┘
```

Phases 2–4 are independent after phase 1 and can land in parallel
PRs.

## Out of scope

- Behavior changes or test-logic edits. If a test turns out to be
  flaky or wrong, fix that separately.
- Deleting obsolete tests. Keep everything that currently passes.
- Changing the tagged_reasoning file — it's already correct.
- Test module helpers promoted to production helpers.
