# Step 3 — Tagged Reasoning Stream Parsing MVP

This step makes the most common local-model reasoning format visible and useful in the transcript.

## Why this step exists

Even with native reasoning controls improving, many local models still emit reasoning inline using tags such as:
- `<think>...</think>`
- `<thinking>...</thinking>`
- `<reasoning>...</reasoning>`

This step turns that tagged content into proper thinking blocks in the transcript.

It is the universal fallback path and one of the highest-ROI usability improvements for local reasoning.

---

## Primary outcomes required from this step

By the end of this step:
- tagged reasoning in assistant text is split into `ThinkingDelta`
- visible answer text remains `TextDelta`
- raw reasoning tags no longer leak into normal answer text when parsing succeeds
- the final provider `AssistantMessage` matches the streamed transcript content

---

## Current code facts

After Step -1, the OpenAI-compatible path should already parse native reasoning-bearing fields like `delta.reasoning`.

This step is different:
- it handles reasoning that arrives inside ordinary `delta.content`
- it must coexist cleanly with tool-call parsing
- it must coexist with the later native-separated reasoning path

Current parser limitation:
- `OpenAiStreamState` treats `delta.content` as plain text today
- there is no streaming tag splitter that can span chunk boundaries

---

## Files expected to change

Primary:
- `crates/anie-providers-builtin/src/openai.rs`

Possible extraction if the code grows:
- a small helper module for streaming tagged-text splitting

Likely test-only touches:
- provider tests in `openai.rs`

---

## Constraints

1. Keep tool-call JSON/argument parsing totally separate from tag parsing.
2. Operate on assistant text only.
3. Do not assume tags are neatly chunk-aligned.
4. Do not drop malformed content silently.
5. Do not let this step replace the future native-separated reasoning path.

---

## Recommended implementation order inside this step

### Sub-step A — introduce a dedicated tagged-text stream splitter

Add a small state machine that consumes assistant `delta.content` fragments and emits:
- visible text outside reasoning tags
- thinking text inside reasoning tags

This logic should be isolated enough that it can be reasoned about independently from the larger OpenAI stream parser.

### Sub-step B — support a built-in alias set

The MVP parser should recognize at least:
- `<think>...</think>`
- `<thinking>...</thinking>`
- `<reasoning>...</reasoning>`

Treat these as built-in aliases even before explicit per-model tag config exists.

### Sub-step C — support chunk-boundary splits

The parser must correctly handle tags broken across streamed fragments.

Examples that must work:
- opening tag split across chunks
- closing tag split across chunks
- reasoning content split across many chunks

This likely requires an internal buffer rather than purely stateless per-delta processing.

### Sub-step D — define malformed-tag fallback behavior

The parser must degrade safely.

Required behavior:
- if a tag is incomplete, buffer until enough data arrives to decide
- if a malformed tag never resolves, prefer visible text over content loss
- if a reasoning span never closes, do not silently drop it

A tolerant parser is more important than a clever parser.

### Sub-step E — keep tool-call parsing isolated

The existing OpenAI-compatible stream parser already has separate logic for `delta.tool_calls`.

Do not send tool-call fragments through the tag splitter.

The tag parser should process only `delta.content` and only after native reasoning-field handling has had its turn.

### Sub-step F — keep final assistant-message assembly aligned with stream output

The provider must accumulate reasoning and visible text consistently so that `ProviderEvent::Done(AssistantMessage)` matches what the user saw while streaming.

That means:
- visible answer text accumulated outside tags becomes `ContentBlock::Text`
- tagged reasoning accumulated inside tags becomes `ContentBlock::Thinking`

This is critical for replay, session persistence, and diffing between streamed and final state.

---

## Detailed code touchpoints

### `crates/anie-providers-builtin/src/openai.rs`

Likely areas:
- `OpenAiStreamState`
- `process_event(...)`
- any helper introduced for processing ordinary text deltas
- final `into_message()` content assembly

Potential internal helper concepts:
- tagged reasoning splitter state
- alias matcher
- buffer flush logic

The specific names do not matter as much as keeping the logic isolated and testable.

---

## Test plan

### Required parser/provider tests

1. **opening tag split across chunks**
2. **closing tag split across chunks**
3. **multiple tagged reasoning spans in one response**
4. **all built-in aliases work**
   - `<think>`
   - `<thinking>`
   - `<reasoning>`
5. **malformed or unclosed tag sequences do not lose content**
6. **tagged reasoning produces `ProviderEvent::ThinkingDelta`**
7. **non-tagged text produces `ProviderEvent::TextDelta`**
8. **tool-call parsing still works**
9. **final `AssistantMessage` contains thinking blocks consistent with stream deltas**
10. **raw tags do not leak into visible answer text when parsing succeeds**

### Good interaction tests

If possible, include one mixed-case test where:
- some reasoning arrives natively via reasoning fields
- some ordinary content is also present
- tag parsing still behaves correctly on content deltas where no native reasoning field exists

---

## Manual validation plan

1. Run a local model known to emit `<think>` and confirm the transcript renders thinking separately.
2. Verify the final answer text is readable and no raw tags remain when parsing succeeds.
3. Test a malformed-tag response if possible and verify visible text is preserved.
4. Test a tool-using prompt and verify tool-call rendering still works.

---

## Risks to watch

1. **over-greedy parsing**
   - do not accidentally treat ordinary markup/code as reasoning tags in a destructive way
2. **chunk-boundary bugs**
   - this is the highest-risk area for correctness
3. **drift between streamed and final content**
   - if the state machine emits correct deltas but final assembly differs, replay becomes confusing
4. **interference with native reasoning fields**
   - tagged parsing must remain a fallback and not corrupt native-separated paths

---

## Exit criteria

This step is complete only when all of the following are true:
- tagged reasoning becomes proper thinking blocks in the transcript
- raw tags do not leak into answer text when parsing succeeds
- tool-call behavior is unchanged
- final assistant messages match the streamed transcript representation

---

## Follow-on step

After this step is green, proceed to:
- `step_4_reasoning_capability_model_and_config.md`
