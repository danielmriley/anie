# anie-rs v1.0.1 Implementation Order

This document defines the implementation order for the current local-model compatibility, TUI navigation, and local-reasoning expansion work.

It is intentionally narrower than `docs/IMPLEMENTATION_ORDER.md`.

Use this file as the sequencing guide for the **v1.0.1 follow-up work**.
If it conflicts with the step docs in `docs/phased_plan_v1-0-1/`, update those docs first and then update this file.

Expanded step-by-step planning for this order lives in:
- `docs/phased_plan_v1-0-1/README.md`
- `docs/phased_plan_v1-0-1/step_*.md`

---

## Scope of this implementation order

This order covers the work planned in `docs/phased_plan_v1-0-1/`:
- `step_minus_1_openai_local_compatibility_hotfixes.md`
- `step_0_tui_transcript_scrolling_and_navigation.md`
- `step_1_openai_system_prompt_insertion_point.md`
- `step_2_local_defaults_and_prompt_steering_mvp.md`
- `step_3_tagged_reasoning_stream_parsing_mvp.md`
- `step_4_reasoning_capability_model_and_config.md`
- `step_5_native_reasoning_controls_for_modern_local_backends.md`
- `step_6_native_separated_reasoning_output.md`
- `step_7_backend_profiles_token_budgets_and_release_validation.md`

Design rationale lives in:
- `docs/local_model_thinking_plan.md`

---

## Primary goals for v1.0.1

1. Fix the current OpenAI-compatible local-provider correctness bugs.
2. Fix TUI transcript navigation so long local reasoning output is usable.
3. Make local reasoning a first-class experience without breaking the architecture:
   - `anie-cli` / controller owns `ThinkingLevel`
   - `anie-providers-builtin` owns request shaping and stream parsing
   - `anie-tui` stays UI-only
4. Preserve hosted-provider stability while expanding local-model support.

---

## Global guardrails

Before starting any implementation, preserve these decisions:

1. **Provider-layer ownership**
   - local reasoning behavior belongs in the provider layer
   - do not move prompt shaping into the controller or TUI

2. **TUI-only scrolling**
   - transcript scrolling/navigation is presentation state
   - do not persist or orchestrate scroll state in the controller

3. **Structured provider behavior**
   - continue using structured `ProviderEvent` / `ProviderError`
   - do not add stringly-typed local-model special cases in upper layers

4. **Control mode and output mode are orthogonal**
   - native reasoning controls may coexist with tagged output
   - native reasoning controls may coexist with native separated reasoning output

5. **Safe fallback is required**
   - unsupported native reasoning fields must degrade safely
   - truly empty successful local responses must not be persisted as normal assistant turns

---

## Implementation sequence

## Step -1 — OpenAI-compatible local compatibility hotfixes

**Do this first. Do not start the TUI or local-reasoning feature phases before it is green.**

Reference:
- `docs/phased_plan_v1-0-1/step_minus_1_openai_local_compatibility_hotfixes.md`

Implement:
- preserve and test the empty-assistant replay guard
- forward `LlmContext.system_prompt` on the OpenAI-compatible path
- parse local reasoning stream fields:
  - `delta.reasoning`
  - `delta.reasoning_content`
  - `delta.thinking`
- persist reasoning content into final `AssistantMessage` as `ContentBlock::Thinking`
- fail loudly on truly empty successful stop responses
- add regression tests for reasoning-only local SSE shapes

Why first:
- current local Ollama/Qwen behavior can produce a meaningful response that the harness drops
- that can lead to empty assistant turns and invalid follow-up requests
- this is a correctness blocker, not a feature enhancement

Gate:
- OpenAI-compatible requests include the system prompt
- reasoning-only local responses are preserved as thinking content
- empty successful assistant turns are rejected instead of persisted
- regression tests cover the known local-response shapes

If this is not green, stop here.

---

## Step 0 — TUI transcript scrolling and navigation

Reference:
- `docs/phased_plan_v1-0-1/step_0_tui_transcript_scrolling_and_navigation.md`

Implement:
- robust transcript scrolling in `anie-tui`
- `PageUp` / `PageDown`
- `Home` / `End`
- mouse-wheel transcript scrolling
- visible “scrolled away from bottom” indication
- correct auto-follow semantics while streaming
- tests for both:
  - long history
  - single long wrapped assistant messages

Why now:
- local reasoning will often produce long transcript blocks
- users must be able to inspect older content and the top of long responses before richer reasoning support is useful

Gate:
- users can reliably scroll older transcript content
- users can reach the beginning of a long wrapped assistant message
- mouse scrolling works
- bottom-follow behavior remains correct
- tests cover long-history and long-message cases

---

## Step 1 — Stabilize the OpenAI-compatible system-prompt insertion point

Reference:
- `docs/phased_plan_v1-0-1/step_1_openai_system_prompt_insertion_point.md`

Implement:
- centralize OpenAI-compatible message-array construction
- keep the system prompt as a provider-owned insertion point for later prompt shaping
- preserve message ordering and tool behavior

Important note:
- the immediate system-prompt bug is fixed in Step -1
- this step is about making that path clean, stable, and explicitly ready for later provider-owned reasoning prompt augmentation

Gate:
- request construction is centralized and test-covered
- tool/message behavior is unchanged except for correct system-prompt inclusion

---

## Step 2 — Local defaults and prompt steering MVP

Reference:
- `docs/phased_plan_v1-0-1/step_2_local_defaults_and_prompt_steering_mvp.md`

Implement:
- stop forcing local onboarding to `thinking = "off"`
- make local auto-detection non-hostile to reasoning support
- add provider-owned prompt steering for local models based on:
  - `off`
  - `low`
  - `medium`
  - `high`

Do **not** enable native reasoning request fields yet as part of this step.

Gate:
- local onboarding no longer silently disables thinking
- `/thinking` changes local-model behavior through prompt shaping
- hosted-provider behavior remains unchanged

---

## Step 3 — Tagged reasoning stream parsing MVP

Reference:
- `docs/phased_plan_v1-0-1/step_3_tagged_reasoning_stream_parsing_mvp.md`

Implement:
- tagged reasoning stream splitter for assistant text deltas
- support built-in aliases at minimum:
  - `<think>...</think>`
  - `<thinking>...</thinking>`
  - `<reasoning>...</reasoning>`
- keep tool-call parsing isolated
- ensure final `AssistantMessage` matches streamed reasoning/text deltas

Gate:
- tagged reasoning becomes thinking blocks in the transcript
- raw tags do not leak into visible answer text when parsing succeeds
- tool-call behavior is unchanged

---

## Step 4 — Capability model and config

Reference:
- `docs/phased_plan_v1-0-1/step_4_reasoning_capability_model_and_config.md`

Implement:
- richer reasoning metadata in `anie-provider`
- per-model config overrides in `anie-config`
- explicit capability representation for:
  - control mode
  - output mode
  - optional tags
- deterministic effective-profile precedence

Gate:
- models can express more than `supports_reasoning: bool`
- config overrides are backward-compatible and test-covered
- control mode and output mode can be combined explicitly

---

## Step 5 — Native reasoning controls for modern local backends

Reference:
- `docs/phased_plan_v1-0-1/step_5_native_reasoning_controls_for_modern_local_backends.md`

Implement:
- native reasoning request shaping in the OpenAI-compatible provider
- backend-aware request strategies for:
  - Ollama
  - LM Studio
  - vLLM
- prefer interoperable fields first
- fallback/retry once on unsupported-field compatibility errors
- negative-capability caching per backend/model/strategy

Backend targets:
- Ollama: top-level `reasoning_effort`
- vLLM: top-level `reasoning_effort`
- LM Studio: try top-level `reasoning_effort`, then nested `reasoning: { effort: ... }` if needed

Gate:
- modern local backends can use native reasoning controls when their profile calls for it
- unsupported field shapes fall back safely
- repeated runs do not keep retrying the same known-bad strategy

---

## Step 6 — Native separated reasoning output

Reference:
- `docs/phased_plan_v1-0-1/step_6_native_separated_reasoning_output.md`

Implement:
- parse native separated reasoning output in the OpenAI-compatible provider
- check fields in this order:
  1. `delta.reasoning`
  2. `delta.reasoning_content`
  3. `delta.thinking`
  4. `delta.content`
- prefer native separated fields first
- fall back to tag parsing when native fields are absent
- ensure final `AssistantMessage` faithfully reproduces streamed reasoning content

Gate:
- native separated reasoning is rendered as thinking blocks
- tag parsing still works as fallback
- final assistant messages match streamed deltas

---

## Step 7 — Backend profiles, token budgets, and release validation

Reference:
- `docs/phased_plan_v1-0-1/step_7_backend_profiles_token_budgets_and_release_validation.md`

Implement:
- conservative backend/model-family defaults
- token-headroom policy for verbose local reasoners
- validation matrix across:
  - Ollama
  - LM Studio
  - vLLM
  - unknown local OpenAI-compatible models
  - explicitly configured models
- regression coverage for session persistence, replay, and compaction under reasoning-heavy transcripts

Gate:
- backend defaults are explainable and conservative
- explicit config still wins
- token budgeting is predictable enough to avoid obvious truncation regressions
- session/replay/compaction remain stable with local reasoning enabled

---

## Recommended stop points

If schedule or risk forces partial delivery, stop only at these boundaries:

### Stop point A — after Step -1
Ship-worthy as a targeted compatibility patch if needed.

Includes:
- system prompt forwarding on OpenAI-compatible path
- reasoning-only delta parsing
- empty-stop protection
- empty-assistant replay regression protection

### Stop point B — after Step 0
Ship-worthy as a compatibility + usability patch.

Includes:
- all of Stop point A
- robust transcript navigation for long outputs

### Stop point C — after Step 3
Ship-worthy as a strong MVP for local reasoning.

Includes:
- compatibility fixes
- usable TUI transcript navigation
- local prompt steering
- tagged reasoning parsing

### Stop point D — after Step 6
Ship-worthy as a first-class local reasoning release.

Includes:
- native reasoning controls
- native separated reasoning output
- fallback behavior
- capability model/config

---

## v1.0.1 completion checklist

Call this work “done enough” for v1.0.1 only when all of the following are true:

- [ ] OpenAI-compatible local-provider correctness bugs are fixed
- [ ] system prompt is forwarded on the OpenAI-compatible path
- [ ] reasoning-only local responses are preserved as thinking content
- [ ] truly empty successful assistant turns are rejected
- [ ] transcript scrolling/navigation is robust in the TUI
- [ ] long wrapped assistant messages are navigable
- [ ] local onboarding no longer silently disables thinking
- [ ] `/thinking` affects local-model behavior
- [ ] tagged local reasoning is rendered as thinking blocks
- [ ] capability metadata/config is in place
- [ ] native local reasoning controls work for modern backends
- [ ] native separated reasoning output works for modern backends
- [ ] unsupported native field shapes degrade safely
- [ ] session persistence, replay, and compaction remain stable
- [ ] hosted-provider behavior remains stable

---

## Relationship to the main implementation order

This file is a focused follow-up order for the current local-model and TUI work.

It does **not** replace the original workspace-wide sequencing in:
- `docs/IMPLEMENTATION_ORDER.md`

Instead, it refines the execution order for the current patch/feature track.
