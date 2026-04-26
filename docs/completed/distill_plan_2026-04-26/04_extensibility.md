# 04 — Extensibility

How `distill` grows over time without becoming a framework.

## Three extension points

Every feature that's not in the core flow goes through one of
three well-defined extension points:

1. **Site extractors** — handle one specific domain (X,
   YouTube, etc.). Trait-based, distributed as separate
   crates.
2. **Output templates** — control the rendered shape
   (markdown, JSON, custom). Tera-based.
3. **Fetcher hooks** — pre/post-fetch callbacks for cookies,
   headers, retries. Function-pointer based.

Anything that doesn't fit one of these doesn't belong in
`distill`; build it on top.

## Site extractors

See [`03_edge_cases.md`](03_edge_cases.md) for the trait
shape. Distribution model:

```toml
[dependencies]
distill-core = "0.2"
distill-extractors-x = "0.2"           # opt-in
distill-extractors-youtube = "0.2"
```

CLI integration via cargo features:

```bash
cargo install distill-cli                          # default plugins
cargo install distill-cli --features x,youtube     # explicit
cargo install distill-cli --no-default-features    # core only
```

Runtime registration in the library API:

```rust
let mut config = distill::Config::default();
config.add_extractor(Box::new(distill_extractors_x::Extractor));
config.add_extractor(Box::new(distill_extractors_youtube::Extractor));
let article = distill::Article::from_url_with_config(url, &config).await?;
```

Plugin discovery order: user-registered plugins → cargo-feature-
enabled built-ins → fall through to the default Defuddle
path. First plugin whose `matches()` returns true handles the
URL.

## Output templates

Tera (Jinja-like) is the template engine. Variables exposed:

| Variable | Type | Description |
|----------|------|-------------|
| `markdown` | string | Markdown body |
| `html_clean` | string | Cleaned HTML body |
| `title` | string | Article title |
| `author` | string? | Author |
| `published` | datetime? | RFC 3339 |
| `description` | string? | Meta description / lede |
| `source` | string | Source URL |
| `site` | string? | Site name (e.g., "example.com") |
| `language` | string? | ISO 639 code |
| `word_count` | u64 | Word count |
| `reading_time` | u32 | Minutes |
| `tags` | Vec<string> | Extracted tags |
| `fetched` | datetime | When `distill` ran |
| `metadata.raw` | json | Full Defuddle output |

Template examples shipped in `examples/templates/`:

- `obsidian.tera` — frontmatter + body in Obsidian
  conventions.
- `commonplace.tera` — minimal heading + body, no
  frontmatter.
- `agent_input.tera` — JSON shape optimized for LLM
  prompting.
- `notion_export.tera` — Notion-friendly heading hierarchy.

User can override:

```bash
distill <URL> --template ./my-template.tera
```

Or set as default in config:

```toml
# ~/.config/distill/config.toml
default_template = "/path/to/template.tera"
```

## Fetcher hooks

Pre-fetch / post-fetch hooks for advanced use cases. Kept
small to avoid a "middleware framework" sprawl.

```rust
let opts = FetchOptions::default()
    .pre_fetch(|req: &mut RequestBuilder| {
        req.header("X-Custom-Header", "value");
    })
    .post_fetch(|resp: &Response| {
        tracing::info!(status = ?resp.status(), "fetched");
    })
    .on_retry(|attempt, err| {
        if attempt > 3 { RetryDecision::GiveUp }
        else { RetryDecision::Retry { delay: Duration::from_secs(attempt as u64) } }
    });
```

These map cleanly to existing `reqwest::Middleware` patterns
if we want to integrate `reqwest-middleware` later. Phase 3
polish, not Phase 1.

## Model Context Protocol (MCP) server

`distill` ships with a built-in MCP server. Two ways to run:

```bash
distill mcp                         # stdio transport (default for Claude Desktop, etc.)
distill mcp --transport http --port 8080
```

MCP tool exposed:

```json
{
  "name": "browse_web",
  "description": "Fetch a URL and return clean Markdown content with metadata. Handles paywalls, JS-heavy sites (with --javascript), and most edge cases gracefully. Returns Markdown formatted for LLM consumption.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "url": { "type": "string", "format": "uri" },
      "javascript": { "type": "boolean", "default": false },
      "format": { "type": "string", "enum": ["markdown", "json", "html_clean"], "default": "markdown" },
      "timeout_seconds": { "type": "integer", "default": 30 },
      "max_size_mb": { "type": "integer", "default": 10 }
    },
    "required": ["url"]
  }
}
```

Crate: `distill-mcp` uses `rmcp` (the official Rust MCP
SDK). Implementation is ~200 LOC mapping MCP requests to
`distill_core::Article::from_url_with_options(...)`.

Claude Desktop registration:

```json
// ~/.config/claude/claude_desktop_config.json
{
  "mcpServers": {
    "distill": {
      "command": "/usr/local/bin/distill",
      "args": ["mcp"]
    }
  }
}
```

## anie integration

Add `distill` to `anie-tools` as a first-class tool:

```rust
// crates/anie-tools/src/web.rs
use anie_tools::ToolDef;
use distill::Article;
use serde::Deserialize;

#[derive(Deserialize)]
struct BrowseArgs {
    url: String,
    #[serde(default)]
    javascript: bool,
}

pub fn browse_web_tool() -> ToolDef {
    ToolDef::new(
        "browse_web",
        "Fetch a URL and return clean Markdown.",
        // schema, handler, ...
    )
}

async fn handle(args: BrowseArgs) -> Result<ToolOutput> {
    let opts = distill::FetchOptions::default()
        .javascript(args.javascript);
    let article = Article::from_url_with_options(&args.url, opts).await?;
    Ok(ToolOutput::Text(article.to_markdown_with_frontmatter()))
}
```

Registration in anie's bootstrap:

```rust
// crates/anie-cli/src/bootstrap.rs
fn build_tool_registry(...) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(bash_tool());
    registry.register(read_tool());
    // ...
    if config.tools.web_enabled {
        registry.register(distill_tool::browse_web_tool());
    }
    Arc::new(registry)
}
```

Config knob in `anie.toml`:

```toml
[tools]
web_enabled = true       # default true, opt-out for sandboxed envs
```

## HTTP API mode

For shared services / multi-agent setups: `distill serve`
runs an `axum` server exposing:

| Endpoint | Method | Body | Returns |
|----------|--------|------|---------|
| `/extract` | POST | `{"url":"...", "javascript":false}` | JSON Article |
| `/extract` | POST | `{"html":"...", "base_url":"..."}` | JSON Article |
| `/health` | GET | — | `{"status":"ok"}` |

Default bind: `127.0.0.1:8080`. `--bind 0.0.0.0` for network
exposure (with `--auth-token` recommended).

Use case: a long-running team service shared by many agents,
where amortizing V8 startup across calls matters.

## Configuration file

```toml
# ~/.config/distill/config.toml or ./distill.toml

[fetch]
user_agent = "distill/0.2 (+https://example.com/distill)"
timeout = "30s"
max_size = "10MB"
respect_robots_txt = true
follow_redirects = true
default_language = "en"

[fetch.rate_limit]
requests_per_second = 1.0
burst = 5

[output]
default_template = "obsidian"  # or path to a .tera file
include_frontmatter = true

[plugins]
enabled = ["x", "reddit", "youtube"]

[headless]
enabled = true                  # globally allow --javascript
chrome_path = "/usr/bin/chromium"
```

Layering (figment / config-rs):
1. Built-in defaults
2. Config file (system-wide → user → project)
3. Environment variables (prefix `DISTILL_`)
4. CLI flags

## Future extension ideas (NOT planned now)

These are flagged here so they don't get built speculatively
during the planned phases — they're separate decisions:

- **Plugin sandboxing** via WebAssembly. Site extractors as
  WASM modules. Compelling once the plugin ecosystem is real.
- **Crawl mode**. `distill crawl <seed-url> --depth 2`. A
  whole separate product; do not add to `distill`.
- **PDF extraction**. `pdfium-render` integration. Add when
  someone reports it as a real workflow gap.
- **Image OCR for image-heavy pages**. Tesseract or cloud
  service. Out of scope.
- **Deduplication / change detection**. "Did this URL's
  content change since last fetch?" Worth exploring as a
  separate `distill-watch` crate.
- **Vector embedding integration**. The output is already
  Markdown; agents already embed Markdown. Don't bake an
  embedding API into the tool.
