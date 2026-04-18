use std::sync::OnceLock;
use std::time::Duration;

use anie_provider::ProviderError;

/// Shared HTTP client for built-in providers.
///
/// Initialized lazily. Failure to initialize (TLS roots missing,
/// etc.) is cached: every caller sees `ProviderError::Transport`
/// until the process restarts. Avoids a crash on provider
/// construction when TLS roots are unavailable (e.g. locked-down
/// CI environments).
static CLIENT: OnceLock<Result<reqwest::Client, ClientInitError>> = OnceLock::new();

#[derive(Debug, Clone)]
struct ClientInitError {
    message: String,
}

/// Return the shared HTTP client.
///
/// Returns `Err(ProviderError::Transport)` if the client failed to
/// build at first use; the same error is returned for every
/// subsequent call. Classified as Transport because the failure is
/// always at the TLS / network-plumbing layer.
pub fn shared_http_client() -> Result<&'static reqwest::Client, ProviderError> {
    match CLIENT.get_or_init(build_client) {
        Ok(client) => Ok(client),
        Err(error) => Err(ProviderError::Transport(format!(
            "failed to initialize HTTP client: {}",
            error.message
        ))),
    }
}

/// Build the shared HTTP client. `Err(ClientInitError)` is cached
/// inside `OnceLock` if construction fails.
fn build_client() -> Result<reqwest::Client, ClientInitError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .connect_timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .map_err(|error| ClientInitError {
            message: error.to_string(),
        })
}

/// Create a new HTTP client with the standard built-in config.
///
/// Kept for call sites that need a client with a different timeout
/// (e.g. `detect_local_servers` uses a 1-second connect timeout).
/// Prefer `shared_http_client()` for the hot provider path.
///
/// Panics if TLS roots cannot be loaded. Only used at startup by
/// cold-path callers.
#[allow(clippy::expect_used)]
pub fn create_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .connect_timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("failed to create HTTP client")
}
