# Plan 01 — OpenRouter

User's top-line provider request. OpenRouter is an
OpenAI-compatible aggregator: one API key gives access to ~200
models across Anthropic, OpenAI, Google, Meta, xAI, and dozens
more. Perfect first "new" provider because the wire protocol is
already supported — we just add a preset and a model catalog.

## User value

- **One key, many models.** Users who don't want to manage
  separate accounts at Anthropic + OpenAI + Google can route
  everything through a single OpenRouter account.
- **Pay-as-you-go access to expensive models.** OpenRouter
  exposes Claude Opus, GPT-4, Gemini Pro, etc. without a monthly
  commitment or team plan.
- **Fallback routing.** OpenRouter routes to the cheapest healthy
  upstream when the user doesn't pin a provider — useful for
  redundancy during rate limits.

## Wire protocol

**Reuses `ApiKind::OpenAICompletions`.** No new provider module
needed. OpenRouter implements OpenAI Chat Completions with a few
additions documented below.

Endpoint: `https://openrouter.ai/api/v1/chat/completions`.

## Auth shape

**API key via `Authorization: Bearer {key}` header.** Standard
OpenAI-compat pattern. The auth-resolver chain
(`anie-auth::AuthResolver`) already handles this when the preset
is registered as `AuthHint::ApiKey { env_var: "OPENROUTER_API_KEY" }`.

API keys come from <https://openrouter.ai/keys>.

## OpenRouter-specific request quirks

### Reasoning field shape (required)

OpenRouter normalizes reasoning across upstreams by putting it in
a **nested** `reasoning: { effort: "high" }` object — not the flat
`reasoning_effort: "high"` field OpenAI's Chat Completions uses.
pi's openai-completions provider has explicit branching for this
via a `thinkingFormat: "openrouter"` compat flag
(`packages/ai/src/providers/openai-completions.ts:429`). Without
handling, sending `reasoning_effort: "high"` against OpenRouter
silently drops the reasoning request and the user gets no
reasoning from models that should support it.

**Implementation.** Our existing `ReasoningCapabilities` +
`ThinkingRequestMode` types cover this: add a
`ThinkingRequestMode::NestedReasoning` variant (if not already
present) and wire it through `reasoning_strategy.rs`. The
OpenRouter catalog entries declare
`reasoning_capabilities.request_mode =
Some(NestedReasoning)`.

### Provider routing preferences (optional but valuable)

OpenRouter accepts a top-level `provider` field in the request
body with a rich routing-preferences object — order of upstream
preferences, fallback behavior, price ceilings, Zero-Data-Retention
filters, quantization filters, sort-by-throughput, etc. pi models
the full shape (`OpenRouterRouting` in
`packages/ai/src/types.ts:307`) so users can pin upstream
providers or require cheapest-routing per request.

For **v1** we ship the routing support at the catalog level only:
a `ReplayCapabilities`-adjacent `openrouter_routing: Option<...>`
field on `Model` that carries the user's preferences through. The
plan 00 preset UI doesn't need to expose the routing knobs —
they're `config.toml`-editable. Follow-up plan can surface them
in `/providers`.

### Leaderboard headers (optional, cosmetic)

`HTTP-Referer` and `X-Title` identify anie on OpenRouter's public
leaderboard. Pi does **not** set these for itself (checked in its
codebase: zero hits). They're not required by the API and only
affect OpenRouter's own public stats. **Drop from v1 scope**; add
later as a `config.toml`-level opt-in if the user wants to show
up on the leaderboard.

## Model catalog entries

Start with a small curated set. The full 200+ model list is
overwhelming in a picker; users who want more can add them via
`/providers` → edit entry → add custom model, or via
`config.toml`. Discovery fetch is available at
`https://openrouter.ai/api/v1/models` and can fill in on-demand.

| Model ID | Display name | Context | Max out | Reasoning | Notes |
|---|---|---|---|---|---|
| `anthropic/claude-sonnet-4.6` | Claude Sonnet 4.6 (OpenRouter) | 1M | 128k | native | Anthropic upstream, signatures intact |
| `anthropic/claude-opus-4.6` | Claude Opus 4.6 (OpenRouter) | 1M | 128k | native | Same caveats |
| `openai/gpt-4o` | GPT-4o (OpenRouter) | 128k | 16k | none | |
| `openai/o4-mini` | o4-mini (OpenRouter) | 200k | 100k | native | |
| `google/gemini-2.0-flash` | Gemini 2.0 Flash (OpenRouter) | 1M | 8k | none | |
| `meta-llama/llama-3.3-70b-instruct` | Llama 3.3 70B (OpenRouter) | 128k | 8k | none | Good value option |
| `deepseek/deepseek-v3` | DeepSeek V3 (OpenRouter) | 128k | 8k | none | Cost-effective frontier |
| `qwen/qwen-2.5-coder-32b-instruct` | Qwen 2.5 Coder 32B (OpenRouter) | 32k | 8k | none | Coding-focused |

**Pricing**: OpenRouter's pricing varies per upstream and changes
frequently. Leave `cost_per_million` at `zero()` in the catalog
entry and let users refer to <https://openrouter.ai/models> for
current numbers. (A future plan can wire live-pricing fetch via
the `/api/v1/models` endpoint.)

**Replay capabilities**: When routing to Anthropic upstream,
OpenRouter *does* relay thinking-signatures on the wire —
verified in community docs, needs confirmation with a manual
smoke test. Mark Anthropic-routed models with
`requires_thinking_signature: true`; if the smoke test fails, fall
back to `None` and document the limitation.

## Onboarding integration

- **Preset name:** `openrouter`
- **Display name:** `OpenRouter`
- **Category:** `OpenAICompatible`
- **Tagline:** `One API key, many models (Anthropic, OpenAI, Google, Meta, …)`
- **Base URL:** `https://openrouter.ai/api/v1`
- **API key URL:** `https://openrouter.ai/keys`
- **env_var:** `OPENROUTER_API_KEY`

Include OpenRouter in the onboarding shortlist (per plan 00). It
belongs there precisely because it lowers the activation energy
for a new user who hasn't picked a single frontier provider yet.

## Model discovery

OpenRouter supports `/api/v1/models` returning a list of all
available models with their IDs, context windows, and per-token
pricing. The existing `ModelDiscoveryCache` machinery (from the
onboarding work) can handle this with the same `OpenAIModelsList`
shape already used for OpenAI — confirm field names match
(`id`, `context_length`). If OpenRouter uses different fields
(e.g., `pricing.prompt` vs `input`), add a small adapter
function; don't reshape the generic discovery path.

## Capability quirks

1. **Rate limiting.** OpenRouter returns `429` with a
   `retry-after` header; the existing retry-policy path honors
   `retry_after_ms`. No changes needed.
2. **Streaming chunk shape.** Standard OpenAI-compat
   `data: {...}\n\n`. No changes.
3. **Vision support** varies by upstream model. We already carry
   a `supports_images: bool` per model entry; populate per-row
   based on the upstream.
4. **Tool calling** works for upstreams that support it. No
   wire changes — OpenRouter transparently forwards OpenAI-style
   `tool_calls` in the delta.

## Test plan

| # | Test |
|---|---|
| 1 | `openrouter_preset_registered` — assert the catalog has the entry with the correct category. |
| 2 | `openrouter_request_uses_nested_reasoning_object` — build a request body for an OpenRouter reasoning model, assert `reasoning.effort` is set as a nested object and `reasoning_effort` is absent. |
| 3 | `openrouter_model_ids_preserve_provider_prefix` — the `anthropic/claude-sonnet-4.6` form roundtrips without the slash getting eaten by any path parsing. |
| 4 | `openrouter_model_discovery_parses_models_endpoint` — fixture response from `/api/v1/models`, assert the cache populates at least one model. |
| 5 | `openrouter_routing_preferences_propagate_when_configured` — per-model `openrouter_routing` with `{ order: ["anthropic"] }` surfaces in the outbound body's `provider` field. |
| 6 | Manual smoke: configure key, send a prompt to `anthropic/claude-sonnet-4.6`, confirm a second turn replay works (tests the thinking-signature round-trip). |

## Exit criteria

- [ ] OpenRouter appears in onboarding's shortlist and
      `/providers` add picker.
- [ ] User can configure an API key via the onboarding flow and
      successfully run a two-turn conversation against
      `anthropic/claude-sonnet-4.6`.
- [ ] `ThinkingRequestMode::NestedReasoning` exists and the
      OpenRouter reasoning catalog entries use it. The outbound
      request body uses `reasoning.effort` (nested), not
      `reasoning_effort`.
- [ ] The initial eight-model catalog entries are present with
      correct context windows.
- [ ] Invariant test suite (from completed api_integrity plan 06)
      exercises the OpenRouter preset at least once.

## Out of scope

- Live-pricing fetch from `/api/v1/models` (nice-to-have).
- `HTTP-Referer` / `X-Title` leaderboard headers (cosmetic;
  revisit post-v1 if a user asks for leaderboard attribution).
- UI surface for `openrouter_routing` preferences — v1 keeps them
  `config.toml`-editable only. `/providers` form extension is a
  follow-up.
- OpenRouter OAuth flow (they do offer one; API key is the
  common path).

## Dependencies

- Plan 00 (provider selection UX) — provides the preset catalog
  this plan's entry lands into.
- None from earlier provider plans (this is plan 01).
