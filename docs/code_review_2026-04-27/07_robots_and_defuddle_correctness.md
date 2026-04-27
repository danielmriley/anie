# 07 — robots.txt and Defuddle extraction correctness

## Rationale

The review found two lower-risk correctness issues in the web reader.

### robots.txt user-agent/origin handling

- `crates/anie-tools-web/src/read/fetch.rs:153-179` —
  `RobotsCache::check()` accepts `user_agent`.
- `crates/anie-tools-web/src/read/fetch.rs:182-197` — `evaluate()` names
  that parameter `_user_agent` and does not use it.
- `crates/anie-tools-web/src/read/fetch.rs:208-224` — `fetch_robots_for()`
  constructs `Robot::new("*", &body)`.
- Cache key is only host string, not origin (`scheme`, host, port) or
  user-agent.

This means site-specific `User-agent: anie` rules may not be honored,
and rules can be reused across different ports/schemes on the same host.

### Defuddle source URL

- `crates/anie-tools-web/src/read/extract.rs:104-114` — trait docs say
  `source_url` is provided for relative-link resolution and metadata.
- `crates/anie-tools-web/src/read/extract.rs:123-128` — production runner
  ignores `_source_url`.
- `crates/anie-tools-web/src/read/extract.rs:150-155` — Defuddle receives
  only the temp HTML file path.

The YAML frontmatter still records the source URL, but the extractor may
not resolve relative links against the original URL.

## Design

Split into two independent correctness fixes.

## Files to touch

- `crates/anie-tools-web/src/read/fetch.rs`
  - Cache robots policies by origin and, if needed, user-agent.
  - Construct/evaluate robots rules using the configured user-agent.
  - Add tests for user-agent-specific rules.
- `crates/anie-tools-web/src/read/extract.rs`
  - Use `source_url` if Defuddle CLI supports a base/source option.
  - Otherwise document limitation and optionally post-process relative
    links in Markdown.
- `docs/web_tool_2026-04-26/*` or README docs
  - Update runtime behavior docs if Defuddle limitations remain.

## Phased PRs

### PR A — robots.txt honors user-agent and origin

**Change:**

- Replace host-only cache key with origin key:
  - scheme;
  - host;
  - port or default port.
- Ensure robots evaluation uses the same user-agent sent on fetch.
- Decide whether to cache:
  1. raw robots bytes per origin, then instantiate/evaluate per
     user-agent; or
  2. parsed `Robot` per `(origin, user_agent)`.

Prefer raw/per-origin cache if `texting_robots` makes that simple; it
avoids refetching when user-agent changes.

**Tests:**

- `User-agent: anie` disallow overrides wildcard allow.
- `User-agent: *` disallow applies when no anie-specific group exists.
- Same host on two ports gets separate cached policies.
- Existing permissive/no-robots tests continue to pass.

**Exit criteria:**

- `user_agent` parameter is no longer unused.
- Robots cache behavior matches origin semantics.

### PR B — Defuddle source URL support or explicit limitation

**Change:**

- Check Defuddle 0.18 CLI for a base/source URL option. If available,
  pass `source_url` while still feeding the temp file as the content
  source so anie's fetch/SSRF/size checks remain in force.
- If no CLI support exists, update docs and code comments to say
  relative-link resolution is best-effort/unsupported when using a
  tempfile.
- Optional fallback: post-process Markdown links/images whose URL is
  relative and resolve them against `source_url`.

**Tests:**

- If CLI flag is used, unit-test command construction through a small
  injectable command builder.
- If post-processing is added, tests for:
  - `[text](/path)` → absolute URL;
  - `![alt](image.png)` → absolute URL;
  - existing absolute URLs unchanged;
  - code blocks untouched.

**Exit criteria:**

- Trait docs match implementation.
- Extracted Markdown either resolves relative links correctly or clearly
  documents why it cannot.

## Test plan

- `cargo test -p anie-tools-web`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Manual smoke:
  - Fetch a page with relative links and inspect output.
  - Hit a fixture robots file with user-agent-specific rules.

## Risks

- `texting_robots::Robot` API may not support changing user-agent after
  parse. If so, cache raw bytes and instantiate per user-agent.
- Markdown link post-processing can corrupt code blocks if implemented
  with regex. Prefer pulldown-cmark events or keep the limitation
  documented.
- Passing the original URL directly to Defuddle must not let Defuddle
  re-fetch the page. The original design deliberately uses a tempfile so
  anie's network guardrails remain authoritative.

## Exit criteria

- robots.txt behavior is user-agent and origin aware.
- Defuddle source URL handling is either implemented or honestly
  documented with tests around any post-processing.

## Deferred

- Full crawler politeness policy (crawl-delay, sitemap usage, persistent
  robots cache). This plan only fixes the correctness gaps found in the
  review.
