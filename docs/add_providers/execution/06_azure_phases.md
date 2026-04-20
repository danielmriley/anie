# Milestone 6 — Azure OpenAI

Two PRs. First adds the shared infrastructure (header selection,
URL shape for deployment routing). Second adds the preset and
the onboarding multi-field form.

Spec reference: [`../05_azure_openai.md`](../05_azure_openai.md).

## Dependencies

- Milestone 0 (Foundation).
- Milestone 1 (UX prerequisite).
- Plan 01 and the batch are **not** dependencies. Azure can ship
  independently.

**Azure + Responses API follow-up** depends on Milestone 5
(OpenAI Responses) landing. Not part of this milestone.

---

## PR A — `UrlShape` + `auth_header_name` on provider module

**Goal:** The shared OpenAI Chat Completions provider gains two
knobs — a URL-shape selector (standard vs Azure deployment) and
an auth-header override — so Azure deployments route correctly
and auth with the `api-key` header instead of `Authorization:
Bearer`.

### Files
- `crates/anie-providers-builtin/src/openai/mod.rs`
- `crates/anie-providers-builtin/src/openai/convert.rs` (if URL
  construction lives there)

### Design

Add to the model's compat blob (from Milestone 0) or to a
per-preset auth configuration:

```rust
pub enum UrlShape {
    Standard,
    AzureDeployment { api_version: String },
}

pub enum AuthHeader {
    Bearer,                    // default: Authorization: Bearer <key>
    Named { name: String },    // e.g. "api-key" for Azure
}
```

Wire both into the request-building path. No Azure catalog
entries yet — that's PR B.

### Test plan

| # | Test |
|---|---|
| 1 | `url_shape_standard_builds_base_url_plus_chat_completions` — regression: existing OpenAI behavior unchanged. |
| 2 | `url_shape_azure_deployment_inserts_deployment_and_api_version` — URL formed as `{base}/openai/deployments/{deployment}/chat/completions?api-version={v}`. |
| 3 | `auth_header_bearer_produces_authorization_bearer` — regression. |
| 4 | `auth_header_named_api_key_produces_api_key_header_not_bearer` |

### Exit criteria

- [ ] `UrlShape` and `AuthHeader` present on the provider's
      internal configuration.
- [ ] Tests 1–4 pass.
- [ ] No provider today uses the Azure variant yet — existing
      behavior unchanged.

---

## PR B — Azure preset + onboarding multi-field form

**Goal:** Users can configure an Azure deployment via a
multi-field onboarding flow and use it end-to-end.

### Files
- `crates/anie-providers-builtin/src/provider_presets.rs`
- `crates/anie-tui/src/overlays/onboarding.rs` (multi-field
  form support)
- `crates/anie-config/src/mutation.rs` (extra keys in the
  `[providers.azure-openai]` section)

### Steps

1. Add the `azure-openai` preset with
   `category: ProviderCategory::Cloud` and a template
   `base_url`.
2. Extend the onboarding form runner to prompt a **sequence** of
   fields when the picked preset is Azure: resource name,
   API version, deployment name, API key. The existing form
   builder already handles single fields; extend it to a list.
3. Persist per-deployment metadata (deployment name,
   api_version) in `config.toml` under the provider block.
4. Each user-added deployment becomes a `Model` catalog entry
   whose `id` is the deployment name.

### Test plan

| # | Test |
|---|---|
| 5 | `azure_preset_produces_template_base_url` |
| 6 | `azure_onboarding_collects_four_fields` — TUI-level test with keystrokes. |
| 7 | `config_persists_azure_deployment_metadata` — round-trip. |
| 8 | `azure_request_body_matches_openai_chat_completions_byte_for_byte` — contrast with the URL difference asserted in PR A test 2. |
| 9 | Invariant suite covers Azure. |

### Exit criteria

- [ ] Azure preset registered under `Cloud` category.
- [ ] Multi-field onboarding form collects all four fields.
- [ ] Config persists correctly.
- [ ] Manual smoke against a real Azure GPT-4o deployment
      logged in the PR description (if a test key is
      available).

---

## Milestone exit criteria

- [ ] Both PRs merged.
- [ ] Azure deployment user journey works end-to-end: add via
      onboarding → appears in `/model` picker → two-turn
      conversation completes.
- [ ] No Azure Responses support yet — flagged in the spec's
      out-of-scope with a note that it's a follow-up after
      Milestone 5 (OpenAI Responses) lands.

## Azure + Responses follow-up (post-milestone)

Once Milestone 5 (OpenAI Responses) has landed, a follow-up PR
adds `ApiKind::OpenAIResponses` support for Azure deployments.
Steps:

- Azure preset gains a note that per-deployment `api_kind`
  override is supported.
- `/providers` add flow asks whether a deployment uses Chat
  Completions or Responses (one extra form field).
- Each deployment's catalog entry carries its own `api_kind`.

Targeted at ~1 PR. Not part of Milestone 6 because Milestone 5
may not land for a while and Azure users shouldn't be blocked on
the Responses rollout.
