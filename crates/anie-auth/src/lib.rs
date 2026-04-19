//! API-key storage and async request-option resolution.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod store;

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::warn;

use anie_config::AnieConfig;
use anie_protocol::Message;
use anie_provider::{Model, ProviderError, RequestOptionsResolver, ResolvedRequestOptions};

pub use store::CredentialStore;

/// Credential storage keyed by provider name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AuthStore {
    /// Provider credentials.
    #[serde(flatten)]
    pub providers: HashMap<String, AuthCredential>,
}

/// Supported v1 credential types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum AuthCredential {
    /// API-key credential.
    #[serde(rename = "api_key")]
    ApiKey { key: String },
}

/// Resolve per-request auth from CLI, persisted credentials, and environment variables.
pub struct AuthResolver {
    /// Optional CLI API key override.
    pub cli_api_key: Option<String>,
    /// Loaded application configuration.
    pub config: AnieConfig,
    credential_store: CredentialStore,
}

impl AuthResolver {
    /// Create a resolver using the default credential store.
    #[must_use]
    pub fn new(cli_api_key: Option<String>, config: AnieConfig) -> Self {
        Self::with_credential_store(cli_api_key, config, CredentialStore::new())
    }

    /// Create a resolver with an explicit credential store, primarily for tests.
    #[must_use]
    pub fn with_credential_store(
        cli_api_key: Option<String>,
        config: AnieConfig,
        credential_store: CredentialStore,
    ) -> Self {
        Self {
            cli_api_key,
            config,
            credential_store,
        }
    }

    /// Override the auth file path, primarily for legacy tests.
    #[must_use]
    #[deprecated(note = "use AuthResolver::with_credential_store instead")]
    pub fn with_auth_path(mut self, auth_path: Option<PathBuf>) -> Self {
        self.credential_store = CredentialStore::with_config("anie", auth_path);
        self
    }
}

#[async_trait]
impl RequestOptionsResolver for AuthResolver {
    async fn resolve(
        &self,
        model: &Model,
        _context: &[Message],
    ) -> Result<ResolvedRequestOptions, ProviderError> {
        if let Some(cli_api_key) = &self.cli_api_key {
            return Ok(ResolvedRequestOptions {
                api_key: Some(cli_api_key.clone()),
                headers: HashMap::new(),
                base_url_override: None,
            });
        }

        let api_key = self.credential_store.get(&model.provider).or_else(|| {
            let configured_env = self
                .config
                .providers
                .get(&model.provider)
                .and_then(|provider| provider.api_key_env.as_deref())
                .map(str::to_owned)
                .or_else(|| builtin_api_key_env(&model.provider).map(str::to_owned))?;
            std::env::var(&configured_env).ok()
        });

        Ok(ResolvedRequestOptions {
            api_key,
            headers: HashMap::new(),
            base_url_override: None,
        })
    }
}

pub(crate) fn default_auth_file_path() -> Option<PathBuf> {
    anie_config::anie_auth_json_path()
}

pub(crate) fn load_auth_store_at(path: &Path) -> Result<AuthStore> {
    if !path.exists() {
        return Ok(AuthStore::default());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata =
            fs::metadata(path).with_context(|| format!("failed to inspect {}", path.display()))?;
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            warn!(
                path = %path.display(),
                mode = format!("{:o}", mode & 0o777),
                "auth file permissions are broader than 0600",
            );
        }
    }

    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read auth file {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse auth file {}", path.display()))
}

pub(crate) fn save_api_key_at(path: &Path, provider: &str, key: &str) -> Result<()> {
    let mut store = load_auth_store_at(path).unwrap_or_default();
    store.providers.insert(
        provider.to_string(),
        AuthCredential::ApiKey {
            key: key.to_string(),
        },
    );

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let contents =
        serde_json::to_string_pretty(&store).context("failed to serialize auth store")?;
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod {}", path.display()))?;
    }

    Ok(())
}

/// Return the default auth.json path.
#[must_use]
#[deprecated(note = "use CredentialStore::new instead")]
pub fn auth_file_path() -> Option<PathBuf> {
    default_auth_file_path()
}

/// Load credentials from the default auth file.
#[deprecated(note = "use CredentialStore::get instead")]
pub fn load_auth_store() -> Result<AuthStore> {
    let path = default_auth_file_path().context("home directory is not available")?;
    load_auth_store_at(&path)
}

/// Load credentials from an explicit auth file path.
#[deprecated(note = "use CredentialStore::with_config(...).get instead")]
pub fn load_auth_store_from(path: &Path) -> Result<AuthStore> {
    load_auth_store_at(path)
}

/// Save an API key to the default auth file.
#[deprecated(note = "use CredentialStore::set instead")]
pub fn save_api_key(provider: &str, key: &str) -> Result<()> {
    let path = default_auth_file_path().context("home directory is not available")?;
    save_api_key_at(&path, provider, key)
}

/// Save an API key to an explicit auth file path.
#[deprecated(note = "use CredentialStore::with_config(...).set instead")]
pub fn save_api_key_to(path: &Path, provider: &str, key: &str) -> Result<()> {
    save_api_key_at(path, provider, key)
}

fn builtin_api_key_env(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "google" => Some("GEMINI_API_KEY"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use anie_provider::{ApiKind, CostPerMillion};

    fn sample_model(provider: &str) -> Model {
        Model {
            id: "model".into(),
            name: "Model".into(),
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
        }
    }

    #[test]
    fn saves_and_loads_api_keys() {
        let tempdir = tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        save_api_key_at(&auth_path, "openai", "sk-test").expect("save api key");
        let store = load_auth_store_at(&auth_path).expect("load auth store");
        assert_eq!(
            store.providers.get("openai"),
            Some(&AuthCredential::ApiKey {
                key: "sk-test".into()
            })
        );
    }

    #[tokio::test]
    async fn resolver_prioritizes_cli_then_credential_store_then_env() {
        let tempdir = tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        let credential_store = CredentialStore::with_config("anie-test", Some(auth_path.clone()))
            .without_native_keyring();
        credential_store
            .set("openai", "auth-key")
            .expect("save auth key");
        // SAFETY: this test uses a process-unique temporary variable name and cleans it up before exit.
        unsafe {
            std::env::set_var("ANIE_TEST_OPENAI_KEY", "env-key");
        }

        let mut config = AnieConfig::default();
        config.providers.insert(
            "openai".into(),
            anie_config::ProviderConfig {
                api_key_env: Some("ANIE_TEST_OPENAI_KEY".into()),
                ..Default::default()
            },
        );

        let cli_resolver = AuthResolver::with_credential_store(
            Some("cli-key".into()),
            config.clone(),
            credential_store.clone(),
        );
        let auth_resolver =
            AuthResolver::with_credential_store(None, config.clone(), credential_store);
        let env_resolver = AuthResolver::with_credential_store(
            None,
            config,
            CredentialStore::with_config("anie-test", Some(tempdir.path().join("missing.json")))
                .without_native_keyring(),
        );

        let cli = cli_resolver
            .resolve(&sample_model("openai"), &[])
            .await
            .expect("cli resolve");
        let auth = auth_resolver
            .resolve(&sample_model("openai"), &[])
            .await
            .expect("auth resolve");
        let env = env_resolver
            .resolve(&sample_model("openai"), &[])
            .await
            .expect("env resolve");

        assert_eq!(cli.api_key.as_deref(), Some("cli-key"));
        assert_eq!(auth.api_key.as_deref(), Some("auth-key"));
        assert_eq!(env.api_key.as_deref(), Some("env-key"));

        // SAFETY: this test removes the process-unique variable it created above.
        unsafe {
            std::env::remove_var("ANIE_TEST_OPENAI_KEY");
        }
    }

    #[tokio::test]
    async fn resolver_allows_local_models_without_api_keys() {
        let resolver = AuthResolver::with_credential_store(
            None,
            AnieConfig::default(),
            CredentialStore::with_config("anie-test", None).without_native_keyring(),
        );
        let resolved = resolver
            .resolve(&sample_model("ollama"), &[])
            .await
            .expect("resolve local model");
        assert_eq!(resolved.api_key, None);
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tempdir = tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        save_api_key_at(&auth_path, "openai", "sk-test").expect("save api key");
        let mode = fs::metadata(&auth_path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
