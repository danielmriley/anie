# v1.0.1 Implementation Review

**Reviewer:** anie coding agent  
**Scope:** Steps 1–7 of `docs/phased_plan_v1-0-1/`  
**Date:** 2026-04-14  
**Test status:** all 151 workspace tests pass

---

## Summary

The implementation is **strong overall**. Every planned step has corresponding code and test coverage. The architecture stayed clean: reasoning behavior lives in the provider layer, the controller still owns only `ThinkingLevel`, and the TUI was not touched for reasoning concerns.

One planned item from the step docs was not implemented. Several areas deserve minor attention. No blockers found.

---

## Step-by-step assessment

### Step 1 — System-prompt insertion point
**Status:** ✅ Complete

- `openai_request_messages(...)` is a centralized helper that prepends the system prompt and appends converted messages
- `effective_system_prompt(...)` provides the clean augmentation hook for later prompt steering
- blank/whitespace-only system prompts are correctly omitted
- message ordering is explicit and test-covered (`request_body_prepends_system_prompt_and_preserves_message_order`, `request_body_omits_blank_system_prompt`)
- `convert_messages(...)` was not reworked — good, separation preserved

### Step 2 — Local defaults and prompt steering MVP
**Status:** ✅ Complete

- onboarding no longer writes `thinking = "off"` for local-first or custom-provider configs — both use `"medium"` now
- `local_reasoning_prompt_steering(...)` provides distinct wording for all four levels
- prompt steering is scoped to local targets via `is_local_openai_compatible_target(...)`
- hosted providers are explicitly unaffected (`hosted_blank_system_prompt_stays_omitted_without_local_prompt_steering`)
- `supports_reasoning` was not blindly flipped — good, avoids premature native field injection
- test coverage: `local_effective_system_prompt_varies_by_thinking_level`, `local_request_body_adds_prompt_steering_without_native_reasoning_fields`
- onboarding tests: `detected_local_config_uses_medium_thinking_by_default`, `custom_provider_config_uses_medium_thinking_by_default`

### Step 3 — Tagged reasoning stream parsing MVP
**Status:** ✅ Complete

- `TaggedReasoningSplitter` is a clean, isolated state machine
- supports all three built-in aliases: `<think>`, `<thinking>`, `<reasoning>`
- chunk-boundary splitting is handled for both opening and closing tags
- malformed/unclosed tags degrade to visible text — no content loss
- tagged parsing only runs on `delta.content` when no native reasoning field was present in the same event — good, avoids double-counting
- final `AssistantMessage` matches streamed deltas (thinking buffer accumulated in `OpenAiStreamState`)
- tool-call parsing stays independent
- test coverage is thorough: chunk splits, aliases, multiple spans, malformed sequences, mixed native+content

### Step 4 — Capability model and config
**Status:** ✅ Complete

- `ReasoningControlMode`, `ReasoningOutputMode`, `ReasoningTags`, `ReasoningCapabilities` added to `anie-provider`
- `Model` now carries optional `reasoning_capabilities`
- config schema extended with `reasoning_control`, `reasoning_output`, `reasoning_tag_open`, `reasoning_tag_close`
- backward compatibility preserved: old configs without new fields still parse (`model_serde_is_backward_compatible_without_reasoning_capabilities`)
- roundtrip tested (`model_serde_roundtrips_reasoning_capabilities`)
- config-to-model mapping via `custom_model_reasoning_capabilities(...)` is clean and tested
- `supports_reasoning: bool` retained for compatibility — good

**Observation — naming divergence:**
The plan docs used `NativeOpenAiReasoning` / `PromptOnly` / `PromptWithTags` + `NativeDeltas` / `TaggedText`. The implementation uses `Native` / `Prompt` + `Separated` / `Tagged`. The implementation names are cleaner and more concise. This is a reasonable simplification, but the step docs should be updated to match if they remain reference material.

### Step 5 — Native reasoning controls for modern local backends
**Status:** ✅ Complete

- `NativeReasoningRequestStrategy` enum with `TopLevelReasoningEffort`, `LmStudioNestedReasoning`, `NoNativeFields`
- `openai_compatible_backend(...)` identifies Ollama / LM Studio / vLLM / unknown local / hosted
- `native_reasoning_request_strategies(...)` selects the strategy chain based on backend + capability profile
- LM Studio gets the three-strategy chain (top-level → nested → no-native) as planned
- Ollama and vLLM get top-level → no-native
- `is_native_reasoning_compatibility_error(...)` classifies 400s narrowly — good, avoids hiding real errors
- negative-capability caching is per `(base_url, model_id, strategy)` via `NativeReasoningCacheKey`
- `send_stream_request(...)` loops through strategies and falls back correctly
- `ThinkingLevel::Off` correctly omits native fields for local native models
- test coverage: Ollama/vLLM/LM Studio strategy chains, fallback after cached failure, cache scoping, compatibility error classification

**Observation — `send_stream_request` ownership change:**
The `Provider::stream(...)` now clones `self` (`provider = self.clone()`) and uses the instance method `provider.send_stream_request(...)` inside the async stream. This is a reasonable approach given the strategy state, but the `Clone` derive on `OpenAIProvider` is worth noting as a design trade-off since it shares the negative-capability cache via `Arc<Mutex<...>>`.

### Step 6 — Native separated reasoning output
**Status:** ✅ Complete

- `native_reasoning_delta(...)` checks `reasoning`, `reasoning_content`, `thinking` in priority order
- native reasoning fields are checked before `delta.content` in `process_event(...)`
- when a native reasoning field is present in a delta, `delta.content` bypasses the tag parser and is treated as plain text — correct, avoids double-counting
- when no native reasoning field is present, `delta.content` goes through the tagged splitter
- per-event mixing of reasoning + content is preserved (`same_event_native_reasoning_and_text_are_both_preserved`)
- final `AssistantMessage` includes `ContentBlock::Thinking` and `ContentBlock::Text` separately
- test coverage: all three native field aliases, mixed reasoning+text, fallback to tagged parsing

### Step 7 — Backend profiles, token budgets, and release validation
**Status:** ⚠️ Mostly complete

**Done well:**
- `default_local_reasoning_capabilities(...)` in `local.rs` provides conservative, explainable heuristic defaults
- known reasoning families (`qwen3`, `qwq`, `deepseek-r1`, `gpt-oss`) get native+separated profile
- unknown local models get prompt-only profile
- non-local models get `None` — good, avoids polluting hosted profiles
- `effective_max_tokens(...)` applies token headroom based on thinking level and visible reasoning likelihood
- headroom values are predictable and tested (`local_reasoning_token_headroom_changes_predictably_with_thinking_level`)
- hosted models are unaffected by headroom
- `dedupe_models(...)` added to controller for catalog hygiene
- session roundtrip test for thinking blocks: `session_roundtrip_preserves_thinking_blocks_after_reopen`
- compaction under reasoning-heavy transcripts: `auto_compact_collects_thinking_deltas_for_reasoning_heavy_transcripts`
- built-in hosted models now have explicit reasoning profiles: `builtin_hosted_models_have_explicit_reasoning_profiles`
- auto-detected local models carry reasoning capabilities based on heuristics

**Observation — token headroom direction is inverted:**
`effective_max_tokens(...)` currently **reduces** `max_tokens` as thinking level increases. The plan says to "reserve extra completion headroom" for verbose reasoners — meaning the model should get **more** output budget, not less, so reasoning doesn't crowd out the final answer. The current implementation subtracts headroom from the user-facing max, which means higher thinking = smaller effective max_tokens. This may be intentional as a "leave room for reasoning within the existing budget" strategy, but it is the opposite of the plan's stated direction. Worth clarifying the intended semantics and documenting the decision.

---

## Missing planned item

### Empty-stop protection (from Step -1 plan)

The step -1 plan called for:
> If an OpenAI-compatible response ends with no text, no thinking, no tool calls, no provider error, and finish_reason = "stop", treat it as a provider/harness error instead of silently returning an empty assistant message.

The current `into_message()` / `finish_stream()` does **not** check for this case. A truly empty successful stop will still produce an `AssistantMessage` with an empty `content` vec and `StopReason::Stop`.

The defensive guard in `assistant_message_to_openai_llm_message(...)` prevents such a message from being replayed as an invalid follow-up, which is good. But the empty turn can still be persisted into the session and displayed as a blank assistant block.

**Recommendation:** add a check in `finish_stream()` — if all buffers are empty and `finish_reason` indicates success, emit `ProviderEvent::Done(...)` with `StopReason::Error` and an `error_message` instead of a blank successful stop. This was explicitly requested in the bug report.

---

## Additional observations

### 1. `effective_reasoning_capabilities` fallback chain
The function `effective_reasoning_capabilities(...)` falls back to `default_local_reasoning_capabilities(...)` when `model.reasoning_capabilities` is `None`. This means auto-detected local models that have no explicit config will still get heuristic profiles at request time. That's the right behavior, but it also means `native_reasoning_request_strategies(...)` can return native strategies for models that were auto-detected without explicit config. This is intentional and matches the plan, but worth being aware of during manual validation.

### 2. `assistant_message_to_openai_llm_message` now includes thinking content
The function now joins `ContentBlock::Thinking` into the replayed assistant text alongside `ContentBlock::Text`. This means a reasoning-only assistant turn (thinking content, no visible text) will be replayed as an assistant message with the thinking text as `content`. This prevents the empty-replay 400, but it means the model will see prior reasoning as if it were visible text. This is an acceptable pragmatic choice for now.

### 3. Config backward compatibility is solid
The new `CustomModelConfig` fields all use `#[serde(default)]` and `Option<...>`, so old TOML files without reasoning fields continue to parse. Tested explicitly.

### 4. No Step 0 (TUI scrolling) changes
TUI scrolling was not part of Steps 1–7, so this is expected. No TUI code was modified for reasoning concerns.

---

## Test coverage summary

| Area | New/updated tests | Assessment |
|---|---|---|
| System prompt forwarding | 2 | ✅ Good |
| Prompt steering | 3 | ✅ Good |
| Native reasoning delta parsing | 3 | ✅ Good |
| Tagged reasoning parsing | 6 | ✅ Thorough |
| Native reasoning request strategies | 5 | ✅ Thorough |
| Negative-capability caching | 2 | ✅ Good |
| Compatibility error classification | 1 | ✅ Good |
| Token headroom | 1 | ✅ Good |
| Model serde backward compat | 2 | ✅ Good |
| Config reasoning capabilities | 1 | ✅ Good |
| Onboarding defaults | 2 | ✅ Good |
| Built-in model profiles | 1 | ✅ Good |
| Local heuristic defaults | 1 | ✅ Good |
| Session thinking persistence | 1 | ✅ Good |
| Compaction with thinking | 1 | ✅ Good |
| Empty-stop protection | 0 | ❌ Missing |

Total new/updated provider-level tests: ~30  
All 151 workspace tests pass.

---

## Recommendations

1. **Add empty-stop protection** — check in `finish_stream()` for truly empty successful completions and convert them to error stops. This was a specific planned deliverable from the bug report.

2. **Clarify token headroom direction** — document whether reducing `max_tokens` on higher thinking levels is intentional or whether the plan's "reserve extra headroom" meant increasing the budget.

3. **Update step doc naming** — if the step docs remain reference material, update the enum names to match the implementation (`Native`/`Prompt`/`Separated`/`Tagged` instead of the longer planned names).

4. **Manual validation** — the automated test coverage is strong, but the manual validation matrix from Step 7 (Ollama recent, LM Studio toggle on/off, vLLM, unknown local, explicit config) should still be exercised before calling v1.0.1 done.
