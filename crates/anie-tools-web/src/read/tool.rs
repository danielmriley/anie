//! `WebReadTool` — the `Tool` impl that the agent calls.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use anie_agent::{Tool, ToolError, ToolExecutionContext, effective_tool_output_budget};
use anie_protocol::{ContentBlock, ToolDef, ToolResult};

use crate::error::WebToolError;
use crate::read::extract::{DefuddleRunner, SubprocessDefuddleRunner};
use crate::read::fetch::{
    self, DEFAULT_RATE_LIMIT_BURST, DEFAULT_RATE_LIMIT_RPS, FetchOptions, HostRateLimiter,
    Resolver, RobotsCache, system_resolver,
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
    rate_limiter: Arc<HostRateLimiter>,
    runner: Arc<dyn DefuddleRunner>,
    resolver: Arc<dyn Resolver>,
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
            rate_limiter: Arc::new(HostRateLimiter::new(
                DEFAULT_RATE_LIMIT_RPS,
                DEFAULT_RATE_LIMIT_BURST,
            )),
            runner: Arc::new(SubprocessDefuddleRunner),
            resolver: system_resolver(),
            respect_robots_txt: true,
        })
    }

    /// Build with a shared rate limiter (so `web_read` and
    /// `web_search` share per-host bucket state).
    pub fn with_rate_limiter(rate_limiter: Arc<HostRateLimiter>) -> Result<Self, WebToolError> {
        Self::with_options(FetchOptions::default(), rate_limiter)
    }

    /// Build with operator-supplied [`FetchOptions`] and a
    /// shared rate limiter. The bootstrap uses this when the
    /// user has configured `[tools.web]` budgets so they take
    /// effect at startup. PR 4.3 of
    /// `docs/code_review_2026-04-27/`.
    pub fn with_options(
        opts: FetchOptions,
        rate_limiter: Arc<HostRateLimiter>,
    ) -> Result<Self, WebToolError> {
        let client = fetch::build_client(&opts)?;
        Ok(Self {
            client,
            fetch_opts: opts,
            robots: RobotsCache::new(),
            rate_limiter,
            runner: Arc::new(SubprocessDefuddleRunner),
            resolver: system_resolver(),
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
            rate_limiter: Arc::new(HostRateLimiter::new(
                DEFAULT_RATE_LIMIT_RPS,
                DEFAULT_RATE_LIMIT_BURST,
            )),
            runner,
            resolver: system_resolver(),
            respect_robots_txt,
        })
    }

    /// Replace the DNS resolver used by the SSRF guard. Tests
    /// inject [`crate::read::fetch::StaticResolver`] here to
    /// simulate hostname-to-private-IP mappings deterministically.
    /// Production code should leave the default
    /// [`crate::read::fetch::SystemResolver`] in place.
    pub fn set_resolver(&mut self, resolver: Arc<dyn Resolver>) {
        self.resolver = resolver;
    }

    async fn run(
        &self,
        args: &WebReadArgs,
        cancel: &CancellationToken,
        update_tx: Option<&mpsc::Sender<ToolResult>>,
        ctx: &ToolExecutionContext,
    ) -> Result<String, WebToolError> {
        // Plan 05 PR D: clamp the per-call fetch byte cap by
        // the model's context window. The configured
        // `fetch_opts.max_bytes` (default 10 MiB) remains the
        // absolute ceiling; for any context window under
        // ~100M tokens the share rule dominates and the
        // operator's default is the upper bound. Floors at
        // `MIN_TOOL_OUTPUT_BUDGET_BYTES`.
        let effective_max_bytes = usize::try_from(effective_tool_output_budget(
            ctx.context_window,
            self.fetch_opts.max_bytes as u64,
        ))
        .unwrap_or(self.fetch_opts.max_bytes);
        if cancel.is_cancelled() {
            return Err(WebToolError::Aborted);
        }
        let url = fetch::validate_url(&args.url, self.fetch_opts.allow_private_ips)?;
        info!(target = %url, javascript = args.javascript, "web_read start");

        if self.respect_robots_txt {
            self.robots
                .check(&url, &self.fetch_opts.user_agent, &self.client, cancel)
                .await?;
            debug!(host = url.host_str().unwrap_or(""), "robots ok");
        }
        if let Some(host) = url.host_str() {
            // Rate-limit waits can be substantial when the
            // bucket is empty (1 rps default). A queued abort
            // must not have to wait out the throttle window.
            tokio::select! {
                _ = cancel.cancelled() => return Err(WebToolError::Aborted),
                _ = self.rate_limiter.acquire(host) => {}
            }
        }

        // Coarse phase progress. PR 4.4 of
        // `docs/code_review_2026-04-27/` — one update per
        // phase, never high-frequency. The send is best-effort
        // (drop on full / closed channel) so a slow consumer
        // can't backpressure the fetch.
        emit_phase(update_tx, &args.url, "fetching").await;

        let html = if args.javascript {
            #[cfg(feature = "headless")]
            {
                use std::time::Duration;
                // Headless path doesn't go through `fetch_html`,
                // so the resolved-IP SSRF check has to happen
                // here. Once Chrome is launched, redirects and
                // subresources go through the browser's network
                // stack — we don't intercept those today, so the
                // headless boundary is strictly weaker than the
                // non-headless `fetch_html` path. This call only
                // covers the initial URL. PR 3.3 of
                // `docs/code_review_2026-04-27/`.
                //
                // anie-specific deviation (Plan 05 PR D of
                // `docs/midturn_compaction_2026-04-27/`):
                // `effective_max_bytes` is computed for this
                // call but only the non-headless branch below
                // honors it. Chrome's render buffer is not
                // capped from anie's side, so a headless render
                // can still return a body larger than the
                // per-call budget on a small-context model.
                // Capping headless renders requires either a
                // post-render trim or a `--virtual-time-budget`
                // tweak; both are speculative and tracked as a
                // deferred follow-up rather than blocking the
                // mid-turn-compaction milestone.
                tokio::select! {
                    _ = cancel.cancelled() => return Err(WebToolError::Aborted),
                    r = fetch::validate_destination(
                        &url,
                        self.resolver.as_ref(),
                        self.fetch_opts.allow_private_ips,
                    ) => r?,
                }
                emit_phase(update_tx, &args.url, "rendering").await;
                crate::read::headless::render_with_chrome(
                    &url,
                    Duration::from_secs(self.fetch_opts.headless_timeout_secs),
                    cancel,
                )
                .await?
            }
            #[cfg(not(feature = "headless"))]
            {
                return Err(WebToolError::HeadlessFailure(
                    "javascript=true requires building anie-tools-web with --features headless"
                        .into(),
                ));
            }
        } else {
            // `fetch_html` validates every URL in the chain
            // (initial + redirects) against `allow_private_ips`.
            // Plan 05 PR D: clone the configured opts and shrink
            // `max_bytes` to the per-call effective budget. The
            // operator-configured `max_bytes` remains the upper
            // bound; small-context models get the smaller of the
            // two.
            let mut per_call_opts = self.fetch_opts.clone();
            per_call_opts.max_bytes = effective_max_bytes;
            fetch::fetch_html(
                &self.client,
                self.resolver.as_ref(),
                cancel,
                &url,
                &per_call_opts,
            )
            .await?
        };
        debug!(bytes = html.len(), "fetched html");

        emit_phase(update_tx, &args.url, "extracting").await;
        let extracted = self.runner.run(&html, url.as_str(), cancel).await?;
        debug!(
            title = extracted.title.as_deref().unwrap_or(""),
            words = ?extracted.word_count,
            "defuddle extracted"
        );

        let yaml = frontmatter::build(&extracted, url.as_str());
        Ok(format!("{yaml}\n{}", extracted.markdown_body()))
    }
}

/// Best-effort phase update. Builds a `ToolResult` shaped like
/// the bash tool's `partial: true` updates so the existing
/// controller/UI plumbing renders it without further work, and
/// uses `try_send` so a slow consumer can't backpressure the
/// fetch — dropping a phase update is far less harmful than
/// stalling the agent.
async fn emit_phase(update_tx: Option<&mpsc::Sender<ToolResult>>, url: &str, phase: &str) {
    let Some(tx) = update_tx else { return };
    let result = ToolResult {
        content: vec![ContentBlock::Text {
            text: format!("web_read: {phase} {url}"),
        }],
        details: serde_json::json!({
            "tool": "web_read",
            "url": url,
            "phase": phase,
            "partial": true,
        }),
    };
    let _ = tx.try_send(result);
}

#[async_trait]
impl Tool for WebReadTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "web_read".into(),
            description: "Fetch a URL from the live web and return its main content as clean Markdown with YAML frontmatter metadata (title, author, date, source, etc.). Use this whenever you need information from a specific web page — articles, documentation, news stories, weather pages, blog posts, reference material, or any URL surfaced by web_search. Not just for coding research. Pass javascript=true for SPA / heavily JS-rendered pages — slower, requires Chrome/Chromium installed and the crate built with --features headless. Note: javascript=true relies on Chrome's network stack and does NOT carry the same private-network protection as the default fetch path; prefer the default unless the page genuinely needs JS.".into(),
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
        cancel: CancellationToken,
        update_tx: Option<mpsc::Sender<ToolResult>>,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: WebReadArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::ExecutionFailed(format!("invalid web_read args: {e}")))?;
        let body = self
            .run(&parsed, &cancel, update_tx.as_ref(), ctx)
            .await
            .map_err(|e| match e {
                WebToolError::Aborted => ToolError::Aborted,
                other => ToolError::ExecutionFailed(other.to_string()),
            })?;
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
            _cancel: &CancellationToken,
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
            .run(
                &WebReadArgs {
                    url,
                    javascript: false,
                },
                &CancellationToken::new(),
                None,
                &ToolExecutionContext::default(),
            )
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
            .run(
                &WebReadArgs {
                    url,
                    javascript: false,
                },
                &CancellationToken::new(),
                None,
                &ToolExecutionContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WebToolError::TooLarge { .. }));
    }

    /// Plan 05 PR D: a small-window context shrinks the
    /// per-call fetch byte cap below `fetch_opts.max_bytes`,
    /// so a fetch that would otherwise succeed (5 KB body
    /// against the default 10 MiB cap) errors with `TooLarge`
    /// when the budget collapses to ~1 KB on an 8 K window.
    #[tokio::test]
    async fn web_read_caps_per_call_max_bytes_by_context_window() {
        let server = MockServer::start_async().await;
        let body = "x".repeat(5 * 1024);
        server
            .mock_async(|when, then| {
                when.method(GET).path("/medium");
                then.status(200).body(body);
            })
            .await;

        let tool = WebReadTool::with_runner(
            // 5 KB body would normally fit fine — 10 MiB cap.
            opts(true),
            Arc::new(StubRunner {
                output: fixed_output(),
            }),
            false,
        )
        .expect("build tool");

        let url = format!("{}/medium", server.base_url());
        let small_ctx = ToolExecutionContext {
            context_window: 8_192,
        };
        let err = tool
            .run(
                &WebReadArgs {
                    url,
                    javascript: false,
                },
                &CancellationToken::new(),
                None,
                &small_ctx,
            )
            .await
            .unwrap_err();
        match err {
            WebToolError::TooLarge { max, .. } => assert_eq!(
                max, 1024,
                "small-window fetch cap should match the floor (1024); got {max}",
            ),
            other => panic!("expected TooLarge, got: {other:?}"),
        }
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
            .run(
                &WebReadArgs {
                    url,
                    javascript: false,
                },
                &CancellationToken::new(),
                None,
                &ToolExecutionContext::default(),
            )
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
            .run(
                &WebReadArgs {
                    url: "http://127.0.0.1/page".into(),
                    javascript: false,
                },
                &CancellationToken::new(),
                None,
                &ToolExecutionContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WebToolError::PrivateAddress(_)));
    }

    #[cfg(not(feature = "headless"))]
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
            .run(
                &WebReadArgs {
                    url: "https://example.com/".into(),
                    javascript: true,
                },
                &CancellationToken::new(),
                None,
                &ToolExecutionContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WebToolError::HeadlessFailure(_)));
    }

    /// PR 4.1 of `docs/code_review_2026-04-27/`. A token that
    /// is already cancelled before `run` is invoked must
    /// short-circuit immediately — no fetch attempt, no
    /// Defuddle invocation. The agent loop relies on this for
    /// cheap "abort just before the call lands" semantics.
    #[tokio::test]
    async fn web_read_honors_cancellation_before_fetch() {
        let tool = WebReadTool::with_runner(
            opts(true),
            Arc::new(StubRunner {
                output: fixed_output(),
            }),
            false,
        )
        .expect("build tool");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = tool
            .run(
                &WebReadArgs {
                    url: "http://example.com/page".into(),
                    javascript: false,
                },
                &cancel,
                None,
                &ToolExecutionContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WebToolError::Aborted), "got: {err:?}");
    }

    /// PR 4.1: cancellation during the Defuddle step must
    /// surface promptly. Use a runner that cooperates by
    /// observing `cancel.cancelled()` before returning, then
    /// fire the cancel from the test once `run` is in flight.
    #[tokio::test]
    async fn web_read_honors_cancellation_while_defuddle_running() {
        // A runner that waits on cancel forever.
        struct StallingRunner;
        #[async_trait]
        impl DefuddleRunner for StallingRunner {
            async fn run(
                &self,
                _html: &str,
                _source_url: &str,
                cancel: &CancellationToken,
            ) -> Result<DefuddleOutput, WebToolError> {
                cancel.cancelled().await;
                Err(WebToolError::Aborted)
            }
        }

        let server = httpmock::MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/page");
                then.status(200)
                    .header("content-type", "text/html; charset=utf-8")
                    .body("<html><body>ok</body></html>");
            })
            .await;

        let tool = WebReadTool::with_runner(opts(true), Arc::new(StallingRunner), false)
            .expect("build tool");
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // Fire cancellation shortly after `run` has started.
        // 50ms is long enough for the fetch to land and reach
        // the Defuddle step on a healthy CI host, short enough
        // that a hung test would still surface within the
        // tokio runtime watchdog.
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });

        let url = format!("{}/page", server.base_url());
        let err = tool
            .run(
                &WebReadArgs {
                    url,
                    javascript: false,
                },
                &cancel,
                None,
                &ToolExecutionContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WebToolError::Aborted), "got: {err:?}");
        canceller.await.expect("canceller task");
    }

    /// PR 4.4 of `docs/code_review_2026-04-27/`. A coarse
    /// `fetching` and `extracting` phase update flows through
    /// the `update_tx` channel for the non-headless success
    /// path. Verifies one update per phase (no high-frequency
    /// progress) and that the partial-update marker is set so
    /// the controller can distinguish phase pings from final
    /// results.
    #[tokio::test]
    async fn web_read_emits_phase_progress_updates() {
        let server = httpmock::MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/page");
                then.status(200)
                    .header("content-type", "text/html; charset=utf-8")
                    .body("<html><body>ok</body></html>");
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

        let (tx, mut rx) = mpsc::channel::<ToolResult>(8);
        let cancel = CancellationToken::new();
        let url = format!("{}/page", server.base_url());
        let _body = tool
            .run(
                &WebReadArgs {
                    url: url.clone(),
                    javascript: false,
                },
                &cancel,
                Some(&tx),
                &ToolExecutionContext::default(),
            )
            .await
            .expect("run ok");
        // Drop tx so the receiver can drain to completion
        // without hanging on a closed but empty channel.
        drop(tx);

        let mut phases: Vec<String> = Vec::new();
        while let Some(result) = rx.recv().await {
            let phase = result
                .details
                .get("phase")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            assert!(
                result
                    .details
                    .get("partial")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                "phase update must be marked partial"
            );
            phases.push(phase);
        }
        assert_eq!(
            phases,
            vec!["fetching".to_string(), "extracting".to_string()],
            "non-headless path emits exactly the two phases, in order"
        );
    }

    /// PR 3.3 of `docs/code_review_2026-04-27/`. The headless
    /// path is not SSRF-equivalent to the non-headless path,
    /// but the *initial* navigation must still go through
    /// `validate_destination` — otherwise a hostname like
    /// `evil.example` resolving to `127.0.0.1` would slip past
    /// the textual `validate_url` check (which only catches
    /// known-private hostnames) and Chrome would happily fetch
    /// the loopback resource. This test pins that the resolved-IP
    /// guard fires before any Chrome launch attempt.
    ///
    /// Feature-gated because the production code path that
    /// invokes `validate_destination` for `javascript=true` only
    /// compiles with `--features headless`. The test asserts on
    /// the validation error, so it does not need a real Chrome
    /// install.
    #[cfg(feature = "headless")]
    #[tokio::test]
    async fn web_read_javascript_path_rejects_hostname_resolving_to_private_ip() {
        use std::net::{IpAddr, Ipv4Addr};

        use crate::read::fetch::StaticResolver;

        let mut tool = WebReadTool::with_runner(
            opts(false),
            Arc::new(StubRunner {
                output: fixed_output(),
            }),
            false,
        )
        .expect("build tool");
        tool.set_resolver(Arc::new(StaticResolver::new(vec![(
            "evil.example",
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        )])));

        let err = tool
            .run(
                &WebReadArgs {
                    url: "http://evil.example/page".into(),
                    javascript: true,
                },
                &CancellationToken::new(),
                None,
                &ToolExecutionContext::default(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, WebToolError::PrivateAddress(_)),
            "got: {err:?}"
        );
    }
}
