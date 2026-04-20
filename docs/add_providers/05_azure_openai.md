# Plan 05 — Azure OpenAI

Enterprise users with OpenAI access via Azure's deployment. Same
wire protocol as OpenAI, different URL shape (deployment routing)
and different auth header (`api-key` not `Authorization: Bearer`).

## User value

- **Enterprise compliance.** Organizations with Azure
  subscriptions and data-residency requirements use Azure
  OpenAI as their only supported path to GPT-4 / o-series.
- **Existing auth infrastructure.** Azure users already have
  resource-scoped keys or Entra ID; a direct OpenAI key isn't an
  option for them.

## Wire protocol

**Reuses `ApiKind::OpenAICompletions`** (and
`ApiKind::OpenAIResponses` once plan 04 lands — see Dependencies).

Request body is byte-identical to OpenAI's. Three URL shape
differences:

1. Base URL is user-specific:
   `https://{resource-name}.openai.azure.com/openai/deployments/{deployment-name}`
2. The path appends `/chat/completions?api-version=2024-10-21`
   (or whichever `api-version` the deployment supports).
3. The deployment name replaces the model ID in the URL; the
   request body's `"model"` field is **ignored** by Azure.

## Auth shape

Two mechanisms Azure supports. This plan ships the first; the
second is a follow-up.

### API key (this plan)

- Header: `api-key: {key}` (not `Authorization: Bearer`).
- Env var: `AZURE_OPENAI_API_KEY`.
- Keys come from the Azure portal → your OpenAI resource →
  Keys and Endpoint.

### Entra ID / Managed Identity (deferred)

- Header: `Authorization: Bearer {entra_token}`.
- Requires Azure SDK dependency or a token-fetcher subprocess.
- Out of scope for this plan. Follow-up after the extension
  system (plan 10) lands, where OAuth-like flows are first-class.

## Configuration shape

Azure users need more config surface than "provider name + base
URL + api key". Per-deployment:

| Field | Example | Source |
|---|---|---|
| Resource name | `my-org-ai-east` | URL |
| Deployment name | `gpt4o-production` | URL |
| API version | `2024-10-21` | URL query param |

Model ID in anie's catalog maps to **deployment name**, not the
underlying model name. Two deployments of `gpt-4o` at different
api-versions are two distinct catalog entries.

Config layout in `config.toml` grows an `[providers.azure-openai]`
section mirroring `[providers.openai]` with extra fields:

```toml
[providers.azure-openai]
base_url = "https://my-org-ai-east.openai.azure.com"
api_key_env = "AZURE_OPENAI_API_KEY"
api_version = "2024-10-21"

[[providers.azure-openai.models]]
id = "gpt4o-production"   # deployment name
name = "GPT-4o (Azure, prod)"
context_window = 128000
max_tokens = 16384
```

Three bits of new machinery:

1. **`api-key` header selection.** The provider honors an
   `auth_header_name: Option<&'static str>` on the preset; when
   set, it replaces `Authorization: Bearer {key}` with
   `{header_name}: {key}`.
2. **Deployment-name routing.** When sending, the URL is
   `{base_url}/openai/deployments/{model_id}/chat/completions?api-version={v}`
   instead of `{base_url}/chat/completions`. The provider gains
   an enum `UrlShape { Standard, AzureDeployment { api_version: String } }`.
3. **Model ID != model name.** The catalog entry's `id` field
   is repurposed as the deployment name; the request body's
   `"model"` is either omitted or set to the deployment name
   (Azure tolerates both).

All three extensions live on the OpenAI provider with
feature-flag-like `UrlShape` + `AuthHint` variants — no new
provider module.

## `ProviderPreset` shape

Azure doesn't fit a single static preset — each deployment is
user-specific. The preset is a **template**:

```rust
ProviderPreset {
    name: "azure-openai",
    display_name: "Azure OpenAI",
    api_kind: ApiKind::OpenAICompletions,
    base_url: "https://<resource>.openai.azure.com",  // template
    auth_hint: AuthHint::ApiKey { env_var: Some("AZURE_OPENAI_API_KEY") },
    category: ProviderCategory::Cloud,
    tagline: "Enterprise OpenAI via Azure deployment",
    ...
}
```

The onboarding flow, when this preset is picked, branches into a
multi-field form: Resource name, API version, Deployment name,
API key. Existing `onboarding.rs` form builder already handles
multi-field prompts — extend its prompt list per preset.

## Model catalog entries

Azure deployments are customer-specific; we ship zero default
catalog entries. Users add their deployments during onboarding
or via `/providers` → Add → Azure OpenAI.

For testing purposes, a conventional "gpt-4o-example" entry with
dummy URL lands in `crates/anie-providers-builtin/src/tests/`
fixtures only.

## Response handling

Azure's responses are identical to OpenAI Chat Completions. No
streaming parser changes. One quirk:

- **429 retry-after uses seconds-with-fractional-seconds**
  (`"retry-after": "3.5"`) in some regions. The existing
  retry-policy parser handles integers and ms; add a decimal-
  seconds branch if fixtures show it.

## Test plan

| # | Test |
|---|---|
| 1 | `azure_preset_produces_template_base_url` |
| 2 | `azure_url_inserts_deployment_name_and_api_version` — given a mock `UrlShape::AzureDeployment` and a deployment name, assert the full URL is built correctly. |
| 3 | `azure_uses_api_key_header_not_bearer` — assert the outgoing headers. |
| 4 | `azure_request_body_matches_openai_chat_completions_byte_for_byte` — same body as OpenAI for identical input. |
| 5 | `azure_onboarding_collects_resource_deployment_api_version` — TUI-level test with the form. |
| 6 | `config_persists_azure_deployment_metadata` — config round-trip. |
| 7 | Manual smoke against a real Azure deployment. |

## Exit criteria

- [ ] Azure OpenAI preset registered under
      `ProviderCategory::Cloud`.
- [ ] Onboarding collects resource / deployment / api-version
      in a multi-field form.
- [ ] Outgoing URL uses deployment-name routing + api-version.
- [ ] Outgoing auth uses `api-key` header.
- [ ] Integration test covers full URL + header shape.

## Out of scope

- Entra ID / Managed Identity auth (follow-up).
- Azure AI Inference (the newer unified endpoint) — separate
  plan if/when it stabilizes.
- DALL-E / embeddings endpoints on Azure (out of anie's scope
  regardless of provider).

## Dependencies

- Plan 00 (provider selection UX).
- Optional: Plan 04 (Responses API). If plan 04 lands before
  this plan, also wire the `AzureOpenAI + OpenAIResponses`
  combination. Otherwise, ship Chat Completions only and add
  Responses support in a follow-up after plan 04 lands.
