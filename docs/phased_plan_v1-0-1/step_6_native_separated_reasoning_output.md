# Step 6 — Native Separated Reasoning Output

This step teaches the OpenAI-compatible provider to prefer native separated reasoning output when the backend exposes it.

## Why this step exists

At this point, local reasoning support should already handle:
- reasoning-only deltas from the immediate hotfix work
- tagged reasoning parsing
- native request controls
- explicit reasoning capability metadata

What remains is to fully support the best current UX path on modern backends:
- reasoning content streaming separately from final answer text

That lets the harness stop reconstructing reasoning solely from tags when the backend already exposes it cleanly.

---

## Primary outcomes required from this step

By the end of this step:
- native separated reasoning fields are parsed first
- visible answer text remains distinct from reasoning content
- tag parsing continues to work as fallback when native separated fields are absent
- final `AssistantMessage` reflects the same separation the user saw during streaming

---

## Backend facts this step should encode

### Ollama

Streaming may expose:
- `delta.reasoning`
- legacy `delta.thinking`
- plus ordinary `delta.content`

Some models may still emit tagged reasoning in `delta.content` instead.

### LM Studio

When its separation toggle is enabled, streaming may expose:
- `delta.reasoning`
- `delta.reasoning_content`

When the toggle is off, reasoning may remain embedded in `delta.content` with tags.

### vLLM

Streaming is standardized primarily around:
- `delta.reasoning`

Legacy tolerance for `reasoning_content` is still useful in the parser.

---

## Files expected to change

Primary:
- `crates/anie-providers-builtin/src/openai.rs`

Possible helper extraction:
- small helper for extracting reasoning-bearing fields from stream events

Likely tests:
- provider stream parser tests
- final assistant message assembly tests

---

## Constraints

1. Prefer native separated fields first, but keep tag parsing as fallback.
2. Do not assume native separated reasoning is always present just because native request control is enabled.
3. Preserve tool-call parsing behavior.
4. Keep final `AssistantMessage` content faithful to the streamed deltas.

---

## Recommended implementation order inside this step

### Sub-step A — centralize delta field extraction order

When parsing a streaming choice delta, reason about fields in this order:
1. `delta.reasoning`
2. `delta.reasoning_content`
3. `delta.thinking`
4. `delta.content`

This order should be made explicit in code rather than distributed across several conditionals.

### Sub-step B — parse reasoning and normal content independently within a single event

A single event may contain both reasoning and answer content.

Required behavior:
- reasoning-bearing fields emit `ProviderEvent::ThinkingDelta`
- `delta.content` still emits normal content behavior
- if no native reasoning field is present, `delta.content` can still be routed through the tag parser

### Sub-step C — define native-first / tag-fallback behavior clearly

Per-event rule:
- if a native reasoning field is present, treat it as native reasoning content
- `delta.content` is still ordinary answer text unless tag parsing is needed on it
- if no native reasoning field is present, `delta.content` may still contain tagged reasoning and should go through the tagged parser

This avoids falsely making native and tagged parsing mutually exclusive at the whole-response level.

### Sub-step D — update final assistant-message assembly

Ensure the provider state machine’s final `AssistantMessage` includes:
- accumulated `ContentBlock::Thinking`
- accumulated `ContentBlock::Text`
- existing tool calls

If the state machine already has separate buffers from earlier steps, this is mainly about ensuring the ordering and completeness are correct.

A conservative first ordering is:
1. thinking block if present
2. text block if present
3. tool calls as already emitted

### Sub-step E — verify coexistence with tool calls

Reasoning parsing must not disturb:
- tool-call start/delta/end events
- tool-call argument assembly
- final tool-call blocks in the assistant message

This is especially important in multi-modal local responses where reasoning and tool planning may appear in nearby chunks.

---

## Detailed code touchpoints

### `crates/anie-providers-builtin/src/openai.rs`

Likely areas:
- `OpenAiStreamState::process_event(...)`
- helper for extracting reasoning-bearing fields from deltas
- `OpenAiStreamState::into_message()`
- tests around multi-field deltas

---

## Test plan

### Required provider tests

1. **`delta.reasoning` emits thinking**
2. **`delta.reasoning_content` is accepted as a legacy alias**
3. **`delta.thinking` is accepted as a legacy alias**
4. **same-event reasoning + content are both preserved**
5. **if native reasoning field is absent, tag parsing still works on `delta.content`**
6. **final `AssistantMessage` includes `ContentBlock::Thinking` consistent with stream deltas**
7. **tool-call parsing still works unchanged**

### Good fallback tests

1. LM Studio-style separated reasoning field present → native path used
2. LM Studio-style separated field absent but `<think>` present in content → tag fallback path used

---

## Manual validation plan

1. Recent Ollama model with separated reasoning renders thinking blocks without depending solely on tags.
2. LM Studio with reasoning separation enabled uses native-separated reasoning fields.
3. LM Studio with separation disabled still works via tags.
4. vLLM with reasoning parser enabled uses `delta.reasoning` successfully.

---

## Risks to watch

1. **double-counting reasoning**
   - do not parse the same content once as native reasoning and again as tag-based content
2. **same-event field confusion**
   - some deltas may contain both reasoning and visible answer text; preserve both deliberately
3. **final-message mismatch**
   - streamed transcript and final assistant content must stay aligned
4. **tool-call regressions**
   - tool calls remain a separate parsing channel and must stay that way

---

## Exit criteria

This step is complete only when all of the following are true:
- native separated reasoning fields are parsed first when present
- tagged parsing still works as fallback when native fields are absent
- final assistant messages preserve the same reasoning/text separation seen during streaming
- tool-call behavior remains unchanged

---

## Follow-on step

After this step is green, proceed to:
- `step_7_backend_profiles_token_budgets_and_release_validation.md`
