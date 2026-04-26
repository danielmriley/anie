# 02 — Phased implementation

Week-by-week build plan for the three phases. Each phase ends
with a usable, shippable milestone.

## Phase 1 — MVP via subprocess (Week 1-2)

**Goal:** ship a working `distill` CLI + library that an
agent can call. Imperfect performance and Node.js dependency
are acceptable trade-offs to validate the design.

### Week 1: scaffolding + fetch + subprocess bridge

**Day 1-2:**
- Initialize Cargo workspace + crates per
  `01_architecture.md`.
- Set up CI (GitHub Actions: cargo fmt, clippy, test, build
  on Linux/macOS).
- License (MIT) + NOTICE (Defuddle attribution).
- `distill-cli` skeleton with `clap` parsing
  (URL_OR_FILE positional + the headline flags).

**Day 3-4:**
- `fetch::client` module: `reqwest::Client` builder with
  sane defaults (gzip, brotli, follow redirects, default
  user-agent string `distill/0.1 (+https://...)`).
- `fetch::robots`: `texting_robots` crate, in-memory cache
  per host, `should_fetch(url, ua) -> bool`.
- `fetch::rate_limit`: simple per-host token bucket; default
  1 req/sec.
- Tests: mock `httpmock` server hitting various status codes,
  redirect chains, gzip responses, robots.txt enforcement.

**Day 5:**
- `extract::subprocess`: spawn `npx defuddle` with `--markdown`
  and `--frontmatter`, pipe stdin (HTML or URL?), parse stdout.
  Defuddle's CLI accepts a URL directly, so we pass the URL
  rather than fetching locally first — *for Phase 1 only*.
  Phase 2 unifies the fetch path.
- Discover Defuddle CLI's exact JSON schema; build a typed
  `DefuddleOutput` struct.
- Unit test: pin a few known-good extractions against a small
  set of public URLs (Wikipedia, a Substack post, a GitHub
  README rendered page).

### Week 2: API surface, output, agent integration

**Day 6-7:**
- `Article` struct + serde-friendly `ArticleMetadata`.
- Markdown rendering: take Defuddle's markdown output, prepend
  YAML frontmatter via `serde_yaml`. Inline link/footnote
  cleanup if needed.
- Tera template support: `--template tmpl.tera` flag.
- `--json` mode: full structured output including the raw
  Defuddle response.

**Day 8-9:**
- Library API polish: `Article::from_url`, `from_html`,
  `from_url_with_options`. Trait-bound errors (`thiserror`).
- `tracing` instrumentation at the right levels (info for
  fetch, debug for extract, trace for diagnostics).
- Integration tests via `httpmock` + serialized fixture pages.

**Day 10:**
- Documentation pass: rustdoc + examples + README.
- `examples/extract_url.rs`, `examples/batch.rs`,
  `examples/agent_tool.rs`.
- Ship Phase 1 to the user for feedback.

### Phase 1 exit criteria

- `distill https://en.wikipedia.org/wiki/Rust_(programming_language)`
  produces clean Markdown + frontmatter on a system with
  `npx defuddle` available.
- `cargo test` green; doc tests in lib.rs pass.
- Library API stable enough that anie can wire `distill` into
  `anie-tools` and use it.
- README documents the Node.js requirement explicitly
  (Phase 2 will remove it).

## Phase 2 — Embedded deno_core (Week 3-6)

**Goal:** drop the system Node dependency. Single static
binary. Same fidelity, faster per-call.

### Week 3: deno_core scaffold

**Day 11-12:**
- Add `deno_core` dependency. Build a minimal "hello world"
  test that runs JS from a Rust string and reads back a
  result. Confirm V8 startup latency on the target machine.
- Investigate V8 snapshots (`deno_core::Snapshot`) — these
  amortize V8 + bundled JS init across invocations.
  Critical for sub-100ms cold-start.

**Day 13-15:**
- Build `distill-defuddle-js`: a sibling crate whose
  `build.rs` runs esbuild on:
  - Defuddle (npm package, pinned version)
  - jsdom (or alternative — see notes)
  - Glue code that exposes a single `extractFromHtml(html,
    url) -> { markdown, frontmatter, raw }` function
- Output: a single bundled JS file → embedded as `&'static
  [u8]` in the crate.
- Decide: bundle jsdom (~300 KB) or use a leaner DOM lib.
  Defuddle's repo notes which DOM features it uses; if it
  works against `domhandler` + `htmlparser2` we save
  significant binary weight.

**Day 16:**
- V8 snapshot generation. Build a `Snapshot` containing
  `JsRuntime` + the bundled module pre-loaded. Measure
  cold-start. Goal: < 50 ms on a modern laptop.

### Week 4: extract::deno bridge

**Day 17-19:**
- `extract::deno::DenoExtractor` struct. Owns a
  `deno_core::JsRuntime` initialized from the snapshot.
- `fn extract(&mut self, html: &str, url: Option<&str>) ->
  Result<DefuddleOutput>` — calls into the JS via
  `JsRuntime::execute_script` + `op` registration for any
  Rust-callable functions Defuddle's bundle needs.
- Pool design: `Arc<Mutex<Vec<DenoExtractor>>>` for
  multi-threaded callers (one extractor per concurrent call;
  V8 isolates aren't `Send`).
- Unit tests against fixture HTML.

**Day 20-22:**
- Wire `extract::deno` as a config-toggleable backend
  alongside `extract::subprocess`. Default switches to deno
  in Phase 2; subprocess available via env var
  (`DISTILL_BACKEND=subprocess`) for debugging.
- Compare outputs between subprocess and deno backends on
  the fixture pages — must be byte-identical (or trivially
  different) before switching the default.

### Week 5: fetching + JS render

**Day 23-25:**
- Move fetching fully into Rust (it was already in Phase 1;
  this is just enforcement). The Phase 1 path of "let
  Defuddle fetch the URL itself" is removed.
- Headless browser support behind `--javascript` flag:
  `chromiumoxide` (puppeteer-like async API, good Rust
  integration). Renders the page, returns final HTML
  string, hands off to extract.
- Test against a simple SPA fixture (a Vite app or similar).
- Fall back gracefully: if Chrome isn't installed,
  `--javascript` returns a typed error suggesting
  installation steps; default `distill` invocation never
  requires Chrome.

**Day 26:**
- Cookie support: `reqwest::cookie::Jar` plumbed through
  `FetchOptions`. CLI flag `--cookies-file` accepts Netscape
  cookie format.

### Week 6: polish + release

**Day 27-28:**
- Performance pass. Goals (P50, modern laptop):
  - URL fetch + extract (no JS): < 500 ms total.
  - Extract-only on already-fetched HTML: < 50 ms.
  - V8 cold-start: < 50 ms (snapshot kicks in).
- Profile with `samply` or `perf`. Likely hot spots are V8
  init and the markdown post-processing.

**Day 29:**
- Distribution: build matrix (Linux x86_64, Linux aarch64,
  macOS arm64, macOS x86_64). Static linking where
  possible. GitHub Releases artifacts.
- Homebrew formula. Cargo install (`cargo install distill-cli`).

**Day 30:**
- Release v0.2.0 ("production"). Update anie's
  `anie-tools/Cargo.toml` to depend on distill-core 0.2.

### Phase 2 exit criteria

- Single static binary, ≤ 80 MB.
- Zero system runtime dependencies for the default flow.
- `--javascript` works against a known SPA when Chrome is
  installed.
- Output byte-identical to subprocess backend on a 50-URL
  test corpus.
- P50 latency targets met.
- crates.io publish: `distill-core`, `distill-cli`,
  `distill-mcp`.

## Phase 3 — Native Rust (open-ended)

**Trigger before starting:** one or more of:
- `defuddle-rs` reaches sufficient maturity that we'd be
  improving rather than starting fresh.
- Binary size complaints from real users.
- Per-call latency is a measurable bottleneck for an agent
  workflow we can point at.

**Approach when triggered:**

1. Audit `defuddle-rs` (and `trek-rs` if relevant). Is it
   close enough to upstream that we can fork and contribute?
2. Identify the hot paths first — fetch and Markdown
   conversion are already pure Rust in Phase 2; the JS
   bridge is the only thing left to replace.
3. Phase the migration: keep deno backend as a fallback
   under an env var, switch default to Rust extractor for
   sites where it's known-good, fall back to deno otherwise.
4. Drive coverage by adding sites to a "Rust backend
   verified" allowlist. When the allowlist covers ~95% of
   real-world traffic, retire deno.

This is a marathon, not a sprint. Don't start it without a
clear external trigger.

## Cross-cutting work (ongoing)

These items run alongside the phases:

### Testing strategy

- **Unit tests** in each module: fetch (mock HTTP), robots
  (fixture robots.txt strings), markdown (golden fixtures),
  extract (golden fixtures of Defuddle output).
- **Integration tests**: a small corpus of HTML files
  (Wikipedia, Substack, NYTimes article snapshot, GitHub
  README, Reddit thread, X thread, YouTube transcript page)
  with expected markdown checked in. Asserts on shape, not
  exact bytes — Defuddle improvements should not silently
  break tests.
- **Snapshot tests** for the YAML frontmatter and JSON
  output (use `insta` crate).
- **Cross-backend tests** in Phase 2: assert subprocess and
  deno produce identical output for the corpus.
- **Property tests** for fetch retry logic (`proptest`):
  random HTTP 4xx / 5xx sequences should produce expected
  retry behavior.

### CI

- `cargo fmt --check`, `cargo clippy -D warnings`, `cargo
  test --workspace`.
- Build matrix: Linux/macOS, x86_64/aarch64, stable Rust.
- Phase 2: also build with `--features bundled-defuddle` to
  ensure the JS bundling step works.
- `cargo deny` check for license / advisory issues.

### Documentation

- rustdoc with examples on every public item. The
  `Article::from_url` doctest should be runnable.
- `README.md` with a 10-line "Hello World" + a 60-second
  setup.
- `docs/recipes/` for common patterns: extract for an
  Obsidian vault, integrate into an MCP server, custom
  templates, batch crawl with rate limit.

### Release cadence

- Phase 1 → v0.1 (Week 2)
- Phase 2 → v0.2 (Week 6) — first production release
- Bug fixes → v0.2.x
- Phase 3 (when triggered) → v0.3
- Stable API → v1.0 once we're happy with the public surface

Use semver strictly. The library API is the contract; the
CLI flags are the contract for scripting users.
