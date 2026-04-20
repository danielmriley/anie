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

## PR B — Upstream-aware capability mapping + OR response quirks

**Goal:** OpenRouter models get the right `ReplayCapabilities`
and `ReasoningCapabilities` based on the upstream prefix.
Reasoning requests use the nested body shape. The four
OR-specific response behaviors that pi documented all work:

- All three reasoning-field names on the way in
  (`reasoning_content`, `reasoning`, `reasoning_text`).
- `reasoning_details` round-trip for o-series encrypted
  reasoning across turns.
- Anthropic `cache_control: ephemeral` insertion for
  `anthropic/*` upstreams.
- Tool-supporting filter on discovered catalog.

### Files
- `crates/anie-providers-builtin/src/openrouter.rs` (new) —
  capability mapping + Anthropic cache-control helper.
- `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs`
  — add `reasoning_text` to the reasoning-field lookup.
- `crates/anie-providers-builtin/src/openai/streaming.rs` — add
  `reasoning_details` capture into a new per-message state
  slot.
- `crates/anie-providers-builtin/src/openai/convert.rs` (or
  wherever outbound messages are built) — emit
  `reasoning_details` on assistant replay messages.
- `crates/anie-protocol/src/content.rs` — add
  `reasoning_details: Option<Vec<serde_json::Value>>` on
  `AssistantMessage`, or (cleaner) `thought_signature:
  Option<String>` on `ContentBlock::ToolCall` to match pi's
  per-tool-call storage.
- `crates/anie-providers-builtin/src/model_discovery.rs` —
  filter discovered entries to models whose
  `supported_parameters` contains `"tools"`, gated to the
  OpenRouter provider.

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
               supports_reasoning_details_replay: false,
           }),
           (Some("openai"), true)
               if is_o_series_or_gpt5(model_id) =>
               Some(ReplayCapabilities {
                   requires_thinking_signature: false,
                   supports_redacted_thinking: false,
                   supports_encrypted_reasoning: false,
                   supports_reasoning_details_replay: true,
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

   Extend `ReplayCapabilities` with
   `supports_reasoning_details_replay: bool` (new field, default
   `false`). Same pattern as `requires_thinking_signature`.

2. **Wire into discovery conversion.** OR's discovery path
   converts `ModelInfo` → `Model` and runs
   `openrouter_capabilities_for` to fill the capability fields.

3. **`reasoning_text` on the way in.** Add `"reasoning_text"`
   to the reasoning-field lookup list in
   `reasoning_strategy.rs:214`. Existing callers pick it up
   automatically.

4. **`reasoning_details` capture.** In `streaming.rs`, parse
   the delta's `reasoning_details` array. For each element:
   - If `type == "reasoning.encrypted"` and `id` matches an
     in-flight tool call, attach the serialized JSON to that
     tool call's `thought_signature` field (adopts pi's
     per-tool-call storage).
   - Else skip for v1 (other detail types we don't understand
     yet).

5. **`reasoning_details` replay.** In the outbound-message
   converter, collect every tool call's non-empty
   `thought_signature`, parse each back to a JSON object, and
   attach the array to the assistant message as
   `reasoning_details: [...]`. Only when the model's
   `supports_reasoning_details_replay` is true.

6. **Anthropic `cache_control` insertion.** In
   `OpenAIProvider::convert_messages`, when the model's
   `base_url` contains `openrouter.ai` AND the model's `id`
   starts with `"anthropic/"`, walk messages back-to-front,
   find the last text part, and attach
   `{ "type": "text", "text": "...", "cache_control": { "type": "ephemeral" } }`.
   No-op for other providers.

7. **Routing preferences in body.** When
   `ModelCompat::OpenAICompletions { openrouter_routing: Some(r) }`
   is set, emit `body["provider"] = serialize(r)`.

8. **Tool-supporting filter on discovery.** Extend the
   OR-branch of `discover_openai_compatible_models` (or add an
   OR-specific branch) to drop entries whose
   `supported_parameters` lacks `"tools"`.

### Test plan

| # | Test |
|---|---|
| 5 | `openrouter_capabilities_anthropic_reasoning_sets_signature` |
| 6 | `openrouter_capabilities_openai_o_series_sets_reasoning_details_replay` |
| 7 | `openrouter_capabilities_google_reasoning_sets_nested_reasoning` |
| 8 | `openrouter_capabilities_non_reasoning_returns_none` |
| 9 | `openrouter_request_uses_nested_reasoning_object` |
| 10 | `openrouter_request_non_reasoning_model_unchanged` |
| 11 | `openrouter_routing_preferences_propagate_to_body` |
| 12 | `openrouter_routing_none_omits_provider_field` |
| 13 | `openrouter_reasoning_text_field_captured_as_thinking` — fixture where stream delta uses `reasoning_text` instead of `reasoning`. |
| 14 | `openrouter_reasoning_details_encrypted_attached_to_tool_call` — fixture delta with `reasoning_details: [{type: "reasoning.encrypted", id: "call_abc", data: "…"}]` plus a tool call `call_abc`; assert the tool call's `thought_signature` has the serialized detail. |
| 15 | `openrouter_reasoning_details_round_trip_on_replay` — captured `thought_signature` on a ToolCall is emitted as `reasoning_details` on the outbound assistant message. |
| 16 | `openrouter_anthropic_upstream_adds_cache_control_to_last_text` — build a request body for an `anthropic/*` model over OR; assert the last user message's last text part has `cache_control: ephemeral`. |
| 17 | `openrouter_non_anthropic_upstream_does_not_add_cache_control` — regression for `openai/*` routed through OR. |
| 18 | `openrouter_discovery_filters_non_tool_models` — fixture with some tool-supporting and some non-tool entries; assert only tool-supporting land in the catalog. |
| 19 | Invariant suite: OpenRouter appears in `provider_replay.rs` with Anthropic-upstream, OpenAI-o-series-upstream, and non-reasoning-upstream fixtures. |

### Exit criteria

- [ ] `openrouter_capabilities_for` unit-tested per the spec
      table including the revised `openai/o*` →
      `supports_reasoning_details_replay: true` row.
- [ ] All three reasoning field names captured on stream-in.
- [ ] `reasoning_details` round-trips for `openai/o*`
      upstreams.
- [ ] Anthropic `cache_control` inserted for `anthropic/*`
      upstreams only.
- [ ] Discovered catalog filtered to tool-supporting models.
- [ ] Reasoning requests use nested body shape.
- [ ] `openrouter_routing` compat flag surfaces when set.
- [ ] Invariant suite covers OR with three distinct upstreams.
- [ ] Manual smoke: two-turn conversation on
      `anthropic/claude-sonnet-4.6` with thinking `high`
      completes without replay errors, documented in PR.
- [ ] Manual smoke: two-turn conversation on `openai/o3` with
      thinking `high` preserves reasoning context (the
      second-turn response acknowledges the first-turn
      reasoning), documented in PR.

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
