# distill — Rust browser/reader tool plan

> **Status (2026-04-26): superseded.** This plan described
> `distill` as a *standalone* Rust crate / CLI tool with its
> own workspace and distribution. After review, the user
> redirected the work to a *native anie tool* that lives
> inside anie's workspace and shells out to Defuddle via
> system install rather than vendoring or embedding a JS
> runtime. The replacement plan is at
> [`../../web_tool_2026-04-26/`](../../web_tool_2026-04-26/).
>
> This doc is kept under `docs/completed/` as a record of the
> standalone-tool design study. Most of the technical
> reasoning (the four-path evaluation, the V8 snapshot
> approach, the Tera template structure) carries over to the
> native plan; what changed is the *product shape* — anie
> tool, not free-standing crate.

Date: 2026-04-26
Branch: `plan/browser-tool` (off main)
Status: superseded

## What we're building

A production-grade, single-binary Rust tool that fetches a web
page, extracts the main readable content using
**Defuddle's algorithm**, and emits LLM-friendly Markdown plus
structured metadata. Usable both as a CLI for direct
invocation and as a library crate for embedding in Rust AI
agents (anie included).

> *"`distill <url>` and you get the article."*

The tool is **not** another generic scraper. The value
proposition is reader-mode quality content extraction — Defuddle
is the heart of it — wrapped in Rust ergonomics: typed APIs,
single binary, clean error model, agent-tool friendly output.

## Recommended name

**`distill`** — short, evocative, the verb describes the
action exactly. The CLI reads naturally: `distill
https://example.com`. Library: `distill::Article::from_url(...)`.

Backup options if `distill` is taken on crates.io:
- `legible` — emphasizes the output
- `marrow` — the essential content
- `crisp` — speed + clarity
- `clearread` — reader mode literally

For the rest of this plan I'll call it `distill`.

## Recommended approach (TL;DR)

**Hybrid with a phased migration.** Path 2 (subprocess) for
Phase 1, Path 3 (embedded JS via `deno_core`) for Phase 2,
Path 1 (native Rust) as an opportunistic Phase 3 if a mature
port emerges or if we hit a per-call cost ceiling.

| Phase | Implementation | Why |
|------|----------------|-----|
| **1 — MVP** | Subprocess `npx defuddle` with structured stdout parsing | Ship usable tool fast; validate API design and Markdown quality without committing to a JS runtime decision |
| **2 — Production** | Embed `deno_core` (V8) and the bundled Defuddle JS module | Single static binary, no system Node required, perfect Defuddle fidelity, ~5–15 ms per-call after warm-up |
| **3 — Native (optional)** | Port hot paths to Rust if `defuddle-rs` matures, or reimplement the core scoring algorithm | Eliminates V8 binary overhead (~50 MB) if size matters more than fidelity |

**Why not "native Rust port" first** (Path 4): Defuddle has
hundreds of site-specific extractor rules and ongoing upstream
development. A native port from day 1 is a multi-month effort
that locks us into perpetual catch-up, *and* loses fidelity
the moment Kepano ships an improvement we haven't ported.

**Why not "subprocess forever"** (Path 2 alone): a hard
runtime dependency on Node.js conflicts with the
"production-ready single-binary" goal. Per-invocation `npm`
spawn cost is also 100–500 ms — fatal for an agent that calls
the tool dozens of times per turn.

**Why not WASM**: Defuddle uses jsdom internally. Compiling
jsdom to WASM is significant work and loses the upstream
parity that's the whole reason we want Defuddle. Revisit if
Defuddle ever ships an official WASM build.

The full evaluation is in
[`00_path_evaluation.md`](00_path_evaluation.md).

## Files in this folder

- [`00_path_evaluation.md`](00_path_evaluation.md) — detailed
  comparison of all four paths against the user's stated
  requirements.
- [`01_architecture.md`](01_architecture.md) — workspace
  layout, crate boundaries, library API, CLI UX.
- [`02_phased_implementation.md`](02_phased_implementation.md)
  — week-by-week build plan for phases 1, 2, 3.
- [`03_edge_cases.md`](03_edge_cases.md) — paywalls, JS-heavy
  sites, social media (X/Reddit/YouTube), infinite scroll.
- [`04_extensibility.md`](04_extensibility.md) — plugin
  system, MCP server adapter, custom templates, Obsidian-
  style frontmatter, agent integration patterns.
- [`05_operations.md`](05_operations.md) — performance
  budget, security model, licensing, distribution.

## Quick CLI mockup

```bash
# Common case: fetch URL, print markdown + frontmatter to stdout
distill https://example.com/article

# JSON output for tool-calling
distill https://example.com/article --json

# Save with default filename derived from title
distill https://example.com/article -o ./

# Read local HTML file
distill ./page.html

# Read from stdin
curl https://example.com | distill -

# JS-heavy site: render with headless browser first
distill https://app.example.com --javascript

# Bulk mode — read URLs from file, parallelize
distill batch urls.txt --jobs 4 --output-dir ./articles/

# Run as MCP server for Claude / agents
distill mcp

# Run as HTTP API for shared services
distill serve --port 8080
```

## Quick library mockup

```rust
use distill::{Article, FetchOptions};

// Default: fetch + extract.
let article = Article::from_url("https://example.com/x").await?;
println!("{}", article.markdown);
println!("title: {}", article.metadata.title);

// Custom options: timeout, JS rendering, user agent.
let article = Article::from_url_with_options(
    "https://example.com/x",
    FetchOptions::default()
        .timeout(Duration::from_secs(30))
        .javascript(true)
        .user_agent("MyAgent/1.0"),
).await?;

// Already have HTML — skip fetching.
let article = Article::from_html(
    raw_html,
    Some("https://example.com/x"),  // base URL for relative links
).await?;
```

## Integration with anie

`anie-tools` already has a tool registry (`Bash`, `Read`,
`Edit`, etc.). Adding `distill` is a single new tool:

```rust
// crates/anie-tools/src/web.rs
pub struct DistillTool;

impl Tool for DistillTool {
    const NAME: &str = "browse_web";
    const DESCRIPTION: &str = "Fetch a URL and return clean Markdown + metadata.";

    async fn execute(&self, args: BrowseWebArgs) -> Result<ToolOutput> {
        let article = distill::Article::from_url(&args.url).await?;
        Ok(ToolOutput::Text(article.markdown))
    }
}
```

`distill` lives as its own workspace (separate repo
recommended; can be vendored into anie if we want lockstep
versions). See
[`01_architecture.md`](01_architecture.md) for the
discussion.

## Principles for this round

- **Fidelity over re-implementation.** Defuddle is the value.
  Don't lose the upstream improvements.
- **Single binary distribution.** The tool ships as one
  static binary; no Node, no Python, no system runtime.
- **AI-friendly output.** Markdown is the lingua franca of
  modern LLM agents. JSON for structured tooling. Both
  available from a single invocation.
- **Production-grade error handling.** Typed errors (no
  string-matching), structured logging, retry hooks for
  transient failures.
- **Polite by default.** Respects robots.txt, sane user-agent,
  reasonable rate limits — without making the user configure
  any of it.
- **Extensible without becoming a framework.** Pluggable
  fetchers, custom extractors for specific sites, template
  overrides for output. But each extension point is small
  and documented; no dependency-injection circus.
