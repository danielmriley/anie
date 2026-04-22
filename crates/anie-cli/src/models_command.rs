use std::{collections::BTreeMap, time::Duration};

use anyhow::Result;

use anie_auth::{AuthCredential, CredentialStore, oauth_request_headers};
use anie_config::{CliOverrides, load_config};
use anie_provider::{ApiKind, ModelInfo};
use anie_providers_builtin::{ModelDiscoveryCache, ModelDiscoveryRequest, detect_local_servers};

pub async fn run_models_command(provider_filter: Option<&str>, refresh: bool) -> Result<()> {
    let config = load_config(CliOverrides::default())?;
    let credential_store = CredentialStore::new();
    let mut requests = configured_requests(&config, &credential_store).await;

    if let Some(filter) = provider_filter {
        requests.retain(|_, request| request.provider_name == filter);
    }

    if requests.is_empty() {
        println!("No providers configured. Run `anie onboard` to set up a provider.");
        return Ok(());
    }

    let mut cache = ModelDiscoveryCache::new(Duration::from_secs(300));
    let mut rows = Vec::<(String, ModelInfo)>::new();
    let mut errors = Vec::<(String, String)>::new();

    for request in requests.values() {
        let result = if refresh {
            cache.refresh(request).await
        } else {
            cache.get_or_discover(request).await
        };
        match result {
            Ok(models) => {
                for model in models {
                    rows.push((request.provider_name.clone(), model));
                }
            }
            Err(error) => errors.push((request.provider_name.clone(), error.to_string())),
        }
    }

    if rows.is_empty() {
        if errors.is_empty() {
            println!("No models were discovered for the selected providers.");
        } else {
            for (provider, error) in errors {
                println!("{provider}: {error}");
            }
        }
        return Ok(());
    }

    rows.sort_by(
        |(left_provider, left_model), (right_provider, right_model)| {
            left_provider
                .cmp(right_provider)
                .then_with(|| left_model.id.cmp(&right_model.id))
        },
    );
    print_models_table(&rows);
    if !errors.is_empty() {
        println!();
        for (provider, error) in errors {
            println!("{provider}: {error}");
        }
    }
    Ok(())
}

async fn configured_requests(
    config: &anie_config::AnieConfig,
    credential_store: &CredentialStore,
) -> BTreeMap<String, ModelDiscoveryRequest> {
    let mut requests = BTreeMap::new();

    for (provider_name, provider_config) in &config.providers {
        let Some(base_url) = provider_config
            .base_url
            .clone()
            .or_else(|| default_base_url(provider_name))
        else {
            continue;
        };
        requests.insert(
            provider_name.clone(),
            ModelDiscoveryRequest {
                provider_name: provider_name.clone(),
                api: provider_config.api.unwrap_or(default_api(provider_name)),
                base_url,
                api_key: resolve_provider_api_key(
                    provider_name,
                    provider_config.api_key_env.as_deref(),
                    credential_store,
                ),
                // Provider-specific headers (Copilot's editor
                // identifiers) must ride along even when the
                // user has added the provider to config.toml
                // manually. Otherwise discovery works the first
                // time via the OAuth fallback loop below but
                // breaks as soon as a config entry exists.
                headers: oauth_request_headers(provider_name),
            },
        );
    }

    if !requests.contains_key(&config.model.provider) {
        let provider_name = config.model.provider.clone();
        if let Some(base_url) = default_base_url(&provider_name) {
            requests.insert(
                provider_name.clone(),
                ModelDiscoveryRequest {
                    provider_name: provider_name.clone(),
                    api: default_api(&provider_name),
                    base_url,
                    api_key: resolve_provider_api_key(&provider_name, None, credential_store),
                    headers: Default::default(),
                },
            );
        }
    }

    // Discover implicit OAuth-backed providers. When a user
    // runs `anie login github-copilot`, the stored credential
    // carries a per-user `api_base_url` — that's enough to run
    // model discovery against the provider without requiring
    // the user to hand-edit config.toml. The credential's
    // presence is the registration signal.
    for provider_name in credential_store.list_providers() {
        if requests.contains_key(&provider_name) {
            continue;
        }
        let Some(AuthCredential::OAuth {
            access_token,
            api_base_url,
            ..
        }) = credential_store.get_credential(&provider_name)
        else {
            continue;
        };
        let Some(base_url) = api_base_url
            .or_else(|| oauth_provider_default_base_url(&provider_name))
        else {
            continue;
        };
        requests.insert(
            provider_name.clone(),
            ModelDiscoveryRequest {
                provider_name: provider_name.clone(),
                api: default_api(&provider_name),
                base_url,
                api_key: Some(access_token),
                headers: oauth_request_headers(&provider_name),
            },
        );
    }

    for server in detect_local_servers().await {
        let api = server
            .models
            .first()
            .map(|model| model.api)
            .unwrap_or(ApiKind::OpenAICompletions);
        let base_url = server
            .models
            .first()
            .map(|model| model.base_url.clone())
            .unwrap_or_else(|| format!("{}/v1", server.base_url.trim_end_matches('/')));
        requests
            .entry(server.name.clone())
            .or_insert(ModelDiscoveryRequest {
                provider_name: server.name,
                api,
                base_url,
                api_key: None,
                headers: Default::default(),
            });
    }

    requests
}

fn resolve_provider_api_key(
    provider_name: &str,
    api_key_env: Option<&str>,
    credential_store: &CredentialStore,
) -> Option<String> {
    credential_store.get(provider_name).or_else(|| {
        api_key_env
            .and_then(|name| std::env::var(name).ok())
            .or_else(|| match provider_name {
                "openai" => std::env::var("OPENAI_API_KEY").ok(),
                "anthropic" => std::env::var("ANTHROPIC_API_KEY").ok(),
                _ => None,
            })
    })
}

fn default_api(provider_name: &str) -> ApiKind {
    match provider_name {
        "anthropic" => ApiKind::AnthropicMessages,
        _ => ApiKind::OpenAICompletions,
    }
}

fn default_base_url(provider_name: &str) -> Option<String> {
    match provider_name {
        "anthropic" => Some("https://api.anthropic.com".to_string()),
        "openai" => Some("https://api.openai.com/v1".to_string()),
        _ => None,
    }
}

/// Fallback base URL for an OAuth-backed provider when its
/// credential is missing `api_base_url`. GitHub Copilot in
/// particular only populates `api_base_url` via a successful
/// Copilot-internal token exchange; if a user somehow stored a
/// credential without it, this keeps discovery from failing
/// outright.
fn oauth_provider_default_base_url(provider_name: &str) -> Option<String> {
    match provider_name {
        "github-copilot" => Some("https://api.individual.githubcopilot.com".to_string()),
        "openai-codex" => Some("https://api.openai.com/v1".to_string()),
        _ => None,
    }
}


fn print_models_table(rows: &[(String, ModelInfo)]) {
    let provider_width = rows
        .iter()
        .map(|(provider, _)| provider.chars().count())
        .max()
        .unwrap_or(8)
        .max("Provider".len());
    let model_width = rows
        .iter()
        .map(|(_, model)| model.id.chars().count())
        .max()
        .unwrap_or(8)
        .max("Model ID".len());
    let context_width = rows
        .iter()
        .map(|(_, model)| format_context(model.context_length).chars().count())
        .max()
        .unwrap_or(7)
        .max("Context".len());

    println!(
        "{:<provider_width$}  {:<model_width$}  {:<context_width$}  {:<9}  {:<6}",
        "Provider", "Model ID", "Context", "Reasoning", "Images",
    );
    println!(
        "{:-<provider_width$}  {:-<model_width$}  {:-<context_width$}  {:-<9}  {:-<6}",
        "", "", "", "", "",
    );

    for (provider, model) in rows {
        println!(
            "{:<provider_width$}  {:<model_width$}  {:<context_width$}  {:<9}  {:<6}",
            provider,
            model.id,
            format_context(model.context_length),
            yes_marker(model.supports_reasoning),
            yes_marker(model.supports_images),
        );
    }
}

fn format_context(value: Option<u64>) -> String {
    match value {
        Some(tokens) => tokens.to_string(),
        None => String::new(),
    }
}

fn yes_marker(value: Option<bool>) -> &'static str {
    if value.unwrap_or(false) { "✓" } else { "" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_auth::AuthCredential;
    use anie_config::AnieConfig;
    use tempfile::tempdir;


    #[tokio::test]
    async fn configured_requests_picks_up_oauth_providers_without_config_entry() {
        // Seed an auth store with a Copilot OAuth credential
        // and verify `configured_requests` surfaces it even
        // though config.toml has nothing under providers.
        let tempdir = tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        let store = CredentialStore::with_config("anie-test", Some(auth_path))
            .without_native_keyring();
        store
            .set_credential(
                "github-copilot",
                AuthCredential::OAuth {
                    access_token: "copilot-token".into(),
                    refresh_token: "github-oauth".into(),
                    expires_at: "2099-01-01T00:00:00Z".into(),
                    account: Some("octocat".into()),
                    api_base_url: Some("https://api.individual.githubcopilot.com".into()),
                    project_id: None,
                },
            )
            .expect("save oauth");

        let config = AnieConfig::default();
        let requests = configured_requests(&config, &store).await;
        let req = requests
            .get("github-copilot")
            .expect("github-copilot surfaces from stored credential");
        assert_eq!(req.base_url, "https://api.individual.githubcopilot.com");
        assert_eq!(req.api_key.as_deref(), Some("copilot-token"));
        assert!(req.headers.contains_key("User-Agent"));
    }

    #[tokio::test]
    async fn configured_requests_prefers_config_base_url_over_credential() {
        // Safety: if someone hand-registers github-copilot in
        // config.toml with an explicit base_url, the config
        // entry wins (matches the loop order in
        // configured_requests).
        let tempdir = tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        let store = CredentialStore::with_config("anie-test", Some(auth_path))
            .without_native_keyring();
        store
            .set_credential(
                "github-copilot",
                AuthCredential::OAuth {
                    access_token: "copilot-token".into(),
                    refresh_token: "github-oauth".into(),
                    expires_at: "2099-01-01T00:00:00Z".into(),
                    account: None,
                    api_base_url: Some("https://api.individual.githubcopilot.com".into()),
                    project_id: None,
                },
            )
            .expect("save");

        let mut config = AnieConfig::default();
        config.providers.insert(
            "github-copilot".into(),
            anie_config::ProviderConfig {
                base_url: Some("https://custom.override".into()),
                ..Default::default()
            },
        );
        let requests = configured_requests(&config, &store).await;
        let req = requests.get("github-copilot").expect("req");
        assert_eq!(req.base_url, "https://custom.override");
    }

    #[test]
    fn formats_context_as_plain_number() {
        assert_eq!(format_context(Some(128_000)), "128000");
        assert_eq!(format_context(None), "");
    }

    #[test]
    fn yes_marker_is_only_set_for_true() {
        assert_eq!(yes_marker(Some(true)), "✓");
        assert_eq!(yes_marker(Some(false)), "");
        assert_eq!(yes_marker(None), "");
    }
}
