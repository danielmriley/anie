//! Model catalog loading, resolution, and mutation.
//!
//! Consolidates the helpers that build the list of available
//! `Model`s (built-in + configured + discovered), resolve user
//! requests (via CLI, session history, runtime state, or slash
//! commands) to a concrete `Model`, and keep the catalog
//! de-duplicated / up-to-date at runtime.
//!
//! Currently a collection of free functions working on
//! `Vec<Model>`. A wrapper struct is the next evolution if/when
//! the field count on `ControllerState` grows enough to justify
//! the indirection.

use std::collections::HashSet;

use anyhow::{Result, anyhow};

use anie_auth::{AuthCredential, CredentialStore};
use anie_config::{AnieConfig, configured_models};
use anie_provider::{ApiKind, CostPerMillion, Model, ModelCompat, ThinkingLevel};
use anie_providers_builtin::{builtin_models, detect_local_servers};

use crate::Cli;
use crate::runtime_state::RuntimeState;
use anie_session::SessionContext;

/// The outcome of `resolve_initial_selection`: the model to use at
/// session open and the thinking level to pair with it.
pub(crate) struct InitialSelection {
    pub model: Model,
    pub thinking: ThinkingLevel,
}

/// Build the model catalog from all sources (builtin, configured,
/// detected local servers, stored OAuth credentials) and return
/// whether any local server was actually detected.
///
/// For OAuth-backed providers we emit a single placeholder
/// entry per provider so the provider appears in the TUI model
/// picker immediately. Real discovery fires on first picker
/// open (see `spawn_model_discovery`), replacing the placeholder
/// with the authoritative list.
pub(crate) async fn build_model_catalog(config: &AnieConfig) -> (Vec<Model>, bool) {
    let credential_store = CredentialStore::new();
    let local_servers = detect_local_servers(config.ollama.default_max_num_ctx).await;
    let local_models = local_servers
        .iter()
        .flat_map(|server| server.models.clone())
        .collect::<Vec<_>>();
    let oauth_models = oauth_placeholder_models(&credential_store);
    let mut model_catalog = builtin_models();
    model_catalog.extend(configured_models(config));
    model_catalog.extend(local_models);
    model_catalog.extend(oauth_models);
    dedupe_models(&mut model_catalog);
    (model_catalog, !local_servers.is_empty())
}

/// Walk the credential store and emit one placeholder model
/// per OAuth-backed provider so the TUI can render them in the
/// model picker without requiring a network round-trip at
/// startup. The placeholder ID is picked to be a reasonable
/// default for each provider (Copilot → claude-sonnet-4.6,
/// Gemini CLI → gemini-2.5-pro, etc.); live discovery replaces
/// them with the real list as soon as the user opens the picker
/// for that provider.
fn oauth_placeholder_models(store: &CredentialStore) -> Vec<Model> {
    let mut out = Vec::new();
    for provider_name in store.list_providers() {
        let Some(AuthCredential::OAuth { api_base_url, .. }) = store.get_credential(&provider_name)
        else {
            continue;
        };
        let Some(placeholder) = oauth_placeholder_model(&provider_name, api_base_url.as_deref())
        else {
            continue;
        };
        out.push(placeholder);
    }
    out
}

/// Pick a reasonable default model ID + base URL for each
/// OAuth-backed provider. These are not authoritative — live
/// discovery replaces them once the user opens the picker —
/// but they need to be plausible enough that a user who types
/// nothing still gets a working selection.
fn oauth_placeholder_model(provider_name: &str, api_base_url: Option<&str>) -> Option<Model> {
    let (id, fallback_base_url, api) = match provider_name {
        "github-copilot" => (
            "claude-sonnet-4.6",
            "https://api.individual.githubcopilot.com",
            ApiKind::OpenAICompletions,
        ),
        "openai-codex" => (
            "gpt-5",
            "https://api.openai.com/v1",
            ApiKind::OpenAICompletions,
        ),
        "google-gemini-cli" => (
            "gemini-2.5-pro",
            "https://cloudcode-pa.googleapis.com",
            ApiKind::OpenAICompletions,
        ),
        "google-antigravity" => (
            "gemini-3-pro-preview",
            "https://cloudcode-pa.googleapis.com",
            ApiKind::OpenAICompletions,
        ),
        // Anthropic OAuth uses the same wire protocol as the
        // API-key flow; falling back to its default base URL
        // is fine when the credential doesn't carry one.
        "anthropic" => (
            "claude-sonnet-4-6",
            "https://api.anthropic.com",
            ApiKind::AnthropicMessages,
        ),
        _ => return None,
    };
    let base_url = api_base_url
        .map(str::to_string)
        .unwrap_or_else(|| fallback_base_url.to_string());
    Some(Model {
        id: id.to_string(),
        name: id.to_string(),
        provider: provider_name.to_string(),
        api,
        base_url,
        context_window: 200_000,
        max_tokens: 16_384,
        supports_reasoning: true,
        reasoning_capabilities: None,
        supports_images: false,
        cost_per_million: CostPerMillion::zero(),
        replay_capabilities: None,
        compat: ModelCompat::None,
    })
}

/// Decide what model and thinking level to use at session open,
/// considering CLI flags, session history, runtime state, config
/// defaults, and the catalog.
pub(crate) fn resolve_initial_selection(
    cli: &Cli,
    config: &AnieConfig,
    runtime_state: &RuntimeState,
    session_context: &SessionContext,
    model_catalog: &[Model],
    local_models_available: bool,
) -> Result<InitialSelection> {
    let cli_model = cli.model.clone();
    let cli_provider = cli.provider.clone();
    let session_model = session_context.model.clone();
    let runtime_model = runtime_state.model.clone();
    let runtime_provider = runtime_state.provider.clone();

    // Priority chain:
    //   1. CLI flag (most specific intent)
    //   2. Session context (resuming → keep the session's model)
    //   3. Explicit `[model]` in config.toml (user's declared
    //      preference — must win over state.json so editing
    //      config.toml actually takes effect).
    //   4. Runtime state (last-used model persistence, filled
    //      in across fresh sessions when the user hasn't
    //      declared a default in config.toml).
    //   5. `ModelConfig::default()` (hardcoded final fallback).
    let config_provider = config
        .model_explicitly_set
        .then(|| config.model.provider.clone());
    let config_model = config.model_explicitly_set.then(|| config.model.id.clone());
    let config_thinking = config.model_explicitly_set.then_some(config.model.thinking);

    let preferred_provider = cli_provider
        .clone()
        .or_else(|| session_model.as_ref().map(|(provider, _)| provider.clone()))
        .or(config_provider)
        .or(runtime_provider)
        .unwrap_or_else(|| config.model.provider.clone());
    let preferred_model = cli_model
        .clone()
        .or_else(|| session_model.as_ref().map(|(_, model)| model.clone()))
        .or(config_model)
        .or(runtime_model)
        .unwrap_or_else(|| config.model.id.clone());
    let thinking = cli
        .thinking
        .or(session_context.thinking_level)
        .or(config_thinking)
        .or(runtime_state.thinking)
        .unwrap_or(config.model.thinking);

    // If the user gave an explicit `--provider`, don't let the
    // local-model fallback silently pick something different.
    // The previous behavior produced extremely surprising runs
    // (user types `--provider github-copilot` → ollama answers)
    // because the catalog doesn't include OAuth-discovered
    // providers at startup.
    let explicit_provider = cli_provider.is_some();

    let credential_store = CredentialStore::new();

    // When the user gave an explicit --provider on the CLI we
    // take a strict path: exact (provider, model_id) match in
    // the catalog, or fabricate via fallback_model_from_provider
    // (reads OAuth credential's api_base_url for OAuth-backed
    // providers), or error. `resolve_model`'s internal fallbacks
    // would otherwise silently return the first catalog entry
    // — the exact bug that made --provider github-copilot
    // --model claude-sonnet-4.6 land on anthropic.
    let model = if explicit_provider {
        exact_or_fabricate(
            &preferred_provider,
            cli_model.as_deref(),
            &preferred_model,
            model_catalog,
            config,
            &credential_store,
        )?
    } else if cli.provider.is_some() && cli.model.is_none() {
        resolve_model(
            Some(preferred_provider.as_str()),
            None,
            model_catalog,
            local_models_available,
        )
        .or_else(|_| {
            fallback_model_from_provider(
                preferred_provider.as_str(),
                preferred_model.as_str(),
                config,
                model_catalog,
                &credential_store,
            )
            .ok_or_else(|| {
                anyhow!(
                    "no model configured for provider '{preferred_provider}'; \
                     pass --model, or add the provider to config.toml"
                )
            })
        })?
    } else {
        resolve_model(
            Some(preferred_provider.as_str()),
            Some(preferred_model.as_str()),
            model_catalog,
            local_models_available,
        )
        .or_else(|_| {
            fallback_model_from_provider(
                preferred_provider.as_str(),
                preferred_model.as_str(),
                config,
                model_catalog,
                &credential_store,
            )
            .ok_or_else(|| {
                anyhow!(
                    "no model named '{preferred_model}' was found for \
                     provider '{preferred_provider}'"
                )
            })
        })
        .or_else(|_| {
            resolve_model(
                Some(preferred_provider.as_str()),
                None,
                model_catalog,
                local_models_available,
            )
        })
        .or_else(|_| {
            resolve_model(
                None,
                Some(&preferred_model),
                model_catalog,
                local_models_available,
            )
        })?
    };

    tracing::info!(
        target: "anie_cli::model_catalog",
        provider = %model.provider,
        id = %model.id,
        base_url = %model.base_url,
        "initial model selection"
    );
    Ok(InitialSelection { model, thinking })
}

/// Strict resolution when the user pinned `--provider` on the
/// CLI. Unlike `resolve_model`, this does NOT silently fall
/// back to "the first thing in the catalog" — the whole reason
/// `--provider` exists is to target a specific one. Order:
///
/// 1. Exact `(provider, model_id)` match in the catalog (only
///    when the user also specified `--model`).
/// 2. Fabricate via `fallback_model_from_provider` — reads
///    config.toml entries + OAuth credentials so a just-logged-
///    in provider works without manual config.
/// 3. Any catalog model under `provider` (when the user didn't
///    specify `--model`).
/// 4. Error with a clear message.
fn exact_or_fabricate(
    provider: &str,
    cli_model: Option<&str>,
    preferred_model: &str,
    catalog: &[Model],
    config: &AnieConfig,
    credential_store: &CredentialStore,
) -> Result<Model> {
    // Step 1: exact catalog match only if the user asked for a
    // specific model. When they passed --provider alone, skip
    // straight to "any model under this provider".
    if cli_model.is_some() {
        if let Some(model) = catalog
            .iter()
            .find(|m| m.provider == provider && m.id == preferred_model)
        {
            return Ok(model.clone());
        }
    } else if let Some(model) = catalog.iter().find(|m| m.provider == provider) {
        return Ok(model.clone());
    }

    // Step 2: fabricate from config / OAuth credential.
    if let Some(model) =
        fallback_model_from_provider(provider, preferred_model, config, catalog, credential_store)
    {
        return Ok(model);
    }

    // Step 3: any catalog entry for the provider, as a last resort.
    if let Some(model) = catalog.iter().find(|m| m.provider == provider) {
        return Ok(model.clone());
    }

    // Step 4: error — don't silently pick something unrelated.
    Err(anyhow!(
        "no model available for provider '{provider}' \
         (asked for '{preferred_model}'). Check that the provider \
         is logged in (`anie login {provider}`) or registered in \
         config.toml."
    ))
}

/// Resolve a user-typed model request (`"provider:id"` or bare id
/// within the current provider).
pub(crate) fn resolve_requested_model(
    requested: &str,
    current_provider: &str,
    catalog: &[Model],
) -> Result<Model> {
    // Plan 08 PR-C finding #28: the previous shape walked the
    // catalog up to three times for a provider-prefixed
    // request ("provider:id") — once to verify the provider
    // existed, once for the full match, and once more to
    // actually extract the found model. Collapse to a single
    // pass: `find` returns the match directly; Option handling
    // distinguishes "match found" from "no such provider:id".
    if let Some((provider, model_id)) = requested.split_once(':')
        && let Some(model) = catalog
            .iter()
            .find(|model| model.provider == provider && model.id == model_id)
    {
        return Ok(model.clone());
    }

    catalog
        .iter()
        .find(|model| model.provider == current_provider && model.id == requested)
        .cloned()
        .or_else(|| catalog.iter().find(|model| model.id == requested).cloned())
        .ok_or_else(|| anyhow!("no model named '{requested}' was found"))
}

/// Resolve a `(provider?, model_id?)` pair to a concrete `Model` via
/// progressively relaxed matching. Falls back to any local model
/// (if local servers are available) or the first catalog entry.
pub(crate) fn resolve_model(
    provider: Option<&str>,
    model_id: Option<&str>,
    model_catalog: &[Model],
    local_models_available: bool,
) -> Result<Model> {
    if let (Some(provider), Some(model_id)) = (provider, model_id)
        && let Some(model) = model_catalog
            .iter()
            .find(|model| model.provider == provider && model.id == model_id)
    {
        return Ok(model.clone());
    }

    if let Some(model_id) = model_id
        && let Some(model) = model_catalog.iter().find(|model| model.id == model_id)
    {
        return Ok(model.clone());
    }

    if let Some(provider) = provider
        && let Some(model) = model_catalog
            .iter()
            .find(|model| model.provider == provider)
    {
        return Ok(model.clone());
    }

    if local_models_available
        && let Some(local_model) = model_catalog
            .iter()
            .find(|model| model.provider == "ollama" || model.provider == "lmstudio")
    {
        return Ok(local_model.clone());
    }

    model_catalog
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("no models are configured or detected"))
}

/// Fabricate a `Model` for a provider that isn't in the catalog but
/// has enough config metadata — or a stored OAuth credential — to
/// send a request.
///
/// Resolution order for the base URL:
/// 1. `config.toml` [providers.<name>] explicit base_url.
/// 2. Any catalog entry already using this provider.
/// 3. Stored OAuth credential's `api_base_url` (Copilot's
///    per-user proxy-ep rewrite lands here).
/// 4. Hardcoded defaults for common providers.
pub(crate) fn fallback_model_from_provider(
    provider: &str,
    model_id: &str,
    config: &AnieConfig,
    catalog: &[Model],
    credential_store: &CredentialStore,
) -> Option<Model> {
    let provider_config = config.providers.get(provider);
    let api = provider_config
        .and_then(|provider| provider.api)
        .or(Some(match provider {
            "anthropic" => ApiKind::AnthropicMessages,
            _ => ApiKind::OpenAICompletions,
        }))?;
    let base_url = provider_config
        .and_then(|provider| provider.base_url.clone())
        .or_else(|| {
            catalog
                .iter()
                .find(|candidate| candidate.provider == provider)
                .map(|candidate| candidate.base_url.clone())
        })
        .or_else(|| oauth_credential_base_url(provider, credential_store))
        .or_else(|| match provider {
            "anthropic" => Some("https://api.anthropic.com".to_string()),
            "openai" => Some("https://api.openai.com/v1".to_string()),
            _ => None,
        })?;

    Some(Model {
        id: model_id.to_string(),
        name: model_id.to_string(),
        provider: provider.to_string(),
        api,
        base_url,
        context_window: 32_768,
        max_tokens: 8_192,
        supports_reasoning: false,
        reasoning_capabilities: None,
        supports_images: false,
        cost_per_million: CostPerMillion::zero(),
        replay_capabilities: None,
        compat: ModelCompat::None,
    })
}

/// Pull `api_base_url` out of a stored OAuth credential, if one
/// exists. Used by `fallback_model_from_provider` so a user who
/// just ran `anie login github-copilot` can immediately use
/// `anie --provider github-copilot --model <id>` without
/// hand-editing config.toml.
fn oauth_credential_base_url(provider: &str, store: &CredentialStore) -> Option<String> {
    match store.get_credential(provider)? {
        AuthCredential::OAuth { api_base_url, .. } => api_base_url,
        AuthCredential::ApiKey { .. } => None,
    }
}

/// Replace or append a model in the catalog by (provider, id).
pub(crate) fn upsert_model(models: &mut Vec<Model>, model: &Model) {
    if let Some(existing) = models
        .iter_mut()
        .find(|existing| existing.provider == model.provider && existing.id == model.id)
    {
        *existing = model.clone();
    } else {
        models.push(model.clone());
    }
}

/// Drop duplicate `(provider, id)` entries, keeping the later
/// occurrence. The inputs from different sources (builtin, config,
/// discovery) may overlap; this normalizes the catalog so every
/// entry is uniquely addressable.
pub(crate) fn dedupe_models(models: &mut Vec<Model>) {
    // Plan 08 PR-C finding #29: the previous shape built a
    // `(String, String)` key per model for the HashSet,
    // cloning provider + id. With a typical 500+ model
    // OpenRouter catalog that's ~1000 String clones. The
    // composite key is only consumed for hash/equality
    // during `retain`, so we can concat the two strings
    // into a single key separated by a byte that can't
    // appear in a valid provider/id — `\0` works.
    let mut seen: HashSet<String> = HashSet::new();
    models.reverse();
    models.retain(|model| {
        let mut key = String::with_capacity(model.provider.len() + 1 + model.id.len());
        key.push_str(&model.provider);
        key.push('\0');
        key.push_str(&model.id);
        seen.insert(key)
    });
    models.reverse();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_state::RuntimeState;
    use anie_provider::{
        ApiKind, CostPerMillion, ReasoningCapabilities, ReasoningControlMode, ReasoningOutputMode,
        ThinkingRequestMode,
    };

    fn model(id: &str, provider: &str) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            provider: provider.into(),
            api: ApiKind::OpenAICompletions,
            base_url: "http://localhost:11434/v1".into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        }
    }

    #[test]
    fn dedupe_models_keeps_later_entries_for_same_provider_and_id() {
        let mut models = vec![
            model("o4-mini", "openai"),
            Model {
                max_tokens: 16_384,
                supports_reasoning: true,
                reasoning_capabilities: Some(ReasoningCapabilities {
                    control: Some(ReasoningControlMode::Native),
                    output: Some(ReasoningOutputMode::Separated),
                    tags: None,
                    request_mode: Some(ThinkingRequestMode::ReasoningEffort),
                }),
                ..model("o4-mini", "openai")
            },
        ];

        dedupe_models(&mut models);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].max_tokens, 16_384);
        assert!(models[0].supports_reasoning);
        assert_eq!(
            models[0].reasoning_capabilities,
            Some(ReasoningCapabilities {
                control: Some(ReasoningControlMode::Native),
                output: Some(ReasoningOutputMode::Separated),
                tags: None,
                request_mode: Some(ThinkingRequestMode::ReasoningEffort),
            })
        );
    }

    #[test]
    fn resolve_model_honors_provider_and_id() {
        let models = vec![model("gpt-4o", "openai"), model("qwen3:32b", "ollama")];
        let resolved =
            resolve_model(Some("ollama"), Some("qwen3:32b"), &models, true).expect("resolve model");
        assert_eq!(resolved.provider, "ollama");
        assert_eq!(resolved.id, "qwen3:32b");
    }

    #[test]
    fn resolve_model_prefers_local_when_no_hints() {
        let models = vec![model("gpt-4o", "openai"), model("qwen3:32b", "ollama")];
        let resolved = resolve_model(None, None, &models, true).expect("resolve model");
        assert_eq!(resolved.provider, "ollama");
    }

    #[test]
    fn exact_or_fabricate_rejects_unknown_provider_without_credential() {
        // Regression: prior to PR G, an unknown --provider with a
        // mismatched --model would silently fall back to the
        // first catalog entry. Now we error instead.
        let config = AnieConfig::default();
        let store =
            anie_auth::CredentialStore::with_config("anie-test-g", None).without_native_keyring();
        let catalog = vec![
            model("claude-sonnet-4-6", "anthropic"),
            model("gpt-4o", "openai"),
        ];
        let err = exact_or_fabricate(
            "github-copilot",
            Some("claude-sonnet-4.6"),
            "claude-sonnet-4.6",
            &catalog,
            &config,
            &store,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("github-copilot"), "{err}");
        assert!(err.contains("anie login"), "{err}");
    }

    #[test]
    fn exact_or_fabricate_uses_oauth_api_base_url_when_present() {
        // Regression: fallback_model_from_provider must consult
        // the credential store for providers with no config
        // entry. The resulting Model must use the OAuth
        // credential's api_base_url (per-user endpoint).
        let tempdir = tempfile::tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        let store = anie_auth::CredentialStore::with_config("anie-test-g", Some(auth_path))
            .without_native_keyring();
        store
            .set_credential(
                "github-copilot",
                anie_auth::AuthCredential::OAuth {
                    access_token: "copilot-token".into(),
                    refresh_token: "github-oauth".into(),
                    expires_at: "2099-01-01T00:00:00Z".into(),
                    account: Some("octocat".into()),
                    api_base_url: Some("https://api.individual.githubcopilot.com".into()),
                    project_id: None,
                },
            )
            .expect("save");
        let config = AnieConfig::default();
        let catalog: Vec<Model> = Vec::new();
        let model = exact_or_fabricate(
            "github-copilot",
            Some("claude-sonnet-4.6"),
            "claude-sonnet-4.6",
            &catalog,
            &config,
            &store,
        )
        .expect("fabricate");
        assert_eq!(model.provider, "github-copilot");
        assert_eq!(model.id, "claude-sonnet-4.6");
        assert_eq!(model.base_url, "https://api.individual.githubcopilot.com");
    }

    #[test]
    fn resolve_initial_selection_prefers_provider_only_override() {
        let cli = Cli {
            command: None,
            interactive: false,
            print: true,
            rpc: false,
            no_tools: false,
            harness_mode: crate::harness_mode::HarnessMode::default(),
            prompt: vec!["hello".into()],
            model: None,
            provider: Some("ollama".into()),
            api_key: None,
            thinking: None,
            resume: None,
            cwd: None,
        };
        let config = AnieConfig::default();
        let runtime_state = RuntimeState::default();
        let session_context = SessionContext::empty();
        let models = vec![model("gpt-4o", "openai"), model("qwen3:32b", "ollama")];

        let selection = resolve_initial_selection(
            &cli,
            &config,
            &runtime_state,
            &session_context,
            &models,
            true,
        )
        .expect("resolve selection");
        assert_eq!(selection.model.provider, "ollama");
    }

    fn default_cli() -> Cli {
        Cli {
            command: None,
            interactive: true,
            print: false,
            rpc: false,
            no_tools: false,
            harness_mode: crate::harness_mode::HarnessMode::default(),
            prompt: Vec::new(),
            model: None,
            provider: None,
            api_key: None,
            thinking: None,
            resume: None,
            cwd: None,
        }
    }

    /// Regression: when the user puts an explicit `[model]`
    /// section in `config.toml`, that declared preference must
    /// override a stale entry in `state.json`. Before the fix,
    /// resolution preferred `runtime_state` over config, so
    /// editing config.toml had no effect — users saw the old
    /// persisted model every launch.
    #[test]
    fn explicit_config_model_overrides_stale_runtime_state() {
        let cli = default_cli();
        let mut config = AnieConfig::default();
        config.model.provider = "ollama".into();
        config.model.id = "qwen3:32b".into();
        config.model_explicitly_set = true;

        let runtime_state = RuntimeState {
            provider: Some("openai".into()),
            model: Some("gpt-4o".into()),
            thinking: None,
            last_session_id: None,
            ..RuntimeState::default()
        };
        let session_context = SessionContext::empty();
        let models = vec![model("gpt-4o", "openai"), model("qwen3:32b", "ollama")];

        let selection = resolve_initial_selection(
            &cli,
            &config,
            &runtime_state,
            &session_context,
            &models,
            true,
        )
        .expect("resolve selection");
        assert_eq!(selection.model.provider, "ollama");
        assert_eq!(selection.model.id, "qwen3:32b");
    }

    /// Contract: when the user has NOT declared a `[model]`
    /// section in config.toml, `runtime_state` still wins over
    /// the hardcoded `ModelConfig::default()` fallback — so the
    /// "last model used" persistence keeps working for users
    /// who never customize the config file.
    #[test]
    fn runtime_state_still_wins_when_config_has_no_explicit_model() {
        let cli = default_cli();
        let config = AnieConfig::default(); // model_explicitly_set = false
        let runtime_state = RuntimeState {
            provider: Some("ollama".into()),
            model: Some("qwen3:32b".into()),
            thinking: None,
            last_session_id: None,
            ..RuntimeState::default()
        };
        let session_context = SessionContext::empty();
        let models = vec![model("gpt-4o", "openai"), model("qwen3:32b", "ollama")];

        let selection = resolve_initial_selection(
            &cli,
            &config,
            &runtime_state,
            &session_context,
            &models,
            true,
        )
        .expect("resolve selection");
        assert_eq!(selection.model.provider, "ollama");
        assert_eq!(selection.model.id, "qwen3:32b");
    }
}
