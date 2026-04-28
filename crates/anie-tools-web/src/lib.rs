//! Web reading and search tools for the anie agent.
//!
//! Two tools live here, both registered through the existing
//! `anie_agent::ToolRegistry`:
//!
//! - `web_read` — fetch a URL and return clean Markdown via
//!   the [Defuddle](https://github.com/kepano/defuddle)
//!   reader-mode extractor.
//! - `web_search` — query a search backend (DuckDuckGo HTML
//!   by default) and return ranked URLs + snippets.
//!
//! See `docs/web_tool_2026-04-26/` in the anie repository for
//! the design rationale.
//!
//! ## Prerequisites
//!
//! `web_read` shells out to a `defuddle` (or `npx defuddle`)
//! subprocess at runtime — same pattern as the bash tool
//! requiring `/bin/sh`. Install with `npm i -g defuddle-cli`
//! to enable the tool. Without it, the tool registers but
//! returns a clear `DefuddleNotFound` error on use.
//!
//! `web_read` with `javascript: true` additionally requires a
//! Chrome / Chromium binary on the system. Build the crate
//! with `--features headless` to enable that path; the binary
//! is located via `CHROME_PATH`, then `which`, then the macOS
//! standard install path. Without the feature flag, requests
//! with `javascript: true` are rejected up front with a clear
//! error message.

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

use std::sync::Arc;

use anie_agent::Tool;

pub mod error;
pub mod read;
pub mod search;

pub use error::WebToolError;
pub use read::WebReadTool;
pub use search::WebSearchTool;

/// Build the default set of web tools registered with anie.
/// Returns the list of `Arc<dyn Tool>` ready for
/// [`anie_agent::ToolRegistry::register`].
///
/// Exposes:
/// - `web_read`
/// - `web_search`
///
/// Both tools share a per-host rate limiter so a search-then-read
/// chain doesn't double-spend the budget for a single host.
pub fn web_tools() -> Result<Vec<Arc<dyn Tool>>, WebToolError> {
    web_tools_with_options(read::fetch::FetchOptions::default())
}

/// Build the web tools using a caller-supplied
/// [`read::fetch::FetchOptions`]. The bootstrap converts
/// `[tools.web]` config into `FetchOptions` and passes it
/// here, so operator-tunable timeouts / byte caps / private-IP
/// policy take effect at startup. PR 4.3 of
/// `docs/code_review_2026-04-27/`.
pub fn web_tools_with_options(
    opts: read::fetch::FetchOptions,
) -> Result<Vec<Arc<dyn Tool>>, WebToolError> {
    use read::fetch::{DEFAULT_RATE_LIMIT_BURST, DEFAULT_RATE_LIMIT_RPS, HostRateLimiter};

    let limiter = Arc::new(HostRateLimiter::new(
        DEFAULT_RATE_LIMIT_RPS,
        DEFAULT_RATE_LIMIT_BURST,
    ));
    Ok(vec![
        Arc::new(WebReadTool::with_options(opts.clone(), limiter.clone())?),
        Arc::new(WebSearchTool::with_options(opts, limiter)?),
    ])
}
