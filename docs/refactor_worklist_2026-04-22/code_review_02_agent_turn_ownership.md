# Plan 02 — agent turn ownership + event payloads

**Findings covered:** #2, #6, #7, #8, #9, #23

This plan removes avoidable deep cloning from the main agent loop.
These are not isolated helper inefficiencies; they sit directly on
the per-turn path.

## Rationale

The review found two classes of waste in `crates/anie-agent/src/agent_loop.rs`:

1. **Whole-context cloning** every turn (`sanitize_context_for_request`)
   even when no provider-specific sanitization is needed (**#2**).
2. **Ownership churn around prompts, tool results, and end-of-turn
   events** (**#6, #7, #8, #9, #23**).

Unlike the registry plan, this is not about caching immutable data;
it is about choosing the right owner at each phase of a run.

## Design

### 1. Add a zero-allocation fast path to context sanitization

`sanitize_context_for_request` should return `Cow<'a, [Message]>`.

Fast path:

- no assistant message contains a block that must be stripped or
  rewritten for the target provider
- return `Cow::Borrowed(messages)`

Slow path:

- allocate the owned sanitized vector exactly as today

This preserves provider-aware sanitization behavior while removing
the unconditional clone from the common case.

### 2. Consume prompts and tool results by value where possible

There are several places where the loop clones values just to
re-wrap or re-store them. The fix is mostly order-of-operations:

- emit event clones first
- move the original into the long-lived collection last

This applies to:

- initial prompt replay
- tool result insertion into `context` and `generated_messages`
- final assistant insertion

### 3. Centralize run-finalization

`generated_messages.clone()` currently appears at multiple exit
sites. The loop should have one `finish_run(...)` helper that owns:

- event emission
- result construction
- stop-reason handling

There are two acceptable implementations:

1. **Preferred:** `AgentEvent::AgentEnd` carries `Arc<[Message]>`
   because the event stream is in-process only.
2. **Fallback:** keep `Vec<Message>` in the event, but centralize
   finalization so there is one clone site instead of several.

Recommendation: take option 1 **only if** `AgentEvent` is confirmed
to be in-process only. If any RPC/serialization consumer depends on
its current shape, use option 2 for the first pass and defer the
event-type redesign.

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-agent/src/agent_loop.rs` | `Cow` sanitization path, prompt/tool-result ownership cleanup, centralized run-finalization helper. |
| `crates/anie-protocol/src/events.rs` | Only if adopting the `Arc<[Message]>` `AgentEnd` payload. |
| `crates/anie-cli/src/controller.rs` / `print_mode.rs` / TUI consumers | Only if `AgentEnd` payload type changes. |
| relevant tests | Add regressions for sanitize fast path and finalization behavior. |

## Phased PRs

### PR A — `Cow` sanitization fast path

1. Change `sanitize_context_for_request` to return `Cow<'_, [Message]>`.
2. Add a cheap "needs sanitization?" scan before cloning.
3. Preserve the exact current sanitization behavior on the owned path.
4. Add tests for:
   - no-sanitization path returns borrowed data
   - Anthropic/OpenAI sanitization still strips the right blocks

### PR B — prompt replay ownership cleanup

1. Rewrite the prompt replay loop to consume prompts by value.
2. Keep event emission order unchanged.
3. Lock down prompt ordering in tests before touching tool results.

### PR C — tool-result ownership cleanup

1. Rewrite the tool-result loop to consume tool results by value.
2. Reorder `TurnEnd` emission only as much as needed to avoid the
   avoidable clone.
3. Add a focused regression test around tool-result ordering.

### PR D — `finish_with_assistant` cleanup

1. Clean up `finish_with_assistant` to avoid repeated message clones.
2. Keep this separate from run-finalization so failures are easier to
   localize.

### PR E — single run-finalization helper

1. Factor all run exits through one helper.
2. Remove the repeated `generated_messages.clone()` sites where
   possible without event-type churn.

### PR F — optional `AgentEnd` payload change

1. Only if still needed after PR E, choose the `AgentEnd` payload
   strategy:
   - `Arc<[Message]>` if event type churn is acceptable
   - otherwise leave one clone site in place and explicitly defer
     protocol-shape cleanup

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | `sanitize_context_fast_path_returns_borrowed_when_no_rewrite_needed` | `agent_loop.rs` tests |
| 2 | `sanitize_context_owned_path_preserves_existing_provider_behavior` | same |
| 3 | `initial_prompts_are_emitted_and_stored_without_extra_context_clone_regressions` | same |
| 4 | `tool_results_are_added_to_context_and_generated_messages_in_order` | same |
| 5 | `agent_end_still_delivers_generated_messages_to_consumers` | controller / print-mode / integration tests |

## Risks

- **Event-type churn:** if `AgentEvent` is used outside the in-process
  controller/TUI path, `Arc<[Message]>` may be the wrong choice.
- **Behavioral ordering drift:** reordering when events fire relative
  to context mutation can create subtle transcript regressions. Tests
  must lock this down.
- **Borrowed fast path correctness:** the detection predicate must
  match the actual sanitization triggers exactly.

## Exit criteria

- [ ] `sanitize_context_for_request` does not clone the full context
      on the common no-sanitization path.
- [ ] Prompt/tool-result loops no longer perform the avoidable
      clone-then-move pattern.
- [ ] Run-finalization has one exit path, not many near-duplicates.
- [ ] Event consumers still observe the same visible transcript.

## Deferred

- Any larger redesign of `AgentEvent` beyond what is needed to remove
  repeated end-of-run cloning.
- Rebuilding the whole request context directly from session entries;
  that is a different architectural change.
