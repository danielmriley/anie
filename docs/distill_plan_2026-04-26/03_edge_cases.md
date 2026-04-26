# 03 — Edge cases

The internet is a hostile environment for clean text
extraction. This doc lists the categories of trouble and how
`distill` handles each.

## Paywalls + login walls

Three flavors:

1. **Hard paywall** (NYT, WSJ, etc.): server returns a teaser
   plus `<script>`-driven blur/redirect for non-subscribers.
2. **Metered paywall** (Medium, Bloomberg): cookies / IP
   tracking allows N free articles.
3. **Login wall** (X.com guests, LinkedIn): server returns a
   "please log in" stub.

**Our policy:** detect, report, do not bypass. Ethically and
legally we don't ship paywall circumvention. If
authenticated access is wanted, the user supplies cookies via
`--cookies-file` and accepts whatever ToS implications that
carries.

Detection heuristics:
- Page word count after extraction is suspiciously low
  (< 100 words) but the page title implies a longer article.
- Common paywall HTML markers (e.g., `data-paywall-status`,
  Substack's `subscriber-only-content` class).
- Defuddle's own metrics: if the extracted-text-to-page-size
  ratio is below a threshold, flag it.

When detected, `Article::metadata.paywall_likely = true` and
the markdown body has a final `<!-- distill: paywall
detected -->` comment. The `--strict` flag turns this into an
error.

## JS-heavy sites (SPAs, infinite scroll)

Many modern sites render content client-side. A plain
`reqwest::get` returns an empty `<div id="root"></div>` and
nothing useful.

**Handling:**
- Default: fetch with reqwest, attempt extraction. If
  Defuddle returns content, we're done — most sites still
  ship server-rendered HTML.
- If extraction yields below-threshold content (a known sign
  of CSR/SPA), suggest `--javascript` in the error message.
- `--javascript` (Phase 2+): use `chromiumoxide` to drive a
  headless Chrome instance, wait for `networkidle2`, dump
  rendered HTML, hand off to Defuddle.

**Infinite scroll:** by default we capture the first viewport.
Add `--scroll N` (Phase 3 polish) to scroll-and-wait N times,
useful for forums and feed-style pages.

## Twitter/X

Massively different post-acquisition. Public unauthenticated
access is now extremely limited; even with auth, X's HTML
structure changes weekly.

**Approach:**
- For `twitter.com` / `x.com` URLs, prefer **nitter mirrors**
  (configurable list) with automatic fallback. nitter renders
  static HTML that Defuddle handles cleanly.
- Custom `SiteExtractor` plugin (`distill-extractors-x`) that:
  - Parses tweet thread structure
  - Preserves quote tweets, threaded replies
  - Emits Markdown like:
    ```
    > **@user** · 2024-08-15
    > Tweet body text here.
    > 
    > > Quoted tweet content.
    ```
- If user provides X auth cookies via `--cookies-file`,
  `--javascript` uses them and parses the result.

## Reddit

Reddit's web UI changes frequently and is JS-heavy, but
**`old.reddit.com` still serves clean static HTML**.

**Approach:**
- Auto-rewrite `reddit.com/r/X/comments/Y` to
  `old.reddit.com/r/X/comments/Y` (configurable, default
  on).
- Custom `SiteExtractor` for Reddit threads: emit OP +
  comments as a nested-blockquote Markdown structure.
  Threads can be huge — paginate or truncate based on a
  `--max-comments` flag.

## YouTube

Two distinct extraction targets:

1. **Video page metadata:** title, description, channel,
   tags, upload date. Defuddle handles this OK with the
   `youtube.com/watch` HTML.
2. **Transcript:** the actually-useful content for an LLM.
   Available via the public timed-text API (no auth
   required). We fetch the `lang=en` track if available and
   include it as the body content.

`distill youtube.com/watch?v=...` should "just work" and
produce a Markdown doc with metadata + transcript.

## Substack / Medium / personal blogs

These are Defuddle's sweet spot — long-form HTML articles
with reasonable semantics. Default flow handles them well.

Watch for:
- **Subscribe walls** (Medium's "metered" wall) — see paywall
  section above.
- **Footnotes** — Defuddle handles native HTML footnotes;
  we preserve them as Markdown footnotes.

## GitHub README pages, package docs

Defuddle handles `github.com/user/repo` (the rendered README
panel) cleanly. For better fidelity:
- For `github.com/user/repo`, optionally fetch the raw README
  from `raw.githubusercontent.com` instead — that's already
  Markdown, so we skip the HTML→Markdown conversion entirely.
  CLI flag: `--prefer-raw`.
- For `docs.rs/crate/X`, content is already well-structured
  HTML; default works.

## Notion / Obsidian Publish / Hugo / static sites

- **Notion public pages:** server-rendered, Defuddle handles
  them. Caveat: image URLs are signed and time-limited; we
  preserve the URL as-is (caller can choose to download
  promptly).
- **Obsidian Publish:** static HTML from Markdown, ideal
  case. Defuddle works.
- **Hugo / Jekyll / Zola / 11ty:** static HTML, ideal case.

## arXiv / academic papers

`arxiv.org/abs/X` returns the abstract page (HTML). Useful
metadata extraction. For the actual paper:
- `--prefer-raw` follows the PDF link and returns a stub
  Markdown noting the PDF source — we don't OCR PDFs in
  Phase 1/2. Phase 3 polish: integrate `pdfium-render` for
  text extraction.

## File:// URLs and stdin

Already supported by the basic flow:
- `distill ./article.html` reads the file.
- `distill -` reads HTML from stdin.

These bypass the fetcher entirely; useful for CI / piping
from `curl` / shell scripting.

## Errors we surface clearly

| Symptom | DistillError variant | User-facing message |
|---------|----------------------|---------------------|
| DNS / network failure | `Fetch` | "Could not reach <host>: <err>" |
| 4xx without body | `HttpStatus` | "Server returned 403 Forbidden" |
| 5xx with retry | retried internally | (succeeds or surfaces final 5xx) |
| robots.txt forbids | `Forbidden` | "robots.txt forbids access. Use --no-robots to override." |
| HTML > max_size | `TooLarge` | "Page is 12 MB; exceeds max-size 10 MB. Use --max-size 20MB." |
| Defuddle returns empty | `ExtractionFailed { reason }` | "Extraction yielded < 50 words. Page may require --javascript or be paywalled." |
| Headless render failed | `HeadlessFailure` | "Headless Chrome failed: <err>. Is Chrome installed?" |
| Timeout | `Timeout` | "Request timed out after 30s. Try --timeout 60s." |

## Site-extractor plugin pattern

Plugins live as separate crates under
`distill-extractors-*`. Each implements:

```rust
#[async_trait]
pub trait SiteExtractor: Send + Sync {
    /// True if this extractor handles `url`.
    fn matches(&self, url: &Url) -> bool;

    /// Extract content given the fetched HTML.
    /// Falls through to default Defuddle extraction if Err.
    async fn extract(
        &self,
        html: &str,
        url: &Url,
    ) -> Result<DefuddleOutput, DistillError>;

    /// Stable name for diagnostics / logging.
    fn name(&self) -> &str;
}
```

Built-in plugins (Phase 2/3, optional features):
- `distill-extractors-x` — Twitter / X
- `distill-extractors-reddit` — Reddit
- `distill-extractors-youtube` — YouTube transcripts
- `distill-extractors-arxiv` — arXiv abstracts

CLI integration: plugins discovered at startup via cargo
features. CLI flag `--extractors x,reddit,youtube` enables
specific plugins; default is "all built-in plugins enabled."

## Anti-bot / Cloudflare / hCaptcha walls

**Out of scope** for `distill`. Bypassing these is an
arms-race that no general-purpose tool wins, and most of the
common "solutions" are commercial scraping services that
violate the goal of "no required cloud services."

If a site is bot-walled, `distill` returns a clear error
suggesting:
1. The user fetches the HTML manually and pipes it via
   `distill -`.
2. They use `--cookies-file` to provide an authenticated
   session.

## Internationalization

Defuddle handles non-English content fine. Things to ensure:

- UTF-8 throughout. `reqwest` decodes per `Content-Type`
  charset; we override to UTF-8 lossy if needed.
- Right-to-left languages render in Markdown without
  modification — markdown clients handle the bidi.
- Date parsing in metadata: use `chrono` with locale-aware
  parsing for common formats. Fall back to "raw string" if
  parse fails.
- `--language en` hint in `FetchOptions` sets
  `Accept-Language` header; useful when sites serve different
  content per locale.
