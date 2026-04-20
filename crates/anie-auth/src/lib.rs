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

/// Write a credential to `path`, preserving any credentials that
/// are already stored there.
///
/// **Never overwrites a store whose existing contents could not be
/// parsed.** If `auth.json` is corrupt (truncated, hand-edited
/// into invalid JSON, etc.), this function quarantines the
/// original as a sibling `auth.json.corrupt.<unix_secs>` file and
/// returns an error rather than silently clobbering the other
/// credentials. Callers can recover by deleting the corrupt file
/// (to start fresh) or by hand-editing the quarantined copy back
/// into place.
///
/// Absent files are not corruption — they cause a fresh empty
/// store to be created.
pub(crate) fn save_api_key_at(path: &Path, provider: &str, key: &str) -> Result<()> {
    let mut store = match load_auth_store_at(path) {
        Ok(store) => store,
        Err(parse_err) => {
            // Path exists (load_auth_store_at only errors on a
            // populated-but-unparseable file) but its contents
            // can't be interpreted. Quarantine the original so
            // the user has a path back to their data, then fail
            // loudly.
            let backup = quarantine_corrupt_auth_file(path)?;
            return Err(anyhow::anyhow!(
                "auth store at {} is unreadable; original quarantined at {}. \
                 Inspect or repair that file, or delete {} to start fresh. \
                 (parse error: {parse_err})",
                path.display(),
                backup.display(),
                path.display(),
            ));
        }
    };
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
    anie_config::atomic_write(path, contents.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod {}", path.display()))?;
    }

    Ok(())
}

/// Copy the corrupt auth file to a timestamped sibling so the
/// user retains a recoverable original.
///
/// Uses `fs::copy` (not `rename`) so that if a subsequent write
/// somehow trashes the original path, the backup still exists.
fn quarantine_corrupt_auth_file(path: &Path) -> Result<PathBuf> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let parent = path.parent().unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "auth".to_string());
    let backup = parent.join(format!("{file_name}.corrupt.{timestamp}"));
    fs::copy(path, &backup).with_context(|| {
        format!(
            "failed to quarantine corrupt auth file at {} to {}",
            path.display(),
            backup.display()
        )
    })?;
    Ok(backup)
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

    // =========================================================================
    // Plan 14 phase B — auth-store corruption discipline.
    //
    // Regression tests for the silent-clobber bug: previously
    // `save_api_key_at` used `unwrap_or_default()` on the load
    // result, which turned a parse failure into an empty store
    // and then overwrote the target. After the fix, a
    // parse-failure path quarantines the original and refuses to
    // write.
    // =========================================================================

    #[test]
    fn save_api_key_with_corrupt_store_preserves_existing_file_and_quarantines() {
        let tempdir = tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        // Write a file that exists but is not valid JSON.
        fs::write(&auth_path, b"{ not valid json").expect("seed corrupt");
        let original_bytes = fs::read(&auth_path).expect("read seed");

        let result = save_api_key_at(&auth_path, "openai", "sk-test");
        assert!(
            result.is_err(),
            "save must refuse to run against a corrupt store"
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("quarantined"), "error must mention quarantine: {err}");

        // Original must be untouched.
        assert_eq!(
            fs::read(&auth_path).expect("read target"),
            original_bytes,
            "target must keep its original bytes"
        );

        // A sibling quarantine file must exist.
        let entries: Vec<_> = fs::read_dir(tempdir.path())
            .expect("list dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let has_quarantine = entries
            .iter()
            .any(|name| name.starts_with("auth.json.corrupt."));
        assert!(
            has_quarantine,
            "expected a quarantine file; found: {entries:?}"
        );
    }

    #[test]
    fn save_api_key_with_empty_file_is_also_corrupt() {
        // Zero-byte auth.json is not valid JSON, so it counts as
        // corrupt and triggers quarantine rather than being
        // silently replaced.
        let tempdir = tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        fs::write(&auth_path, b"").expect("seed empty");
        let result = save_api_key_at(&auth_path, "openai", "sk-test");
        assert!(result.is_err(), "empty file must be rejected");
    }

    #[test]
    fn save_api_key_creates_new_store_when_file_absent() {
        // Missing is NOT corruption. Creating a fresh store on a
        // non-existent path is the expected happy path.
        let tempdir = tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("subdir").join("auth.json");
        assert!(!auth_path.exists());

        save_api_key_at(&auth_path, "openai", "sk-test").expect("creates new store");

        let store = load_auth_store_at(&auth_path).expect("load after save");
        assert_eq!(
            store.providers.get("openai"),
            Some(&AuthCredential::ApiKey {
                key: "sk-test".into()
            })
        );
    }

    #[test]
    fn save_api_key_on_valid_store_still_preserves_existing_credentials() {
        // Happy-path regression. Saving a new provider into an
        // existing valid store must preserve all other providers.
        let tempdir = tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        save_api_key_at(&auth_path, "openai", "sk-one").expect("first save");
        save_api_key_at(&auth_path, "anthropic", "sk-two").expect("second save");

        let store = load_auth_store_at(&auth_path).expect("reload");
        assert_eq!(
            store.providers.get("openai"),
            Some(&AuthCredential::ApiKey {
                key: "sk-one".into()
            })
        );
        assert_eq!(
            store.providers.get("anthropic"),
            Some(&AuthCredential::ApiKey {
                key: "sk-two".into()
            })
        );
    }
}
