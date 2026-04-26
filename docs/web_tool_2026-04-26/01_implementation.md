# 01 — Implementation

Concrete build plan for `web_read`. Phased PRs, each with
file paths, test names, and exit criteria.

## Prerequisites (one-time setup)

Before starting any of the PRs below:

1. Install `defuddle-cli` once on the dev machine:
   `npm i -g defuddle-cli`
2. Note the installed version. Pin that in
   `crates/anie-tools-web/src/read/extract.rs::DEFUDDLE_VERSION`.
3. Verify `defuddle https://example.com --markdown --json`
   produces the expected JSON shape on the dev machine.
   Capture a sample output as the test fixture.

## PR 1 — Crate scaffold + fetch + SSRF guard

**Goal**: build the `anie-tools-web` crate, get URL
validation, robots.txt, rate limiting, and HTTP fetching
working as a standalone happy-path. No Defuddle, no tool
registration yet.

**Files**

- `crates/anie-tools-web/Cargo.toml` — declares the crate,
  its features, and its workspace deps. Adds the new crate
  to the root workspace `[workspace.members]`.
- `crates/anie-tools-web/src/lib.rs` — initial public surface
  (just module declarations and a placeholder
  `web_tools()` returning empty Vec).
- `crates/anie-tools-web/src/error.rs` — full
  `WebToolError` enum.
- `crates/anie-tools-web/src/read/mod.rs` — module skeleton.
- `crates/anie-tools-web/src/read/fetch.rs` — `validate_url`,
  `host_is_private`, `RobotsCache`, `RateLimiter`,
  `fetch_html`. All async.
- `crates/anie-tools-web/tests/fetch_basic.rs` — integration
  test against `httpmock`.

**Tests added**

- `validate_url_accepts_https`
- `validate_url_rejects_file_scheme`
- `validate_url_rejects_loopback_when_private_disallowed`
- `validate_url_resolves_dns_before_classification` (DNS
  rebinding guard)
- `robots_cache_caches_per_host`
- `robots_disallows_known_disallowed_paths`
- `rate_limiter_blocks_when_burst_exhausted`
- `rate_limiter_recovers_after_window`
- `fetch_returns_body_within_max_size`
- `fetch_rejects_body_above_max_size`
- `fetch_follows_redirects_up_to_max`
- `fetch_decodes_gzip` / `_brotli`

**Cargo.toml content (sketch)**

```toml
[package]
name = "anie-tools-web"
version.workspace = true
edition.workspace = true
license.workspace = true

[features]
default = []
# Always-on features when this crate is compiled.
# Pulling in this crate at all means web_read is available;
# additional features below opt into specific backends.
headless = ["dep:chromiumoxide"]

[dependencies]
anie-agent = { path = "../anie-agent" }
anie-protocol = { path = "../anie-protocol" }
anie-config = { path = "../anie-config" }
anyhow.workspace = true
async-trait.workspace = true
chrono.workspace = true
governor = "0.6"
reqwest = { workspace = true, features = ["gzip", "brotli", "stream"] }
serde = { workspace = true, features = ["derive"] }
serde_json.workspace = true
serde_yaml.workspace = true
schemars = { workspace = true }
texting_robots = "0.2"
thiserror.workspace = true
tokio = { workspace = true, features = ["process", "io-util", "macros"] }
tracing.workspace = true
url.workspace = true
which = "6"
chromiumoxide = { version = "0.6", optional = true, default-features = false, features = ["tokio-runtime"] }

[dev-dependencies]
httpmock = "0.7"
tempfile.workspace = true
tokio = { workspace = true, features = ["test-util"] }
```

The crate doesn't carry a `default = ["headless"]`; opting
into headless rendering is a separate feature flag so users
who don't need JS rendering don't pull in chromiumoxide's
dependency tree.

**Exit criteria for PR 1**

- All 12+ tests pass.
- `cargo build -p anie-tools-web` clean.
- `cargo clippy -p anie-tools-web --all-targets -- -D warnings` clean.
- The crate is *not* yet integrated with anie-cli's tool
  registry. That happens in PR 2.

## PR 2 — Defuddle bridge + WebReadTool registration

**Goal**: wire Defuddle in via subprocess, build the
`WebReadTool` impl, register it in `anie-cli`'s bootstrap.

**Files**

- `crates/anie-tools-web/src/read/extract.rs` — `locate_defuddle`,
  `run_defuddle`, `DefuddleOutput` struct (matches Defuddle's
  JSON schema).
- `crates/anie-tools-web/src/read/frontmatter.rs` —
  `build_frontmatter(metadata: &DefuddleMetadata) -> String`
  emitting YAML.
- `crates/anie-tools-web/src/read/mod.rs` — `WebReadTool`
  struct implementing the `Tool` trait. JSON schema for
  args via `schemars`. `execute()` orchestrates the pipeline.
- `crates/anie-tools-web/src/lib.rs` — `web_tools()` returns
  the populated Vec.
- `crates/anie-cli/Cargo.toml` — add
  `anie-tools-web = { path = "../anie-tools-web", optional = true }`
  and a `web` cargo feature.
- `crates/anie-cli/src/bootstrap.rs` — conditional
  registration:

  ```rust
  #[cfg(feature = "web")]
  if config.tools.web.enabled {
      for tool in anie_tools_web::web_tools() {
          registry.register(tool);
      }
  }
  ```

- `crates/anie-config/src/lib.rs` — add `WebToolConfig` to
  the `[tools]` section, with all the knobs from
  [`00_design.md`](00_design.md).
- `crates/anie-tools-web/tests/extract_defuddle.rs` —
  integration test that drives `WebReadTool::execute` against
  a fixture HTML page (no real network; uses `httpmock` for
  the fetch and a mocked `defuddle` via fixture-replay).

**The schema-aware Tool impl**

Anie's existing `Tool` trait (verify exact shape in
`anie-agent/src/tool.rs`; this is the rough contract):

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> serde_json::Value;
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext)
        -> Result<ToolOutput, ToolError>;
}
```

Implementation:

```rust
// crates/anie-tools-web/src/read/mod.rs
pub struct WebReadTool {
    fetch: Arc<FetchClient>,        // built once, shared
    config: WebToolConfig,
}

#[derive(Deserialize, JsonSchema)]
pub struct WebReadArgs {
    /// The URL to fetch and read.
    pub url: String,
    /// Render JavaScript before extracting (slower, requires Chrome).
    #[serde(default)]
    pub javascript: bool,
}

#[async_trait]
impl Tool for WebReadTool {
    fn name(&self) -> &'static str { "web_read" }

    fn description(&self) -> &'static str {
        "Fetch a URL and return its main content as clean Markdown \
         with YAML frontmatter metadata (title, author, date, etc.). \
         Use this for reading articles, documentation, blog posts, \
         and similar content. Pass javascript=true for SPA / heavily \
         JS-rendered pages (slower, requires Chrome)."
    }

    fn input_schema(&self) -> serde_json::Value {
        // Generated via schemars from WebReadArgs.
        serde_json::to_value(schemars::schema_for!(WebReadArgs))
            .expect("static schema is serializable")
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext)
        -> Result<ToolOutput, ToolError>
    {
        let args: WebReadArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let body = self.run(&args).await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(ToolOutput::Text(body))
    }
}

impl WebReadTool {
    async fn run(&self, args: &WebReadArgs) -> Result<String, WebToolError> {
        let url = fetch::validate_url(&args.url, self.config.allow_private_ips)?;
        if self.config.respect_robots_txt {
            self.fetch.robots.check(&url, &self.config.user_agent).await?;
        }
        self.fetch.rate_limit.acquire(&url).await;

        let html = if args.javascript {
            #[cfg(feature = "headless")]
            { self.fetch.render_with_chrome(&url).await? }
            #[cfg(not(feature = "headless"))]
            { return Err(WebToolError::HeadlessFailure(
                "javascript=true requires building with --features headless".into(),
            )); }
        } else {
            self.fetch.fetch_html(&url).await?
        };

        let extracted = extract::run_defuddle(&html, url.as_str()).await?;
        let yaml = frontmatter::build(&extracted.metadata);
        Ok(format!("{yaml}\n{}", extracted.markdown))
    }
}
```

**Tests added**

- `web_read_tool_name_is_stable`
- `web_read_input_schema_round_trips`
- `web_read_executes_against_fixture_html` (uses captured
  Defuddle output to mock the subprocess)
- `web_read_surfaces_robots_disallow_as_typed_error`
- `web_read_surfaces_too_large_as_typed_error`
- `web_read_surfaces_defuddle_not_found_with_install_message`
- `web_read_emits_yaml_frontmatter_then_markdown`

For the subprocess tests we don't actually spawn `defuddle`
during CI — that would require Node + the npm package. We
add a feature-gated test (`#[cfg(feature = "live-defuddle-tests")]`)
that hits a real `defuddle` install for local dev, and the
default test path uses a `MockDefuddle` shim that returns
canned JSON.

**Exit criteria for PR 2**

- All tests pass under `cargo test --workspace`.
- `cargo build --features web -p anie-cli` succeeds and the
  built binary registers `web_read` in its tool list (verify
  via a unit test on `bootstrap::build_tool_registry`).
- Manual smoke: `cargo run -p anie-cli --features web` →
  agent → `web_read https://en.wikipedia.org/wiki/Rust_(programming_language)`
  returns Markdown.
- README in the new crate documents the Node.js prereq.

## PR 3 — Headless Chrome path (optional feature)

**Goal**: implement `javascript: true` via `chromiumoxide`,
gated behind the `headless` cargo feature.

**Files**

- `crates/anie-tools-web/src/read/headless.rs` —
  `render_with_chrome(url, timeout) -> Result<String>`.
- Tests against a local fixture that's actually JS-rendered
  (vite app dev server in test setup, or pre-built HTML
  with `<script>` mutating DOM).

**Why a separate PR**: chromiumoxide pulls in a non-trivial
dep tree. Keeping this feature optional and isolated in its
own commit lets reviewers and users opt out cleanly.

**Tests added**

- `headless_render_returns_post_dom_html_for_spa_fixture`
- `headless_render_times_out_on_hanging_page`
- `headless_render_surfaces_chrome_not_found_with_install_message`

**Exit criteria for PR 3**

- Tests pass when Chrome is installed locally.
- `cargo test -p anie-tools-web` runs the non-headless tests
  (default features); chromiumoxide-specific tests gated.
- Manual: agent calls `web_read` against a known SPA with
  `javascript=true`, gets meaningful markdown.

## PR 4 — web_search (DuckDuckGo HTML)

See [`02_web_search.md`](02_web_search.md). Same crate
(`anie-tools-web`), separate sub-module (`src/search/`).

## Documentation deliverables

Each PR includes:

- rustdoc on the crate root and on every public type/fn.
- An entry in the project root's `docs/` if the user-visible
  behavior changes (config knobs, tool description string).
- Updated `Cargo.lock` reviewed for new transitive deps.

The crate-level README:

```markdown
# anie-tools-web

Web reading and search tools for the anie agent. Currently
exposes:

- `web_read` — fetch a URL, return clean Markdown via Defuddle
- `web_search` — query a search backend, return ranked URLs

## Prerequisites (default build)

- **Node.js + Defuddle CLI** for `web_read`. Install via
  `npm i -g defuddle-cli`. The tool will fall through to
  `npx defuddle@<pinned-version>` if the global install is
  absent.
- **Chrome / Chromium** *only* for the optional
  `javascript=true` mode. Build with `--features headless` to
  enable.

## Compiling out the web tools

If you're shipping anie without web access:

  cargo build -p anie-cli --no-default-features
              --features <whatever-else-you-want-but-not-web>

The web crate isn't compiled in, no web-specific deps are
pulled, no web tools registered with the agent.
```

## Cross-cutting concerns

### Logging

Use `tracing` instrumentation:

- `tracing::info!` for fetch-start / fetch-complete with URL
  and elapsed.
- `tracing::debug!` for per-stage timing (validate, robots,
  rate-limit, fetch, extract).
- `tracing::trace!` for the actual subprocess command line
  used (helps debug `defuddle` PATH issues).

### Concurrency

Multiple agent calls to `web_read` in parallel must not
interfere:

- `RobotsCache` uses `tokio::sync::RwLock` (mostly reads).
- `RateLimiter` uses `governor::DirectRateLimiter` which is
  inherently per-host.
- `FetchClient::client` (a `reqwest::Client`) is `Clone +
  Send + Sync` and uses connection pooling internally.
- The Defuddle subprocess is spawned fresh per invocation;
  no shared state.

A future optimization is a process pool for Defuddle (avoid
the ~150ms Node startup per call), but it's premature until
benchmarks show it matters.

### Error recovery

The agent's retry loop is upstream of the tool. We don't
implement tool-level retry. Transient HTTP failures (5xx,
network errors) surface as typed errors; the agent decides.

The exception is `defuddle` not being found — that's a
permanent failure for the session, not retriable. We surface
it clearly so the agent can communicate the install steps to
the user instead of trying again.

### Sandboxing / permissions

Reuse anie's existing tool-permission model. `web_read` and
`web_search` register as tools that require network access.
If anie's policy layer ever gains "deny network tools," these
get blocked there — same as bash's deny patterns.

### Performance budget

Targets at P50 on a typical dev laptop:

| Stage | Target |
|-------|--------|
| URL validation + robots check | < 10 ms (cache hit) |
| HTTP fetch (typical article, ~100 KB) | < 500 ms |
| Defuddle subprocess (warm npx cache) | < 250 ms |
| Frontmatter + format | < 5 ms |
| **Total `web_read` invocation** | **< 1 s** |
| `javascript=true` end-to-end | < 5 s |

If we end up consistently over the totals, profile and
consider:

1. Chrome process pool for headless mode.
2. Defuddle process pool / worker model.
3. The Phase 2 `--features web-embedded` deno_core path.

### Testing strategy

- Unit tests for fetch path components (URL, robots, rate
  limit) using `httpmock` and crafted fixtures.
- Mocked-Defuddle integration test for the read pipeline
  (canned JSON output covers the schema).
- Live-Defuddle integration test gated behind a feature flag,
  runs in a separate CI job that has Node installed.
- Live-network smoke test (`#[ignore]` by default), runs
  against a small allowlist of stable URLs.
