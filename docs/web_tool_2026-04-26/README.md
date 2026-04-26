# Web tooling for anie — 2026-04-26

A pair of native anie tools that give the agent the ability to
discover and read web content:

- **`web_read`** — fetch a URL and return clean Markdown +
  metadata. Reader-mode quality content extraction via
  [Defuddle](https://github.com/kepano/defuddle).
- **`web_search`** — given a query, return ranked URLs and
  snippets. Lets the agent find pages worth reading before
  calling `web_read`.

The two tools compose: `web_search("topic") → URL list → web_read(url) → answer`.

## Files in this folder

- [`00_design.md`](00_design.md) — the architectural shape:
  why subprocess, how the Defuddle integration works, where
  it lives in the workspace, the tool contract.
- [`01_implementation.md`](01_implementation.md) — step-by-
  step build plan. Two-week phased PR sequence with file
  paths, test names, and exit criteria.
- [`02_web_search.md`](02_web_search.md) — companion plan for
  `web_search`. Backend choice, output shape, rate limit
  story.

## Headline decisions

| Decision | Choice |
|----------|--------|
| Crate location | New sub-crate `crates/anie-tools-web/` in anie's workspace |
| Tool registration | Same pattern as bash/edit/read — through `anie-tools` registry |
| Tool name (read) | `web_read` |
| Tool name (search) | `web_search` |
| Defuddle integration | **Subprocess.** Spawn `defuddle` CLI (or `npx defuddle@<pin>`); zero JS in anie's source tree |
| Defuddle distribution | System install (`npm i -g defuddle-cli`); identical pattern to bash needing `/bin/sh` |
| Cargo feature gate | `web` (default on for normal builds; opt out via `--no-default-features` for lean installs) |
| Embedded `deno_core` backend | Deferred to opt-in `--features web-embedded`. Standalone-plan reasoning preserved in [`../completed/distill_plan_2026-04-26/`](../completed/distill_plan_2026-04-26/) for future reference. |
| Native Rust port | Deferred until `defuddle-rs` matures or external trigger forces it. |
| Tool output format | Markdown body with YAML frontmatter, single string. Matches what an LLM consumes. |
| Auto-detect JS-heavy pages | No. `javascript: false` is the default; agent passes `javascript: true` if first fetch yields too little. |
| Markdown post-processing | Trust Defuddle for v1. Revisit if specific issues surface. |

## What this is and isn't

**Is:**
- Two production-grade anie tools for web reading and search.
- Defuddle-quality content extraction via a thin Rust wrapper.
- Predictable system-dependency model (Node + defuddle-cli for `web_read`; HTTP client only for `web_search`).
- Full integration with anie's existing tool registry, error
  taxonomy, and config system.

**Isn't:**
- A standalone CLI or library. (See archived
  [`distill_plan_2026-04-26`](../completed/distill_plan_2026-04-26/).)
- A web crawler. Single-URL fetch only; the agent decides
  what to read and when.
- A scraper for arbitrary JS-rendered pages. `javascript: true`
  uses a headless Chrome subprocess; out of the box requires
  Chrome/Chromium installed on the system.
- A replacement for human reading. The output is for
  agent consumption — formatted for LLM inputs, not
  pretty-printed for humans.

## Suggested PR ordering

1. **Read PR** (Week 1): `web_read` Phase 1 with subprocess
   Defuddle. ~400 LOC of Rust.
2. **Search PR** (Week 2): `web_search` with DuckDuckGo
   HTML scrape backend. ~250 LOC.
3. *(later)* Optional `--features web-embedded` for the
   single-binary deployment story.

## Principles

- **Anie's existing patterns first.** Tools live in
  `crates/anie-tools-*` (or a sub-crate), implement the
  existing `Tool` trait, surface errors via the existing
  `ToolError` taxonomy. We're not redesigning the tool
  abstraction.
- **Subprocess Defuddle, not vendored.** Same shape as bash
  needing `/bin/sh`. Documented prereq, graceful error if
  missing, no Node code in anie's repo.
- **Markdown is the contract with the agent.** Defuddle's
  Markdown output passes through unchanged for v1.
- **Tools ship optionally.** A user who doesn't browse the
  web should be able to compile out the web tools entirely
  via cargo features.
- **No silent network access.** robots.txt is respected by
  default; rate limiting is on by default; SSRF guards are on
  by default.
