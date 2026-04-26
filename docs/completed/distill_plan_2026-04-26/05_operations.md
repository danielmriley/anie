# 05 — Operations: performance, security, licensing, distribution

The non-functional concerns. Each gets its own concrete
target and its own enforcement mechanism.

## Performance budget

End-to-end latency targets at P50 on a modern dev laptop
(M2 Mac or equivalent, fast residential connection):

| Operation | Target P50 | Hard ceiling P99 |
|-----------|-----------:|-----------------:|
| HTML-only extraction (already have HTML) | 50 ms | 200 ms |
| URL → Markdown (server-rendered page) | 500 ms | 2 s |
| URL → Markdown (`--javascript`, simple SPA) | 3 s | 10 s |
| V8 cold start (Phase 2) | 50 ms | 150 ms |
| MCP `tools/call` overhead | < 5 ms beyond extraction | < 20 ms |

How we hit them:

- **V8 snapshot** at compile time. `deno_core::Snapshot` with
  Defuddle bundle pre-loaded means an initialized JsRuntime
  comes up in tens of ms instead of hundreds.
- **Extractor pool**. `Arc<Mutex<Vec<DenoExtractor>>>` so
  concurrent calls don't pay the V8 init cost. Pool size
  defaults to `num_cpus` and grows on demand.
- **DNS / connection pooling**. `reqwest::Client` is reused
  across calls; HTTP/2 keep-alive amortizes TLS handshakes.
- **Robots.txt cache**. In-memory LRU keyed by host;
  per-process cache so a CLI invocation re-checks robots.txt
  per call, but `serve` mode amortizes.
- **Streaming markdown emission**. The Defuddle output isn't
  huge, but markdown templating with Tera is fast enough
  that we don't cache it.

How we measure:

- `criterion` benchmarks for the hot paths:
  - HTML→Markdown only
  - V8 cold start
  - Pool-warm extraction
- Real-page integration timing test: 50 URLs across the corpus,
  capture P50/P95/P99.
- Built-in tracing: `--debug` prints per-stage timing
  (fetch / extract / markdown / template).

How we enforce:

- CI runs the criterion benches on every PR. > 10% regression
  fails the build.
- `distill bench` subcommand for power users to validate
  their environment.

## Security model

`distill` fetches arbitrary URLs and parses arbitrary HTML.
The threat surface is real.

### Threats we defend against

| Threat | Defense |
|--------|---------|
| **SSRF** (caller asks distill to fetch internal/private URL) | Default deny for RFC 1918 / loopback / link-local IPs. `--allow-private-ips` opt-in. |
| **Resource exhaustion** (huge pages, slow loris) | `max_size`, `timeout`, max-redirects all bounded by default. |
| **Decompression bomb** | reqwest decompression is bounded by `max_size`. |
| **HTML parser bombs** (deeply nested DOM, billion laughs) | Defuddle / jsdom have hardening; we add a `max_html_bytes` ceiling. |
| **Cookie exfiltration via redirects** | reqwest doesn't auto-leak cookies cross-origin. We pass `CookieJar` only to the original origin. |
| **Headless Chrome RCE** (in `--javascript` mode) | Chromium runs as a subprocess in sandbox mode (`--no-sandbox` is *not* passed). User must have Chrome installed; we don't ship it. |
| **JS bundle compromise** (Phase 2) | The Defuddle JS bundle is pinned at compile time and shipped in the binary; `cargo audit` for the npm chain via the `build.rs` lockfile. |
| **TLS MITM** | reqwest uses `rustls` with the system or webpki root store; no custom cert overrides without explicit env var. |

### Threats we don't defend against (and won't)

- **Malicious site exploiting Chromium 0-days** — that's
  Chromium's problem. Users running `--javascript` against
  hostile sites accept this risk.
- **Side-channel attacks via timing** — out of scope.
- **Compromise of crates.io or npm registry** — we trust the
  supply chain; `cargo-audit` and `Cargo.lock` are our
  defense.

### Privacy

- No telemetry by default. Period.
- `--debug` logs full URLs and response sizes; user controls
  log destination.
- `serve` mode logs request URLs to stdout in standard
  Common Log Format; document this.

### URL validation

```rust
fn validate_url(url: &Url, allow_private: bool) -> Result<(), DistillError> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(DistillError::UnsupportedScheme);
    }
    if !allow_private {
        let host = url.host().ok_or(DistillError::InvalidUrl)?;
        if host_is_private(&host) {
            return Err(DistillError::PrivateAddress);
        }
    }
    Ok(())
}
```

`host_is_private` checks RFC 1918, loopback, link-local,
multicast, and `*.localhost` / `*.local` mDNS. For DNS names
that resolve to private IPs, we re-check after resolution
(IDN attacks).

## Licensing

| Component | License | Compatibility |
|-----------|---------|---------------|
| `distill-core`, `distill-cli`, etc. | **MIT** | Matches Defuddle |
| Defuddle (npm package, bundled in Phase 2) | MIT | Mirror in NOTICE |
| jsdom (bundled in Phase 2) | MIT | Mirror in NOTICE |
| `reqwest`, `tokio`, etc. | MIT/Apache-2.0 dual | Compatible |
| `deno_core` | MIT | Compatible |
| `chromiumoxide` | MIT/Apache-2.0 | Compatible |
| Chromium (runtime dep for `--javascript`) | BSD-3-Clause + LGPL bits | Not bundled — user installs |

NOTICE file lists every bundled JS dependency with its
license. `cargo-deny` enforces our allowlist:

```toml
# deny.toml
[licenses]
allow = ["MIT", "Apache-2.0", "BSD-3-Clause", "ISC", "Unicode-DFS-2016", "MPL-2.0"]
copyleft = "deny"
```

Defuddle attribution: README + LICENSE-NOTICE + a `--credits`
CLI flag that prints the bundled-component versions and
licenses.

## Distribution

### Binaries

GitHub Releases per tag. Build matrix:

- `distill-x86_64-unknown-linux-gnu`
- `distill-aarch64-unknown-linux-gnu`
- `distill-x86_64-unknown-linux-musl` (statically linked)
- `distill-aarch64-apple-darwin`
- `distill-x86_64-apple-darwin`
- `distill-x86_64-pc-windows-msvc` (Phase 2.5; needs the
  Windows-compat work for the workspace)

Checksums + Sigstore signatures. SBOM generated by
`cargo-cyclonedx`.

### Package managers

- **Homebrew**: `brew tap distill-rs/tap && brew install
  distill` (formula in `homebrew-tap` repo).
- **Cargo**: `cargo install distill-cli`.
- **Nix**: flake at `flake.nix`, package goes into `nixpkgs`
  once stable.
- **Arch**: AUR package `distill-bin`.

### Containers

`ghcr.io/distill-rs/distill:latest` — Alpine-based image with
`distill` pre-installed. Useful for `distill serve` deployments
and CI integrations.

For the Phase 2 binary that includes V8: image is ~80 MB
compressed (mostly the V8 .text section).

## Versioning

Strict semver. The library API is the contract:

- **MAJOR**: any breaking change to public types or function
  signatures.
- **MINOR**: new features, additive only.
- **PATCH**: bug fixes, no behavior change.

CLI flags follow a parallel contract:

- Renaming an existing flag = MAJOR.
- Adding a flag = MINOR.
- Changing default behavior = MAJOR.

`distill --version` prints both: lib version, CLI version,
and bundled Defuddle version.

## Observability

`tracing` subscriber installed by the CLI. Default level is
WARN. `--quiet` is ERROR. `--debug` is DEBUG (sets per-target
filters so we get distill's traces but not reqwest's flood).

Structured logging in JSON via `--log-format json` for
production deployments. Goes to stderr; the article output
remains stdout.

Built-in spans for the hot stages so external profilers and
tracing collectors see the structure:

- `distill.fetch` (URL, status, elapsed)
- `distill.extract` (backend, elapsed, word_count)
- `distill.markdown` (elapsed, byte_count)
- `distill.template` (elapsed)

## CI workflow

Per PR:
1. `cargo fmt --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`
4. `cargo deny check`
5. `cargo audit`
6. Build matrix (Linux x86, Linux ARM, macOS ARM)
7. Phase 2: build with `--features bundled-defuddle` and run
   the cross-backend equivalence test
8. Phase 2: build the V8 snapshot and verify cold-start time
   under target threshold
9. Criterion benches (warn on > 10% regression, fail on > 25%)

Per release tag:
1. All of the above
2. Build all release artifacts
3. Generate SBOM, sign artifacts
4. Publish to crates.io (tools, then libs in dependency
   order)
5. Publish to GitHub Releases
6. Update Homebrew formula

## Cost ceiling for the project

This is a serious tool but not infinite scope. Hard limits:

- **Phase 1 + Phase 2 effort**: 6 weeks of focused work
  (one engineer). If Phase 2 hits 8 weeks, audit before
  extending.
- **Binary size**: ≤ 80 MB for Phase 2 release. If V8 + bundled
  JS push beyond, reconsider bundling jsdom vs. lighter DOM.
- **Per-call latency**: ≤ 1s P50 for the URL→Markdown happy
  path. Beyond this we're losing the agent-friendliness
  story.
- **Maintenance load**: ≤ 2 hours/week for upstream Defuddle
  updates and incoming bug reports. If it blows past this,
  Phase 3 (native port) becomes a forcing function.

If any limit is breached, that's an explicit project review,
not silent scope expansion.
