# Plan 01 — OpenRouter

**Sole focus for the add_providers initiative in this iteration.**
Connect anie to OpenRouter flawlessly, surface the full model
catalog (not a curated subset), and make search usable at the
scale OpenRouter exposes.

## Rationale for scope

Earlier drafts of this plan proposed an eight-model curated
catalog. That's the wrong shape for OpenRouter: its whole value
is **breadth** — ~500 models across every frontier provider plus
dozens of open-weight options. A hand-picked eight is almost
always the wrong eight for any given user.

The right shape is:

1. **Discovery-driven.** Live fetch from
   `https://openrouter.ai/api/v1/models` on first configure and
   on user-requested refresh. No hand-authored OpenRouter entries
   in `builtin_models()`.
2. **Upstream-aware capabilities.** Each OpenRouter model ID
   carries the upstream as a prefix (`anthropic/claude-...`,
   `openai/o3`, `google/gemini-...`). We pattern-match on that
   prefix to assign the right `ReplayCapabilities` and
   `ReasoningCapabilities` automatically, rather than
   hand-authoring per entry.
3. **Search that scales.** The existing `ModelPickerPane` already
   has case-insensitive substring search over id + name. Polish
   it — provider-prefix grouping and fuzzy scoring — only if the
   substring search turns out to be insufficient on 500+ rows.
   Ship with the current picker first.

## User value

- One API key, hundreds of models. No per-provider account
  management.
- Claude Opus, GPT-4, o3, Gemini Pro, DeepSeek, Llama 3.3, Qwen,
  Mistral, and every new model OpenRouter adds — all reachable
  without updating anie.
- Pay-as-you-go. No monthly commitment to try a new model.

## Wire protocol

Reuses `ApiKind::OpenAICompletions`. Endpoint:
`https://openrouter.ai/api/v1/chat/completions`.

## Auth shape

- `Authorization: Bearer {key}`.
- env var: `OPENROUTER_API_KEY`.
- Key URL: `https://openrouter.ai/keys`.

## OpenRouter-specific request behavior

### Nested `reasoning` object (required for reasoning models)

OpenRouter normalizes reasoning across upstreams via
`reasoning: { effort: "high" }`, not OpenAI's flat
`reasoning_effort`. pi's
`packages/ai/src/providers/openai-completions.ts:429` has the
pattern. Without this, reasoning against OpenRouter reasoning
models silently no-ops.

Requires a new `ThinkingRequestMode::NestedReasoning` variant
(foundation work — see `execution/00_foundation.md` PR B). Each
OpenRouter model identified as reasoning-capable (from the
discovery response's `supported_parameters`) gets
`reasoning_capabilities.request_mode = NestedReasoning` set
automatically during `ModelInfo → Model` conversion.

### Provider routing preferences (optional, via compat blob)

OpenRouter's request body accepts a top-level `provider` object
for routing preferences (ordered upstream preferences, ZDR
filters, price ceilings, quantization filters, etc.). Users can
configure these per-model via the `ModelCompat::OpenAICompletions
{ openrouter_routing }` compat blob (foundation work — see
`execution/00_foundation.md` PR A).

Not surfaced in the UI for v1 — `config.toml`-editable only. The
cases where a user actually wants this (pin to Anthropic-only
upstream for privacy reasons, sort by throughput, require ZDR)
are advanced-user scenarios.

### Leaderboard headers — not shipped

`HTTP-Referer` and `X-Title` identify anie on OpenRouter's
public leaderboard. Pi doesn't set them. Cosmetic-only. Skip.

## Discovery-first catalog

### Flow

```
onboarding: pick "OpenRouter" preset
  → prompt for API key
  → save to CredentialStore
  → immediately fetch https://openrouter.ai/api/v1/models
  → convert each response entry to Model (see mapping below)
  → populate OpenRouter's per-provider catalog
  → user returns to TUI with all models available in /model picker
```

`ModelDiscoveryCache` (from the onboarding plans that shipped)
already handles the fetch-and-TTL. We extend it with a richer
parser that captures OpenRouter's extra fields.

### Discovery response parsing

OpenRouter's `/api/v1/models` returns entries with this shape
(abbreviated):

```json
{
  "data": [
    {
      "id": "anthropic/claude-sonnet-4.6",
      "name": "Anthropic: Claude Sonnet 4.6",
      "context_length": 1000000,
      "pricing": {
        "prompt": "0.000003",
        "completion": "0.000015",
        "request": "0",
        "image": "0.0048"
      },
      "top_provider": {
        "context_length": 1000000,
        "max_completion_tokens": 128000,
        "is_moderated": true
      },
      "supported_parameters": [
        "tools", "reasoning", "temperature", "top_p", ...
      ],
      "architecture": {
        "modality": "text+image->text",
        "input_modalities": ["text", "image"],
        "output_modalities": ["text"]
      }
    }
  ]
}
```

Fields we parse:

| Response field | Destination |
|---|---|
| `id` | `Model.id` |
| `name` | `Model.name` |
| `context_length` or `top_provider.context_length` | `Model.context_window` |
| `top_provider.max_completion_tokens` | `Model.max_tokens` |
| `pricing.prompt` (per-token, string) × 1_000_000 | `Model.cost_per_million.input` |
| `pricing.completion` × 1_000_000 | `Model.cost_per_million.output` |
| `architecture.input_modalities` contains `"image"` | `Model.supports_images` |
| `supported_parameters` contains `"reasoning"` | `Model.supports_reasoning` |

The existing `OpenAiModelsResponse` parser captures most of these
fields already; this plan extends it with `pricing` +
`supported_parameters` + a richer `top_provider` sub-object.

### Upstream-aware capability mapping

The OpenRouter model ID's prefix (everything before the first
`/`) identifies the upstream. Our `ModelInfo → Model` conversion
for OpenRouter rewrites `replay_capabilities` and
`reasoning_capabilities` based on this prefix:

| Prefix | `requires_thinking_signature` | `supports_encrypted_reasoning` | `request_mode` |
|---|---|---|---|
| `anthropic/*` with reasoning | `true` | `false` | `NestedReasoning` |
| `openai/o1*`, `openai/o3*`, `openai/gpt-5*` | `false` | `false` (\*) | `NestedReasoning` |
| `google/*` with reasoning | `false` (\*\*) | `false` | `NestedReasoning` |
| `meta-llama/*`, `deepseek/*`, `qwen/*`, `mistralai/*` (no reasoning) | `false` | `false` | `None` |
| Non-reasoning (per `supported_parameters`) | `false` | `false` | `None` |

(\*) OpenRouter proxies OpenAI's Responses API's encrypted reasoning
back to the caller but we can't actually replay `encrypted_content`
through Chat Completions — so we mark it `false` for now and
accept a small continuity loss across multi-turn o3 conversations.
Plan 04 (direct OpenAI Responses API, deferred) addresses this
for non-OR direct users.

(\*\*) Gemini's `thoughtSignature` over OpenRouter is unverified
— the signature may or may not traverse the proxy. Mark `false`
for v1; revisit if users report broken multi-turn Gemini
reasoning.

A small `openrouter_capability_mapping` function in
`anie-providers-builtin` owns this logic, with a unit test per
row.

## Onboarding integration

We're not landing the full preset-catalog refactor (plan 00) in
this iteration — only one new provider is going in, and the
existing onboarding form can accommodate one more row without
the refactor.

Onboarding flow after this plan:

1. First-run shortlist becomes: `[Anthropic] [OpenAI] [OpenRouter]
   [Ollama] [Skip]`.
2. Picking OpenRouter:
   - Prompt for API key.
   - Save via `CredentialStore`.
   - **Immediately** call `ModelDiscoveryCache::get_or_discover`
     — a one-time fetch that takes ~1s and populates the local
     catalog for that provider.
   - Show a spinner during discovery.
   - On completion, return to TUI with OpenRouter as the active
     provider; `/model` opens the picker pre-populated with the
     full discovered list.
3. Network error during discovery: keep the key saved, skip the
   populate, surface a system message ("Models will be
   discovered on first `/model` usage"). `/model` picker
   re-attempts discovery.

## Model picker usability at scale

The existing `ModelPickerPane` has:

- Case-insensitive substring search over `id + name`.
- Scrollable list with keyboard navigation.
- Loading state + inline error.

For 500+ models this is sufficient in v1. User types `sonnet`
→ filters to ~5 rows. Types `opus` → 3 rows. Types `anthropic/`
→ all Anthropic-upstream models.

### Optional polish (stretch goal)

If substring search feels slow or noisy in practice, two small
improvements:

1. **Fuzzy scoring.** Replace substring with `fuzzy_filter`
   (already implemented in `anie-tui/src/widgets/select_list.rs`
   tests; lift into a shared `fuzzy` utility). Scores matches
   higher when characters appear contiguously or in word-start
   positions.
2. **Provider-prefix grouping.** When the filter is empty, show
   section headers (`── anthropic ──`, `── openai ──`, etc.).
   Implemented via list-item-kind extension in
   `ModelPickerPane`.

Both are out of scope for this plan's merge; ship a follow-up
if v1 usability shows the need.

## Test plan

| # | Test |
|---|---|
| 1 | `openrouter_preset_registered` — the onboarding shortlist includes OpenRouter. |
| 2 | `openrouter_request_uses_nested_reasoning_object` — outbound body for a reasoning model has `reasoning.effort` and not `reasoning_effort`. |
| 3 | `openrouter_request_body_without_reasoning_is_unchanged` — non-reasoning models emit plain Chat Completions shape. |
| 4 | `openrouter_discovery_parses_models_endpoint_full_response` — fixture with the rich OpenRouter response; asserts pricing, context, modalities parse correctly. |
| 5 | `openrouter_capability_mapping_sets_anthropic_replay_signature` — `anthropic/claude-sonnet-4.6` from discovery gets `requires_thinking_signature: true`. |
| 6 | `openrouter_capability_mapping_reasoning_model_gets_nested_request_mode` — `openai/o3` gets `request_mode = NestedReasoning`. |
| 7 | `openrouter_capability_mapping_non_reasoning_model_has_no_reasoning_cap` — `meta-llama/llama-3.3-70b-instruct` has `reasoning_capabilities = None`. |
| 8 | `openrouter_routing_preferences_propagate_to_body` — with a `compat` carrying `openrouter_routing`, the outbound body includes the `provider` object. |
| 9 | `openrouter_onboarding_populates_catalog_on_configure` — integration-level TUI test: add OpenRouter → assert catalog has >10 entries after the flow. |
| 10 | `openrouter_discovery_failure_does_not_lose_credential` — discovery fetch fails, the key is still saved and the flow exits cleanly with a system message. |
| 11 | Invariant suite: OpenRouter appears in every cross-provider invariant fixture in `provider_replay.rs`. |

## Exit criteria

- [ ] OpenRouter appears in the onboarding first-run shortlist.
- [ ] Configuring OpenRouter via onboarding triggers a live
      catalog discovery and returns with models immediately
      available in `/model`.
- [ ] A user with an OpenRouter key can run a two-turn
      conversation against any upstream (Anthropic, OpenAI,
      Google, Meta, DeepSeek, Mistral, Qwen) without editing
      `config.toml`.
- [ ] Reasoning models use the nested `reasoning.effort` body
      shape.
- [ ] Anthropic-upstream reasoning models survive turn-2 replay
      (thinking signatures captured and echoed).
- [ ] Invariant suite covers OpenRouter.
- [ ] Manual smoke: two-turn conversation on
      `anthropic/claude-sonnet-4.6` with thinking `high`
      documented in the merge PR.

## Out of scope

- **Curated model catalog.** No hand-authored OpenRouter entries
  in `builtin_models()` — everything is discovery-driven.
- **Preset catalog refactor** (plan `00_provider_selection_ux.md`).
  Deferred to a future round when we're adding multiple
  providers at once.
- **Fuzzy search / provider-prefix grouping** in the model
  picker. Ship with the current substring search; iterate if
  users report usability issues.
- **Leaderboard headers** (`HTTP-Referer`, `X-Title`). Cosmetic.
- **UI surface for `openrouter_routing`**. Config-only in v1.
- **OpenRouter OAuth flow**. API key is the common path.
- **Live pricing refresh.** Pricing is captured once at
  discovery and updated whenever the user triggers refresh.
- **Encrypted-reasoning replay for o3 via OpenRouter.** Marked
  false; addressed in plan 04 for users going direct to OpenAI.
- **Every other provider plan in this folder.** xAI, Groq,
  Cerebras, Mistral, Gemini, Azure, Bedrock, OpenAI Responses —
  all deferred. The specs stay in place for when they're
  prioritized.

## Dependencies

- Milestone 0 (foundation) from
  [`execution/00_foundation.md`](execution/00_foundation.md):
  only the two PRs that matter for OpenRouter — the `Model.compat`
  blob and `ThinkingRequestMode::NestedReasoning`. The third
  foundation PR (`thought_signature` for Gemini) is not needed
  and is deferred.
