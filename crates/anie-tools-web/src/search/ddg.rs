//! DuckDuckGo HTML scrape backend.
//!
//! Hits `duckduckgo.com/html/?q=<query>` and parses the
//! returned SERP. DuckDuckGo returns static HTML for this
//! endpoint specifically (vs. their JS-driven main UI), so we
//! don't need a headless browser.
//!
//! The parser is tested against captured fixture HTML so
//! breaking layout changes show up in CI rather than in
//! production. Update the fixture when DDG ships a layout
//! change.

use scraper::{Html, Selector};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::WebToolError;
use crate::read::fetch::{FetchOptions, Resolver, fetch_html};
use crate::search::SearchHit;

const DDG_HTML_BASE: &str = "https://duckduckgo.com/html/";

/// Run a DuckDuckGo HTML search. Returns up to `max` hits.
/// Honors `cancel` cooperatively via `fetch_html`.
pub async fn search(
    client: &reqwest::Client,
    resolver: &dyn Resolver,
    cancel: &CancellationToken,
    fetch_opts: &FetchOptions,
    query: &str,
    max: usize,
) -> Result<Vec<SearchHit>, WebToolError> {
    let url = Url::parse(&format!("{DDG_HTML_BASE}?q={}", urlencoding::encode(query),))
        .map_err(|e| WebToolError::SearchBackend(e.to_string()))?;

    let html = fetch_html(client, resolver, cancel, &url, fetch_opts).await?;
    parse_ddg_html(&html, max)
}

/// Parse DDG's SERP HTML into [`SearchHit`]s. Public so tests
/// can drive captured fixtures without making real requests.
pub fn parse_ddg_html(html: &str, max: usize) -> Result<Vec<SearchHit>, WebToolError> {
    let doc = Html::parse_document(html);
    // DDG's stable selectors (verified against multiple
    // snapshots over years): `.result` for each row,
    // `.result__a` for the title anchor, `.result__snippet`
    // for the body snippet. Updates land here when the
    // layout shifts.
    let result_sel = Selector::parse(".result").map_err(selector_err)?;
    let title_sel = Selector::parse("a.result__a").map_err(selector_err)?;
    let snippet_sel = Selector::parse(".result__snippet").map_err(selector_err)?;

    let mut hits = Vec::new();
    for node in doc.select(&result_sel).take(max) {
        let Some(title_el) = node.select(&title_sel).next() else {
            continue;
        };
        let snippet_el = node.select(&snippet_sel).next();

        let raw_href = title_el.value().attr("href").unwrap_or_default();
        let Ok(url) = decode_ddg_redirect(raw_href) else {
            continue; // skip malformed rows
        };
        let title_text = collect_text(&title_el).trim().to_string();
        let snippet_text = snippet_el
            .map(|s| collect_text(&s).trim().to_string())
            .unwrap_or_default();
        let site = url.host_str().unwrap_or("").to_string();
        if title_text.is_empty() || site.is_empty() {
            continue;
        }
        hits.push(SearchHit {
            title: title_text,
            url,
            snippet: snippet_text,
            site,
        });
    }
    Ok(hits)
}

/// DDG wraps result links as `/l/?uddg=<percent-encoded-url>&...`.
/// Decode to the original URL. Falls through to the raw value
/// if the redirect format isn't present (already absolute).
pub fn decode_ddg_redirect(raw: &str) -> Result<Url, WebToolError> {
    let stripped = raw.trim_start_matches("//duckduckgo.com");
    let stripped = stripped.trim_start_matches("//");

    // Three shapes we've seen:
    // 1. /l/?uddg=<url>
    // 2. //duckduckgo.com/l/?uddg=<url>
    // 3. https://example.com/page  (already absolute)
    let absolute_form = if let Some(rest) = stripped.strip_prefix("l/?") {
        format!("https://duckduckgo.com/l/?{rest}")
    } else if raw.starts_with('/') {
        format!("https://duckduckgo.com{raw}")
    } else {
        raw.to_string()
    };

    let parsed = Url::parse(&absolute_form)
        .map_err(|e| WebToolError::SearchBackend(format!("bad redirect URL: {e}")))?;
    if let Some((_, value)) = parsed.query_pairs().find(|(k, _)| k == "uddg") {
        return Url::parse(&value)
            .map_err(|e| WebToolError::SearchBackend(format!("bad uddg target: {e}")));
    }
    Ok(parsed)
}

fn collect_text(node: &scraper::ElementRef<'_>) -> String {
    node.text().collect::<String>()
}

fn selector_err(_e: scraper::error::SelectorErrorKind<'_>) -> WebToolError {
    // The selector strings above are constants; this only
    // fires if scraper changes its grammar. Surface as a
    // SearchBackend error so the agent gets a clear signal.
    WebToolError::SearchBackend("internal: invalid selector (please report a bug)".into())
}

/// Format hits as Markdown for the agent to read.
pub fn format_results(query: &str, hits: &[SearchHit]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "# Search results: \"{query}\"");
    let _ = writeln!(out);
    if hits.is_empty() {
        out.push_str("(no results)\n");
        return out;
    }
    for (idx, hit) in hits.iter().enumerate() {
        let _ = writeln!(out, "{}. **{}** — *{}*", idx + 1, hit.title, hit.site);
        let _ = writeln!(out, "   {}", hit.url);
        if !hit.snippet.is_empty() {
            let _ = writeln!(out, "   {}", hit.snippet);
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(
        out,
        "({} result{} returned by DuckDuckGo.)",
        hits.len(),
        if hits.len() == 1 { "" } else { "s" },
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic fixture mirroring DDG's HTML structure. Real
    /// captures should be saved to `tests/fixtures/` once a
    /// live snapshot is feasible; the synthetic version
    /// pins the parser contract regardless.
    const SYNTHETIC_DDG: &str = r##"
<html>
<body>
<div class="result">
  <h2 class="result__title">
    <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Ffirst&amp;rut=1">
      First Result Title
    </a>
  </h2>
  <a class="result__url" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Ffirst">
    example.com
  </a>
  <a class="result__snippet">First result snippet text.</a>
</div>
<div class="result">
  <h2 class="result__title">
    <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fother-site.org%2Fsecond&amp;rut=1">
      Second Result
    </a>
  </h2>
  <a class="result__snippet">Second snippet body.</a>
</div>
<div class="result">
  <h2 class="result__title">
    <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fthird.example%2Fpage">
      Third
    </a>
  </h2>
  <a class="result__snippet">Third.</a>
</div>
</body>
</html>
"##;

    #[test]
    fn decode_ddg_redirect_extracts_uddg_target() {
        let url = decode_ddg_redirect("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpath")
            .expect("decode");
        assert_eq!(url.as_str(), "https://example.com/path");
    }

    #[test]
    fn decode_ddg_redirect_handles_root_relative() {
        let url = decode_ddg_redirect("/l/?uddg=https%3A%2F%2Fexample.com%2F").expect("decode");
        assert_eq!(url.as_str(), "https://example.com/");
    }

    #[test]
    fn decode_ddg_redirect_passes_through_absolute() {
        let url = decode_ddg_redirect("https://example.com/page").expect("decode");
        assert_eq!(url.as_str(), "https://example.com/page");
    }

    #[test]
    fn decode_ddg_redirect_rejects_garbage() {
        let err = decode_ddg_redirect("not a url at all").unwrap_err();
        assert!(matches!(err, WebToolError::SearchBackend(_)));
    }

    #[test]
    fn parse_ddg_html_returns_expected_hits() {
        let hits = parse_ddg_html(SYNTHETIC_DDG, 10).expect("parse");
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].title, "First Result Title");
        assert_eq!(hits[0].url.as_str(), "https://example.com/first");
        assert_eq!(hits[0].site, "example.com");
        assert!(hits[0].snippet.contains("First result snippet"));
        assert_eq!(hits[1].url.host_str().unwrap(), "other-site.org");
        assert_eq!(hits[2].url.host_str().unwrap(), "third.example");
    }

    #[test]
    fn parse_ddg_html_truncates_at_max() {
        let hits = parse_ddg_html(SYNTHETIC_DDG, 2).expect("parse");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn parse_ddg_html_skips_rows_without_title_anchor() {
        let html = r##"
<html><body>
<div class="result"></div>
<div class="result">
  <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fok.example%2F">OK</a>
  <a class="result__snippet">snip</a>
</div>
</body></html>
"##;
        let hits = parse_ddg_html(html, 10).expect("parse");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "OK");
    }

    #[test]
    fn parse_ddg_html_returns_empty_for_no_results() {
        let html = "<html><body><p>No results.</p></body></html>";
        let hits = parse_ddg_html(html, 10).expect("parse");
        assert!(hits.is_empty());
    }

    #[test]
    fn format_results_emits_numbered_list_with_urls() {
        let hits = vec![
            SearchHit {
                title: "Alpha".into(),
                url: Url::parse("https://alpha.example/x").unwrap(),
                snippet: "first snippet".into(),
                site: "alpha.example".into(),
            },
            SearchHit {
                title: "Beta".into(),
                url: Url::parse("https://beta.example/y").unwrap(),
                snippet: "second snippet".into(),
                site: "beta.example".into(),
            },
        ];
        let out = format_results("query text", &hits);
        assert!(out.contains("# Search results: \"query text\""));
        assert!(out.contains("1. **Alpha** — *alpha.example*"));
        assert!(out.contains("https://alpha.example/x"));
        assert!(out.contains("first snippet"));
        assert!(out.contains("2. **Beta**"));
        assert!(out.contains("(2 results"));
    }

    #[test]
    fn format_results_handles_no_hits() {
        let out = format_results("nothing", &[]);
        assert!(out.contains("# Search results"));
        assert!(out.contains("(no results)"));
    }

    #[test]
    fn format_results_singularizes_one_result() {
        let hits = vec![SearchHit {
            title: "Solo".into(),
            url: Url::parse("https://example.com/").unwrap(),
            snippet: String::new(),
            site: "example.com".into(),
        }];
        let out = format_results("q", &hits);
        assert!(out.contains("(1 result returned"));
    }
}
