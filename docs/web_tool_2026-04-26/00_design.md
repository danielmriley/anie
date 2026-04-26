# 00 — Design

The architectural shape: where the tool lives, how it talks
to Defuddle, what its contract with the agent looks like, how
errors surface, and what the user-visible dependencies are.

## Where it lives in the workspace

A new sub-crate under `crates/`:

```
crates/
├── anie-agent/
├── anie-auth/
├── anie-cli/
├── anie-config/
├── anie-protocol/
├── anie-provider/
├── anie-providers-builtin/
├── anie-session/
├── anie-tools/             # bash, edit, read, write, grep, find, ls
├── anie-tools-web/         # NEW: web_read + web_search
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs          # public Tool registrations
│       ├── error.rs        # WebToolError taxonomy
│       ├── read/
│       │   ├── mod.rs      # WebReadTool impl
│       │   ├── fetch.rs    # reqwest + robots.txt + SSRF guard
│       │   ├── extract.rs  # Defuddle subprocess bridge
│       │   └── frontmatter.rs  # YAML metadata emission
│       └── search/
│           ├── mod.rs      # WebSearchTool impl
│           └── ddg.rs      # DuckDuckGo HTML scrape backend
└── anie-tui/
```

**Why a separate crate** instead of folding into `anie-tools`:

- Web-specific deps (`reqwest` is already a workspace dep,
  but `texting_robots`, `chromiumoxide` for `--javascript`,
  etc. are new) shouldn't bloat `anie-tools` for users who
  compile out the web feature.
- Cleaner `cargo deny` story for the network-touching code:
  it lives behind one crate boundary.
- If we later split anie's release into "lean" and "full"
  variants (workspace already supports this via cargo
  features), the tool crate is the natural seam.

**Why not a totally separate workspace** (the standalone
plan): user clarified the tool is for anie specifically. No
need for crates.io publishing, no need for an independent CLI,
no need for the MCP adapter (anie itself integrates with MCP
elsewhere if we want that).

## Tool registration

`anie-tools-web` exposes one function:

```rust
// crates/anie-tools-web/src/lib.rs
use anie_agent::Tool;

pub fn web_tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(read::WebReadTool::default()),
        Box::new(search::WebSearchTool::default()),
    ]
}
```

`anie-cli/src/bootstrap.rs` registers them alongside the
existing tools, gated by config:

```rust
fn build_tool_registry(cwd: &Path, ...) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(bash_tool());
    registry.register(read_tool());
    // ... existing tools

    if config.tools.web.enabled {
        for tool in anie_tools_web::web_tools() {
            registry.register(tool);
        }
    }

    Arc::new(registry)
}
```

Config knob in `anie.toml`:

```toml
[tools.web]
enabled = true                  # default: true on full builds, n/a on lean

# web_read settings
read_timeout = "30s"
read_user_agent = "anie/0.x (+https://github.com/.../anie)"
read_max_size_mb = 10
read_respect_robots_txt = true
read_javascript = false          # default for the tool's own default arg

# web_search settings
search_default_max_results = 10
search_backend = "duckduckgo"   # only option for v1

# rate limiting (per-host, applies to read + search)
rate_limit_rps = 1.0
rate_limit_burst = 5
```

Tool execution path is unchanged from anie's existing tools:
agent calls `web_read` with `{"url": "..."}`, registry
dispatches to `WebReadTool::execute`, the result becomes a
tool message in the conversation.

## How web_read uses Defuddle

For Phase 1, **subprocess only**. anie's repo has zero
JavaScript. The integration is one Rust function that spawns
the `defuddle` binary, pipes the URL, captures stdout, parses
JSON, returns Markdown.

```text
agent calls web_read(url)
  └─> WebReadTool::execute(args)
      ├─> validate_url(args.url)              # SSRF guard, scheme check
      ├─> robots_check(args.url)              # texting_robots cache
      ├─> rate_limit_acquire(host)            # per-host token bucket
      ├─> if args.javascript: render_with_chrome(url) → html
      │   else: reqwest_fetch(url) → html
      ├─> extract::run_defuddle(html, url) → DefuddleOutput
      │     (spawn `defuddle` subprocess, write html to stdin,
      │      parse stdout JSON)
      ├─> frontmatter::build(metadata) → yaml
      └─> ToolResult::Text(yaml + "\n\n" + markdown)
```

The Defuddle subprocess wrapper:

```rust
// crates/anie-tools-web/src/read/extract.rs

const DEFUDDLE_VERSION: &str = "0.6.x";  // pinned, bumped in lockstep with tests

pub async fn run_defuddle(html: &str, source_url: &str)
    -> Result<DefuddleOutput, WebToolError>
{
    let cmd_path = locate_defuddle()?;  // PATH lookup, fall through to npx
    let mut child = tokio::process::Command::new(&cmd_path.binary)
        .args(&cmd_path.args)
        .args(&["--url", source_url, "--markdown", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| WebToolError::DefuddleSpawn(e.to_string()))?;

    let mut stdin = child.stdin.take().expect("piped");
    tokio::io::AsyncWriteExt::write_all(&mut stdin, html.as_bytes()).await?;
    drop(stdin);

    let output = child.wait_with_output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(WebToolError::DefuddleFailed {
            exit_code: output.status.code(),
            stderr: stderr.to_string(),
        });
    }
    serde_json::from_slice(&output.stdout)
        .map_err(WebToolError::DefuddleOutputParse)
}

struct DefuddleCmd {
    binary: PathBuf,
    args: Vec<String>,
}

fn locate_defuddle() -> Result<DefuddleCmd, WebToolError> {
    // 1. Direct binary on PATH (fastest).
    if let Ok(path) = which::which("defuddle") {
        return Ok(DefuddleCmd { binary: path, args: vec![] });
    }
    // 2. Fall through to `npx defuddle@<pin>` if `npx` is on PATH.
    if let Ok(npx) = which::which("npx") {
        return Ok(DefuddleCmd {
            binary: npx,
            args: vec![
                "--yes".into(),  // auto-install on first run
                format!("defuddle@{}", DEFUDDLE_VERSION),
            ],
        });
    }
    // 3. Neither available — clear error with install instructions.
    Err(WebToolError::DefuddleNotFound)
}
```

`WebToolError::DefuddleNotFound` surfaces to the agent as a
helpful tool-output message, not a panic:

```
Tool 'web_read' is unavailable: defuddle is not installed.

To enable web reading, install Node.js (https://nodejs.org/)
and then run: npm i -g defuddle-cli

Or install via npx (no global install needed): the tool will
auto-fetch defuddle on first use, but each call adds ~200ms
of resolution overhead.
```

Pinning: `DEFUDDLE_VERSION` stays at a tested-against value in
the source. Bumping the version is a deliberate change with
test verification, not a passive consequence of the user's
local install.

## Tool contract: web_read

The schema the agent sees (via the existing tool-arg JSON
schema generation):

```json
{
  "name": "web_read",
  "description": "Fetch a URL and return its main content as clean Markdown with YAML frontmatter metadata (title, author, date, etc.). Use this for reading articles, documentation, blog posts, and similar content. Pass javascript=true for SPA / heavily JS-rendered pages (slower, requires Chrome).",
  "input_schema": {
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
        "description": "Render JavaScript before extracting. Set to true for SPAs or pages that show 'enable JavaScript' on first fetch. Requires Chrome/Chromium installed."
      }
    },
    "required": ["url"]
  }
}
```

Output shape: a single `String` containing YAML frontmatter
followed by Markdown body.

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
fetched: 2026-04-26T15:23:01Z
---

# Article body...
```

Why a single String and not structured JSON: matches how
anie's other tools return content, lets the agent consume it
without an extra deserialization step in the prompt template,
and the YAML frontmatter is itself parseable if the agent
needs structured access.

## Error taxonomy

```rust
// crates/anie-tools-web/src/error.rs
#[derive(thiserror::Error, Debug)]
pub enum WebToolError {
    /// User passed a malformed URL.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    /// URL scheme not supported (we accept http/https only).
    #[error("unsupported URL scheme: {0}")]
    UnsupportedScheme(String),

    /// URL points to a private / loopback / link-local IP and
    /// `allow_private_ips` is false. SSRF defense.
    #[error("URL resolves to a private address: {0}")]
    PrivateAddress(String),

    /// robots.txt forbids access to this URL.
    #[error("robots.txt forbids access to {0}")]
    Forbidden(String),

    /// Network / fetch failure.
    #[error("fetch failed: {0}")]
    Fetch(String),

    /// HTTP non-success.
    #[error("HTTP {code}: {body_excerpt}")]
    HttpStatus { code: u16, body_excerpt: String },

    /// Page exceeded `max_size_mb`.
    #[error("page size {bytes} exceeds max {max} bytes")]
    TooLarge { bytes: usize, max: usize },

    /// Fetch / extract / render exceeded the configured timeout.
    #[error("timed out after {seconds}s")]
    Timeout { seconds: u64 },

    /// Headless Chrome failed (only when javascript=true).
    #[error("headless render failed: {0}")]
    HeadlessFailure(String),

    /// `defuddle` and `npx` both unavailable.
    #[error("defuddle is not installed")]
    DefuddleNotFound,

    /// Spawning the defuddle subprocess failed.
    #[error("failed to spawn defuddle: {0}")]
    DefuddleSpawn(String),

    /// Defuddle exited non-zero.
    #[error("defuddle exited non-zero ({exit_code:?}): {stderr}")]
    DefuddleFailed { exit_code: Option<i32>, stderr: String },

    /// Defuddle's JSON output failed to parse.
    #[error("failed to parse defuddle output: {0}")]
    DefuddleOutputParse(#[from] serde_json::Error),

    /// IO error during subprocess plumbing.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Search backend (DDG, etc.) failed.
    #[error("search backend failed: {0}")]
    SearchBackend(String),
}
```

Surfacing to the agent: each variant has a human-readable
`Display` that maps to the tool's text output. The agent sees
e.g. `"HTTP 403: Cloudflare block page detected"` and can
decide whether to retry, search elsewhere, or report back to
the user. No string-matching on error messages — the agent
consumes them as text but anie's internal retry/circuit-break
logic uses the typed variant.

## Fetch path details

### URL validation + SSRF guard

```rust
fn validate_url(raw: &str, allow_private: bool) -> Result<Url, WebToolError> {
    let url = Url::parse(raw).map_err(|e| WebToolError::InvalidUrl(e.to_string()))?;
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(WebToolError::UnsupportedScheme(other.into())),
    }
    if !allow_private {
        // Resolve host, check IPs against private/loopback/etc.
        // Use std::net::IpAddr::is_loopback / .is_private (etc.)
        // We resolve EARLY so that DNS rebinding can't bypass.
    }
    Ok(url)
}
```

`allow_private_ips` defaults to `false`. Set via config
`[tools.web] allow_private_ips = true` for users who
explicitly want to read internal docs servers.

### robots.txt

`texting_robots` crate. In-memory LRU keyed by host, populated
on first request per host. TTL 1 hour. Override via
`respect_robots_txt = false` config or `--no-robots` agent
arg (intentionally absent from the default tool schema —
the user has to configure it; the agent can't bypass).

### Rate limiting

Per-host token bucket via the `governor` crate (already
familiar shape from anie's other rate-limited paths). Default
1 req/sec, burst 5. Configurable via `[tools.web]`.

### HTTP fetching

`reqwest::Client` with:

- Default timeout from config (30s).
- gzip + brotli enabled.
- Follow redirects up to 10 hops.
- User-agent from config (default `anie/<version> (+repo URL)`).
- Bounded body via `max_size_mb`. Read incrementally; bail when
  the byte counter exceeds the cap.

### `javascript: true` path

Shells out to a headless Chrome via `chromiumoxide`. The
binary is found via:

1. `CHROME_PATH` env var
2. `which::which("chromium") | which::which("chrome") | which::which("google-chrome")`
3. macOS standard install path
4. Fail with `WebToolError::HeadlessFailure("chrome not found")`

We don't bundle Chrome. The headless library uses the system
install. Same dependency-shape rationale as Defuddle:
explicit, documented, optional.

Render flow: launch a fresh Chrome instance (or reuse from a
pool if we add one later), navigate, wait for `networkidle2`
or 5s timeout, capture DOM, hand HTML to Defuddle.

## What we're explicitly not building

- **Cookie management.** `cookies_file` flag in the
  standalone plan was useful for paywalls; the agent context
  doesn't need it. Add later if a real workflow requires it.
- **Custom site extractors as plugins.** Any per-site fixes
  go upstream into Defuddle. anie doesn't grow a plugin
  system for this.
- **Output templates.** Markdown-with-frontmatter is the
  output. If users want custom formatting, they prompt the
  agent to reformat.
- **Multiple Defuddle backends pre-Phase 2.** No deno_core
  dance in v1. If the standalone-plan single-binary
  motivation comes back, we add `--features web-embedded`
  later — but the default is subprocess.

## Phase 2+ stretches (not promised)

- **`--features web-embedded`** — pulls in `deno_core`,
  bundles Defuddle JS at build time, removes the Node
  prereq for users who want the single-binary deploy story.
  Trigger to revisit: real user reports that Node is a
  blocker. Cost: ~50 MB binary growth, gated behind cargo
  feature so opt-in only.
- **Native `defuddle-rs` integration** — drops Node entirely
  if a Rust port reaches feature parity. Same opt-in
  feature gate.
- **Image / asset fetch** — agent might ask for the article
  alongside images. Out of scope; agent can extract image
  URLs from the markdown and call `web_read` on them
  separately if needed.
- **Streaming output** — unlikely to be useful for an LLM
  context window; skip until requested.

## Dependency summary

For a default `cargo build` with `--features web` (the
recommended config):

| Dep | Source | Purpose | Optional? |
|-----|--------|---------|-----------|
| `reqwest` | crates.io (already in workspace) | HTTP | no |
| `tokio` | already in workspace | async runtime | no |
| `texting_robots` | crates.io | robots.txt parsing | no |
| `governor` | crates.io | rate limit | no |
| `which` | crates.io | locate binaries on PATH | no |
| `url` | already in workspace | URL parsing | no |
| `serde_yaml` | already in workspace | frontmatter | no |
| `chromiumoxide` | crates.io | headless render | optional (`web-headless` feature) |
| `chrono` | already in workspace | date formatting | no |
| **Defuddle CLI** | npm (system) | content extraction | runtime dep, not Rust |
| **Chromium / Chrome** | system | JS rendering when `javascript=true` | runtime dep, only for that path |

For lean builds (`cargo build --no-default-features` or
`--no-default-features --features <whatever-else>`):

- `anie-tools-web` is not compiled in.
- No web tools registered.
- Agent doesn't see `web_read` or `web_search` in its tool
  list.
- Zero web-specific dep weight.
