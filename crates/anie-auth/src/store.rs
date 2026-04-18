use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::{
    AuthCredential, AuthStore, default_auth_file_path, load_auth_store_at, save_api_key_at,
};

#[derive(Debug, Clone)]
enum NativeKeyringMode {
    System,
    Disabled,
    #[cfg(test)]
    Json(PathBuf),
}

/// Credential storage backed by the OS keyring with JSON fallbacks for compatibility.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    #[cfg(feature = "keyring-native")]
    app_name: String,
    json_fallback: Option<PathBuf>,
    native_keyring_mode: NativeKeyringMode,
}

impl CredentialStore {
    /// Create a store using the default app name and fallback path.
    #[must_use]
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "keyring-native")]
            app_name: "anie".to_string(),
            json_fallback: default_auth_file_path(),
            native_keyring_mode: NativeKeyringMode::System,
        }
    }

    /// Create a store with custom settings, primarily for tests.
    #[must_use]
    pub fn with_config(app_name: impl Into<String>, json_fallback: Option<PathBuf>) -> Self {
        let app_name = app_name.into();
        #[cfg(not(feature = "keyring-native"))]
        let _ = &app_name;

        Self {
            #[cfg(feature = "keyring-native")]
            app_name,
            json_fallback,
            native_keyring_mode: NativeKeyringMode::System,
        }
    }

    /// Disable native keyring access and use JSON-only fallbacks.
    #[must_use]
    pub fn without_native_keyring(mut self) -> Self {
        self.native_keyring_mode = NativeKeyringMode::Disabled;
        self
    }

    #[cfg(test)]
    #[must_use]
    fn with_test_native_keyring(mut self, path: PathBuf) -> Self {
        self.native_keyring_mode = NativeKeyringMode::Json(path);
        self
    }

    /// Retrieve a credential for the given provider.
    #[must_use]
    pub fn get(&self, provider: &str) -> Option<String> {
        if let Some(key) = self.get_from_native_keyring(provider) {
            return Some(key);
        }

        self.read_provider_from_path(self.json_fallback.as_deref(), provider)
            .or_else(|| {
                self.read_provider_from_path(self.migrated_fallback_path().as_deref(), provider)
            })
    }

    /// Return whether a legacy auth.json file should be migrated into the native keyring.
    #[must_use]
    pub fn should_migrate(&self) -> bool {
        if matches!(self.native_keyring_mode, NativeKeyringMode::Disabled)
            || !self.supports_native_keyring()
        {
            return false;
        }

        let Some(path) = self.json_fallback.as_deref() else {
            return false;
        };
        if !path.exists() {
            return false;
        }
        if self
            .migrated_fallback_path()
            .as_deref()
            .is_some_and(Path::exists)
        {
            return false;
        }

        load_store_if_present(Some(path)).is_some_and(|store| !store.providers.is_empty())
    }

    /// Import any legacy JSON credentials into the native keyring.
    pub fn migrate_from_json(&self) -> Result<usize> {
        if !self.should_migrate() {
            return Ok(0);
        }

        let path = self
            .json_fallback
            .as_deref()
            .context("home directory is not available for credential migration")?;
        let backup_path = self
            .migrated_fallback_path()
            .context("failed to compute migrated credential backup path")?;
        let store = load_auth_store_at(path).with_context(|| {
            format!("failed to load {} for credential migration", path.display())
        })?;

        for (provider, credential) in &store.providers {
            let AuthCredential::ApiKey { key } = credential;
            self.set_in_native_keyring(provider, key).with_context(|| {
                format!("failed to migrate credential for provider '{provider}'")
            })?;
        }

        fs::rename(path, &backup_path).with_context(|| {
            format!(
                "failed to rename {} to {} during credential migration",
                path.display(),
                backup_path.display()
            )
        })?;
        info!(
            count = store.providers.len(),
            backup = %backup_path.display(),
            "migrated credentials to native keyring"
        );
        Ok(store.providers.len())
    }

    /// Store a credential for the given provider.
    pub fn set(&self, provider: &str, key: &str) -> Result<()> {
        match self.set_in_native_keyring(provider, key) {
            Ok(()) => {
                if let Some(path) = self.json_fallback.as_deref()
                    && let Err(error) = save_api_key_at(path, provider, key)
                {
                    warn!(
                        provider,
                        %error,
                        "failed to mirror credential into JSON compatibility store after native keyring write"
                    );
                }
                return Ok(());
            }
            Err(error) => {
                warn!(
                    provider,
                    %error,
                    "failed to store credential in native keyring; falling back to JSON auth store"
                );
            }
        }

        let path = self
            .json_fallback
            .as_deref()
            .context("home directory is not available for JSON credential fallback")?;
        save_api_key_at(path, provider, key)
    }

    /// Delete a credential from all supported backends.
    pub fn delete(&self, provider: &str) -> Result<()> {
        let mut first_error = None;

        if let Err(error) = self.delete_from_native_keyring(provider) {
            warn!(provider, %error, "failed to delete credential from native keyring");
            first_error = Some(error);
        }

        for path in [
            self.json_fallback.as_deref(),
            self.migrated_fallback_path().as_deref(),
        ] {
            if let Err(error) = delete_provider_from_path(path, provider) {
                warn!(provider, %error, "failed to delete credential from JSON store");
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }

        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    /// List provider names discoverable from JSON-based compatibility stores.
    #[must_use]
    pub fn list_providers(&self) -> Vec<String> {
        let mut providers = BTreeSet::new();
        for path in [
            self.json_fallback.as_deref(),
            self.migrated_fallback_path().as_deref(),
        ] {
            if let Some(store) = load_store_if_present(path) {
                providers.extend(store.providers.into_keys());
            }
        }
        providers.into_iter().collect()
    }

    fn read_provider_from_path(&self, path: Option<&Path>, provider: &str) -> Option<String> {
        let store = load_store_if_present(path)?;
        store
            .providers
            .get(provider)
            .map(|AuthCredential::ApiKey { key }| key.clone())
    }

    fn migrated_fallback_path(&self) -> Option<PathBuf> {
        self.json_fallback.as_ref().map(|path| {
            let file_name = path.file_name().and_then(|name| name.to_str()).map_or_else(
                || "auth.json.migrated".to_string(),
                |name| format!("{name}.migrated"),
            );
            path.with_file_name(file_name)
        })
    }

    fn supports_native_keyring(&self) -> bool {
        match self.native_keyring_mode {
            NativeKeyringMode::System => cfg!(feature = "keyring-native"),
            NativeKeyringMode::Disabled => false,
            #[cfg(test)]
            NativeKeyringMode::Json(_) => true,
        }
    }

    fn get_from_native_keyring(&self, provider: &str) -> Option<String> {
        match &self.native_keyring_mode {
            NativeKeyringMode::Disabled => None,
            #[cfg(test)]
            NativeKeyringMode::Json(path) => self.read_provider_from_path(Some(path), provider),
            NativeKeyringMode::System => self.get_from_system_keyring(provider),
        }
    }

    fn set_in_native_keyring(&self, provider: &str, key: &str) -> Result<()> {
        match &self.native_keyring_mode {
            NativeKeyringMode::Disabled => anyhow::bail!("native keyring support is disabled"),
            #[cfg(test)]
            NativeKeyringMode::Json(path) => save_api_key_at(path, provider, key),
            NativeKeyringMode::System => self.set_in_system_keyring(provider, key),
        }
    }

    fn delete_from_native_keyring(&self, provider: &str) -> Result<()> {
        match &self.native_keyring_mode {
            NativeKeyringMode::Disabled => Ok(()),
            #[cfg(test)]
            NativeKeyringMode::Json(path) => {
                delete_provider_from_path(Some(path.as_path()), provider)
            }
            NativeKeyringMode::System => self.delete_from_system_keyring(provider),
        }
    }

    #[cfg(feature = "keyring-native")]
    fn get_from_system_keyring(&self, provider: &str) -> Option<String> {
        let entry = keyring::Entry::new(&self.app_name, provider).ok()?;
        match entry.get_password() {
            Ok(key) => Some(key),
            Err(error) => {
                warn!(provider, %error, "failed to read credential from native keyring");
                None
            }
        }
    }

    #[cfg(not(feature = "keyring-native"))]
    fn get_from_system_keyring(&self, _provider: &str) -> Option<String> {
        None
    }

    #[cfg(feature = "keyring-native")]
    fn set_in_system_keyring(&self, provider: &str, key: &str) -> Result<()> {
        let entry = keyring::Entry::new(&self.app_name, provider)
            .context("failed to open native keyring entry")?;
        entry
            .set_password(key)
            .context("failed to store credential in native keyring")
    }

    #[cfg(not(feature = "keyring-native"))]
    fn set_in_system_keyring(&self, _provider: &str, _key: &str) -> Result<()> {
        anyhow::bail!("native keyring support is disabled")
    }

    #[cfg(feature = "keyring-native")]
    fn delete_from_system_keyring(&self, provider: &str) -> Result<()> {
        let entry = keyring::Entry::new(&self.app_name, provider)
            .context("failed to open native keyring entry")?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(anyhow::Error::new(error)
                .context("failed to delete credential from native keyring")),
        }
    }

    #[cfg(not(feature = "keyring-native"))]
    fn delete_from_system_keyring(&self, _provider: &str) -> Result<()> {
        Ok(())
    }
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

fn load_store_if_present(path: Option<&Path>) -> Option<AuthStore> {
    let path = path?;
    if !path.exists() {
        return None;
    }
    match load_auth_store_at(path) {
        Ok(store) => Some(store),
        Err(error) => {
            warn!(path = %path.display(), %error, "failed to load JSON credential store");
            None
        }
    }
}

fn delete_provider_from_path(path: Option<&Path>, provider: &str) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if !path.exists() {
        return Ok(());
    }

    let mut store = load_auth_store_at(path)
        .with_context(|| format!("failed to load auth store {}", path.display()))?;
    if store.providers.remove(provider).is_none() {
        return Ok(());
    }

    if store.providers.is_empty() {
        fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))?;
        return Ok(());
    }

    write_store_to_path(path, &store)
}

fn write_store_to_path(path: &Path, store: &AuthStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let contents = serde_json::to_string_pretty(store).context("failed to serialize auth store")?;
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod {}", path.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn json_round_trip_works_when_native_keyring_is_disabled() {
        let tempdir = tempdir().expect("tempdir");
        let path = tempdir.path().join("auth.json");
        let store =
            CredentialStore::with_config("anie-test", Some(path.clone())).without_native_keyring();

        store.set("openai", "sk-test").expect("set credential");
        assert_eq!(store.get("openai").as_deref(), Some("sk-test"));

        store.delete("openai").expect("delete credential");
        assert_eq!(store.get("openai"), None);
        assert!(!path.exists());
    }

    #[test]
    fn list_providers_reads_from_json_compatibility_store() {
        let tempdir = tempdir().expect("tempdir");
        let path = tempdir.path().join("auth.json");
        let store = CredentialStore::with_config("anie-test", Some(path)).without_native_keyring();

        store.set("openai", "sk-openai").expect("set openai key");
        store
            .set("anthropic", "sk-anthropic")
            .expect("set anthropic key");

        assert_eq!(
            store.list_providers(),
            vec!["anthropic".to_string(), "openai".to_string()]
        );
    }

    #[test]
    fn migration_imports_and_renames_legacy_auth_json() {
        let tempdir = tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        save_api_key_at(&auth_path, "openai", "legacy-openai").expect("save openai");
        save_api_key_at(&auth_path, "anthropic", "legacy-anthropic").expect("save anthropic");

        let native_path = tempdir.path().join("native.json");
        let store = CredentialStore::with_config("anie-test", Some(auth_path.clone()))
            .with_test_native_keyring(native_path.clone());

        assert!(store.should_migrate());
        assert_eq!(store.migrate_from_json().expect("migrate"), 2);
        assert!(!auth_path.exists());
        assert!(tempdir.path().join("auth.json.migrated").exists());
        assert_eq!(store.get("openai").as_deref(), Some("legacy-openai"));
        assert_eq!(store.get("anthropic").as_deref(), Some("legacy-anthropic"));
        assert!(!store.should_migrate());

        let migrated = load_auth_store_at(&tempdir.path().join("auth.json.migrated"))
            .expect("load migrated backup");
        assert_eq!(migrated.providers.len(), 2);
        let native = load_auth_store_at(&native_path).expect("load native test store");
        assert_eq!(native.providers.len(), 2);
    }

    #[test]
    fn migration_is_idempotent() {
        let tempdir = tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        save_api_key_at(&auth_path, "openai", "legacy-openai").expect("save openai");

        let store = CredentialStore::with_config("anie-test", Some(auth_path))
            .with_test_native_keyring(tempdir.path().join("native.json"));

        assert_eq!(store.migrate_from_json().expect("first migrate"), 1);
        assert_eq!(store.migrate_from_json().expect("second migrate"), 0);
    }

    #[cfg(unix)]
    #[test]
    fn json_fallback_permissions_are_restrictive() {
        use std::os::unix::fs::PermissionsExt;

        let tempdir = tempdir().expect("tempdir");
        let path = tempdir.path().join("auth.json");
        let store =
            CredentialStore::with_config("anie-test", Some(path.clone())).without_native_keyring();

        store.set("openai", "sk-test").expect("set credential");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(feature = "keyring-native")]
    #[test]
    #[ignore = "requires a working system keyring on the test machine"]
    fn native_keyring_round_trip_can_be_run_manually() {
        let tempdir = tempdir().expect("tempdir");
        let path = tempdir.path().join("auth.json");
        let store = CredentialStore::with_config("anie-test-manual", Some(path));
        let provider = "manual-keyring-provider";

        store
            .set(provider, "manual-test-key")
            .expect("set credential");
        assert_eq!(store.get(provider).as_deref(), Some("manual-test-key"));
        store.delete(provider).expect("delete credential");
    }
}
