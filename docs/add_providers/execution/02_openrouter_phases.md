# Milestone 1 — OpenRouter

Three PRs. First two are required for a working OpenRouter
integration; the third is a conditional polish pass for search
UX if the shipped substring search turns out to be insufficient.

Spec reference: [`../01_openrouter.md`](../01_openrouter.md).

## Dependencies

- Milestone 0 (Foundation) — both PRs.

## PR A — OpenRouter preset, onboarding, discovery parser

**Goal:** User adds OpenRouter via the existing onboarding
form, anie fetches the `/api/v1/models` catalog, and OpenRouter
becomes a configured provider with its full model list
available in the `/model` picker.

### Files
- `crates/anie-providers-builtin/src/provider_presets.rs` (new
  or added-to) — OpenRouter preset entry.
- `crates/anie-providers-builtin/src/model_discovery.rs` —
  extend `OpenAiModelsResponse` fields + parsing to capture
  OpenRouter's pricing / context / modalities.
- `crates/anie-provider/src/model.rs` — extend `ModelInfo` if
  needed to carry pricing + reasoning flags through to the
  conversion step.
- `crates/anie-tui/src/overlays/onboarding.rs` — add OpenRouter
  to the first-run shortlist.
- `crates/anie-auth/src/lib.rs` — no changes; the existing
  `CredentialStore` handles OPENROUTER_API_KEY via the
  provider's `env_var`.

### Steps

1. **Preset.** Register the OpenRouter preset (category
   `OpenAICompatible` if the preset enum is in place; otherwise
   inline the onboarding entry directly).
   - name: `openrouter`
   - display: `OpenRouter`
   - api_kind: `OpenAICompletions`
   - base_url: `https://openrouter.ai/api/v1`
   - env_var: `OPENROUTER_API_KEY`
   - api_key_url: `https://openrouter.ai/keys`

2. **Discovery parser upgrade.** Extend the existing
   `OpenAiModelsResponse` / `OpenAiModelEntry` shapes to capture
   OpenRouter's richer fields:

   ```rust
   pub(crate) struct OpenAiModelEntry {
       pub id: String,
       pub name: Option<String>,
       pub context_length: Option<u64>,
       // new:
       pub pricing: Option<PricingInfo>,
       pub top_provider: Option<TopProviderInfo>,
       pub supported_parameters: Option<Vec<String>>,
       pub architecture: Option<ArchitectureInfo>,
   }

   pub(crate) struct PricingInfo {
       pub prompt: Option<String>,    // per-token, string-encoded
       pub completion: Option<String>,
       pub request: Option<String>,
       pub image: Option<String>,
   }
   ```

   All new fields are `Option<T>` with `#[serde(default)]` so the
   existing OpenAI `/models` endpoint (which doesn't return
   these) keeps working.

3. **ModelInfo → Model conversion.** `ModelInfo::to_model` (in
   `anie-provider/src/model.rs`) currently doesn't know about
   pricing or reasoning supported-parameter flags. Either extend
   it to carry optional pricing, or populate those downstream in
   PR B's capability-mapping step. PR A's test uses whichever
   seam is simpler.

4. **Onboarding shortlist.** Add OpenRouter as the fourth entry:
   `[Anthropic] [OpenAI] [OpenRouter] [Ollama] [Skip]`. When
   selected, prompt for API key → save to credential store →
   call `ModelDiscoveryCache::get_or_discover` immediately →
   return to TUI with OpenRouter set as the active provider.

5. **Discovery failure handling.** If the fetch fails (network
   error, bad key), the key is still saved; a system message
   tells the user "Models will be discovered next time /model
   is opened." The picker's existing retry-on-open path handles
   this.

### Test plan

| # | Test |
|---|---|
| 1 | `openrouter_preset_registered_and_in_onboarding_shortlist` |
| 2 | `openrouter_discovery_parses_full_response` — fixture file with a realistic OpenRouter `/models` response slice; assert pricing, context_length, supported_parameters all parsed. |
| 3 | `openrouter_discovery_falls_back_when_fetch_fails` — mock network failure, assert system message + credential preserved. |
| 4 | `openrouter_preset_flow_populates_catalog` — integration-level test using the mock discovery path; assert the catalog has the test fixture's entries after onboarding completes. |

### Exit criteria

- [ ] OpenRouter appears in onboarding first-run shortlist.
- [ ] Configure flow succeeds with API key → discovery → return
      to TUI with models populated.
- [ ] Discovery parser captures pricing, context,
      supported_parameters, architecture.
- [ ] Discovery failure leaves credential intact and surfaces
      a recoverable system message.
- [ ] Existing OpenAI discovery continues to work.
- [ ] Tests 1–4 pass.

---

## PR B — Upstream-aware capability mapping + nested reasoning

**Goal:** OpenRouter models get the right `ReplayCapabilities`
and `ReasoningCapabilities` automatically based on the upstream
prefix, and reasoning requests use the nested
`reasoning: { effort }` body shape.

### Files
- `crates/anie-providers-builtin/src/openrouter.rs` (new) — or
  a small module within `provider_presets.rs` — hosts the
  `openrouter_capability_mapping` function.
- `crates/anie-providers-builtin/src/lib.rs` — hook the mapping
  function into the discovery→Model conversion for the
  OpenRouter provider specifically.
- Request-building path (provider_presets → openai/mod.rs) —
  ensure the compat blob's `openrouter_routing` (if set) is
  emitted into the body as `provider: {...}`.

### Steps

1. **Capability mapping function.**

   ```rust
   pub(crate) fn openrouter_capabilities_for(
       model_id: &str,
       supports_reasoning: bool,
   ) -> (Option<ReplayCapabilities>, Option<ReasoningCapabilities>) {
       let upstream = model_id.split_once('/').map(|(u, _)| u);
       let replay = match (upstream, supports_reasoning) {
           (Some("anthropic"), true) => Some(ReplayCapabilities {
               requires_thinking_signature: true,
               supports_redacted_thinking: false,
               supports_encrypted_reasoning: false,
           }),
           // ... per the spec table
           _ => None,
       };
       let reasoning = if supports_reasoning {
           Some(ReasoningCapabilities {
               control: Some(ReasoningControlMode::Native),
               output: Some(ReasoningOutputMode::Separated),
               request_mode: Some(ThinkingRequestMode::NestedReasoning),
               tags: None,
           })
       } else {
           None
       };
       (replay, reasoning)
   }
   ```

2. **Wire into the conversion.** When OpenRouter's
   `ModelDiscoveryCache` yields `Vec<ModelInfo>`, convert each
   via `to_model` → run through `openrouter_capabilities_for`
   → merge the returned capabilities into the `Model`.

3. **Routing preferences in body.** When building an outbound
   Chat Completions body for an OpenRouter-configured model
   whose `compat` has
   `ModelCompat::OpenAICompletions { openrouter_routing: Some(r) }`,
   include `body["provider"] = serialize(r)`.

### Test plan

| # | Test |
|---|---|
| 5 | `openrouter_capabilities_anthropic_reasoning_sets_signature` — input `anthropic/claude-sonnet-4.6` + `supports_reasoning=true` → `requires_thinking_signature: true, request_mode: NestedReasoning`. |
| 6 | `openrouter_capabilities_openai_o_series_sets_nested_reasoning` — input `openai/o3` → `request_mode: NestedReasoning`. |
| 7 | `openrouter_capabilities_google_reasoning_sets_nested_reasoning` — input `google/gemini-2.5-pro` + reasoning true → `request_mode: NestedReasoning`, `requires_thinking_signature: false` (the thoughtSignature-via-OR open question; default false). |
| 8 | `openrouter_capabilities_non_reasoning_returns_none` — input `meta-llama/llama-3.3-70b-instruct` → capabilities None. |
| 9 | `openrouter_request_uses_nested_reasoning_object` — build a request body for a reasoning model, assert `reasoning.effort = "high"` and no top-level `reasoning_effort`. |
| 10 | `openrouter_request_non_reasoning_model_unchanged` — non-reasoning model's body has neither `reasoning` nor `reasoning_effort`. |
| 11 | `openrouter_routing_preferences_propagate_to_body` — set `openrouter_routing = { order: Some(vec!["anthropic"]), only: None, ... }`, assert outbound body has `provider.order = ["anthropic"]`. |
| 12 | `openrouter_routing_none_omits_provider_field` — regression: no routing preferences means no `provider` field in body. |
| 13 | Invariant suite: OpenRouter appears in `provider_replay.rs` with at least three fixtures (Anthropic upstream, OpenAI upstream, non-reasoning upstream). |

### Exit criteria

- [ ] `openrouter_capabilities_for` exists and is unit-tested
      per the spec's table.
- [ ] Discovered OpenRouter models land with correct replay +
      reasoning capabilities.
- [ ] Reasoning requests use nested `reasoning.effort` shape.
- [ ] `openrouter_routing` compat flag surfaces in the body
      when set.
- [ ] Invariant suite covers OpenRouter.
- [ ] Manual smoke: two-turn conversation on
      `anthropic/claude-sonnet-4.6` with thinking `high`
      completes without replay errors, documented in PR.

---

## PR C — Model picker polish (CONDITIONAL)

**Goal:** If 500+ models in the picker feels unusable under
substring search, add fuzzy scoring + optional provider-prefix
grouping.

### When to do this

**Do not open this PR unless PR B has shipped and at least one
user (you) has tried the picker with a real OpenRouter catalog
and reported the search is too noisy or too slow.** We ship PR
B first with the existing case-insensitive substring search.

### If needed: files
- `crates/anie-tui/src/widgets/fuzzy.rs` (new or lifted from
  existing test helper)
- `crates/anie-tui/src/overlays/model_picker.rs` — swap
  substring for fuzzy

### If needed: steps

1. Implement or extract a `fuzzy_score(query, candidate) -> u32`
   that favors:
   - exact matches (highest)
   - prefix matches
   - word-start matches (e.g. `anthropic/claude` → `a/c` scores
     high)
   - contiguous substring matches
2. Replace `ModelPickerPane::apply_filter`'s
   `contains(&search)` check with fuzzy scoring; sort filtered
   results by score descending.
3. Optional: when `search.is_empty()`, render section headers
   (`── anthropic ──`, etc.) before each provider-prefix group.
   This is list-item-kind UI work; test with TestBackend
   snapshots.

### Exit criteria (if landed)

- [ ] Fuzzy search scores matches correctly per table tests.
- [ ] 500-entry picker filters responsively (no perceptible
      lag on typing).
- [ ] Substring-match regression suite still passes — fuzzy
      is strictly more permissive.

### Exit criteria (if NOT needed)

- [ ] Explicit note in the milestone retro: "Substring search
      at 500+ models was sufficient in practice. Fuzzy polish
      deferred."

---

## Milestone exit criteria

- [ ] PRs A and B merged.
- [ ] User with an OPENROUTER_API_KEY can run onboarding, land
      in the TUI with OpenRouter as the active provider, open
      `/model`, search for any upstream, and successfully
      converse with at least one Anthropic-upstream reasoning
      model across two turns.
- [ ] No curated OpenRouter entries in `builtin_models()` —
      everything discovery-driven.
- [ ] PR C closed (landed if needed, deferred with a note
      otherwise).

## What ships to main at the end

A user with an OpenRouter key can use any model OpenRouter
exposes, including reasoning models from Anthropic / OpenAI /
Google / DeepSeek, with correct replay handling across turns.
The four other originally-planned providers (xAI, Groq,
Cerebras, Mistral) remain unshipped but are reachable through
OpenRouter's catalog — a user who wants Grok 4 configures
`xai/grok-4` through OpenRouter rather than opening a direct xAI
account.
