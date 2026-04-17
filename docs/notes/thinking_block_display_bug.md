# Thinking Block Display Bug

## Summary

Thinking text sometimes appears visible at the end of assistant messages
instead of being properly hidden or collapsed.

## Current State

- Thinking blocks stream correctly and render in their own TUI section
- But sometimes the final rendered message shows thinking text at the end
- May be a race condition in streaming finalization or output formatting

## What Was Fixed (reasoning_fix_plan.md)

The reasoning fix plan (phases 1–3) addressed related but distinct issues:
- Thinking-only completions no longer accepted as valid (phase 1)
- Thinking blocks stripped from OpenAI-compatible replay (phase 1)
- Provider-aware replay policy (phase 2)

This display bug is about **TUI rendering**, not about replay or completion
validity. The fixes above may have reduced its frequency but the root cause
in the rendering path should still be investigated.

## Action Items

### 1. Investigate the rendering path
Trace how `ContentBlock::Thinking` blocks flow from `AgentEvent::MessageDelta`
through the TUI transcript renderer. Look for cases where thinking content
leaks into the visible text section.

### 2. Check streaming finalization
The `AssistantMessageBuilder::finish()` method assembles content blocks.
Verify that `Thinking` blocks always stay in their own section and never
get concatenated with `Text` blocks during finalization.

### 3. Check session resume rendering
When a session is resumed, assistant messages are replayed from JSONL.
Verify that `Thinking` blocks in persisted messages render correctly in
the thinking section, not inline with visible text.

## Priority

High — directly visible to users and undermines trust in the output.
