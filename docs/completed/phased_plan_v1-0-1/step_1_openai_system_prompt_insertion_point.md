# Step 1 — Stabilize the OpenAI-Compatible System-Prompt Insertion Point

This step takes the immediate system-prompt fix from Step -1 and turns it into a cleaner, durable provider-side insertion point for later local-reasoning prompt shaping.

## Why this step exists

Step -1 should land the minimal correctness fix: forward `LlmContext.system_prompt` on the OpenAI-compatible path.

That alone is necessary, but not sufficient for later work.

Subsequent steps need a stable provider-owned place to:
- add local prompt steering
- keep message ordering predictable
- avoid rewriting conversation messages directly

So this step is about **clean structure**, not just raw correctness.

---

## Primary outcomes required from this step

By the end of this step:
- OpenAI-compatible request-message construction is centralized
- the system prompt is inserted in one obvious place
- later provider-owned reasoning prompt augmentation has a clean attachment point
- conversation message conversion remains separate from system-prompt handling

---

## Current code facts

After Step -1, the provider should already prepend a `role = system` message when `LlmContext.system_prompt` is present.

However, that hotfix may be implemented in a minimal way directly inside `build_request_body(...)`.

This step is where we should make the construction logic explicit and readable.

---

## Files expected to change

Primary:
- `crates/anie-providers-builtin/src/openai.rs`

Likely test-only touch:
- provider tests in `openai.rs`

---

## Constraints

1. Do not change user-visible behavior beyond code cleanup/clarification.
2. Do not add prompt-steering wording yet.
3. Do not change tool conversion semantics.
4. Do not rework `convert_messages(...)` into something that mixes in system-prompt logic.

---

## Recommended implementation order inside this step

### Sub-step A — factor message-array construction into a helper

Create a helper dedicated to producing the final OpenAI-compatible `messages` array sent on the wire.

Conceptually:
- input: `LlmContext`
- output: `Vec<serde_json::Value>` ready for the request body

Responsibilities:
- prepend system message when appropriate
- append converted conversation messages after it
- preserve original ordering of conversation messages

This helper should be used from `build_request_body(...)`.

### Sub-step B — keep conversion layers separate

Preserve the separation of responsibilities:
- `convert_messages(...)` converts canonical protocol messages into provider-native `LlmMessage`
- the new request-message builder decides how `system_prompt` and converted messages are assembled into final OpenAI-compatible wire messages

This split matters because later prompt steering should modify the provider-owned system prompt path, not rewrite persisted conversation messages.

### Sub-step C — add one clear augmentation hook

Even if the actual prompt-steering logic arrives in Step 2, leave one obvious hook point now.

For example, the helper may work conceptually like:
- `effective_system_prompt(context.system_prompt, model, options)`
- then prepend it if non-empty

The exact function name is not important.

The important part is that Step 2 can later augment the system prompt without reworking request construction again.

### Sub-step D — make empty-system behavior explicit

The code should intentionally omit a system message when the effective system prompt is empty or blank.

This should be expressed clearly in the helper rather than as a side effect buried in `build_request_body(...)`.

---

## Detailed code touchpoints

### `crates/anie-providers-builtin/src/openai.rs`

Likely areas:
- `build_request_body(...)`
- new helper for final wire-message construction
- existing request-body tests

Potential helper names (illustrative only):
- `openai_request_messages(...)`
- `build_openai_messages(...)`
- `effective_system_prompt(...)`

Do not treat these names as required API commitments.

---

## Test plan

### Required provider tests

1. **system prompt is still included correctly**
   - leading `role = system`
   - original conversation order preserved after it

2. **blank system prompt still omitted**

3. **tool-call history remains unchanged**
   - tool-only assistant messages still serialize the same way they did before

4. **reasoning-only assistant messages are still preserved**
   - ensure Step -1 behavior does not regress while refactoring

5. **request-body tests remain green**
   - especially existing hosted reasoning request construction tests

---

## Manual validation plan

1. Run a local OpenAI-compatible prompt and verify behavior is unchanged from Step -1.
2. Run a tool-using prompt and ensure tool behavior remains unchanged.
3. Verify hosted OpenAI-compatible behavior still works with the centralized helper.

---

## Risks to watch

1. **accidental duplication of system messages**
   - if the helper is wired incorrectly, a system message could be inserted twice
2. **blurring conversion boundaries**
   - avoid stuffing too much policy into `convert_messages(...)`
3. **future prompt-steering confusion**
   - leave a clear place for system-prompt augmentation instead of scattering conditions

---

## Exit criteria

This step is complete only when all of the following are true:
- OpenAI-compatible request construction has one clear system-prompt insertion point
- message ordering is explicit and test-covered
- tool behavior is unchanged
- the code is ready for provider-owned prompt augmentation in Step 2

---

## Follow-on step

After this step is green, proceed to:
- `step_2_local_defaults_and_prompt_steering_mvp.md`
