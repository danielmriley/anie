/// Structured provider failures propagated through the core architecture.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ProviderError {
    /// Non-success HTTP status returned by a provider.
    #[error("HTTP error: {status} {body}")]
    Http { status: u16, body: String },
    /// Authentication or authorization failure.
    #[error("Authentication failed: {0}")]
    Auth(String),
    /// Request construction or dispatch failure.
    #[error("Request building error: {0}")]
    Request(String),
    /// Mid-stream transport or protocol failure.
    #[error("Stream error: {0}")]
    Stream(String),
    /// Rate-limited response.
    #[error("Rate limited (retry after {retry_after_ms:?}ms)")]
    RateLimited { retry_after_ms: Option<u64> },
    /// Context-window overflow.
    #[error("Context overflow: {0}")]
    ContextOverflow(String),
    /// Miscellaneous provider failure.
    #[error("{0}")]
    Other(String),
}

impl ProviderError {
    /// Whether this provider error should be retried automatically.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } => true,
            Self::Http { status, .. } => matches!(status, 429 | 500 | 502 | 503 | 529),
            Self::Stream(_) => true,
            Self::ContextOverflow(_) => false,
            Self::Auth(_) | Self::Request(_) | Self::Other(_) => false,
        }
    }

    /// Suggested retry-after delay in milliseconds, when available.
    #[must_use]
    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            Self::RateLimited { retry_after_ms } => *retry_after_ms,
            _ => None,
        }
    }
}

impl From<anyhow::Error> for ProviderError {
    fn from(value: anyhow::Error) -> Self {
        Self::Other(value.to_string())
    }
}
