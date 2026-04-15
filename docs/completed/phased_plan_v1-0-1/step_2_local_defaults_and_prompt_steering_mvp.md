# Step 2 — Local Defaults and Prompt Steering MVP

This step is the first true local-reasoning feature step.

It does not require native backend-specific reasoning controls yet. Instead, it ensures local models stop feeling artificially downgraded and that `ThinkingLevel` has an immediate, observable effect.

## Why this step exists

Right now local models are conservative by default in two important ways:
- local-first onboarding writes `thinking = "off"`
- auto-detected local models are represented in a way that is hostile to later reasoning support

That leads to a poor first impression even on modern local reasoning models.

This step fixes the defaults and adds provider-owned prompt steering so users can feel the effect of `/thinking` before native-control paths are introduced.

---

## Primary outcomes required from this step

By the end of this step:
- local onboarding no longer silently disables thinking
- local requests can be shaped by `ThinkingLevel` through provider-owned system-prompt augmentation
- this behavior is limited to the intended local OpenAI-compatible path
- hosted providers are not affected

---

## Current code facts

Relevant current behavior:
- `crates/anie-cli/src/onboarding.rs` currently writes `thinking = "off"` for local-first setup paths
- auto-detected local models in `crates/anie-providers-builtin/src/local.rs` are conservative and currently not ready for first-class reasoning support
- `crates/anie-providers-builtin/src/openai.rs` is the correct place to apply local prompt steering because it owns OpenAI-compatible request shaping

Important caution:
- today `supports_reasoning` is still tied to native reasoning request-field behavior in the OpenAI-compatible provider
- this step must **not** simply flip all detected locals to `supports_reasoning = true`, or it may trigger native request fields too early

---

## Files expected to change

Primary:
- `crates/anie-cli/src/onboarding.rs`
- `crates/anie-providers-builtin/src/local.rs`
- `crates/anie-providers-builtin/src/openai.rs`

Likely test-only touches:
- onboarding tests
- provider tests in `openai.rs`

---

## Constraints

1. Do not add native reasoning request fields yet.
2. Do not formalize the full capability model yet.
3. Do not rewrite user messages.
4. Keep all prompt shaping provider-owned.
5. Keep the effect scoped to the intended local OpenAI-compatible path.

---

## Recommended implementation order inside this step

### Sub-step A — remove `thinking = "off"` from local onboarding defaults

In `crates/anie-cli/src/onboarding.rs`:
- stop hard-coding `thinking = "off"` for local-first generated config
- use the normal default reasoning level instead (currently `medium`) unless a later onboarding design explicitly changes it

This is a low-risk, high-impact change and should land first inside the step.

### Sub-step B — make auto-detected local metadata non-hostile to later reasoning

In `crates/anie-providers-builtin/src/local.rs`:
- keep the metadata conservative
- but stop expressing local models in a way that implies reasoning is categorically impossible

For this MVP step, the goal is not to solve full capability modeling yet.

The goal is simply that:
- local models remain eligible for prompt-based reasoning shaping
- we avoid prematurely enabling native request fields

This may mean keeping the existing `supports_reasoning` behavior conservative while relying on a separate local-only prompt-steering predicate in the provider.

### Sub-step C — add a provider-owned local prompt-steering helper

In `crates/anie-providers-builtin/src/openai.rs`, add a helper that takes:
- model identity / provider identity
- current effective system prompt
- `ThinkingLevel`

and returns an augmented system prompt for local OpenAI-compatible requests when appropriate.

Important requirements:
- user messages remain untouched
- persisted conversation history remains untouched
- the helper augments only the provider-owned system prompt path

### Sub-step D — define stable prompt-steering semantics

The exact phrasing can be improved later, but the semantics should be stable now.

Recommended intent per level:

- `Off`
  - answer directly
  - avoid an explicit reasoning block unless required

- `Low`
  - do a brief internal plan
  - keep reasoning concise

- `Medium`
  - do balanced internal planning
  - check assumptions before responding

- `High`
  - reason more deliberately
  - verify the answer before finalizing

Keep the first version short and robust. Avoid elaborate wording.

### Sub-step E — scope the behavior correctly

This prompt-steering path should apply only to local OpenAI-compatible targets.

Candidate signals include:
- provider name like `ollama` or `lmstudio`
- local base URL patterns
- detected local-server identity

Do **not** apply this path to:
- Anthropic
- hosted OpenAI reasoning models
- other hosted OpenAI-compatible endpoints by default

### Sub-step F — ensure prompt steering composes with future steps

The helper introduced here should not assume it is the permanent end-state.

Later steps will add:
- tagged reasoning parsing
- capability metadata
- native request controls

So the prompt-steering code should be clearly framed as:
- MVP local request shaping
- provider-owned
- safe to refine or restrict later based on capability metadata

---

## Detailed code touchpoints

### `crates/anie-cli/src/onboarding.rs`

Update the generated config snippets/templates for local-first onboarding so they stop writing `thinking = "off"` by default.

### `crates/anie-providers-builtin/src/local.rs`

Review how detected local models are represented today and make only the minimum metadata changes needed to stop fighting the MVP prompt-steering path.

### `crates/anie-providers-builtin/src/openai.rs`

Likely areas:
- the stabilized system-prompt helper from Step 1
- a new local-only prompt-steering helper
- request-body tests

---

## Test plan

### Required onboarding tests

1. **local onboarding no longer writes `thinking = off`**
2. **hosted onboarding behavior is unchanged unless explicitly intended otherwise**

### Required provider tests

1. **local request + `ThinkingLevel::Off` produces direct-answer steering**
2. **local request + `Low` / `Medium` / `High` produce distinct augmented system prompts**
3. **user messages remain unchanged**
4. **tool definitions remain unchanged**
5. **hosted providers do not receive the local-only prompt-steering augmentation**
6. **native reasoning request fields are not accidentally enabled by this step**

---

## Manual validation plan

1. Run first-time local onboarding and verify the generated config no longer disables thinking.
2. In the TUI, switch `/thinking off`, `low`, `medium`, and `high` against a local model and observe meaningful differences in behavior.
3. Verify hosted Anthropic behavior is unchanged.
4. Verify tool-using prompts still behave normally.

---

## Risks to watch

1. **overly verbose prompt steering**
   - heavy wording may make local models worse rather than better
2. **too-broad targeting**
   - accidentally applying local steering to hosted endpoints could be surprising
3. **native-field leakage**
   - do not accidentally route this step through the old `supports_reasoning` gate in a way that adds native fields early
4. **prompt duplication**
   - ensure augmentation composes with the existing system prompt rather than replacing it clumsily

---

## Exit criteria

This step is complete only when all of the following are true:
- local onboarding no longer disables thinking by default
- local `ThinkingLevel` changes affect provider-owned prompt shaping
- hosted providers are unaffected
- no native request-field behavior was accidentally introduced

---

## Follow-on step

After this step is green, proceed to:
- `step_3_tagged_reasoning_stream_parsing_mvp.md`
