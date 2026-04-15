# TUI Thinking Presentation Follow-up Plan

This document is a focused follow-up plan for the TUI assistant transcript presentation.

It is specifically about **how thinking content is visually presented** once it is already being parsed and rendered in the correct top-before-answer order.

## Why this follow-up exists

The recent ordering fix makes thinking appear above the visible answer, which is the correct semantic order.

However, the current presentation is still visually weak:
- thinking and answer content live in the same assistant block with only a blank-line separator
- thinking is rendered as a single prefixed paragraph: `thinking: ...`
- long wrapped thinking can still feel visually blended with the final answer
- the streaming indicator is generic and does not help distinguish whether the model is still thinking or already answering

So the next improvement is not about provider correctness or transcript order.
It is about **readability and separation** in `anie-tui`.

---

## Primary outcomes required from this work

By the end of this follow-up:
- thinking remains visually above the visible answer
- thinking is clearly distinguishable from the final answer at a glance
- long wrapped thinking remains readable and does not visually merge into the answer body
- streamed and replayed assistant messages render with the same section structure
- this remains a pure TUI presentation change

---

## Current code facts

In `crates/anie-tui/src/output.rs` today:
- assistant rendering already places thinking before visible answer text
- thinking is rendered with:
  - dark gray
  - italic style
  - a simple `thinking: {thinking}` prefix
- answer text is rendered as ordinary plain assistant text
- a blank line is inserted between thinking and answer when both exist
- the streaming indicator is appended as a generic trailing line: `⠋ streaming...`

This means the semantic order is now right, but the visual distinction is still too weak.

In `crates/anie-tui/src/tests.rs` today:
- there are tests that lock in ordering
- there is not yet a richer visual contract for how thinking should be separated from the answer

---

## Files expected to change

Primary:
- `crates/anie-tui/src/output.rs`
- `crates/anie-tui/src/tests.rs`

Possible small touch:
- `crates/anie-tui/src/app.rs`
  - only if the streaming indicator wording/placement needs a small app-level adjustment

No provider/session/controller changes should be required.

---

## Constraints

1. Keep this fully inside `anie-tui`.
2. Do not change provider, protocol, session, or controller behavior.
3. Do not add config knobs yet.
4. Preserve the current transcript ordering invariant:
   - thinking first
   - answer second
5. Avoid overly heavy chrome that wastes too much terminal width.
6. Do not make the answer text harder to scan just to decorate thinking.

---

## Recommended presentation direction

Use a **lightweight dedicated thinking section**, not just an inline prefix.

Recommended first version:
- a short muted heading, for example `thinking`
- thinking body rendered beneath it with a subtle left gutter / indentation
- a blank line between the thinking section and the visible answer
- answer text remains plain and prominent

Conceptually:

```text
thinking
│ plan the change
│ inspect the error path
│ verify the final state

Final answer starts here.
```

Why this is preferred over the current `thinking: ...` prefix:
- wrapped continuation lines are visually grouped
- the answer body no longer looks like a continuation of the thinking paragraph
- the section remains readable without introducing a full bordered box
- it uses less width than a full boxed sub-panel

---

## Recommended implementation order

### Sub-step A — isolate assistant-section rendering

Refactor the assistant branch in `block_lines(...)` so it renders three conceptual sections separately:
1. thinking section
2. answer section
3. streaming-status section

The immediate goal is clarity of structure in code before tuning the visuals.

Likely helper concepts:
- `assistant_thinking_lines(...)`
- `assistant_answer_lines(...)`
- `assistant_streaming_lines(...)`

The exact names do not matter; the separation of responsibilities does.

### Sub-step B — replace the inline `thinking: ...` paragraph with a real section

Do not keep the current `thinking: {thinking}` format as the final presentation.

Instead, render:
- a section label line
- then the thinking content with indentation or a muted gutter on every wrapped line

This is the key readability improvement.

### Sub-step C — keep answer text visually simple

The final answer should remain the visually primary content.

That means:
- no heavy prefix required for answer text in the first iteration
- no answer box unless later testing shows it is necessary
- the answer should simply begin after a blank line following the thinking section

### Sub-step D — make wrapped thinking lines preserve their visual grouping

This is the highest-value formatting detail.

If the thinking content wraps across many lines, continuation lines must still look like part of the thinking section.

That likely means the wrap/render logic cannot rely solely on a one-line `thinking:` prefix anymore.

The implementation may need:
- a thinking-section line builder that wraps plain text first
- then applies a gutter/prefix to each rendered line

### Sub-step E — review streaming indicator semantics

The current trailing `⠋ streaming...` line is generic.

Evaluate whether the first version should remain generic or become slightly more informative.

Reasonable options:
- keep `streaming...` for minimal churn
- or use section-aware wording such as:
  - `thinking...` when no answer text exists yet
  - `responding...` once visible answer text has begun

This is optional for the first pass, but it should be considered while restructuring the assistant block renderer.

### Sub-step F — add stronger visual regression tests

Add tests that go beyond ordering only.

At minimum cover:
- replayed assistant with thinking + answer
- streaming assistant with thinking + answer
- long wrapped thinking section above answer
- assistant with answer only remains sane
- assistant with thinking only remains sane

These tests should assert enough structure that future tweaks do not collapse back into the old blended format.

---

## Detailed code touchpoints

### `crates/anie-tui/src/output.rs`

Likely areas:
- `block_lines(...)`
- `wrap_text(...)`
- possibly a new helper for prefixing/guttering wrapped lines

The likely shape is:
- wrap raw thinking text into lines
- decorate each line with a muted gutter/prefix
- append a blank line before the answer section when both are present

### `crates/anie-tui/src/tests.rs`

Add visual rendering tests that inspect the final test-buffer output and assert:
- section order
- label/gutter presence
- wrapped thinking remains grouped
- answer text appears after the thinking section and not inside it

---

## What should *not* change in this follow-up

- no provider parsing logic
- no `ContentBlock` changes
- no transcript persistence format changes
- no controller involvement
- no new user-facing settings yet
- no attempt to hide or suppress thinking content

This is presentation work only.

---

## Test plan

### Required automated tests

1. **replayed thinking section renders above answer**
2. **streaming thinking section renders above answer**
3. **wrapped thinking lines keep their section formatting across line breaks**
4. **answer-only assistant rendering remains readable**
5. **thinking-only assistant rendering remains readable**
6. **existing transcript scrolling tests remain green**
   - especially long-message navigation, since formatting changes will affect rendered line counts

### Good snapshot-like assertions

If practical, assert details such as:
- a dedicated `thinking` label line exists
- wrapped thinking lines begin with the expected gutter/prefix
- the final answer appears only after the blank separator

---

## Manual validation plan

1. Run the TUI with a model that emits substantial reasoning.
2. Confirm thinking appears above the answer.
3. Confirm long reasoning is visually grouped and easy to skim.
4. Confirm the answer still stands out as the main visible output.
5. Confirm scrolling through long reasoning remains usable.
6. Confirm replayed sessions look structurally the same as live streaming output.

---

## Risks to watch

1. **too much chrome**
   - a heavy box or banner can waste width and make wrapped output worse
2. **wrap regressions**
   - adding a gutter/prefix can accidentally break line wrapping or alignment
3. **streamed vs replayed drift**
   - both paths must render with the same section structure
4. **line-count changes affecting scroll tests**
   - formatting changes will alter rendered line counts, so transcript navigation coverage must remain strong

---

## Exit criteria

This follow-up is complete only when all of the following are true:
- thinking is visually separated from the answer, not just semantically ordered first
- long wrapped thinking remains grouped and readable
- replayed and streaming assistant messages render consistently
- the answer remains easy to scan as the primary visible content
- existing scrolling/navigation behavior remains intact

---

## Relationship to the v1.0.1 phased plan

This is a presentation-focused follow-up to:
- `docs/phased_plan_v1-0-1/step_0_tui_transcript_scrolling_and_navigation.md`
- the local-reasoning stream/output work in Steps 3 and 6

It does **not** replace those plans.
It narrows the next TUI refinement pass to the visual treatment of thinking vs. final answer.