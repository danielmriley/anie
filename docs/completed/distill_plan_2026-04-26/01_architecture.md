# 01 — Architecture

Workspace layout, crate boundaries, library API, CLI shape.

## Repo structure

`distill` lives as **its own Cargo workspace and Git repo**.
Reasons:
- The tool is generic-Rust-agent friendly. Coupling it to
  anie's release cadence would block other agents from using
  it.
- License clarity — `distill` may take an MIT license to
  match Defuddle; anie's licensing is still being finalized.
- Independent CI, independent publishing to crates.io.

`anie` consumes `distill` as a normal crate dependency in
`anie-tools`. If we want lockstep development we can vendor
it as a git submodule under `crates/distill/` during
prototyping; switch to crates.io once stabilized.

```
distill/
├── Cargo.toml                      # workspace
├── README.md
├── LICENSE                         # MIT (matches Defuddle)
├── NOTICE                          # Defuddle attribution
├── crates/
│   ├── distill-core/               # Core library + Defuddle bridge
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs              # public API: Article, FetchOptions, ...
│   │       ├── article.rs          # Article struct, Markdown rendering
│   │       ├── metadata.rs         # YAML frontmatter, structured metadata
│   │       ├── error.rs            # typed errors (no string-matching)
│   │       ├── fetch/              # HTTP fetching
│   │       │   ├── mod.rs
│   │       │   ├── client.rs       # reqwest wrapper with retries
│   │       │   ├── robots.rs       # robots.txt parsing + cache
│   │       │   ├── rate_limit.rs
│   │       │   └── headless.rs     # opt-in JS rendering (Phase 2+)
│   │       ├── extract/            # Defuddle bridge
│   │       │   ├── mod.rs
│   │       │   ├── subprocess.rs   # Phase 1: spawn npx defuddle
│   │       │   ├── deno.rs         # Phase 2: embedded deno_core
│   │       │   ├── native.rs       # Phase 3: pure Rust (placeholder)
│   │       │   └── snapshot.rs     # V8 heap snapshot for fast startup
│   │       ├── markdown.rs         # HTML → Markdown via pulldown-html
│   │       ├── template.rs         # Tera-based templates
│   │       └── plugin.rs           # custom site extractor trait
│   ├── distill-cli/                # CLI binary
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs
│   │       ├── args.rs             # clap definitions
│   │       ├── batch.rs            # bulk mode
│   │       └── serve.rs            # HTTP API mode (axum)
│   ├── distill-mcp/                # Model Context Protocol server
│   │   ├── Cargo.toml
│   │   └── src/lib.rs
│   └── distill-defuddle-js/        # Bundled Defuddle JS for Phase 2
│       ├── Cargo.toml              # build.rs that bundles the JS
│       ├── build.rs
│       └── src/lib.rs              # exposes a const &[u8] of the bundle
├── examples/
│   ├── extract_url.rs
│   ├── batch.rs
│   └── as_mcp_tool.rs
├── tests/                          # integration tests
│   └── fixtures/                   # snapshot HTML pages
└── docs/
    ├── README.md                   # this plan, copied at v1.0
    ├── api/                        # rustdoc landing
    └── recipes/                    # how-tos
```

## Crate boundaries

### `distill-core`

The library every consumer depends on. Public API exposes:

```rust
pub struct Article {
    pub markdown: String,
    pub html_clean: String,
    pub metadata: ArticleMetadata,
    pub source_url: Option<String>,
    pub fetched_at: DateTime<Utc>,
}

pub struct ArticleMetadata {
    pub title: String,
    pub author: Option<String>,
    pub published: Option<DateTime<Utc>>,
    pub modified: Option<DateTime<Utc>>,
    pub description: Option<String>,
    pub site_name: Option<String>,
    pub language: Option<String>,
    pub word_count: u64,
    pub reading_time_minutes: u32,
    pub favicon: Option<String>,
    pub canonical_url: Option<String>,
    pub tags: Vec<String>,
    pub raw: serde_json::Value,  // Defuddle's full output
}

impl Article {
    pub async fn from_url(url: &str) -> Result<Self, DistillError> { ... }
    pub async fn from_url_with_options(url: &str, opts: FetchOptions) -> Result<Self, DistillError> { ... }
    pub async fn from_html(html: &str, base_url: Option<&str>) -> Result<Self, DistillError> { ... }
    pub async fn from_html_with_options(html: &str, opts: ExtractOptions) -> Result<Self, DistillError> { ... }

    pub fn to_markdown_with_frontmatter(&self) -> String { ... }
    pub fn to_json(&self) -> Result<String, DistillError> { ... }
    pub fn to_template(&self, tmpl: &Template) -> Result<String, DistillError> { ... }
}
```

`FetchOptions` covers the fetch path:

```rust
pub struct FetchOptions {
    pub timeout: Duration,
    pub user_agent: String,
    pub follow_redirects: bool,
    pub max_redirects: usize,
    pub respect_robots_txt: bool,
    pub javascript: bool,           // headless browser mode
    pub cookies: Option<CookieJar>,
    pub headers: HashMap<String, String>,
    pub language: Option<String>,   // Accept-Language preference
    pub max_size: usize,            // bytes; reject larger pages
    pub rate_limit: Option<RateLimit>,
}
```

`ExtractOptions` covers the extraction-only path:

```rust
pub struct ExtractOptions {
    pub markdown: bool,             // emit markdown (default true)
    pub metadata: bool,             // emit frontmatter (default true)
    pub keep_classes: Vec<String>,  // pass through to Defuddle
    pub remove_selectors: Vec<String>,
    pub site_extractor: Option<Box<dyn SiteExtractor>>,
}
```

Errors are typed; **no string-matching**. Mirrors anie's
`ProviderError` pattern:

```rust
pub enum DistillError {
    /// Fetch failed: network, DNS, TLS, etc.
    Fetch(reqwest::Error),
    /// HTTP non-success.
    HttpStatus { code: u16, body_excerpt: String },
    /// robots.txt forbids access.
    Forbidden(String),
    /// Defuddle parse / extraction failure.
    ExtractionFailed { reason: String },
    /// Page is too large.
    TooLarge { bytes: usize, max: usize },
    /// Timeout.
    Timeout,
    /// JS renderer failed (Phase 2+).
    HeadlessFailure(String),
    /// Site extractor's plugin returned an error.
    Plugin(Box<dyn std::error::Error + Send + Sync>),
    /// Defuddle subprocess / runtime error (Phase 1/2).
    Runtime(String),
    /// Output template rendering failed.
    Template(tera::Error),
}
```

### `distill-cli`

Thin layer over `distill-core`. Responsibilities:
- CLI parsing (`clap`)
- Output formatting (markdown / json / file / stdout)
- Batch mode
- HTTP server mode (`distill serve`)

### `distill-mcp`

Adapter exposing `distill` as a Model Context Protocol server.
A single binary subcommand: `distill mcp` (alternately a
separate `distill-mcp` binary). Implements MCP's `tools/list`
and `tools/call` for `browse_web`. See
[`04_extensibility.md`](04_extensibility.md).

### `distill-defuddle-js`

Build-time bundle of the Defuddle npm package + jsdom +
glue code. Built by `build.rs` running esbuild or rollup
once at compile time, producing a `&'static [u8]` constant
that `distill-core` embeds. Phase 2+.

## Crate dependencies

Sorted by purpose:

| Purpose | Crate |
|---------|-------|
| HTTP client | `reqwest` (async, multipart, gzip, brotli) |
| Async runtime | `tokio` |
| HTML parsing (light path) | `scraper`, `html5ever` |
| Markdown emission | `pulldown-cmark` (read), or our own walker |
| HTML→Markdown | `html2md` or `pulldown-html` |
| robots.txt | `texting_robots` (Rust port of Google's robots library) |
| URL handling | `url` |
| YAML frontmatter | `serde_yaml` |
| Templates | `tera` (Jinja-like, agent-friendly) |
| CLI | `clap` (derive macros) |
| Logging | `tracing`, `tracing-subscriber` |
| Errors | `thiserror` (typed errors), `miette` (CLI-friendly diagnostics) |
| Time | `chrono` or `time` |
| Config | `figment` or `config-rs` (CLI flags + env + file) |
| MCP | `rmcp` (official Rust MCP SDK), or hand-rolled JSON-RPC |
| Headless browser (Phase 2/3) | `headless_chrome` or `chromiumoxide` |
| JS embedding (Phase 2) | `deno_core` |
| HTTP server | `axum` |

All deps need to be on stable Rust, no nightly. All MIT/Apache
compatible.

## CLI UX in detail

```
distill <URL_OR_FILE>                 # default: markdown to stdout
distill <URL_OR_FILE> -o FILE         # write to file
distill <URL_OR_FILE> -o DIR/         # write to <DIR>/<slug>.md
distill <URL_OR_FILE> --json          # JSON output
distill <URL_OR_FILE> --html-clean    # clean HTML, no markdown conversion
distill <URL_OR_FILE> --no-frontmatter
distill <URL_OR_FILE> --template path/to/tmpl.tera
distill <URL_OR_FILE> --javascript    # opt-in JS render
distill <URL_OR_FILE> --timeout 30s
distill <URL_OR_FILE> --user-agent "..."
distill <URL_OR_FILE> --language en
distill <URL_OR_FILE> --no-robots     # opt-out of robots.txt check
distill <URL_OR_FILE> --max-size 10MB
distill <URL_OR_FILE> --quiet         # suppress non-error logging
distill <URL_OR_FILE> --debug         # verbose tracing

distill -                             # stdin: feed HTML directly

# Subcommands
distill batch URLS.txt --jobs 4 --output-dir DIR/
distill mcp                           # MCP server on stdio
distill serve --port 8080 [--bind 0.0.0.0]
distill version
distill check-deps                    # validates Phase-1 npx availability
```

Output design:

- **Default**: Markdown with YAML frontmatter, to stdout.
- **`--json`**: full structured JSON. Matches Defuddle's
  `output: 'json'` mode plus `markdown` and `frontmatter`
  fields.
- **Exit codes**: 0 success, 2 fetch failure, 3 extraction
  failure, 4 forbidden by robots, 5 too large, 6 timeout.
  Non-zero exits print error context to stderr in
  human-readable form (and to stdout JSON if `--json`).

Frontmatter shape (Obsidian-compatible):

```yaml
---
title: "Article title"
author: "Jane Doe"
published: 2024-08-15T10:30:00Z
description: "Lede or meta description"
source: https://example.com/article
site: example.com
language: en
word_count: 1234
reading_time: 6
tags:
  - rust
  - ai
fetched: 2026-04-26T15:23:01Z
distill_version: 0.1.0
---

# Article body...
```

## Library API in detail

The library is async-first. Synchronous wrappers can be added
later if there's user demand; agents are async by nature so
this is the right primary shape.

```rust
// Simplest case — one line.
let article = distill::Article::from_url("https://example.com").await?;

// Full options.
let opts = distill::FetchOptions::new()
    .timeout(Duration::from_secs(30))
    .user_agent("Mozilla/5.0 (compatible; my-agent/1.0)")
    .javascript(false)
    .respect_robots_txt(true)
    .max_size(5 * 1024 * 1024);
let article = distill::Article::from_url_with_options(url, opts).await?;

// Already have HTML.
let article = distill::Article::from_html(html, Some(base_url)).await?;

// Custom template (Tera).
let tmpl = distill::Template::from_str(r#"# {{ title }}

By {{ author | default(value="anonymous") }}

{{ markdown }}
"#)?;
println!("{}", article.to_template(&tmpl)?);

// Plugin: custom extractor for a specific site.
struct MyTwitterExtractor;
impl SiteExtractor for MyTwitterExtractor { ... }

let opts = ExtractOptions::default()
    .site_extractor(Box::new(MyTwitterExtractor));
let article = distill::Article::from_url_with_extract_options(url, opts).await?;
```

## Output: Markdown quality bar

Markdown emission must handle:

- Headings (h1-h6) preserved
- Paragraphs with proper line wrapping (configurable; default
  no wrap to keep diffs clean)
- **Bold** / *italic* / ~~strike~~ / `inline code`
- Code blocks with language hint (from `<pre><code class="language-X">`)
- Lists (ordered, unordered, nested, tight vs. loose)
- Tables with column alignment
- Blockquotes
- Footnotes (Defuddle exposes these; we keep them as
  Markdown footnotes `[^1]`)
- Math (KaTeX/MathJax delimiters preserved as `$...$` /
  `$$...$$`)
- Callouts (`> [!note]`-style if source uses them)
- Links (preserve URLs; don't strip)
- Images (preserve `![alt](url)`; alt text from source)
- Horizontal rules

Quality bar: pass through Markdown that re-renders cleanly in
Obsidian, GitHub, and Pandoc. We borrow Defuddle's
markdown-output mode and clean up only where needed.

## What lives where (summary)

| Concern | Crate | Module |
|---------|-------|--------|
| Public API | `distill-core` | `lib.rs` |
| Fetching (HTTP, robots, rate limit) | `distill-core` | `fetch/` |
| Defuddle bridge | `distill-core` | `extract/` |
| Markdown emission | `distill-core` | `markdown.rs` |
| Templates | `distill-core` | `template.rs` |
| CLI | `distill-cli` | `main.rs`, `args.rs` |
| MCP server | `distill-mcp` | `lib.rs` |
| Bundled JS (Phase 2) | `distill-defuddle-js` | build artifact |
| Headless rendering | `distill-core` | `fetch/headless.rs` |
| Site extractors (plugins) | user-defined | implement `SiteExtractor` trait |
