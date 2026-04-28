//! Typed errors for the web tools.
//!
//! Mirrors the pattern used by `anie_provider::ProviderError`:
//! every distinct failure mode is a variant, callers
//! pattern-match on the variant rather than substring-matching
//! the `Display`. The `Display` text is what surfaces to the
//! agent as a tool result, so each message is short and
//! actionable.

use thiserror::Error;

/// Errors produced by the web tools (`web_read`, `web_search`).
#[derive(Debug, Error)]
pub enum WebToolError {
    /// User passed a malformed URL.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    /// URL scheme is not http or https.
    #[error("unsupported URL scheme: {0}")]
    UnsupportedScheme(String),

    /// URL resolves to a private / loopback / link-local address
    /// and `allow_private_ips` is false. SSRF defense.
    #[error("URL resolves to a private address: {0}")]
    PrivateAddress(String),

    /// robots.txt disallows access.
    #[error("robots.txt forbids access to {0}")]
    Forbidden(String),

    /// Network / fetch failure.
    #[error("fetch failed: {0}")]
    Fetch(String),

    /// Server returned a non-success status code.
    #[error("HTTP {code}: {body_excerpt}")]
    HttpStatus {
        /// The HTTP status code.
        code: u16,
        /// Up to a few hundred bytes of the response body, for
        /// the agent to use when deciding whether to retry.
        body_excerpt: String,
    },

    /// Page exceeded the configured size cap.
    #[error("page size {bytes} exceeds max {max} bytes")]
    TooLarge {
        /// Bytes received before the cap was hit.
        bytes: usize,
        /// Configured ceiling.
        max: usize,
    },

    /// Operation exceeded the configured timeout.
    #[error("timed out after {seconds}s")]
    Timeout {
        /// Seconds before timeout fired.
        seconds: u64,
    },

    /// Operation was cancelled by the caller (Ctrl+C, agent
    /// abort, etc.). Mapped to [`anie_agent::ToolError::Aborted`]
    /// at the tool boundary so the agent loop sees an abort
    /// rather than a generic execution failure.
    #[error("aborted")]
    Aborted,

    /// Headless Chrome render failed (only when `javascript=true`).
    #[error("headless render failed: {0}")]
    HeadlessFailure(String),

    /// Neither `defuddle` nor `npx` were available on PATH.
    #[error(
        "defuddle is not installed. Install Node.js + run `npm i -g defuddle-cli`, or ensure `npx` is on PATH so it can be fetched on demand."
    )]
    DefuddleNotFound,

    /// Spawning the defuddle subprocess failed.
    #[error("failed to spawn defuddle: {0}")]
    DefuddleSpawn(String),

    /// Defuddle exited non-zero.
    #[error("defuddle exited non-zero ({exit_code:?}): {stderr}")]
    DefuddleFailed {
        /// Process exit code, if any.
        exit_code: Option<i32>,
        /// Captured stderr for diagnostics.
        stderr: String,
    },

    /// Defuddle's JSON output failed to parse.
    #[error("failed to parse defuddle output: {0}")]
    DefuddleOutputParse(#[from] serde_json::Error),

    /// IO error (subprocess plumbing, body streaming, etc).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Search backend (DuckDuckGo, etc.) failed.
    #[error("search backend failed: {0}")]
    SearchBackend(String),

    /// Server returned a non-HTML content type. Defuddle's
    /// parser crashes on plain-text or JSON bodies, so we
    /// reject these up front with a clear error rather than
    /// passing through a confusing stack trace.
    #[error(
        "unsupported response content-type: {0}. web_read expects HTML; try a web-page URL or a different endpoint."
    )]
    UnsupportedContentType(String),
}

impl WebToolError {
    /// Truncate a body excerpt to a stable upper bound for the
    /// `HttpStatus` variant. Avoids spilling megabytes of
    /// captured 500 pages into the tool result.
    pub fn truncate_excerpt(text: &str) -> String {
        const MAX: usize = 256;
        if text.len() <= MAX {
            text.to_string()
        } else {
            let mut out = text.chars().take(MAX).collect::<String>();
            out.push('…');
            out
        }
    }
}
