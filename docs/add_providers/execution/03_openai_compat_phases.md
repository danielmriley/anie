# Milestone 3 — OpenAI-compat batch

Four PRs, one per provider. Order: Mistral → xAI → Groq →
Cerebras. Mistral goes first because it's the least quirky and
sets the template. xAI's `reasoning_effort` for Grok 4 is the
most interesting branch-case and benefits from the template
being in place.

Spec reference: [`../02_openai_compat_batch.md`](../02_openai_compat_batch.md).

## Dependencies

- Milestone 0 (Foundation).
- Milestone 1 (UX prerequisite).

Plan 01 (OpenRouter) is **not** a dependency. These providers
are independent of each other.

## Parallelism

The four sub-providers inside this milestone are independent —
different engineers can take them. A single engineer shipping
sequentially should follow the order below because each PR
reuses patterns established by the previous one.

---

## PR A — Mistral (OpenAI-compat, non-reasoning models)

**Goal:** Mistral Large, Codestral, Ministral 8B usable via the
shared OpenAI Chat Completions provider.

### Files
- `crates/anie-providers-builtin/src/provider_presets.rs`
- `crates/anie-providers-builtin/src/models.rs`

### Catalog entries

Three entries per the spec's Mistral table. No reasoning
capabilities — magistral models are out of scope for v1 and are
flagged in the spec as a follow-up.

### Test plan

| # | Test |
|---|---|
| 1 | `mistral_preset_registered` |
| 2 | `mistral_catalog_contains_large_codestral_ministral` |
| 3 | `mistral_request_targets_api_mistral_ai_base_url` |
| 4 | Invariant suite: Mistral gets `mistral_model()` and `build_mistral_body()` helpers; cross-provider invariants all pass. |

### Exit criteria

- [ ] Mistral preset registered under
      `ProviderCategory::OpenAICompatible`.
- [ ] Three catalog entries.
- [ ] Invariant suite covers Mistral.
- [ ] Manual smoke: `mistral-large-latest` with a simple prompt.

---

## PR B — xAI (Grok with reasoning_effort)

**Goal:** Grok 2/3/4 usable. Grok 4's reasoning flows through
the existing `ReasoningEffort` `ThinkingRequestMode` — this is
flat `reasoning_effort`, not the nested OpenRouter variant.

### Files
- `crates/anie-providers-builtin/src/provider_presets.rs`
- `crates/anie-providers-builtin/src/models.rs`

### Catalog entries

Three entries per the spec's xAI table. Grok 4 gets
`reasoning_capabilities.request_mode = ReasoningEffort` (existing
variant); Grok 2 Vision and Grok 3 get `reasoning_capabilities =
None`.

### Test plan

| # | Test |
|---|---|
| 5 | `xai_preset_registered` |
| 6 | `grok_4_reasoning_uses_flat_reasoning_effort_field` — contrast with plan 01's nested form. |
| 7 | Invariant suite covers xAI. |

### Exit criteria

- [ ] xAI preset registered.
- [ ] Three catalog entries.
- [ ] Reasoning flows via the flat `reasoning_effort` field.
- [ ] Manual smoke: `grok-4` with thinking `high`.

---

## PR C — Groq (tagged reasoning for DeepSeek-R1)

**Goal:** Groq models usable, with DeepSeek-R1 Distill's
`<think>…</think>` reasoning surfacing through the existing
`tagged_reasoning.rs` splitter.

### Files
- `crates/anie-providers-builtin/src/provider_presets.rs`
- `crates/anie-providers-builtin/src/models.rs`
- (No splitter changes expected — plan 02 verified the existing
  splitter handles this. Confirm via the test below.)

### Catalog entries

Four entries per the spec's Groq table. DeepSeek-R1 Distill gets
`reasoning_capabilities.control = PromptWithTags, output =
Tagged`. Others have `reasoning_capabilities = None`.

### Test plan

| # | Test |
|---|---|
| 8 | `groq_preset_registered` |
| 9 | `deepseek_r1_distill_tagged_reasoning_routes_through_existing_splitter` — fixture where the stream content contains `<think>…</think>`, assert the resulting `AssistantMessage` has a Thinking block and a Text block with the non-tagged content. |
| 10 | `groq_x_groq_field_in_delta_does_not_trigger_unsupported_warning` — fixture with the quirky Groq-specific delta field, assert no stderr log. |
| 11 | Invariant suite covers Groq. |

### Exit criteria

- [ ] Groq preset registered.
- [ ] Four catalog entries.
- [ ] Tagged reasoning test passes against the existing
      splitter (no splitter changes needed).
- [ ] Manual smoke: `deepseek-r1-distill-llama-70b` with a
      prompt that elicits reasoning.

---

## PR D — Cerebras

**Goal:** Cerebras Llama 3.3 70B, Llama 3.1 8B, Qwen 3 32B usable.

### Files
- `crates/anie-providers-builtin/src/provider_presets.rs`
- `crates/anie-providers-builtin/src/models.rs`

### Catalog entries

Three entries per the spec's Cerebras table. Qwen 3 32B gets
tagged reasoning capabilities identical to Groq's DeepSeek-R1
row.

### Test plan

| # | Test |
|---|---|
| 12 | `cerebras_preset_registered` |
| 13 | `cerebras_catalog_contains_three_models` |
| 14 | Invariant suite covers Cerebras. |

### Exit criteria

- [ ] Cerebras preset registered.
- [ ] Three catalog entries.
- [ ] Invariant coverage.
- [ ] Manual smoke: `llama-3.3-70b` fast-inference confirms
      latency win.

---

## Milestone exit criteria

- [ ] All four PRs merged.
- [ ] All four providers appear in the category picker's
      `OpenAICompatible` group.
- [ ] Invariant suite exercises Mistral, xAI, Groq, and
      Cerebras on every cross-provider invariant.
- [ ] No changes required to the shared OpenAI Chat Completions
      provider module — everything ships as preset + catalog
      additions.
