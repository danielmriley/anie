//! `WebReadTool` — the `Tool` impl that the agent calls.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use anie_agent::{Tool, ToolError};
use anie_protocol::{ContentBlock, ToolDef, ToolResult};

use crate::error::WebToolError;
use crate::read::extract::{DefuddleRunner, SubprocessDefuddleRunner};
use crate::read::fetch::{
    self, DEFAULT_RATE_LIMIT_BURST, DEFAULT_RATE_LIMIT_RPS, FetchOptions, HostRateLimiter,
    RobotsCache,
};
use crate::read::frontmatter;

/// `web_read` tool implementation.
///
/// Fetches a URL via the shared `reqwest::Client`, runs
/// Defuddle on the body, prepends YAML frontmatter, and
/// returns the result as a single `ToolResult` text block.
pub struct WebReadTool {
    client: reqwest::Client,
    fetch_opts: FetchOptions,
    robots: RobotsCache,
    rate_limiter: HostRateLimiter,
    runner: Arc<dyn DefuddleRunner>,
    respect_robots_txt: bool,
}

impl WebReadTool {
    /// Build a `WebReadTool` with default options and the
    /// production Defuddle runner.
    pub fn new() -> Result<Self, WebToolError> {
        let opts = FetchOptions::default();
        let client = fetch::build_client(&opts)?;
        Ok(Self {
            client,
            fetch_opts: opts,
            robots: RobotsCache::new(),
            rate_limiter: HostRateLimiter::new(
                DEFAULT_RATE_LIMIT_RPS,
                DEFAULT_RATE_LIMIT_BURST,
            ),
            runner: Arc::new(SubprocessDefuddleRunner),
            respect_robots_txt: true,
        })
    }

    /// Build a `WebReadTool` with explicit fetch options and a
    /// pluggable Defuddle runner. Tests use this to inject a
    /// canned-output runner.
    pub fn with_runner(
        opts: FetchOptions,
        runner: Arc<dyn DefuddleRunner>,
        respect_robots_txt: bool,
    ) -> Result<Self, WebToolError> {
        let client = fetch::build_client(&opts)?;
        Ok(Self {
            client,
            fetch_opts: opts,
            robots: RobotsCache::new(),
            rate_limiter: HostRateLimiter::new(
                DEFAULT_RATE_LIMIT_RPS,
                DEFAULT_RATE_LIMIT_BURST,
            ),
            runner,
            respect_robots_txt,
        })
    }

    async fn run(&self, args: &WebReadArgs) -> Result<String, WebToolError> {
        let url = fetch::validate_url(&args.url, self.fetch_opts.allow_private_ips)?;
        info!(target = %url, javascript = args.javascript, "web_read start");

        if self.respect_robots_txt {
            self.robots
                .check(&url, &self.fetch_opts.user_agent, &self.client)
                .await?;
            debug!(host = url.host_str().unwrap_or(""), "robots ok");
        }
        if let Some(host) = url.host_str() {
            self.rate_limiter.acquire(host).await;
        }

        let html = if args.javascript {
            return Err(WebToolError::HeadlessFailure(
                "javascript=true requires building anie-tools-web with --features headless"
                    .into(),
            ));
        } else {
            fetch::fetch_html(&self.client, &url, &self.fetch_opts).await?
        };
        debug!(bytes = html.len(), "fetched html");

        let extracted = self.runner.run(&html, url.as_str()).await?;
        debug!(
            title = extracted.title.as_deref().unwrap_or(""),
            words = ?extracted.word_count,
            "defuddle extracted"
        );

        let yaml = frontmatter::build(&extracted, url.as_str());
        Ok(format!("{yaml}\n{}", extracted.markdown_body()))
    }
}

#[async_trait]
impl Tool for WebReadTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "web_read".into(),
            description: "Fetch a URL and return its main content as clean Markdown with YAML frontmatter metadata (title, author, date, source, etc.). Use this for reading articles, documentation, blog posts, and similar content. Pass javascript=true for SPA / heavily JS-rendered pages — slower, requires Chrome/Chromium installed and the crate built with --features headless.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "format": "uri",
                        "description": "The URL to fetch and read."
                    },
                    "javascript": {
                        "type": "boolean",
                        "default": false,
                        "description": "Render JavaScript before extracting. Requires Chrome/Chromium installed and the crate built with --features headless. Most pages don't need this; try without first."
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        args: serde_json::Value,
        _cancel: CancellationToken,
        _update_tx: Option<mpsc::Sender<ToolResult>>,
    ) -> Result<ToolResult, ToolError> {
        let parsed: WebReadArgs = serde_json::from_value(args).map_err(|e| {
            ToolError::ExecutionFailed(format!("invalid web_read args: {e}"))
        })?;
        let body = self
            .run(&parsed)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        Ok(ToolResult {
            content: vec![ContentBlock::Text { text: body }],
            details: serde_json::json!({
                "tool": "web_read",
                "url": parsed.url,
            }),
        })
    }
}

/// Parsed shape of the `web_read` arguments.
///
/// Wider than the JSON schema (we accept missing `javascript`,
/// defaulting to `false`); the JSON Schema in `definition()`
/// is the contract the agent sees.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WebReadArgs {
    pub url: String,
    #[serde(default)]
    pub javascript: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::extract::DefuddleOutput;
    use httpmock::Method::GET;
    use httpmock::MockServer;

    /// Test runner that returns a canned `DefuddleOutput`
    /// without spawning a subprocess. Lets us exercise the
    /// full pipeline (fetch + frontmatter) in CI without
    /// requiring Node + defuddle on the test runner.
    struct StubRunner {
        output: DefuddleOutput,
    }

    #[async_trait]
    impl DefuddleRunner for StubRunner {
        async fn run(
            &self,
            _html: &str,
            _source_url: &str,
        ) -> Result<DefuddleOutput, WebToolError> {
            Ok(self.output.clone())
        }
    }

    fn fixed_output() -> DefuddleOutput {
        DefuddleOutput {
            title: Some("Test Article".into()),
            author: Some("Jane Doe".into()),
            published: Some("2024-08-15T10:30:00Z".into()),
            description: Some("A short test description.".into()),
            domain: Some("example.com".into()),
            language: Some("en".into()),
            word_count: Some(42),
            content_markdown: Some("# Test Article\n\nBody of the article.".into()),
            ..DefuddleOutput::default()
        }
    }

    fn opts(allow_private: bool) -> FetchOptions {
        FetchOptions {
            allow_private_ips: allow_private,
            ..FetchOptions::default()
        }
    }

    #[test]
    fn web_read_definition_has_expected_name_and_required_url() {
        let tool = WebReadTool::new().expect("build tool");
        let def = tool.definition();
        assert_eq!(def.name, "web_read");
        assert!(def.description.contains("Markdown"));
        let required = def
            .parameters
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array");
        assert!(required.iter().any(|v| v.as_str() == Some("url")));
    }

    #[tokio::test]
    async fn web_read_executes_against_fixture_html() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/article");
                then.status(200)
                    .header("content-type", "text/html; charset=utf-8")
                    .body("<html><body><h1>Test Article</h1><p>Body.</p></body></html>");
            })
            .await;

        let tool = WebReadTool::with_runner(
            opts(true),
            Arc::new(StubRunner {
                output: fixed_output(),
            }),
            false, // skip robots check for the mock server
        )
        .expect("build tool");

        let url = format!("{}/article", server.base_url());
        let body = tool
            .run(&WebReadArgs {
                url,
                javascript: false,
            })
            .await
            .expect("run ok");

        assert!(body.starts_with("---\n"));
        assert!(body.contains("title: \"Test Article\""));
        assert!(body.contains("author: \"Jane Doe\""));
        assert!(body.contains("source: "));
        assert!(body.contains("# Test Article"));
        assert!(body.contains("Body of the article."));
    }

    #[tokio::test]
    async fn web_read_surfaces_too_large_as_typed_error() {
        let server = MockServer::start_async().await;
        let big = "x".repeat(50 * 1024);
        server
            .mock_async(|when, then| {
                when.method(GET).path("/big");
                then.status(200).body(big);
            })
            .await;

        let tool = WebReadTool::with_runner(
            FetchOptions {
                allow_private_ips: true,
                max_bytes: 1024,
                ..FetchOptions::default()
            },
            Arc::new(StubRunner {
                output: fixed_output(),
            }),
            false,
        )
        .expect("build tool");

        let url = format!("{}/big", server.base_url());
        let err = tool
            .run(&WebReadArgs {
                url,
                javascript: false,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, WebToolError::TooLarge { .. }));
    }

    #[tokio::test]
    async fn web_read_surfaces_http_error_as_typed_error() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/missing");
                then.status(404).body("nope");
            })
            .await;

        let tool = WebReadTool::with_runner(
            opts(true),
            Arc::new(StubRunner {
                output: fixed_output(),
            }),
            false,
        )
        .expect("build tool");

        let url = format!("{}/missing", server.base_url());
        let err = tool
            .run(&WebReadArgs {
                url,
                javascript: false,
            })
            .await
            .unwrap_err();
        match err {
            WebToolError::HttpStatus { code, .. } => assert_eq!(code, 404),
            other => panic!("expected HttpStatus, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn web_read_rejects_private_address() {
        let tool = WebReadTool::with_runner(
            opts(false),
            Arc::new(StubRunner {
                output: fixed_output(),
            }),
            false,
        )
        .expect("build tool");
        let err = tool
            .run(&WebReadArgs {
                url: "http://127.0.0.1/page".into(),
                javascript: false,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, WebToolError::PrivateAddress(_)));
    }

    #[tokio::test]
    async fn web_read_rejects_javascript_without_headless_feature() {
        let tool = WebReadTool::with_runner(
            opts(true),
            Arc::new(StubRunner {
                output: fixed_output(),
            }),
            false,
        )
        .expect("build tool");
        let err = tool
            .run(&WebReadArgs {
                url: "https://example.com/".into(),
                javascript: true,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, WebToolError::HeadlessFailure(_)));
    }
}
