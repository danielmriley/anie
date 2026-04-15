use std::time::Duration;

/// Create the shared HTTP client used by built-in providers.
pub fn create_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .connect_timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("failed to create HTTP client")
}
