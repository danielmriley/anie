# Plan 01 — `openai.rs` module split + streaming tests

## Motivation

`crates/anie-providers-builtin/src/openai.rs` is 2084 lines and
conflates four distinct responsibilities:

1. The `Provider` trait impl and HTTP wiring (`OpenAIProvider`,
   `send_stream_request`-style retry).
2. The streaming state machine (`OpenAiStreamState`,
   `OpenAiToolCallState`).
3. The tagged-reasoning text splitter
   (`TaggedReasoningSplitter` and helpers).
4. Protocol ↔ wire-format conversion
   (`assistant_message_to_openai_llm_message`,
   `llm_message_to_openai_message`, `join_text_content`).

These four concerns change for different reasons, have different
test shapes, and today share one `mod tests` block. The tagged
reasoning splitter in particular is a hand-rolled character-level
state machine with zero unit coverage — `docs/reasoning_fix_plan.md`
depends on changing code in this file and would benefit enormously
from fast-feedback tests.

## Design principles

1. **One file, one reason to change.** Request building, streaming
   reassembly, tagged reasoning, and message conversion each get
   their own module.
2. **No behavioral change.** This plan is pure restructuring +
   additional tests. No logic moves between modules except by
   straight relocation.
3. **Streaming state machines become unit-testable in isolation.**
   After the split, `OpenAiStreamState::process_event` and
   `TaggedReasoningSplitter::drain` must be callable from tests
   without spinning up an HTTP mock.
4. **`openai.rs` stays a facade.** External users
   (`crates/anie-providers-builtin/src/lib.rs`,
   `anie-providers-builtin` re-exports) must not need to change their
   import paths.

## Current file layout (verified 2026-04-17)

| Lines | Contents |
|---|---|
| 1–22 | Imports |
| 23–279 | `OpenAIProvider` struct + impl (construction, `send_stream_request`, native reasoning strategy selection, retry loop) |
| 280–285 | `impl Default for OpenAIProvider` |
| 286–365 | `impl Provider for OpenAIProvider` |
| 366–465 | Message conversion (`assistant_message_to_openai_llm_message`, `llm_message_to_openai_message`, `join_text_content`) |
| 468–515 | `reasoning_effort`, `is_local_openai_compatible_target`, `local_reasoning_prompt_steering` |
| 517–530 | `NativeReasoningRequestStrategy`, `OpenAiCompatibleBackend` |
| 532–587 | `effective_reasoning_capabilities`, `effective_max_tokens`, `openai_compatible_backend` |
| 589–625 | `is_native_reasoning_compatibility_error`, `native_reasoning_delta` |
| 629–779 | `StreamContentPart`, `TaggedReasoningMode`, `TaggedReasoningSplitter` + helpers |
| 782–1019 | `OpenAiStreamState` + impl |
| 1020–1035 | `OpenAiToolCallState` + impl |
| 1036–end | `mod tests` |

---

## Phase 1 — Convert `openai.rs` into a module directory

**Goal:** Turn the file into `openai/` with a `mod.rs` facade. No
logic moves yet. This phase exists to make the subsequent moves
atomic and reviewable one by one.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/openai.rs` | Delete after content moves |
| `crates/anie-providers-builtin/src/openai/mod.rs` | New — full content of the old file, verbatim |
| `crates/anie-providers-builtin/src/lib.rs` | Update `mod openai;` declaration site if explicit (no-op if it's already `mod openai;`) |

### Sub-step A — Rename the file

Move the file to `openai/mod.rs`. Do not edit contents in this
sub-step. Run `cargo check --workspace` before committing.

### Sub-step B — Verify callers

The only caller of this module should be
`crates/anie-providers-builtin/src/lib.rs`. Confirm the declaration
is `pub mod openai;` or `mod openai;` and that public re-exports
(`OpenAIProvider`) still resolve.

### Test plan

| # | Test |
|---|------|
| 1 | `cargo check --workspace` passes unchanged. |
| 2 | `cargo test -p anie-providers-builtin` passes unchanged. |
| 3 | `cargo test -p anie-integration-tests` passes unchanged. |

### Exit criteria

- [ ] `openai.rs` no longer exists at the crate root.
- [ ] `openai/mod.rs` has identical content to the old file.
- [ ] All tests pass.
- [ ] `git log --follow` on the new path shows the prior history.

---

## Phase 2 — Extract `tagged_reasoning` as its own module with full tests

**Goal:** Move the tagged-reasoning splitter to its own module and
backfill unit tests. This is the highest-value chunk to split first
because it is pure code with no HTTP or async dependencies.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/openai/tagged_reasoning.rs` | New — `StreamContentPart`, `TaggedReasoningMode`, `TaggedReasoningSplitter`, `tagged_reasoning_open_tag`, `is_prefix_of_any_open_tag`, `drain_first_char` |
| `crates/anie-providers-builtin/src/openai/mod.rs` | Remove moved items, add `pub(super) mod tagged_reasoning;` and re-export the symbols the rest of the module uses |
| `crates/anie-providers-builtin/src/openai/tagged_reasoning/tests.rs` | New — unit tests (inline `#[cfg(test)] mod tests { ... }` in the same file is also fine) |

(Only 2 real files touched; the third is test code colocated.)

### Sub-step A — Move the code

Cut lines 629–779 from `openai/mod.rs` into
`openai/tagged_reasoning.rs`. Keep items module-private where
possible; expose only what the stream state machine needs.

Items the stream state machine uses externally:

- `TaggedReasoningSplitter` (type + `new`, `drain`, `finish`)
- `StreamContentPart` (used by `drain` return)
- `TaggedReasoningMode` (only if the stream state constructs the
  splitter in a specific mode; check for direct uses)

Mark everything else `pub(crate)` or `pub(super)` at most.

### Sub-step B — Add unit tests

Add a test module with at least the following cases. Each test
constructs a splitter, feeds it input, and asserts on the
`Vec<StreamContentPart>` output. Refer to the current implementation
for tag keywords (`<think>`, `<thinking>`, `<reasoning>`).

Test cases:

| # | Test name | What it checks |
|---|---|---|
| 1 | `plain_text_passes_through` | No tags → one `Text` part with the full input |
| 2 | `think_tag_emits_thinking` | `<think>foo</think>bar` → `[Thinking("foo"), Text("bar")]` |
| 3 | `thinking_tag_emits_thinking` | `<thinking>foo</thinking>` |
| 4 | `reasoning_tag_emits_thinking` | `<reasoning>foo</reasoning>` |
| 5 | `tag_split_across_chunks` | Feed `<thi`, then `nk>foo</think>` — splitter buffers correctly |
| 6 | `partial_open_tag_at_end_is_buffered` | Feed `hello <thi`, assert no output yet; then `nk>x</think>` completes it |
| 7 | `unterminated_open_tag_on_finish_flushes_as_text` | Feed `<think>oops`, call `finish()`, assert the buffered text is emitted as `Text` |
| 8 | `nested_tags_are_treated_as_text` | `<think>outer<think>inner</think></think>` — current behavior documented (match whatever the code does today; this test pins it) |
| 9 | `utf8_boundary_inside_tag_name` | Feed `<thin` then `k>😀</think>`; assert the emoji survives intact |
| 10 | `multiple_thinking_segments` | `<think>a</think>b<think>c</think>` → three parts |
| 11 | `empty_thinking_block` | `<think></think>` → empty `Thinking` or omitted per current behavior (pin it) |
| 12 | `case_sensitivity` | `<THINK>` — whatever the code does today is fine; pin it |

Tests 8, 11, 12 are **characterization tests**: they pin current
behavior, they do not assert a particular correctness story. This is
deliberate: the split is behavior-preserving; if we disagree with
current behavior, change it in a later plan, not here.

### Files that must NOT change

- `openai/mod.rs` except to remove the moved items and declare the
  new submodule.
- `openai/tagged_reasoning.rs` logic should be byte-for-byte identical
  to the old code.

### Exit criteria

- [ ] `TaggedReasoningSplitter` lives in its own file.
- [ ] At least the 12 tests above pass.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] No public API change outside the `openai` module.

---

## Phase 3 — Extract `streaming` (stream state + tool-call state)

**Goal:** Move the streaming reassembly to its own module and add
unit tests covering the specific behaviors called out in
`docs/reasoning_fix_plan.md`.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/openai/streaming.rs` | New — `OpenAiStreamState`, `OpenAiToolCallState` + impls |
| `crates/anie-providers-builtin/src/openai/mod.rs` | Remove moved items, declare submodule |
| `crates/anie-providers-builtin/src/openai/streaming/tests.rs` | New (or inline `#[cfg(test)] mod tests`) — unit tests for stream state |

### Sub-step A — Move the code

Cut lines 782–1035 from `openai/mod.rs` into
`openai/streaming.rs`. Adjust imports. `OpenAiStreamState` may need
to `use super::tagged_reasoning::{TaggedReasoningSplitter,
StreamContentPart};`.

### Sub-step B — Add unit tests

The tests should construct an `OpenAiStreamState`, feed synthetic
`serde_json::Value` events (standing in for decoded SSE frames), and
assert the emitted `ProviderEvent`s plus the terminal result of
`finish_stream()`.

Test cases (minimum):

| # | Test name | What it checks |
|---|---|---|
| 1 | `pure_text_stream_finishes_as_stop` | One content delta + stop reason stop → assistant message with `Text` block, `StopReason::Stop` |
| 2 | `tool_call_reassembly_across_chunks` | Tool-call id in one chunk, name in another, arguments split across three — assembles into a single `ToolCall` block |
| 3 | `multiple_tool_calls_in_one_stream` | Index 0 and index 1 tool calls interleaved; both emit correctly |
| 4 | `native_reasoning_delta_emits_thinking` | Deltas containing `reasoning` field emit `Thinking` content (pins the native path) |
| 5 | `tagged_reasoning_text_extracts_thinking` | Text content containing `<think>…</think>` drains as both `Thinking` and `Text` parts |
| 6 | `reasoning_only_stream_is_an_error` | No visible text, no tool calls, only thinking → `finish_stream()` returns the "empty assistant response" error. This matches the target behavior in `reasoning_fix_plan.md` Phase 1 Sub-step B. |
| 7 | `reasoning_with_visible_text_still_succeeds` | Thinking + text → success. (Same plan, Sub-step B.) |
| 8 | `usage_fields_accumulate` | Multiple chunks reporting partial usage sum correctly |
| 9 | `stop_reason_length_maps_to_max_tokens` | Finish reason `length` → `StopReason::MaxTokens` |
| 10 | `stop_reason_tool_calls_maps_correctly` | Finish reason `tool_calls` → `StopReason::ToolUse` |

### Sub-step C — Align with the reasoning-fix plan

Tests 6 and 7 above are explicitly the tests
`docs/reasoning_fix_plan.md` Phase 1 Sub-step B calls out. After
this plan lands, the reasoning-fix plan can delete that half of its
work and focus only on behavioral change.

### Files that must NOT change

- `openai/mod.rs` except for removing moved items and adding the
  submodule declaration.
- `crates/anie-providers-builtin/src/sse.rs` — SSE parsing stays
  where it is.
- `crates/anie-providers-builtin/src/anthropic.rs` — out of scope.

### Exit criteria

- [ ] `OpenAiStreamState` and `OpenAiToolCallState` live in
      `openai/streaming.rs`.
- [ ] At least 10 unit tests as above pass.
- [ ] Existing integration tests still pass.
- [ ] `openai/mod.rs` is at most ~1000 LOC (down from 2084).

---

## Phase 4 — Extract `convert` (protocol ↔ wire-format conversion)

**Goal:** Move `assistant_message_to_openai_llm_message`,
`llm_message_to_openai_message`, and `join_text_content` out.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/openai/convert.rs` | New — message conversion functions |
| `crates/anie-providers-builtin/src/openai/mod.rs` | Remove moved items, declare submodule |
| `crates/anie-providers-builtin/src/openai/convert/tests.rs` | New — unit tests for message conversion |

### Sub-step A — Move the code

Cut lines 366–465 into `openai/convert.rs`.

### Sub-step B — Add unit tests

| # | Test name | What it checks |
|---|---|---|
| 1 | `user_text_message_roundtrip` | User `Message` → OpenAI shape has `role: "user"` + `content` |
| 2 | `assistant_text_message_roundtrip` | Assistant `Message` with `Text` block → `role: "assistant"` + `content` |
| 3 | `thinking_omitted_from_replay` | Assistant message with only `Thinking` block → message is dropped or has empty content (matches `reasoning_fix_plan.md` Phase 1 Sub-step C) |
| 4 | `thinking_plus_text_keeps_text_only` | Thinking + Text → Text in content, Thinking stripped |
| 5 | `tool_call_serializes_as_openai_tool_call` | Assistant with `ToolCall` block → OpenAI `tool_calls` field |
| 6 | `tool_result_serializes_as_tool_role` | Tool result message → `role: "tool"` with the expected shape |
| 7 | `join_text_content_joins_only_text` | Mixed content → joined text ignores non-text blocks |
| 8 | `image_blocks_survive` | User `Image` block serializes with the expected OpenAI image shape |

### Exit criteria

- [ ] Message conversion is in its own module with unit tests.
- [ ] `openai/mod.rs` is under 900 LOC.
- [ ] No public API change.

---

## Phase 5 — Extract `reasoning_strategy` (request-side reasoning logic)

**Goal:** Move the native-reasoning strategy selection and backend
detection into their own module.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs` | New — `NativeReasoningRequestStrategy`, `OpenAiCompatibleBackend`, `effective_reasoning_capabilities`, `effective_max_tokens`, `openai_compatible_backend`, `reasoning_effort`, `is_local_openai_compatible_target`, `local_reasoning_prompt_steering`, `is_native_reasoning_compatibility_error`, `native_reasoning_delta` |
| `crates/anie-providers-builtin/src/openai/mod.rs` | Remove moved items, declare submodule |
| `crates/anie-providers-builtin/src/openai/reasoning_strategy/tests.rs` | New — unit tests |

### Sub-step A — Move the code

Cut lines 468–625 into `openai/reasoning_strategy.rs`.

### Sub-step B — Add unit tests

| # | Test |
|---|------|
| 1 | `reasoning_effort_maps_levels` | `ThinkingLevel::{Off, Low, Medium, High}` → correct `Option<&str>` |
| 2 | `local_target_detection_by_base_url` | Localhost URLs detected as local |
| 3 | `local_target_detection_by_family` | Known local families flagged |
| 4 | `effective_max_tokens_respects_option_override` | `StreamOptions::max_tokens` beats model default |
| 5 | `compatibility_error_detection` | Known error strings → `true`; unrelated → `false` (pin current behavior) |
| 6 | `backend_detection_lmstudio` / `_ollama` / `_vllm` / `_hosted` | Each known backend detected |

### Exit criteria

- [ ] Reasoning strategy logic is in its own module.
- [ ] `openai/mod.rs` is under 800 LOC and contains only the
      `Provider` impl, HTTP wiring, and module declarations.
- [ ] All tests pass.

---

## Phase 6 — Final pass on `openai/mod.rs`

**Goal:** After phases 2–5, the remaining `mod.rs` should be just:

1. Imports.
2. `OpenAIProvider` struct, construction, and HTTP wiring (the retry
   loop around `send_stream_request`).
3. `impl Provider for OpenAIProvider`.
4. `mod` declarations for the four submodules.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/openai/mod.rs` | Trim; ensure it's ≤800 LOC |
| `crates/anie-providers-builtin/src/openai/http.rs` *(optional)* | Only if the retry loop is large enough to warrant its own module |

### Sub-step A — Decide on one more split

If the `send_stream_request` retry loop and request-body building is
itself >300 LOC, extract it to `openai/http.rs`. If it's smaller,
leave it in `mod.rs`.

### Sub-step B — Re-run the workspace tests

Full workspace build + test + clippy + fmt.

### Exit criteria

- [ ] `openai/mod.rs` ≤ 800 LOC.
- [ ] Four (possibly five) submodules, each with its own tests.
- [ ] `cargo test --workspace` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] No public API change compared to pre-plan.

---

## Files that must NOT change in any phase

- `crates/anie-providers-builtin/src/anthropic.rs` — subject of
  plan 04 and optionally plan 05.
- `crates/anie-providers-builtin/src/model_discovery.rs` — subject
  of plan 04.
- `crates/anie-providers-builtin/src/sse.rs` — fine as-is.
- `crates/anie-provider/src/provider.rs` — trait is untouched.
- `crates/anie-protocol/*` — wire format not redefined.
- Any consumer of `anie_providers_builtin::openai::*` outside the
  crate.

## Dependency graph

```
Phase 1 ──► Phase 2 ──► Phase 3 ──► Phase 4 ──► Phase 5 ──► Phase 6
(rename)   (tagged)    (streaming)  (convert)   (strategy)  (trim)
```

Phases 2–5 are independent of each other after phase 1; they can be
reordered, but doing them in this order minimizes the blast radius
per PR (smallest self-contained piece first).

## Out of scope

- Any behavior change. This is a restructuring + test-backfill plan.
- Tightening `ProviderError` — that's plan 05.
- Unifying OpenAI and Anthropic shared logic — that's plan 04.
- Changes to `docs/reasoning_fix_plan.md` Phase 1 — the behavioral
  fixes stay in that plan, and this plan only makes them cheaper.
