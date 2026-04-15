# Step 4 — Reasoning Capability Model and Config

This step formalizes the local-reasoning model so the provider can stop relying on ad hoc behavior and start selecting request/output behavior intentionally.

## Why this step exists

The earlier steps can deliver valuable behavior quickly, but they are still MVP paths.

Long-term, we need explicit metadata to describe:
- how reasoning should be requested
- how reasoning may appear in responses
- how explicit config overrides should beat heuristics

Without this step, later native reasoning support would remain too implicit and too fragile.

---

## Primary outcomes required from this step

By the end of this step:
- `Model` can express richer reasoning capabilities than a boolean
- config can explicitly override reasoning behavior per model
- provider code has a deterministic way to resolve an effective reasoning profile
- backward compatibility with existing configs remains intact

---

## Current code facts

Today the model/config path still relies heavily on:
- `supports_reasoning: bool`

That is too coarse because modern local backends can combine behaviors like:
- native control + tagged output
- native control + native separated reasoning output
- prompt-with-tags + tagged output
- prompt-only + no separated output

This step exists to represent those combinations directly.

---

## Files expected to change

Primary:
- `crates/anie-provider/src/model.rs`
- `crates/anie-provider/src/lib.rs`
- `crates/anie-config/src/lib.rs`
- `crates/anie-providers-builtin/src/models.rs`
- `crates/anie-providers-builtin/src/local.rs`

Likely tests:
- provider-model serde tests
- config parsing tests
- local model-resolution tests

---

## Constraints

1. Keep backward compatibility with existing config files.
2. Do not remove `supports_reasoning` until it is safe to do so.
3. Keep the new types provider-layer owned.
4. Preserve the architecture where controller owns only `ThinkingLevel` and not reasoning mode details.

---

## Recommended implementation order inside this step

### Sub-step A — introduce provider-layer reasoning capability types

In `anie-provider`, add types conceptually equivalent to:
- `ReasoningControlMode`
- `ReasoningOutputMode`
- `ReasoningTags`
- `ReasoningCapabilities`

Key design rule:
- control mode and output mode remain orthogonal

That means the type model must naturally represent combinations such as native control + tagged output.

### Sub-step B — extend `Model`

Add reasoning capability metadata to `Model`.

Keep `supports_reasoning` temporarily if needed for compatibility, but stop treating it as sufficient.

Good migration posture:
- old code can still read `supports_reasoning`
- new code prefers richer reasoning capability metadata when present

### Sub-step C — extend config schema backward-compatibly

In `anie-config`, add optional per-model fields conceptually like:
- `reasoning_control`
- `reasoning_output`
- `reasoning_tag_open`
- `reasoning_tag_close`

Behavior requirements:
- old config files continue to load
- old configs without the new fields behave as before unless later code intentionally maps them to defaults
- `supports_reasoning = true` can remain as a coarse hint while richer fields are absent

### Sub-step D — annotate built-in hosted models explicitly

In `crates/anie-providers-builtin/src/models.rs`, annotate hosted models so their reasoning behavior is explicit instead of inferred from local heuristics.

Examples:
- Anthropic hosted models
- hosted OpenAI reasoning models

This prevents the new model from becoming local-only duct tape.

### Sub-step E — allow auto-detected local models to carry richer profiles

In `crates/anie-providers-builtin/src/local.rs`, make sure detected local models can carry reasoning capability metadata even if `/v1/models` itself provides only minimal info.

This step does **not** require final heuristics yet.

It only requires that the data model can represent the result once heuristics are added.

### Sub-step F — define effective-profile precedence in one place

Add a helper or documented resolution path that chooses the effective reasoning profile in this order:
1. explicit model config override
2. built-in hosted model metadata
3. detected local-server profile
4. local model-family heuristic/profile
5. safe fallback

The exact implementation location can vary, but it should be centralized rather than spread across multiple call sites.

---

## Detailed code touchpoints

### `crates/anie-provider/src/model.rs`

Likely additions:
- capability enums/structs
- `Model` fields referencing them
- serde derives consistent with existing model serialization style

### `crates/anie-provider/src/lib.rs`

Re-export any new reasoning capability types needed by config/providers.

### `crates/anie-config/src/lib.rs`

Likely additions:
- config enums matching provider-layer reasoning modes
- custom-model parsing / merging logic
- backward-compatible defaults

### `crates/anie-providers-builtin/src/models.rs`

Add explicit hosted reasoning profiles.

### `crates/anie-providers-builtin/src/local.rs`

Make detected local models capable of carrying a reasoning profile, even if the heuristics remain conservative for now.

---

## Test plan

### Required tests

1. **new reasoning capability types serialize/deserialize correctly**
2. **existing model serde remains backward-compatible**
3. **old config files still parse unchanged**
4. **new config fields parse and round-trip**
5. **explicit config overrides beat heuristics/defaults**
6. **models can represent native control + tagged output simultaneously**
7. **built-in hosted model profiles are explicit and stable**

### Good resolution tests

Add tests around the effective-profile precedence helper so the order is locked in early rather than inferred later.

---

## Manual validation plan

1. Add example config entries for multiple reasoning modes and verify they load.
2. Confirm older configs still behave normally.
3. Confirm hosted models still resolve to the expected reasoning behavior.

---

## Risks to watch

1. **schema churn**
   - avoid overcomplicating the initial shape beyond what the planned provider behavior needs
2. **backward-compatibility regressions**
   - especially in config parsing and built-in model construction
3. **duplicated precedence logic**
   - if multiple parts of the provider stack resolve profiles differently, bugs will follow
4. **semantic confusion around `supports_reasoning`**
   - document clearly that it is no longer the full story once richer metadata exists

---

## Exit criteria

This step is complete only when all of the following are true:
- `Model` can represent richer reasoning capabilities
- config can override reasoning behavior per model
- precedence is centralized and test-covered
- older config remains compatible

---

## Follow-on step

After this step is green, proceed to:
- `step_5_native_reasoning_controls_for_modern_local_backends.md`
