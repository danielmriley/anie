# Step 5 — Native Reasoning Controls for Modern Local Backends

This step adds the preferred request-shaping path for modern local backends that support native reasoning controls.

## Why this step exists

By this point, we should already have:
- correctness fixes for local OpenAI-compatible parsing
- stable system-prompt handling
- prompt-steering MVP
- tagged parsing MVP
- explicit reasoning capability metadata

That is enough foundation to add native request controls safely.

As of 2026, native reasoning controls are common enough on local backends that they should be a first-class path rather than a special-case add-on.

---

## Primary outcomes required from this step

By the end of this step:
- the OpenAI-compatible provider can send native reasoning controls when the effective profile calls for them
- backend-specific field strategies are handled deliberately
- unsupported field shapes degrade safely without repeated failure loops

---

## Backend facts this step should encode

### Ollama

Preferred field:
- top-level `reasoning_effort`

Legacy compatibility exists via `think`, but that should not be the default strategy in the first implementation.

### LM Studio

Most reliable native shape:
- nested `reasoning: { effort: ... }`

Some versions also accept:
- top-level `reasoning_effort`

This means LM Studio may need a two-strategy approach.

### vLLM

Preferred field:
- top-level `reasoning_effort`

This is reliable when the server is configured with an appropriate reasoning parser for the model family.

---

## Files expected to change

Primary:
- `crates/anie-providers-builtin/src/openai.rs`

Possible helper extraction:
- small internal request-strategy helper
- small in-memory negative-capability cache helper

Likely tests:
- provider request-body tests
- retry/fallback classification tests

---

## Constraints

1. Do not require native controls for local reasoning to work.
2. Do not remove prompt-steering or tagged/native-output fallback paths.
3. Keep control mode and output mode orthogonal.
4. Retry only on clear compatibility failures, not on unrelated provider errors.

---

## Recommended implementation order inside this step

### Sub-step A — introduce a request-strategy abstraction

Do not let `build_request_body(...)` become a large nested backend-branch function.

Add a small strategy concept such as:
- top-level reasoning effort
- LM Studio nested reasoning object
- no native reasoning fields

The effective reasoning profile plus backend identity should choose the initial strategy.

### Sub-step B — map `ThinkingLevel` to native field values

For native-control mode:
- `Low` → `low`
- `Medium` → `medium`
- `High` → `high`

For `Off`:
- start conservatively by omitting native reasoning fields unless a backend-specific disable semantic becomes necessary later

### Sub-step C — select initial backend strategy

Recommended first strategy selection:
- Ollama → top-level `reasoning_effort`
- vLLM → top-level `reasoning_effort`
- LM Studio → top-level `reasoning_effort` first, then nested `reasoning: { effort: ... }` on compatibility failure

This preserves a common path while still acknowledging LM Studio’s special case.

### Sub-step D — classify compatibility failures narrowly

Add a narrow fallback trigger for native reasoning field rejection.

Appropriate triggers:
- HTTP 400 or equivalent request-construction failure
- body clearly indicates unsupported field / unknown field / bad request semantics

Do **not** trigger strategy fallback for:
- auth errors
- context overflow
- rate limiting
- generic transient network failures

### Sub-step E — retry once with alternate or no-native strategy

Fallback policy:
- for LM Studio, if top-level fails on a clear compatibility error, retry once with nested `reasoning: { effort: ... }`
- if native strategies are exhausted or clearly unsupported, retry once without native reasoning fields

Keep this controlled and small. Do not create a broad retry matrix here.

### Sub-step F — cache negative results in-memory

Store rejected native strategies per:
- `base_url`
- `model_id`
- request strategy

This prevents repeated known-bad requests during the same process lifetime.

The first version can be in-memory only.

### Sub-step G — preserve fallback composition

Even when native request control succeeds:
- do not assume output will be native-separated
- do not disable fallback output parsing paths

This is important because native control and tagged output often coexist.

---

## Detailed code touchpoints

### `crates/anie-providers-builtin/src/openai.rs`

Likely areas:
- request-body construction
- effective reasoning profile use
- backend identity checks
- request retry/fallback logic
- strategy cache

Potential internal concepts:
- `NativeReasoningRequestStrategy`
- compatibility retry helper
- negative-capability cache

Names are illustrative only.

---

## Test plan

### Required provider tests

1. **Ollama profile emits top-level `reasoning_effort`**
2. **vLLM profile emits top-level `reasoning_effort`**
3. **LM Studio can try top-level first**
4. **LM Studio fallback to nested `reasoning.effort` works when top-level is rejected**
5. **`ThinkingLevel::Off` does not accidentally force native fields by default**
6. **unsupported native-field failures trigger the intended single fallback path**
7. **negative-capability results are cached per backend/model/strategy**
8. **prompt/tag fallback paths are not disabled by native request control selection**

### Good interaction tests

1. a backend with unsupported native fields succeeds after fallback instead of failing the whole run
2. repeated requests do not keep retrying a known-bad strategy

---

## Manual validation plan

1. Recent Ollama + reasoning-capable model uses top-level `reasoning_effort` successfully.
2. LM Studio accepts either the shared or nested shape and falls back cleanly when necessary.
3. vLLM accepts top-level `reasoning_effort` when configured for reasoning-capable models.
4. A backend that rejects native reasoning fields still completes the request after fallback.

---

## Risks to watch

1. **over-broad compatibility fallback**
   - do not hide real provider errors behind native-field fallback logic
2. **request-body complexity explosion**
   - keep the strategy abstraction small and testable
3. **cache key mistakes**
   - a bad cache key could incorrectly suppress valid native strategies on other models/endpoints
4. **accidental coupling to output behavior**
   - native request control success does not guarantee native separated output

---

## Exit criteria

This step is complete only when all of the following are true:
- native reasoning request control works for modern local backends when their profile calls for it
- LM Studio’s special-case request shape is handled safely
- unsupported native-field strategies fall back once and stop repeating
- fallback output paths remain intact

---

## Follow-on step

After this step is green, proceed to:
- `step_6_native_separated_reasoning_output.md`
