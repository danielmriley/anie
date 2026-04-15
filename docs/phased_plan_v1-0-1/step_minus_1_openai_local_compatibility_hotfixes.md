# Step -1 — OpenAI-Compatible Local Compatibility Hotfixes

This step is the immediate correctness patch for the local OpenAI-compatible path.

It must land before any of the broader local-reasoning feature work.

## Why this step exists

The current bug stack is not merely missing functionality; it is active misbehavior:

1. local backends can return meaningful assistant output in reasoning fields
2. the current OpenAI-compatible parser ignores those fields
3. the harness can therefore collapse a real response into an empty assistant turn
4. that empty turn can later be replayed into a bad follow-up request
5. local OpenAI-compatible requests also currently miss the main system prompt entirely

That means this step is a blocker-level hotfix.

---

## Primary outcomes required from this step

By the end of this step:
- OpenAI-compatible requests include the system prompt when present
- reasoning-only local streaming responses are preserved as thinking content
- the provider never silently treats a truly empty successful stop as a normal assistant reply
- replay no longer emits invalid empty assistant turns for the known failure shape

---

## Current code facts

### Already present

In `crates/anie-providers-builtin/src/openai.rs`:
- `assistant_message_to_openai_llm_message(...)` already skips replaying a fully empty assistant message
- that behavior must be preserved

### Still missing

Also in `crates/anie-providers-builtin/src/openai.rs`:
- `build_request_body(...)` still builds `messages` only from `context.messages`
- `context.system_prompt` is not prepended as a `role = "system"` message
- `OpenAiStreamState::process_event(...)` handles:
  - `delta.content`
  - `delta.tool_calls`
- but it does **not** handle:
  - `delta.reasoning`
  - `delta.reasoning_content`
  - `delta.thinking`
- final assistant message assembly currently only persists normal text plus tool calls
- there is no explicit error path for a successful-but-empty stop response

---

## Files expected to change

Primary:
- `crates/anie-providers-builtin/src/openai.rs`

Likely test-only touches:
- `crates/anie-agent/src/tests.rs`
- `crates/anie-tui/src/tests.rs`
- possibly `crates/anie-session/src/lib.rs` tests if replay/persistence coverage needs tightening

Documentation touch-ups are optional in this step and should be minimal.

---

## Constraints

1. Keep this step focused on correctness, not the full local-reasoning feature set.
2. Do not introduce the new capability model here.
3. Do not add prompt-steering heuristics here.
4. Do not add native request-field strategies here.
5. Do not move logic upward into the controller.

---

## Recommended implementation order inside this step

### Sub-step A — lock in the empty-assistant replay guard

Preserve the existing behavior in `assistant_message_to_openai_llm_message(...)` that returns `None` when the assistant message has:
- no text
- no tool calls

Add or strengthen tests so this stays intentional.

Important nuance:
- after reasoning support is added in this same step, a reasoning-only assistant turn will no longer be “empty” because it will contain `ContentBlock::Thinking`
- that means the skip-empty logic remains correct rather than conflicting with the fix

### Sub-step B — forward `LlmContext.system_prompt`

Add a helper for building the OpenAI-compatible `messages` array so that:
- a non-empty `context.system_prompt` becomes a leading `{"role":"system","content": ...}` message
- then the converted conversation messages follow in their current order

Keep the helper local to `openai.rs` for now. Centralization/refinement can happen later in Step 1.

Key rules:
- omit the system message entirely if the prompt is blank/whitespace-only
- do not rewrite user or assistant messages in this hotfix step

### Sub-step C — parse reasoning-bearing delta fields

Extend `OpenAiStreamState::process_event(...)` so that each streaming delta can emit thinking content from:
1. `delta.reasoning`
2. `delta.reasoning_content`
3. `delta.thinking`

Treat those as aliases for reasoning-bearing content.

Expected behavior:
- if one of these fields is present and non-empty, emit `ProviderEvent::ThinkingDelta(...)`
- preserve existing `delta.content` handling for normal answer text
- keep tool-call parsing independent and unchanged

A single event may contain both reasoning content and normal content. The parser should handle both.

### Sub-step D — store thinking in provider state and final assistant message

The provider state machine must accumulate reasoning text separately from normal text.

That means `OpenAiStreamState` likely needs:
- a dedicated `thinking: String` buffer in addition to `text: String`

Then `into_message()` must emit:
- `ContentBlock::Thinking { thinking: ... }` when reasoning content exists
- `ContentBlock::Text { text: ... }` when visible answer text exists
- existing tool calls afterward as they work today

This is required because the agent loop treats `ProviderEvent::Done(AssistantMessage)` as canonical.

### Sub-step E — reject truly empty successful stop responses

Add an explicit guard for the case where the provider reaches a successful stop but has accumulated:
- no text
- no thinking
- no tool calls
- no error

This must **not** become a normal assistant message.

Preferred first behavior:
- convert it into a structured provider failure such as `ProviderError::Stream("empty assistant response")`
- or another explicit structured error with a clear message

The important thing is to avoid silently persisting a blank successful turn.

Implementation note:
- this may require deciding whether the error is raised inside `process_event(...)`, inside `into_message()`, or just before yielding `ProviderEvent::Done(...)`
- choose the smallest change that preserves structured error behavior cleanly

### Sub-step F — add regression tests with captured local shapes

Add tests using representative reasoning-only SSE payloads, including shapes like:

```json
{"choices":[{"delta":{"role":"assistant","content":"","reasoning":"hello from reasoning"}}]}
```

And also legacy-compatible variants:
- `delta.reasoning_content`
- `delta.thinking`

The tests should ensure these are no longer dropped.

---

## Detailed code touchpoints

### `crates/anie-providers-builtin/src/openai.rs`

Likely areas:
- `build_request_body(...)`
- new helper for building the OpenAI-compatible `messages` array
- `assistant_message_to_openai_llm_message(...)`
- `OpenAiStreamState` fields
- `OpenAiStreamState::process_event(...)`
- `OpenAiStreamState::into_message()`
- provider tests at bottom of file

### `crates/anie-agent/src/agent_loop.rs`

Probably no semantic changes needed.

But it may be worth verifying with tests that:
- streamed thinking deltas continue to build into the final assistant message correctly
- a provider-side empty-stop error is surfaced as a structured terminal error

---

## Test plan

### Required provider tests

1. **system prompt included when present**
   - request body contains a leading `role = system` entry

2. **system prompt omitted when empty**
   - no synthetic system message for blank prompt

3. **empty assistant replay entries are still skipped**
   - assistant with no text/thinking/tool calls is omitted from converted history

4. **`delta.reasoning` emits `ProviderEvent::ThinkingDelta`**

5. **`delta.reasoning_content` emits `ProviderEvent::ThinkingDelta`**

6. **`delta.thinking` emits `ProviderEvent::ThinkingDelta`**

7. **reasoning-only stream produces final `ContentBlock::Thinking`**
   - not an empty assistant message

8. **mixed reasoning + text stream preserves both**
   - if an event includes both `reasoning` and `content`, both are retained

9. **tool-call parsing still works**
   - unchanged from current behavior

10. **truly empty successful stop becomes an error**
   - not a successful blank assistant response

### Useful integration tests

1. **agent loop surfaces thinking from provider final message correctly**
2. **session replay no longer reconstructs invalid empty assistant turns from this failure mode**

---

## Manual validation plan

1. Run a local Ollama reasoning-capable model that streams answer text only in `delta.reasoning`.
2. Confirm the TUI or print-mode output shows thinking content rather than nothing.
3. Confirm the session can be resumed without generating the previous invalid-empty follow-up request.
4. Verify an OpenAI-compatible local run now sees the system prompt and uses tools/instructions more reliably.

---

## Risks to watch

1. **Ordering of content blocks**
   - do not accidentally reorder tool calls ahead of text/thinking in a surprising way
2. **False-positive empty-stop detection**
   - avoid treating a valid tool-call-only assistant turn as empty
3. **Legacy alias ambiguity**
   - tolerate `reasoning_content` and `thinking`, but do not let them interfere with tool-call parsing
4. **Behavior drift for hosted OpenAI-compatible providers**
   - the added system prompt should be correct, but response parsing changes must remain tolerant for hosted paths

---

## Exit criteria

This step is complete only when all of the following are true:
- system prompt forwarding is implemented and test-covered
- reasoning-only local streams are preserved into final assistant messages
- empty successful assistant turns are rejected instead of silently persisted
- replay no longer emits the known invalid-empty assistant history shape
- existing tool-call behavior still passes

---

## Follow-on step

After this step is green, proceed to:
- `step_0_tui_transcript_scrolling_and_navigation.md`
