//! `WebSearchTool` — the `Tool` impl exposed to the agent.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use anie_agent::{Tool, ToolError};
use anie_protocol::{ContentBlock, ToolDef, ToolResult};

use crate::error::WebToolError;
use crate::read::fetch::{
    self, DEFAULT_RATE_LIMIT_BURST, DEFAULT_RATE_LIMIT_RPS, FetchOptions, HostRateLimiter,
    Resolver, system_resolver,
};
use crate::search::ddg;

const HARD_MAX_RESULTS: u32 = 25;
const DEFAULT_MAX_RESULTS: u32 = 10;

/// Choice of search backend. Currently only DDG is shipped;
/// Brave and SearXNG are planned Phase 2 additions.
#[derive(Debug, Clone, Copy, Default)]
pub enum SearchBackend {
    /// DuckDuckGo HTML scrape (default).
    #[default]
    DuckDuckGo,
}

/// `web_search` tool implementation.
pub struct WebSearchTool {
    client: reqwest::Client,
    fetch_opts: FetchOptions,
    rate_limiter: Arc<HostRateLimiter>,
    resolver: Arc<dyn Resolver>,
    backend: SearchBackend,
}

impl WebSearchTool {
    /// Build a `WebSearchTool` with default options and the
    /// DuckDuckGo HTML backend.
    pub fn new() -> Result<Self, WebToolError> {
        let opts = FetchOptions::default();
        let client = fetch::build_client(&opts)?;
        Ok(Self {
            client,
            fetch_opts: opts,
            rate_limiter: Arc::new(HostRateLimiter::new(
                DEFAULT_RATE_LIMIT_RPS,
                DEFAULT_RATE_LIMIT_BURST,
            )),
            resolver: system_resolver(),
            backend: SearchBackend::default(),
        })
    }

    /// Build with a shared rate limiter (so `web_read` and
    /// `web_search` share per-host bucket state).
    pub fn with_rate_limiter(rate_limiter: Arc<HostRateLimiter>) -> Result<Self, WebToolError> {
        let opts = FetchOptions::default();
        let client = fetch::build_client(&opts)?;
        Ok(Self {
            client,
            fetch_opts: opts,
            rate_limiter,
            resolver: system_resolver(),
            backend: SearchBackend::default(),
        })
    }

    /// Replace the DNS resolver used by the SSRF guard. See
    /// [`crate::WebReadTool::set_resolver`] for the rationale.
    pub fn set_resolver(&mut self, resolver: Arc<dyn Resolver>) {
        self.resolver = resolver;
    }

    async fn run(
        &self,
        args: &WebSearchArgs,
        cancel: &CancellationToken,
    ) -> Result<String, WebToolError> {
        if cancel.is_cancelled() {
            return Err(WebToolError::Aborted);
        }
        if args.query.trim().is_empty() {
            return Err(WebToolError::SearchBackend("query is empty".into()));
        }
        let max = args
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, HARD_MAX_RESULTS);

        info!(query = %args.query, max, "web_search start");

        // Per-host rate limit on the backend host. DDG gets
        // its own bucket because it's a separate hostname
        // from any article hosts the agent might read.
        tokio::select! {
            _ = cancel.cancelled() => return Err(WebToolError::Aborted),
            _ = self.rate_limiter.acquire("duckduckgo.com") => {}
        }

        let hits = match self.backend {
            SearchBackend::DuckDuckGo => {
                ddg::search(
                    &self.client,
                    self.resolver.as_ref(),
                    cancel,
                    &self.fetch_opts,
                    &args.query,
                    max as usize,
                )
                .await?
            }
        };

        debug!(returned = hits.len(), "web_search results");
        Ok(ddg::format_results(&args.query, &hits))
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "web_search".into(),
            description: "Search the live web and return ranked URLs with titles and snippets. Use this for any question that needs current, real-world information — weather, news, current events, library/package docs, pricing, definitions, public facts — not just for coding research. Pair with web_read to fetch the actual content of a hit. Returns up to max_results items (default 10, hard cap 25).".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query. Quoted phrases and operators where the backend supports them."
                    },
                    "max_results": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": HARD_MAX_RESULTS,
                        "default": DEFAULT_MAX_RESULTS,
                        "description": "Maximum number of results (1 to 25)."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
        _update_tx: Option<mpsc::Sender<ToolResult>>,
    ) -> Result<ToolResult, ToolError> {
        let parsed: WebSearchArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::ExecutionFailed(format!("invalid web_search args: {e}")))?;
        let body = self.run(&parsed, &cancel).await.map_err(|e| match e {
            WebToolError::Aborted => ToolError::Aborted,
            other => ToolError::ExecutionFailed(other.to_string()),
        })?;
        Ok(ToolResult {
            content: vec![ContentBlock::Text { text: body }],
            details: serde_json::json!({
                "tool": "web_search",
                "query": parsed.query,
                "max_results": parsed.max_results.unwrap_or(DEFAULT_MAX_RESULTS),
            }),
        })
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WebSearchArgs {
    pub query: String,
    #[serde(default)]
    pub max_results: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_search_definition_has_expected_name_and_required_query() {
        let tool = WebSearchTool::new().expect("build tool");
        let def = tool.definition();
        assert_eq!(def.name, "web_search");
        assert!(def.description.contains("ranked URLs"));
        let required = def
            .parameters
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array");
        assert!(required.iter().any(|v| v.as_str() == Some("query")));
    }

    #[test]
    fn web_search_definition_caps_max_results_at_25() {
        let tool = WebSearchTool::new().expect("build tool");
        let def = tool.definition();
        let max = def
            .parameters
            .get("properties")
            .and_then(|p| p.get("max_results"))
            .and_then(|m| m.get("maximum"))
            .and_then(|m| m.as_u64())
            .expect("maximum field");
        assert_eq!(max, 25);
    }

    #[tokio::test]
    async fn web_search_rejects_empty_query() {
        let tool = WebSearchTool::new().expect("build tool");
        let err = tool
            .run(
                &WebSearchArgs {
                    query: "   ".into(),
                    max_results: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WebToolError::SearchBackend(_)));
    }

    /// PR 4.1 of `docs/code_review_2026-04-27/`. A token that
    /// is already cancelled must short-circuit before the
    /// rate-limit acquire and before the DDG fetch goes out.
    #[tokio::test]
    async fn web_search_honors_cancellation_before_fetch() {
        let tool = WebSearchTool::new().expect("build tool");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = tool
            .run(
                &WebSearchArgs {
                    query: "anything".into(),
                    max_results: None,
                },
                &cancel,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WebToolError::Aborted), "got: {err:?}");
    }
}
