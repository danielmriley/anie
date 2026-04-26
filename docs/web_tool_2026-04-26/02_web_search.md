# 02 — web_search

Companion tool to `web_read`. Given a query, return ranked
URLs + snippets so the agent can decide what to read next.

## Why a separate tool (vs. one combined tool)

- **Different cost profile.** Search is one HTTP request with
  modest output (~10 hits, snippet text); read is one HTTP
  request *plus* extraction. Separate tools let the agent
  fan out reads in parallel after a single search.
- **Different failure modes.** Search backends rate-limit
  more aggressively than article-host servers. Decoupling
  means a dropped search call doesn't bubble through a
  composite tool's reasoning.
- **Composability.** Agent flow becomes natural:
  `web_search("rust async") → choose 2-3 URLs → web_read each → synthesize`.

## Backend choice

Three real options for v1:

### Option A: DuckDuckGo HTML scrape ✓ recommended for v1

`duckduckgo.com/html/?q=...` returns a static HTML SERP
without JavaScript. Easy to scrape, no API key, no auth.

**Pros:**
- Free.
- No registration / API key.
- Public-document parsing — no ToS gymnastics for fair use.
- Returns reasonable result quality; broad index.

**Cons:**
- Subject to DuckDuckGo's rate limits (anecdotally ~10
  rps before throttling).
- HTML format can change; we'd need a small parser to
  extract title + URL + snippet.
- No advanced operators (date filters, site: limits) without
  more URL-fu.
- IP-banning risk if we look bot-like; mitigated by sane
  user-agent and rate limiting.

### Option B: Brave Search API

Requires a Brave Search API key (free tier exists, ~2,000
queries/month). JSON response, structured.

**Pros:**
- Stable JSON shape.
- High-quality results.
- No HTML scraping.

**Cons:**
- Requires API key — user must register.
- "100% local / no required cloud services" original
  requirement bends here. Acceptable for an *opt-in* backend,
  not as the v1 default.

### Option C: SearXNG (self-hosted meta-search)

A SearXNG instance the user runs locally or that points at a
public mirror.

**Pros:**
- True local control if self-hosted.
- Aggregates results from many backends.
- No per-call cost.

**Cons:**
- Requires user to set up and maintain an instance, or trust
  a public one.
- Quality varies by configured backends.
- Public mirrors rate-limit and disappear.

### Recommendation

**Default = DuckDuckGo HTML.** Acceptable for v1: free, no
auth, stable enough. We treat the parser as version-pinned
and add tests against a captured fixture so regressions show
up in CI rather than at runtime.

**Add Brave / SearXNG as opt-in backends in Phase 2** if
users complain about DDG's rate limits or quality. Selecting
backend via:

```toml
[tools.web]
search_backend = "duckduckgo"          # default
# search_backend = "brave"             # alt; requires `search_brave_api_key`
# search_backend = "searxng"           # alt; requires `search_searxng_url`
search_brave_api_key = "..."
search_searxng_url = "https://my-searxng.example/search"
```

The crate exposes the backend enum, and the tool resolves it
at construction time so there's no runtime dispatch per call.

## Tool contract

Schema the agent sees:

```json
{
  "name": "web_search",
  "description": "Search the web and return ranked URLs with titles and snippets. Use this to find pages worth reading; pair with web_read to fetch the actual content. Returns up to max_results items (default 10, hard cap 25).",
  "input_schema": {
    "type": "object",
    "properties": {
      "query": {
        "type": "string",
        "description": "Search query. Supports basic operators (quoted phrases, +/-) where the backend supports them."
      },
      "max_results": {
        "type": "integer",
        "minimum": 1,
        "maximum": 25,
        "default": 10,
        "description": "Maximum number of results to return."
      }
    },
    "required": ["query"]
  }
}
```

Output: a single Markdown-formatted string the agent can
read directly.

```markdown
# Search results: "rust async runtime comparison"

1. **Comparing Async Rust Runtimes** — *blog.example.com*
   https://blog.example.com/async-runtimes-2024
   A look at tokio, async-std, smol, and embassy with a
   focus on cost models and scheduling…

2. **Choosing a Rust async runtime** — *another-site.org*
   https://another-site.org/posts/rust-async
   This post compares the major async runtimes…

3. ...

(10 results total. Returned by DuckDuckGo, fetched 2026-04-26T15:23:01Z.)
```

The numbered list, URL on its own line, and italicized site
name are deliberate. They make it easy for the agent to:
- Cite a specific result by number.
- Parse out the URL with a simple regex if it wants
  programmatic structure.
- Understand at a glance how many results there are.

## File layout (within the existing crate)

```
crates/anie-tools-web/src/
├── search/
│   ├── mod.rs           # WebSearchTool, backend dispatch
│   ├── ddg.rs           # DuckDuckGo HTML scrape backend
│   ├── brave.rs         # (Phase 2) Brave Search API
│   └── searxng.rs       # (Phase 2) SearXNG backend
```

## Implementation sketch

```rust
// crates/anie-tools-web/src/search/mod.rs
pub struct WebSearchTool {
    backend: SearchBackend,
    fetch_client: Arc<FetchClient>,        // shared with web_read
    config: WebToolConfig,
}

#[derive(Debug, Clone)]
pub enum SearchBackend {
    DuckDuckGo,
    // Brave { api_key: String },          // Phase 2
    // SearXNG { base_url: Url },          // Phase 2
}

#[derive(Deserialize, JsonSchema)]
pub struct WebSearchArgs {
    /// The search query.
    pub query: String,
    /// Maximum results to return (1-25, default 10).
    #[serde(default = "default_max_results")]
    pub max_results: u32,
}

fn default_max_results() -> u32 { 10 }

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str { "web_search" }

    fn description(&self) -> &'static str {
        "Search the web and return ranked URLs with titles and \
         snippets. Use this to find pages worth reading; pair \
         with web_read to fetch the actual content."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(WebSearchArgs))
            .expect("static schema is serializable")
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext)
        -> Result<ToolOutput, ToolError>
    {
        let args: WebSearchArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let max = args.max_results.min(25).max(1);
        let results = match &self.backend {
            SearchBackend::DuckDuckGo =>
                ddg::search(&self.fetch_client, &args.query, max).await,
        }
        .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(ToolOutput::Text(format_results(&args.query, &results, &self.backend)))
    }
}

#[derive(Debug)]
pub struct SearchHit {
    pub title: String,
    pub url: Url,
    pub snippet: String,
    pub site: String,
}
```

DuckDuckGo backend (the meat of the work):

```rust
// crates/anie-tools-web/src/search/ddg.rs
const DDG_HTML_URL: &str = "https://duckduckgo.com/html/";

pub async fn search(client: &FetchClient, query: &str, max: u32)
    -> Result<Vec<SearchHit>, WebToolError>
{
    let url = format!("{DDG_HTML_URL}?q={}", urlencoding::encode(query));
    let html = client.fetch_html_simple(&url).await?;
    let hits = parse_ddg_html(&html, max as usize)?;
    Ok(hits)
}

fn parse_ddg_html(html: &str, max: usize) -> Result<Vec<SearchHit>, WebToolError> {
    use scraper::{Html, Selector};
    let doc = Html::parse_document(html);
    let result_sel = Selector::parse("div.result").unwrap();
    let title_sel = Selector::parse("a.result__a").unwrap();
    let snippet_sel = Selector::parse(".result__snippet").unwrap();

    let mut hits = Vec::new();
    for node in doc.select(&result_sel).take(max) {
        let title_el = node.select(&title_sel).next();
        let snippet_el = node.select(&snippet_sel).next();
        let (Some(title), Some(snippet)) = (title_el, snippet_el) else {
            continue;
        };

        let raw_href = title.value().attr("href").unwrap_or_default();
        let url = decode_ddg_redirect(raw_href)?;
        let title_text = title.text().collect::<String>().trim().to_string();
        let snippet_text = snippet.text().collect::<String>().trim().to_string();
        let site = url.host_str().unwrap_or("").to_string();
        hits.push(SearchHit {
            title: title_text,
            url,
            snippet: snippet_text,
            site,
        });
    }
    Ok(hits)
}

/// DDG wraps result URLs in a redirect: `/l/?uddg=<encoded>&...`.
/// Extract the `uddg` parameter and percent-decode.
fn decode_ddg_redirect(raw: &str) -> Result<Url, WebToolError> {
    let parsed = if raw.starts_with('/') {
        Url::parse(&format!("https://duckduckgo.com{raw}"))
    } else {
        Url::parse(raw)
    }
    .map_err(|e| WebToolError::SearchBackend(e.to_string()))?;
    let final_url = parsed
        .query_pairs()
        .find(|(k, _)| k == "uddg")
        .map(|(_, v)| v.to_string());
    match final_url {
        Some(u) => Url::parse(&u).map_err(|e| WebToolError::SearchBackend(e.to_string())),
        None => Ok(parsed),  // not a redirect, use as-is
    }
}
```

## Tests

Unit:

- `decode_ddg_redirect_extracts_uddg_target`
- `decode_ddg_redirect_handles_already_absolute_url`
- `parse_ddg_html_returns_expected_hits` against captured DDG
  HTML fixture stored in `tests/fixtures/ddg-rust-async.html`
- `parse_ddg_html_truncates_at_max`
- `format_results_emits_expected_markdown_layout`

Integration:

- `web_search_executes_against_mocked_backend` — uses
  `httpmock` to serve the captured HTML.
- `web_search_surfaces_backend_failure_as_typed_error`

Live (gated behind `#[ignore]` so default test runs don't hit
the network):

- `live_web_search_returns_results_for_real_query` — run
  manually before pinning the parser version to a new DDG
  HTML release.

## Rate limiting + abuse

Search backends rate-limit aggressively. We:

- Apply the same per-host token bucket from `web_read`. DDG
  is a separate host from article hosts, so it gets its own
  bucket, but the limit is the same conservative
  `1 req/sec` default.
- User-Agent string is the documented `anie/<version>` —
  not a fake browser string. Search backends are within
  their rights to block automated traffic; we don't pretend
  not to be automated.
- On consistent failure (e.g., 429 from DDG), surface a clear
  error so the agent can fall back to user-prompted URLs:

  ```
  web_search backend (duckduckgo) is rate-limiting requests.
  Try again in a few minutes, or configure search_backend
  = "brave" with an API key in [tools.web].
  ```

## Maintenance reality check

DuckDuckGo HTML scraping is **fragile** by definition. The
HTML structure changes; CSS selectors break; redirect
schemes evolve. Our defenses:

1. **Captured fixture in the test suite.** Any change to the
   parser is gated by tests that exercise the captured HTML.
   When DDG ships a layout change, we update the fixture and
   the parser together.
2. **Conservative selectors.** `div.result` and the title /
   snippet selectors have been stable for ~3 years per the
   public DDG HTML mirror history. We monitor and update.
3. **Live-test job in CI** (weekly cron, not per-PR) that
   runs the live-network test. Failure means the parser
   needs an update — catch this *before* a user reports a
   broken `web_search` in the wild.
4. **Documented fallback path.** If the parser breaks
   between updates, users can switch to Brave (Phase 2) by
   setting `search_backend = "brave"` and supplying an API
   key. We document this prominently.

## When to add Brave / SearXNG

Triggers for prioritizing Phase 2 backends:

- **DuckDuckGo rate-limits us in real use.** A user reports
  > 10% failure rate from DDG → time to add Brave.
- **DDG HTML changes break the parser frequently.** If we're
  shipping parser updates more than monthly, the fragility
  cost has exceeded the simplicity benefit.
- **A user with a self-hosted search index asks for SearXNG
  support.** Concrete pull, not speculative.

Until any of those triggers, the simpler one-backend story
wins.

## What we're explicitly not building (in v1)

- **Image / video / news search** verticals. Generic web
  search only. Add later if a workflow demands it.
- **Pagination / "more results."** First page is enough for
  v1. The agent can refine the query if it wants more.
- **Result deduplication / re-ranking.** Pass through what
  the backend returns.
- **Search history / cache.** Each call is independent.
  Caching introduces freshness questions we don't want to
  answer in v1.
- **Cite-aware result formatting** (e.g., RFC-style citation
  blocks). Out of scope; the agent can format citations from
  the returned URLs if asked.

## Phased PR (within the larger web tool work)

This is **PR 4 of the implementation plan**:

- PR 1: scaffold + fetch + SSRF guard (`web_read` infra)
- PR 2: Defuddle bridge + `WebReadTool` registration
- PR 3: headless Chrome (optional)
- **PR 4: this — `WebSearchTool` with DuckDuckGo backend**

Lands in the same crate as `web_read`. Estimated 1 week of
work (~250 LOC + tests + 1 captured fixture).

Exit criteria for PR 4:

- All 5+ tests pass.
- `web_search_executes_against_mocked_backend` covers the
  full execute path.
- Manual smoke: agent → `web_search("rust async")` → 10
  results in expected Markdown.
- Documentation in the crate README updated.
