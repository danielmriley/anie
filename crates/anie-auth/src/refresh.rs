//! Refresh-with-lock broker for OAuth credentials.
//!
//! When multiple agent processes share the same anie config
//! (common: two terminals open at once, or a background
//! compaction + an interactive session), they also share one
//! `auth.json`. If two processes see an expired access token at
//! the same time, both calling `OAuthProvider::refresh` would
//! burn refresh tokens (Anthropic rotates refresh on every
//! refresh — a duplicate call invalidates the earlier rotation
//! and can land one of the processes in a broken state).
//!
//! pi solves this with `proper-lockfile`. We use `fs4`, the
//! same advisory-file-lock crate anie-session already depends
//! on, so there's no new locking primitive in the workspace.
//!
//! Lock path: `<auth_dir>/auth.lock/<provider>.lock`. One lock
//! file per provider so refreshes for different providers don't
//! serialize against each other.
//!
//! Flow, per pi's `core/auth-storage.ts:369`:
//!
//! 1. Read current credential.
//! 2. If not expired, return the cached access_token.
//! 3. Acquire the per-provider lock (blocking).
//! 4. Re-read credential — another process may have just
//!    refreshed. (The double-check is what makes this race-safe.)
//! 5. If still expired, call `OAuthProvider::refresh`, persist
//!    the rotated tokens.
//! 6. Release the lock.
//! 7. Return the (possibly freshly-refreshed) access_token.

use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use fs4::fs_std::FileExt;
use thiserror::Error;
use time::OffsetDateTime;
use tracing::debug;

use crate::{
    AuthCredential,
    oauth::{OAuthCredentialData, OAuthProvider, parse_expires_at},
};

/// Typed failures surfaced from the refresh-with-lock path.
/// Callers route these through the existing retry policy:
/// `LockTimeout` is worth retrying, `NotOAuth` is a
/// configuration error, `RefreshFailed` carries the provider's
/// message unchanged.
#[derive(Debug, Error)]
pub enum RefreshError {
    /// Couldn't acquire the provider lock before the timeout.
    /// Shouldn't happen in normal operation — another process
    /// is stuck, or the lock file's filesystem is borked.
    #[error("timed out acquiring OAuth refresh lock for {provider} after {timeout:?}")]
    LockTimeout { provider: String, timeout: Duration },
    /// The stored credential isn't an OAuth entry; the caller
    /// probably has `type: "api_key"` and expected OAuth.
    #[error("credential for {provider} is not an OAuth entry")]
    NotOAuth { provider: String },
    /// No credential at all. User hasn't logged in yet.
    #[error("no credential stored for {provider}; run `anie login {provider}`")]
    Missing { provider: String },
    /// The provider's refresh endpoint returned an error.
    /// Wraps the anyhow error from OAuthProvider::refresh.
    #[error("OAuth refresh failed for {provider}: {source:#}")]
    RefreshFailed {
        provider: String,
        #[source]
        source: anyhow::Error,
    },
    /// Persisting the refreshed credential to auth.json failed.
    #[error("failed to persist refreshed credential for {provider}: {source:#}")]
    Persist {
        provider: String,
        #[source]
        source: anyhow::Error,
    },
    /// Malformed `expires_at` on the stored credential. Usually
    /// indicates a hand-edited auth.json.
    #[error("malformed expires_at on stored credential for {provider}: {source:#}")]
    Expiry {
        provider: String,
        #[source]
        source: anyhow::Error,
    },
}

/// Additional expiry margin beyond the 5 minutes baked into
/// `compute_expires_at`. A refresh-check margin protects
/// against the narrow window where a request is issued right
/// before the token's recorded expiry but lands on the server
/// after. 30 s is an anie-specific addition; pi uses 0.
///
/// **anie-specific (not in pi):** the extra margin costs an
/// earlier refresh but saves a class of 401 on the next request.
const REFRESH_SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// Default lock acquisition timeout. Keeps a stuck process
/// from wedging a second one forever while still being
/// generous enough for slow network refreshes.
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(15);

/// Callback for reading / writing credentials during refresh.
/// Injected so callers can pass either the real
/// `CredentialStore` or a test fake without this module
/// depending on the store's synchronous signature.
pub trait CredentialPersistence: Send + Sync {
    fn load(&self, provider: &str) -> Option<AuthCredential>;
    fn save(&self, provider: &str, credential: AuthCredential) -> Result<()>;
}

/// Broker that refreshes OAuth tokens under a per-provider
/// advisory lock. One instance per `(provider, auth_file)`
/// pair is enough; it holds no mutable state between calls.
pub struct OAuthRefresher<'a> {
    provider: &'a dyn OAuthProvider,
    persistence: &'a dyn CredentialPersistence,
    lock_dir: PathBuf,
    lock_timeout: Duration,
}

impl<'a> OAuthRefresher<'a> {
    /// Construct a broker. `lock_dir` is where
    /// `<provider>.lock` files live — pass
    /// `<auth_file>.parent()/auth.lock` for the default layout.
    #[must_use]
    pub fn new(
        provider: &'a dyn OAuthProvider,
        persistence: &'a dyn CredentialPersistence,
        lock_dir: PathBuf,
    ) -> Self {
        Self {
            provider,
            persistence,
            lock_dir,
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
        }
    }

    /// Override the lock acquisition timeout. Useful in tests
    /// that want a shorter deadline or in CI where lock
    /// contention should never exceed a small bound.
    #[must_use]
    pub fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
        self
    }

    /// Return a non-expired access token for the provider,
    /// refreshing under lock if the cached one is near expiry.
    ///
    /// Errors distinguish between "no credential", "not an
    /// OAuth credential", "lock timeout", "refresh rejected",
    /// and "persist failed" so callers can surface precise
    /// messages to the user.
    pub async fn resolve_access_token(&self) -> Result<String, RefreshError> {
        let provider_name = self.provider.name();

        // Fast path: read-only credential check. If the token
        // has headroom, skip the lock entirely. pi does this
        // too — the lock is only held during refresh.
        let current = self.load_oauth_data(provider_name)?;
        if !is_near_expiry(&current.expires_at, provider_name)? {
            return Ok(current.access_token);
        }

        // Slow path: acquire the lock, re-check, refresh if
        // still needed, persist.
        debug!(provider = provider_name, "acquiring OAuth refresh lock");
        let lock_file = self.acquire_lock(provider_name)?;

        // Scope the lock so it releases even on early returns.
        let result = self.refresh_under_lock(provider_name).await;

        // fs4 releases on drop; explicit unlock for clarity.
        FileExt::unlock(&lock_file).ok();
        result
    }

    async fn refresh_under_lock(&self, provider_name: &str) -> Result<String, RefreshError> {
        // Double-check: another process may have refreshed while
        // we waited for the lock.
        let current = self.load_oauth_data(provider_name)?;
        if !is_near_expiry(&current.expires_at, provider_name)? {
            debug!(
                provider = provider_name,
                "token refreshed by another process while waiting on lock"
            );
            return Ok(current.access_token);
        }

        let refreshed = self.provider.refresh(&current).await.map_err(|source| {
            RefreshError::RefreshFailed {
                provider: provider_name.to_string(),
                source,
            }
        })?;

        let credential = AuthCredential::OAuth {
            access_token: refreshed.access_token.clone(),
            refresh_token: refreshed.refresh_token,
            expires_at: refreshed.expires_at,
            account: refreshed.account,
        };
        self.persistence
            .save(provider_name, credential)
            .map_err(|source| RefreshError::Persist {
                provider: provider_name.to_string(),
                source,
            })?;

        Ok(refreshed.access_token)
    }

    fn load_oauth_data(&self, provider_name: &str) -> Result<OAuthCredentialData, RefreshError> {
        match self.persistence.load(provider_name) {
            Some(AuthCredential::OAuth {
                access_token,
                refresh_token,
                expires_at,
                account,
            }) => Ok(OAuthCredentialData {
                access_token,
                refresh_token,
                expires_at,
                account,
            }),
            Some(AuthCredential::ApiKey { .. }) => Err(RefreshError::NotOAuth {
                provider: provider_name.to_string(),
            }),
            None => Err(RefreshError::Missing {
                provider: provider_name.to_string(),
            }),
        }
    }

    fn acquire_lock(&self, provider_name: &str) -> Result<File, RefreshError> {
        fs::create_dir_all(&self.lock_dir)
            .with_context(|| format!("failed to create lock dir {}", self.lock_dir.display()))
            .map_err(|source| RefreshError::Persist {
                provider: provider_name.to_string(),
                source,
            })?;
        let path = self.lock_dir.join(format!("{provider_name}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("failed to open lock file {}", path.display()))
            .map_err(|source| RefreshError::Persist {
                provider: provider_name.to_string(),
                source,
            })?;

        let deadline = std::time::Instant::now() + self.lock_timeout;
        loop {
            match FileExt::try_lock_exclusive(&file) {
                Ok(true) => return Ok(file),
                Ok(false) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(RefreshError::LockTimeout {
                            provider: provider_name.to_string(),
                            timeout: self.lock_timeout,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(err) => {
                    return Err(RefreshError::Persist {
                        provider: provider_name.to_string(),
                        source: anyhow!("lock acquisition failed: {err}"),
                    });
                }
            }
        }
    }
}

fn is_near_expiry(expires_at: &str, provider: &str) -> Result<bool, RefreshError> {
    let deadline = parse_expires_at(expires_at).map_err(|source| RefreshError::Expiry {
        provider: provider.to_string(),
        source,
    })?;
    let now = OffsetDateTime::now_utc();
    let margin = time::Duration::seconds(
        i64::try_from(REFRESH_SAFETY_MARGIN.as_secs()).unwrap_or(30),
    );
    Ok(deadline - now <= margin)
}

/// Compute the default lock directory alongside an auth file.
/// `~/.anie/auth.json` → `~/.anie/auth.lock/`.
#[must_use]
pub fn default_lock_dir(auth_file: &Path) -> PathBuf {
    auth_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("auth.lock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tempfile::tempdir;
    use time::format_description::well_known::Rfc3339;

    use crate::oauth::LoginFlow;

    /// Test-only persistence backed by a shared Mutex<HashMap>.
    #[derive(Clone, Default)]
    struct InMemoryPersistence(Arc<Mutex<std::collections::HashMap<String, AuthCredential>>>);

    impl InMemoryPersistence {
        fn with(provider: &str, cred: AuthCredential) -> Self {
            let this = Self::default();
            this.0
                .lock()
                .expect("init lock")
                .insert(provider.to_string(), cred);
            this
        }
    }

    impl CredentialPersistence for InMemoryPersistence {
        fn load(&self, provider: &str) -> Option<AuthCredential> {
            self.0
                .lock()
                .expect("load lock")
                .get(provider)
                .cloned()
        }

        fn save(&self, provider: &str, credential: AuthCredential) -> Result<()> {
            self.0
                .lock()
                .expect("save lock")
                .insert(provider.to_string(), credential);
            Ok(())
        }
    }

    /// Counting OAuth provider fake. Every `refresh` call
    /// increments `refresh_calls` so tests can assert on it.
    struct CountingProvider {
        refresh_calls: Arc<AtomicUsize>,
        next_token: String,
        new_expires_at: String,
    }

    #[async_trait]
    impl OAuthProvider for CountingProvider {
        fn name(&self) -> &'static str {
            "anthropic"
        }

        async fn begin_login(&self) -> Result<LoginFlow> {
            unreachable!("begin_login not exercised in refresh tests")
        }

        async fn complete_login(
            &self,
            _flow: &LoginFlow,
            _code: &str,
        ) -> Result<OAuthCredentialData> {
            unreachable!("complete_login not exercised in refresh tests")
        }

        async fn refresh(&self, _credential: &OAuthCredentialData) -> Result<OAuthCredentialData> {
            self.refresh_calls.fetch_add(1, Ordering::SeqCst);
            Ok(OAuthCredentialData {
                access_token: self.next_token.clone(),
                refresh_token: "rotated-refresh".into(),
                expires_at: self.new_expires_at.clone(),
                account: Some("user@example.com".into()),
            })
        }
    }

    fn rfc3339_in(offset_secs: i64) -> String {
        let instant = OffsetDateTime::now_utc() + time::Duration::seconds(offset_secs);
        instant.format(&Rfc3339).expect("format")
    }

    fn oauth(access: &str, expires_at: &str) -> AuthCredential {
        AuthCredential::OAuth {
            access_token: access.into(),
            refresh_token: "old-refresh".into(),
            expires_at: expires_at.into(),
            account: None,
        }
    }

    #[tokio::test]
    async fn valid_token_returns_without_calling_refresh() {
        let persistence = InMemoryPersistence::with(
            "anthropic",
            oauth("still-good", &rfc3339_in(3_600)),
        );
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let provider = CountingProvider {
            refresh_calls: refresh_calls.clone(),
            next_token: "should-not-issue".into(),
            new_expires_at: rfc3339_in(3_600),
        };
        let tempdir = tempdir().expect("tempdir");
        let refresher = OAuthRefresher::new(&provider, &persistence, tempdir.path().to_path_buf());

        let token = refresher.resolve_access_token().await.expect("resolve");
        assert_eq!(token, "still-good");
        assert_eq!(refresh_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn expired_token_triggers_refresh_and_persists_result() {
        let persistence = InMemoryPersistence::with(
            "anthropic",
            oauth("expired", &rfc3339_in(-60)),
        );
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let provider = CountingProvider {
            refresh_calls: refresh_calls.clone(),
            next_token: "fresh-token".into(),
            new_expires_at: rfc3339_in(3_600),
        };
        let tempdir = tempdir().expect("tempdir");
        let refresher = OAuthRefresher::new(&provider, &persistence, tempdir.path().to_path_buf());

        let token = refresher.resolve_access_token().await.expect("resolve");
        assert_eq!(token, "fresh-token");
        assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);

        // Persisted credential reflects the refresh.
        let persisted = persistence.load("anthropic").expect("persisted");
        let AuthCredential::OAuth {
            access_token,
            refresh_token,
            ..
        } = persisted
        else {
            panic!("persisted credential is not OAuth");
        };
        assert_eq!(access_token, "fresh-token");
        assert_eq!(refresh_token, "rotated-refresh");
    }

    #[tokio::test]
    async fn token_within_30s_margin_is_refreshed() {
        // 20 s until expiry — inside the 30 s anie-specific
        // refresh margin, so we refresh even though the token
        // technically still works.
        let persistence = InMemoryPersistence::with(
            "anthropic",
            oauth("almost-expired", &rfc3339_in(20)),
        );
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let provider = CountingProvider {
            refresh_calls: refresh_calls.clone(),
            next_token: "new-token".into(),
            new_expires_at: rfc3339_in(3_600),
        };
        let tempdir = tempdir().expect("tempdir");
        let refresher = OAuthRefresher::new(&provider, &persistence, tempdir.path().to_path_buf());
        refresher.resolve_access_token().await.expect("resolve");
        assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_refresh_attempts_coalesce_via_lock() {
        // Two concurrent calls against one expired credential
        // must only produce ONE refresh call — the second sees
        // a freshly-refreshed credential after the lock frees.
        let persistence = InMemoryPersistence::with(
            "anthropic",
            oauth("expired", &rfc3339_in(-60)),
        );
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let provider = CountingProvider {
            refresh_calls: refresh_calls.clone(),
            next_token: "fresh-token".into(),
            new_expires_at: rfc3339_in(3_600),
        };
        let tempdir = tempdir().expect("tempdir");
        let lock_dir = tempdir.path().to_path_buf();
        let refresher_a = OAuthRefresher::new(&provider, &persistence, lock_dir.clone());
        let refresher_b = OAuthRefresher::new(&provider, &persistence, lock_dir);

        // Launch concurrently. Both will see the expired token
        // on the fast-path read; one will acquire the lock,
        // refresh, and persist. The other's double-check inside
        // `refresh_under_lock` will see the freshly-refreshed
        // credential and skip the refresh call.
        let (token_a, token_b) = tokio::join!(
            refresher_a.resolve_access_token(),
            refresher_b.resolve_access_token(),
        );

        assert_eq!(token_a.expect("a"), "fresh-token");
        assert_eq!(token_b.expect("b"), "fresh-token");
        assert_eq!(
            refresh_calls.load(Ordering::SeqCst),
            1,
            "lock must coalesce concurrent refreshes to a single call"
        );
    }

    #[tokio::test]
    async fn missing_credential_surfaces_typed_error() {
        let persistence = InMemoryPersistence::default();
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let provider = CountingProvider {
            refresh_calls,
            next_token: String::new(),
            new_expires_at: rfc3339_in(3_600),
        };
        let tempdir = tempdir().expect("tempdir");
        let refresher = OAuthRefresher::new(&provider, &persistence, tempdir.path().to_path_buf());
        let err = refresher.resolve_access_token().await.unwrap_err();
        assert!(matches!(err, RefreshError::Missing { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn api_key_credential_surfaces_typed_error() {
        let persistence = InMemoryPersistence::with(
            "anthropic",
            AuthCredential::ApiKey {
                key: "sk-test".into(),
            },
        );
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let provider = CountingProvider {
            refresh_calls,
            next_token: String::new(),
            new_expires_at: rfc3339_in(3_600),
        };
        let tempdir = tempdir().expect("tempdir");
        let refresher = OAuthRefresher::new(&provider, &persistence, tempdir.path().to_path_buf());
        let err = refresher.resolve_access_token().await.unwrap_err();
        assert!(matches!(err, RefreshError::NotOAuth { .. }), "{err:?}");
    }

    #[test]
    fn default_lock_dir_places_subdir_next_to_auth_file() {
        let auth_file = Path::new("/home/x/.anie/auth.json");
        let lock_dir = default_lock_dir(auth_file);
        assert_eq!(lock_dir, Path::new("/home/x/.anie/auth.lock"));
    }
}
