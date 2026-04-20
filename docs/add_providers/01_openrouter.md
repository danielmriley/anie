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

Two optional request-body/header additions OpenRouter uses for
leaderboards and billing metadata. Neither is required for the
API to work, but both are idiomatic and we should set them:

| Header | Purpose | Value |
|---|---|---|
| `HTTP-Referer` | Identifies anie on OpenRouter's leaderboard. | `https://github.com/danielmriley/anie` (configurable in `config.toml`) |
| `X-Title` | Human-readable app name on the leaderboard. | `anie` (same, configurable) |

**Implementation:** the existing `OpenAIProvider` takes
`extra_headers` via `ResolvedRequestOptions`. Either extend that
path (clean) or accept a `model.extra_headers` field on a
per-catalog-entry basis (also clean). The preset for OpenRouter
sets these defaults; users can override in config.

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
| 2 | `openrouter_request_includes_http_referer_and_x_title` — build a request body via `OpenAIProvider::build_request_body_for_test` with the OpenRouter preset applied, assert the two headers appear in the outbound options. |
| 3 | `openrouter_model_ids_preserve_provider_prefix` — the `anthropic/claude-sonnet-4.6` form roundtrips without the slash getting eaten by any path parsing. |
| 4 | `openrouter_model_discovery_parses_models_endpoint` — fixture response from `/api/v1/models`, assert the cache populates at least one model. |
| 5 | Manual smoke: configure key, send a prompt to `anthropic/claude-sonnet-4.6`, confirm a second turn replay works (tests the thinking-signature round-trip). |

## Exit criteria

- [ ] OpenRouter appears in onboarding's shortlist and
      `/providers` add picker.
- [ ] User can configure an API key via the onboarding flow and
      successfully run a two-turn conversation against
      `anthropic/claude-sonnet-4.6`.
- [ ] `HTTP-Referer` and `X-Title` headers are set on outbound
      requests with overridable defaults.
- [ ] The initial eight-model catalog entries are present with
      correct context windows.
- [ ] Invariant test suite (plan 06 integration tests) exercises
      the OpenRouter preset at least once.

## Out of scope

- Live-pricing fetch from `/api/v1/models` (nice-to-have).
- OpenRouter's cost-routing headers (e.g. `X-Or-Ignore-Providers`,
  `X-Or-Order`). Users can set them manually via config if they
  need fine-grained upstream control.
- OpenRouter OAuth flow (they do offer one; API key is the
  common path).

## Dependencies

- Plan 00 (provider selection UX) — provides the preset catalog
  this plan's entry lands into.
- None from earlier provider plans (this is plan 01).
