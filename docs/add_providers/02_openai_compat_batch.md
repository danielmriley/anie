# Plan 02 — OpenAI-compatible batch (xAI, Groq, Cerebras, Mistral)

Bundled because these four providers share the same
implementation shape: OpenAI Chat Completions wire protocol,
bearer-token auth, differ from OpenAI only in base URL, model
catalog, and minor response-shape quirks. Cheaper to land them
together than one at a time.

## User value

| Provider | Niche |
|---|---|
| **xAI** | Grok 2/3/4 access. Similar pricing to OpenAI, distinct model family. |
| **Groq** | Fast inference (100s of tokens/sec) on Llama, DeepSeek, Qwen. Low latency wins. |
| **Cerebras** | Even faster inference (1000s t/s) on Llama-3 / Qwen. Research and prototype workflows. |
| **Mistral** | Mistral-native models (Large, Codestral, Ministral). EU-hosted, open-weight lineage. |

All four are "I want one more provider besides OpenAI" additions
— users already comfortable with the OpenAI wire format who want
their own key somewhere else.

## Wire protocol

**All four reuse `ApiKind::OpenAICompletions`.** Zero new provider
modules. Each adds a preset entry + a set of catalog entries.

| Provider | Base URL |
|---|---|
| xAI | `https://api.x.ai/v1` |
| Groq | `https://api.groq.com/openai/v1` |
| Cerebras | `https://api.cerebras.ai/v1` |
| Mistral | `https://api.mistral.ai/v1` |

## Auth shape

All four: `Authorization: Bearer {key}`. Standard OpenAI-compat.
No new code in `anie-auth` — each preset registers with
`AuthHint::ApiKey { env_var }`:

| Provider | env_var |
|---|---|
| xAI | `XAI_API_KEY` |
| Groq | `GROQ_API_KEY` |
| Cerebras | `CEREBRAS_API_KEY` |
| Mistral | `MISTRAL_API_KEY` |

API key URLs (for onboarding shortcut):

- xAI: <https://console.x.ai/>
- Groq: <https://console.groq.com/keys>
- Cerebras: <https://cloud.cerebras.ai/>
- Mistral: <https://console.mistral.ai/api-keys/>

## Model catalog entries

Conservative starter sets. Discovery via each provider's
`/models` endpoint (all four support it) fills in the rest
on-demand.

### xAI

| Model ID | Display | Context | Max out | Reasoning |
|---|---|---|---|---|
| `grok-4` | Grok 4 | 256k | 64k | native (reasoning effort parameter) |
| `grok-3` | Grok 3 | 128k | 8k | native |
| `grok-2-vision-1212` | Grok 2 Vision | 32k | 4k | none |

### Groq

| Model ID | Display | Context | Max out | Reasoning |
|---|---|---|---|---|
| `llama-3.3-70b-versatile` | Llama 3.3 70B (Groq) | 128k | 8k | none |
| `deepseek-r1-distill-llama-70b` | DeepSeek R1 Distill 70B | 128k | 8k | tagged (`<think>…</think>`) |
| `qwen-2.5-coder-32b` | Qwen 2.5 Coder 32B | 32k | 8k | none |
| `mixtral-8x7b-32768` | Mixtral 8x7B | 32k | 8k | none |

### Cerebras

| Model ID | Display | Context | Max out | Reasoning |
|---|---|---|---|---|
| `llama-3.3-70b` | Llama 3.3 70B (Cerebras) | 128k | 8k | none |
| `llama3.1-8b` | Llama 3.1 8B (Cerebras) | 128k | 8k | none |
| `qwen-3-32b` | Qwen 3 32B (Cerebras) | 32k | 8k | tagged |

### Mistral

| Model ID | Display | Context | Max out | Reasoning |
|---|---|---|---|---|
| `mistral-large-latest` | Mistral Large | 128k | 8k | none |
| `codestral-latest` | Codestral | 32k | 8k | none |
| `ministral-8b-latest` | Ministral 8B | 128k | 8k | none |

All entries: `cost_per_million` initialized from the provider's
current public pricing at time of writing; document the date and
revisit quarterly.

## Per-provider quirks

Each of the four has one or two things to watch. All are handled
without a new provider module.

### xAI

- **Reasoning effort** for Grok 4: sent as `reasoning_effort: "high"`
  in the request body, mirroring OpenAI's o-series. The existing
  `reasoning_strategy.rs` translation layer already handles this
  when `ReasoningControlMode::Native` is set. Populate Grok 4's
  catalog entry with `ReasoningCapabilities { control: Native,
  output: Separated, request_mode: Some(ReasoningEffort) }`.
- **Rate-limit headers** use `retry-after-ms` (milliseconds, not
  seconds). Document in the xAI-specific part of the catalog
  comment; the retry-policy code already reads
  `error.retry_after_ms()` regardless of unit.

### Groq

- **Tagged reasoning**: DeepSeek-R1 Distill emits
  `<think>…</think>` inline in the content stream. The existing
  `tagged_reasoning.rs` splitter already handles this — populate
  the catalog entry with `ReasoningCapabilities { control:
  PromptWithTags, output: Tagged, ... }`.
- **Groq response chunks** sometimes include an `x_groq` field in
  the delta with Groq-specific metadata (queue time, etc.). It's
  ignored by our parser — confirm with a fixture test that no
  `_ => {}` warning fires in the streaming state machine.

### Cerebras

- **Context windows** shorter than declared in docs for some
  models; confirm via each entry.
- **No tool-calling support** on most Cerebras models today.
  Leave `supports_tool_use: false` if that field exists (it
  doesn't today; add to catalog as needed in a follow-up, not
  this plan).

### Mistral

- **Function calling** uses OpenAI-compat `tool_calls` format —
  works with existing parser.
- **Safe prompt flag** (`safe_prompt: bool`) is a Mistral-
  specific request-body field. Not plumbed. Ignore for v1; add
  as an `extra_params` on the preset if a user requests it.

## Onboarding integration

All four go into the `OpenAICompatible` category (per plan 00's
preset enum). None of them should appear in the first-run
shortlist — that's reserved for providers with broad default
appeal (Anthropic, OpenAI, OpenRouter, Ollama). Users hitting
"More providers…" find the full four in the category picker.

## Implementation ordering within the batch

No ordering constraint — they're independent. Suggested merge
order for ease of review:

1. Mistral (smallest quirks; proves the template)
2. xAI (grok-4 reasoning is the interesting case)
3. Groq (tagged reasoning via existing splitter)
4. Cerebras (fewest models, copies Groq's shape)

Each can ship as a separate commit. The test suite from the
first one becomes the template for the rest.

## Test plan

Per provider:

| # | Test |
|---|---|
| 1 | `<provider>_preset_registered` — catalog entry present with correct category, env_var, base_url. |
| 2 | `<provider>_catalog_contains_expected_models` — hard-coded starter catalog matches the table above. |
| 3 | `<provider>_request_targets_provider_base_url` — the existing `build_request_body_for_test` helper produces a `ResolvedRequestOptions` with the right `base_url_override`. |
| 4 | Any provider-specific quirk test (tagged reasoning for Groq, reasoning-effort for xAI). |

Plus: plug each provider into the cross-provider invariant suite
(`crates/anie-integration-tests/tests/provider_replay.rs`). The
per-provider tests in the invariant file get `<provider>_model()`
and `build_<provider>_body()` helpers added.

Plus: one manual two-turn smoke each against a real API key,
following the `01e_rollout_status.md` template.

## Exit criteria

For the batch as a whole:

- [ ] Four `ProviderPreset` entries registered.
- [ ] At least three model catalog entries per provider.
- [ ] All four appear in `/providers` category picker under
      `OpenAICompatible`.
- [ ] Per-provider quirk tests pass.
- [ ] Invariant suite exercises each provider with one entry.
- [ ] Manual smoke documented for each.

## Out of scope

- Mistral's "JSON mode" flag, xAI's "live search" tool — both
  provider-specific features that don't belong in the generic
  OpenAI-compat provider.
- Groq's Whisper audio endpoints.
- xAI's image-generation models (different endpoint, not
  `/chat/completions`).

## Dependencies

- Plan 00 (provider selection UX) — prerequisite.
- Plan 01 (OpenRouter) — shares the `extra_headers` pattern if
  plan 01 chose to extend `ResolvedRequestOptions`. If plan 01
  went with catalog-entry-level headers instead, this plan uses
  the same mechanism. No changes either way — just follow
  whichever shape plan 01 settled on.
