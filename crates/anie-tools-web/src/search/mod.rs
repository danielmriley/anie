//! `web_search` — discover URLs to read.
//!
//! Composes naturally with [`crate::read::WebReadTool`]: the
//! agent calls `web_search("topic")` to find ranked URLs +
//! snippets, then `web_read(url)` on the ones worth reading.

pub mod ddg;
mod tool;

pub use tool::{SearchBackend, WebSearchTool};

/// A single hit from a search backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Title of the result page.
    pub title: String,
    /// Canonical URL.
    pub url: url::Url,
    /// Snippet / excerpt around the matching content.
    pub snippet: String,
    /// Hostname (e.g. `example.com`).
    pub site: String,
}
