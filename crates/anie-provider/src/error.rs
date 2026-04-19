/// Structured provider failures propagated through the core architecture.
///
/// The taxonomy is designed so callers can make retry / recovery
/// decisions via exhaustive `match`, never by inspecting error
/// messages. When a new failure mode is genuinely distinct from the
/// existing variants, add a variant rather than widening an existing
/// one.
///
/// Retry *decisions* live in `anie-cli`'s `RetryPolicy::decide`.
/// This type only carries descriptive error data plus trivial field
/// accessors such as `retry_after_ms()`.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ProviderError {
    // ---------------------------------------------------------------
    // HTTP / network boundary
    // ---------------------------------------------------------------
    /// Non-success HTTP status returned by a provider.
    #[error("HTTP error: {status} {body}")]
    Http { status: u16, body: String },

    /// Authentication or authorization failure (401 / 403 / missing
    /// credentials).
    #[error("Authentication failed: {0}")]
    Auth(String),

    /// Rate-limited response. `retry_after_ms` carries the server's
    /// `Retry-After` hint when present.
    #[error("Rate limited (retry after {retry_after_ms:?}ms)")]
    RateLimited { retry_after_ms: Option<u64> },

    /// Context-window overflow. Triggers the compaction-retry path
    /// rather than the transient-retry path.
    #[error("Context overflow: {0}")]
    ContextOverflow(String),

    // ---------------------------------------------------------------
    // Request-side failures (before and during send)
    // ---------------------------------------------------------------
    /// Building the request body failed locally (serialization,
    /// missing required data). Not retryable — a retry would fail
    /// the same way.
    #[error("Request build error: {0}")]
    RequestBuild(String),

    /// Transport-level failure talking to the provider (DNS, TLS,
    /// connect timeout, network I/O). Retryable.
    #[error("Transport error: {0}")]
    Transport(String),

    // ---------------------------------------------------------------
    // Streaming failures (post-connect, parsing the SSE stream)
    // ---------------------------------------------------------------
    /// The stream completed with no visible assistant text and no
    /// tool calls — only hidden reasoning or nothing. The existing
    /// retry loop treats this as transient (the model sometimes
    /// produces reasoning-only output on the first shot).
    #[error("empty assistant response")]
    EmptyAssistantResponse,

    /// An SSE frame could not be parsed as JSON. Usually a transient
    /// upstream issue; retryable.
    #[error("invalid stream JSON: {0}")]
    InvalidStreamJson(String),

    /// An SSE frame parsed but had the wrong shape (missing required
    /// field, unexpected event type we must honor).
    #[error("malformed stream event: {0}")]
    MalformedStreamEvent(String),

    /// A tool-call's `arguments` JSON was malformed when the stream
    /// finished. Not retryable — the model produced bad output,
    /// re-running is unlikely to fix it.
    #[error("tool call arguments not valid JSON: {0}")]
    ToolCallMalformed(String),

    /// The provider rejected our native-reasoning request fields
    /// (`reasoning_effort` / `reasoning.effort`). Caller should
    /// retry with `NoNativeFields` strategy; not a user-facing error
    /// unless the fallback also fails.
    #[error("native reasoning not supported by target: {0}")]
    NativeReasoningUnsupported(String),
}

impl ProviderError {
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
    /// anyhow errors surface from construction-time failures (auth
    /// resolution, credential store, etc.). Map them to
    /// `RequestBuild` — they happen before any transport is touched.
    fn from(value: anyhow::Error) -> Self {
        Self::RequestBuild(value.to_string())
    }
}
