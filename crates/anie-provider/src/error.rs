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

    /// The provider signalled output-budget exhaustion
    /// (`finish_reason: "length"`, Anthropic `stop_reason:
    /// "max_tokens"`, or equivalent) before the model produced any
    /// visible text or tool call — i.e. the response was truncated
    /// *during reasoning* because the total output token budget was
    /// exhausted. Distinct from
    /// `EmptyAssistantResponse` because the fix is different: the
    /// model didn't run out of ideas, it ran out of room. Common
    /// on OpenRouter when hosted reasoning models emit several
    /// thousand tokens of reasoning before answering and the
    /// configured `max_tokens` is too small.
    #[error(
        "response truncated before a visible answer was produced (max_tokens reached during reasoning); lower the thinking level, raise the model's max_tokens, or switch to a non-reasoning model"
    )]
    ResponseTruncated,

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

    /// The provider rejected the request because the requested
    /// resources (typically Ollama's `num_ctx`) exceed available
    /// memory at model-load time. Carries a halved-and-floored
    /// suggestion the caller can retry with, plus the original
    /// provider body verbatim for surfacing to the user.
    ///
    /// The retry semantics live in the provider impl
    /// (`OllamaChatProvider::stream` does one same-request retry
    /// with `num_ctx_override = Some(suggested_num_ctx)` before
    /// the error reaches the controller). The retry policy
    /// classifies this as terminal so the controller doesn't
    /// double-retry.
    ///
    /// anie-specific (not in pi): pi has no native `/api/chat`
    /// codepath and never sends `num_ctx`, so this failure mode
    /// does not exist in pi's error taxonomy. See
    /// `docs/ollama_load_failure_recovery/README.md`.
    #[error(
        "model load failed: {body} — try a smaller context window (e.g. /context-length {suggested_num_ctx})"
    )]
    ModelLoadResources {
        /// Original provider body, unmodified.
        body: String,
        /// Suggested smaller `num_ctx` (half of the requested
        /// value, floored at 2048).
        suggested_num_ctx: u64,
    },
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
