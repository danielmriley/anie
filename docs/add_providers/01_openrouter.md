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

## OpenRouter-specific behavior (verified against pi)

Auditing pi's `packages/ai/src/providers/openai-completions.ts`
end-to-end surfaced more OpenRouter-specific handling than the
earlier draft of this plan captured. Each item below is tied to
the file + line where pi does the thing, so future drift can be
re-audited.

### Request side

#### Nested `reasoning` object (required for reasoning models)

pi ref: `openai-completions.ts:429–438` —
`compat.thinkingFormat === "openrouter"` emits
`reasoning: { effort: "high" }` (or `"none"` for level Off)
instead of OpenAI's flat `reasoning_effort`. Without this,
reasoning silently no-ops.

Requires `ThinkingRequestMode::NestedReasoning` (foundation PR
B). Each discovered OpenRouter model with `supported_parameters`
including `"reasoning"` gets `request_mode = NestedReasoning`
during `ModelInfo → Model` conversion.

#### Provider routing preferences (optional)

pi ref: `openai-completions.ts:445–447` — if
`baseUrl.includes("openrouter.ai") && model.compat?.openRouterRouting`
is set, emits a top-level `provider` object carrying the
preferences.

Config-only in v1 via the `ModelCompat::OpenAICompletions`
compat blob (foundation PR A). UI surface deferred.

#### Anthropic `cache_control` for `anthropic/*` models

pi ref: `openai-completions.ts:470–501`
(`maybeAddOpenRouterAnthropicCacheControl`) — when the model
is `anthropic/*` routed through OR, pi walks the messages and
adds `cache_control: { type: "ephemeral" }` on the last text
part of the last user/assistant message. OpenRouter does NOT
insert this for you; Anthropic's prompt caching only kicks in
when the client marks cache breakpoints.

**Cost impact.** Without this, users on Claude-via-OR pay the
full input-token rate on every turn. With it, long
conversations benefit from Anthropic's ~90% cache-hit discount.
Non-trivial: for a 50-turn coding session on Claude Opus,
roughly a 5× bill difference at steady state.

Implementation: a small helper runs during
`OpenAIProvider::convert_messages` when the model's
`base_url` contains `openrouter.ai` and `model.id.starts_with("anthropic/")`.
Walk messages back-to-front, find the last text part, attach
`cache_control: { type: "ephemeral" }`. No-op for other
upstreams.

#### Leaderboard headers — not shipped

`HTTP-Referer` / `X-Title` — pi doesn't set them. Cosmetic
only. Skip.

### Response side

#### Reasoning field names (three possible)

pi ref: `openai-completions.ts:186–226` — checks for reasoning
in three possible delta fields and takes the first non-empty:

1. `reasoning_content` (e.g., llama.cpp servers routed through
   OR)
2. `reasoning` (most standard)
3. `reasoning_text` (some OR upstreams)

We already handle `reasoning` and `reasoning_content`
(`crates/anie-providers-builtin/src/openai/reasoning_strategy.rs:214`).
**Missing: `reasoning_text`.** Small add — one string in the
reasoning-field lookup list.

#### `reasoning_details` — encrypted reasoning passthrough

pi ref: `openai-completions.ts:267–279` + `600–641` — this is
the mechanism that lets OpenAI's o-series encrypted reasoning
survive multi-turn conversations through OpenRouter's Chat
Completions proxy. **Reverses my earlier answer** that said
we couldn't round-trip encrypted content through OR.

**On receive** (delta handling): each `delta.reasoning_details`
is an array of `{ type: "reasoning.encrypted", id, data }`
objects. Pi attaches the whole object (serialized as JSON) to
the matching ToolCall block's `thoughtSignature` field, keyed by
the `id` matching the tool call's id.

**On replay** (outgoing assistant message): pi collects the
`thoughtSignature` values off each ToolCall, parses them back
to objects, and sets
`assistant_message.reasoning_details = [...]` on the outgoing
message. OpenRouter then forwards to the OpenAI upstream which
uses the encrypted blobs to preserve reasoning context.

**Scope adjustment.** Our `ContentBlock::ToolCall` doesn't have
a `thought_signature` (or equivalent) field today. This plan
adds one as a scoped extension. The field is
`Option<serde_json::Value>` — opaque, pass-through only. Add a
`reasoning_details: Option<Vec<serde_json::Value>>` field to
`AssistantMessage` or piggyback on ToolCall's state the way pi
does. Decide at implementation time based on cleanest diff.

Gated to OpenRouter (and later, direct OpenAI Responses) via
the same discovery-driven capability routing that already exists
for Anthropic signatures — catalog entries for
`openai/o1*`, `openai/o3*`, `openai/gpt-5*` routed through OR
flip a new `supports_openrouter_reasoning_details` capability or
equivalent.

#### Ordinary tool_calls, text, finish_reason

All standard OpenAI Chat Completions shape. No divergence.

### Model-discovery specifics

pi ref: `scripts/generate-models.ts:60–108`.

OpenRouter's `/api/v1/models` returns:

| Field | Our use |
|---|---|
| `id` | `Model.id` |
| `name` | `Model.name` |
| `context_length` | `Model.context_window` |
| `top_provider.max_completion_tokens` | `Model.max_tokens` |
| `pricing.prompt` × 1_000_000 | `cost_per_million.input` |
| `pricing.completion` × 1_000_000 | `cost_per_million.output` |
| `pricing.input_cache_read` × 1_000_000 | `cost_per_million.cache_read` |
| `pricing.input_cache_write` × 1_000_000 | `cost_per_million.cache_write` |
| `architecture.modality` contains `image` | `supports_images: true` |
| `supported_parameters` contains `reasoning` | `supports_reasoning: true` |
| `supported_parameters` contains `tools` | used as a filter |

**Filter to tool-supporting models.** Pi filters out models that
don't advertise `"tools"` in `supported_parameters` — anie is a
coding agent built on tool use, so non-tool models are mostly
useless. We adopt the same filter. Users who want a text-only
model can still configure it by hand.

This makes the catalog size manageable too — filter to
tool-supporting drops the count from ~500 to ~150, still
comprehensive but less overwhelming in the picker.

### Summary: what OpenRouter normalizes vs passes through

**OpenRouter normalizes (so clients write once):**
- reasoning _requests_ via nested `reasoning: { effort }`
- provider routing via top-level `provider` object
- encrypted reasoning passthrough via `reasoning_details`

**OpenRouter does NOT normalize (client must handle):**
- reasoning _response_ field naming — three variants exist
- Anthropic prompt-caching markers — upstream-specific, client
  inserts
- upstream-specific features (Gemini `thoughtSignature`,
  Anthropic `thinking.signature`) — partial passthrough,
  verify per upstream

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

| Prefix | `requires_thinking_signature` | `supports_reasoning_details_replay` | `request_mode` |
|---|---|---|---|
| `anthropic/*` with reasoning | `true` | `false` | `NestedReasoning` |
| `openai/o1*`, `openai/o3*`, `openai/gpt-5*` | `false` | `true` (\*) | `NestedReasoning` |
| `google/*` with reasoning | `false` (\*\*) | `false` | `NestedReasoning` |
| `meta-llama/*`, `deepseek/*`, `qwen/*`, `mistralai/*` (no reasoning) | `false` | `false` | `None` |
| Non-reasoning (per `supported_parameters`) | `false` | `false` | `None` |

(\*) Revised: OpenRouter **does** round-trip encrypted reasoning
state via the `reasoning_details` field on assistant messages
(pi ref: `openai-completions.ts:267–279` and `629–641`).
Multi-turn o-series reasoning works through OR. The capability
is tracked as `supports_reasoning_details_replay: true` on the
model's `ReplayCapabilities` (new flag; piggybacks on the
existing replay-capability plumbing). See "Response side →
`reasoning_details`" above for the mechanism.

(\*\*) Gemini's `thoughtSignature` over OpenRouter is unverified
— the signature may or may not traverse the proxy. Mark `false`
for v1; revisit with a smoke test once a `google/gemini-2.5-pro`
model is reached through OR.

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
