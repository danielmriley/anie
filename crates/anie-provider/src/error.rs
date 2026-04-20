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
    #[error("{}", format_rate_limited(*.retry_after_ms))]
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
    /// tool calls — typically a local tagged-reasoning model
    /// (Qwen, DeepSeek) that emits `<think>...</think>` with
    /// nothing after. Classified as terminal in the retry policy
    /// because replaying the same context reproduces the same
    /// thinking block.
    #[error(
        "model returned no visible content (only reasoning); rephrase, lower the thinking level, or switch models"
    )]
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

    /// The provider stream contained a content-block or event type
    /// this client cannot round-trip. Usually indicates a server-
    /// side feature (server tools, citations, web search) was
    /// enabled somewhere but the client was not built to preserve
    /// the resulting blocks. Not retryable — the same block would
    /// appear on a retry.
    ///
    /// See docs/api_integrity_plans/03b_unsupported_block_rejection.md.
    #[error("Unsupported provider stream feature: {0}")]
    UnsupportedStreamFeature(String),

    /// A 400 whose body indicates that the request carried a message
    /// or content block that's structurally invalid *for replay*
    /// (e.g. a thinking block missing its `signature`). Not
    /// retryable; the session should be restarted. Distinct from a
    /// generic `Http { status: 400, body }` so UI layers can show
    /// an actionable message and logs can filter on variant.
    ///
    /// See docs/api_integrity_plans/04_replay_error_taxonomy.md.
    #[error("Replay fidelity error ({provider_hint}): {detail}")]
    ReplayFidelity {
        provider_hint: &'static str,
        detail: String,
    },

    /// A 400 whose body indicates a feature the request referenced
    /// is not supported by this deployment (model, region, account
    /// tier). Not retryable; distinct from `NativeReasoningUnsupported`,
    /// which has a specific fallback path.
    #[error("Feature not supported by provider: {0}")]
    FeatureUnsupported(String),
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

fn format_rate_limited(retry_after_ms: Option<u64>) -> String {
    match retry_after_ms {
        Some(ms) if ms >= 1_000 => format!("Rate limited (retry after {}s)", ms / 1_000),
        Some(ms) => format!("Rate limited (retry after {ms}ms)"),
        None => "Rate limited (no retry hint from provider)".to_string(),
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
