# v1.0.1 Post-Review Fixes

This document addresses the three issues raised in `docs/v1-0-1_review.md`.

Each fix is scoped to a single concern, has a clear implementation plan, and includes the tests needed to close it.

---

## Fix 1 — Empty-stop protection

### Problem

A truly empty successful assistant stop is still silently accepted.

If an OpenAI-compatible backend finishes a response with `finish_reason = "stop"` but produces:
- no text
- no thinking
- no tool calls
- no provider error

the current `finish_stream()` / `into_message()` will yield a `ProviderEvent::Done(AssistantMessage)` with an empty `content` vec and `StopReason::Stop`.

The downstream replay guard in `assistant_message_to_openai_llm_message(...)` prevents this from becoming an invalid follow-up request, which is good. But the empty turn can still be:
- persisted into the session as a normal success
- displayed as a blank assistant block in the TUI

This was explicitly called out in the original bug report as a required fix.

### Where to fix

`crates/anie-providers-builtin/src/openai.rs`, inside `into_message()`.

### Implementation plan

After content assembly (thinking, text, tool calls) but before returning the `AssistantMessage`, add a check:

```
if content.is_empty() && self.finish_reason is a successful stop {
    set stop_reason = StopReason::Error
    set error_message = Some("Provider returned an empty response with no text, reasoning, or tool calls.")
}
```

Specifically:
- `content.is_empty()` means no thinking, no text, no tool calls were accumulated
- successful stop means `finish_reason` is `Some("stop")`, `Some("length")`, or `None`
- do **not** override the stop reason for `finish_reason = "tool_calls"` — a tool-call-only stop with empty content would already have been caught by tool-call assembly

This keeps the fix narrow:
- the provider still returns a `ProviderEvent::Done(AssistantMessage)`
- but the message carries `StopReason::Error` and a clear `error_message`
- the agent loop already treats `StopReason::Error` as a terminal error and surfaces it
- the controller's existing transient-retry path can catch it if appropriate

Do **not** convert this into a `ProviderError` stream error — that would require changing `finish_stream()` to return `Result`, which is a larger change than needed.

### Tests required

Add to `openai.rs` tests:

1. **truly empty stop becomes error stop**
   - feed a stream with only `finish_reason = "stop"` and no content/reasoning/tool deltas
   - verify the final `AssistantMessage` has:
     - `stop_reason == StopReason::Error`
     - `error_message.is_some()`
     - `content.is_empty()`

2. **tool-call-only stop is not affected**
   - feed a stream with tool calls but no text/reasoning
   - verify `stop_reason == StopReason::ToolUse`
   - verify content contains the tool calls

3. **reasoning-only stop is not affected**
   - feed a stream with only `delta.reasoning` and no text
   - verify `stop_reason == StopReason::Stop`
   - verify content contains `ContentBlock::Thinking`

---

## Fix 2 — Token headroom direction

### Problem

`effective_max_tokens(...)` currently **reduces** `max_tokens` as thinking level increases.

For example with `max_tokens = 8192`:
- `Off` → 8192
- `Low` → 7245
- `Medium` → 6298
- `High` → 5632

The original plan said to "reserve extra completion headroom" for verbose local reasoners, implying the model should get **more** output budget so reasoning does not crowd out the final answer.

The current implementation does the opposite: it shrinks the available budget as thinking increases.

### Analysis

Both interpretations have a defensible use case:

**Current behavior (shrink):**
- treats the existing `max_tokens` as a total output budget
- reserves part of it so the model doesn't exhaust everything on reasoning
- protects against runaway reasoning that leaves no room for the final answer
- downside: may cause truncation if the model needs the full budget for both reasoning + answer

**Alternative behavior (grow):**
- treats the existing `max_tokens` as the desired answer budget
- adds extra headroom on top so reasoning doesn't eat into it
- ensures the final answer is not truncated by reasoning overhead
- downside: may request more tokens than the model's configured cap, leading to longer/more expensive responses

### Recommended resolution

Keep the **current shrink behavior** but rename and document it so the intent is unambiguous.

Reasoning:
- for local models, `max_tokens` is typically already set to the model's practical output limit
- growing beyond it may exceed server-side caps and cause errors
- shrinking within it is a safer default

Changes needed:
1. Add a doc comment on `effective_max_tokens(...)` explaining the design intent
2. Update the test name and assertions to make the policy explicit
3. Update `docs/phased_plan_v1-0-1/step_7_backend_profiles_token_budgets_and_release_validation.md` to match

### Where to fix

`crates/anie-providers-builtin/src/openai.rs`:
- add a doc comment to `effective_max_tokens(...)`

`docs/phased_plan_v1-0-1/step_7_backend_profiles_token_budgets_and_release_validation.md`:
- update wording to say "reserves a portion of the existing output budget" instead of "reserve extra headroom"

### Tests

No new tests needed — the existing `local_reasoning_token_headroom_changes_predictably_with_thinking_level` test covers the behavior. Just ensure the test name and any code comments reflect the "shrink within budget" intent.

---

## Fix 3 — Doc naming alignment

### Problem

The design reference doc `docs/local_model_thinking_plan.md` still uses the old planned enum names:
- `NativeOpenAiReasoning`
- `PromptOnly`
- `PromptWithTags`
- `NativeDeltas`
- `TaggedText`

The actual implementation uses shorter, cleaner names:
- `Native`
- `Prompt`
- `Separated`
- `Tagged`

The step docs in `docs/phased_plan_v1-0-1/` are already clean — they reference the types generically without the old long names.

### Where to fix

Only `docs/local_model_thinking_plan.md` needs updating.

### Implementation plan

In `docs/local_model_thinking_plan.md`, replace all occurrences:

| Old name | New name |
|---|---|
| `NativeOpenAiReasoning` | `Native` |
| `PromptOnly` | `Prompt` |
| `PromptWithTags` | _(remove — not a separate variant in the implementation)_ |
| `NativeDeltas` | `Separated` |
| `TaggedText` | `Tagged` |

Also update:
- the conceptual Rust code block to match the actual `ReasoningControlMode` and `ReasoningOutputMode` enums
- the profile table column values
- the mode descriptions (Mode A / B / C headings and text)

Note on `PromptWithTags`:
- the implementation does not have a separate `PromptWithTags` control mode
- instead, `Prompt` control mode can coexist with `Tagged` output mode
- this is actually cleaner because it preserves the orthogonality principle better
- the doc should reflect that prompt-with-tags is a **combination** of `Prompt` control + `Tagged` output, not its own control mode

### Tests

No code tests needed — this is a documentation-only fix.

---

## Implementation order

These three fixes are independent and can be done in any order.

Recommended sequence for smallest diffs:

1. **Fix 3** (doc naming) — zero code risk, makes docs match code
2. **Fix 2** (token headroom docs/comments) — minimal code, clarifies intent
3. **Fix 1** (empty-stop protection) — small code change + new tests

---

## Exit criteria

These fixes are complete when:
- [ ] truly empty successful stops produce `StopReason::Error` with a clear message
- [ ] empty-stop, tool-only-stop, and reasoning-only-stop cases are all test-covered
- [ ] `effective_max_tokens` has a doc comment explaining the shrink-within-budget policy
- [ ] step 7 doc wording matches the actual token policy
- [ ] `docs/local_model_thinking_plan.md` enum names match the implemented types
- [ ] all existing tests still pass
