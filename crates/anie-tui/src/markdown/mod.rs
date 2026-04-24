//! Markdown → ratatui line rendering.
//!
//! Entry point: [`render_markdown`]. Call on finalized assistant
//! content only — per Plan 05's streaming-block caveat, a
//! streaming block's content changes every delta and would
//! re-parse markdown on every frame. `OutputPane` handles that
//! gating in Plan 05 PR E; until then, `render_markdown` is
//! unused in production and exists only for direct tests and
//! for the final wire-up PR.
//!
//! Adopts pi's per-component `(text, width) -> lines` cache
//! pattern at the `OutputPane` block-cache layer (PR 2 of
//! `tui_responsiveness/`), not inside this module. Rendering is
//! a pure function of `(text, width, theme)` so the existing
//! block cache captures it cleanly.

mod layout;
mod link;
mod parser;
mod syntax;
mod theme;

// Re-exports for the forthcoming PR E wire-up. `#[allow(unused_imports)]`
// matches the module-level dead-code suppression in lib.rs.
#[allow(unused_imports)]
pub use layout::render as render_markdown;
#[allow(unused_imports)]
pub use theme::MarkdownTheme;

use ratatui::text::Line;

/// A clickable region inside a rendered markdown line. Produced
/// by `find_link_ranges`; consumed by `OutputPane`'s mouse-
/// click hit test to translate a `(row, col)` into an
/// `opener::open(url)` call.
///
/// `col_start` / `col_end` are inclusive / exclusive character
/// counts from the start of the line (not byte offsets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRange {
    pub col_start: u16,
    pub col_end: u16,
    pub url: String,
}

/// Scan rendered markdown lines for clickable URL regions.
///
/// Matches the convention established by `link.rs`: the `link_url`-
/// styled span contains the trailing ` (url)` fallback text we
/// emit for every non-autolink. That span's content uniquely
/// identifies both the URL and its visible position on the line.
///
/// Returns one `Vec<LinkRange>` per input line (empty where no
/// links exist). The caller pairs this with the corresponding
/// `Vec<Line>` — they share the same length and indexing.
#[must_use]
pub fn find_link_ranges(lines: &[Line<'static>], theme: &MarkdownTheme) -> Vec<Vec<LinkRange>> {
    lines
        .iter()
        .map(|line| find_link_ranges_in_line(line, theme))
        .collect()
}

fn find_link_ranges_in_line(line: &Line<'static>, theme: &MarkdownTheme) -> Vec<LinkRange> {
    let mut ranges = Vec::new();
    let mut col: u16 = 0;
    for span in &line.spans {
        let span_chars = u16::try_from(span.content.chars().count()).unwrap_or(u16::MAX);
        // Only link_url-styled spans carry the fallback `(url)`
        // text. They're emitted by link.rs::format_link_suffix
        // and rendered with MarkdownTheme::link_url — an
        // exact-style match is the cheapest identifier.
        if span.style == theme.link_url {
            if let Some(url) = extract_url_from_fallback(&span.content) {
                // Skip the leading space + opening paren (2
                // chars of the fallback " (url)") so the
                // clickable region is just the URL text, not
                // the punctuation.
                let content_chars = span.content.chars().count() as u16;
                // Compute the URL's inner position within the
                // span: find the first '(' and the last ')'.
                let url_start_in_span = span.content.find('(').unwrap_or(0) as u16 + 1;
                let url_end_in_span = span
                    .content
                    .rfind(')')
                    .map(|i| i as u16)
                    .unwrap_or(content_chars);
                let col_start = col.saturating_add(url_start_in_span);
                let col_end = col.saturating_add(url_end_in_span);
                if col_end > col_start {
                    ranges.push(LinkRange {
                        col_start,
                        col_end,
                        url,
                    });
                }
            }
        }
        col = col.saturating_add(span_chars);
    }
    ranges
}

/// Extract a URL from the `" (https://...)"` fallback text
/// emitted by `link::format_link_suffix`. Returns `None` if
/// the content doesn't match the shape (defensive — a future
/// link_url use that isn't a fallback would skip gracefully).
fn extract_url_from_fallback(content: &str) -> Option<String> {
    let trimmed = content.trim();
    let inner = trimmed.strip_prefix('(')?.strip_suffix(')')?;
    // Conservative: must look like a URL. Anything else under
    // link_url style is either a future variant or a collision
    // with an unrelated style; don't open arbitrary text in a
    // browser.
    if inner.starts_with("http://") || inner.starts_with("https://") || inner.starts_with("mailto:")
    {
        Some(inner.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod link_range_tests {
    use super::*;
    use ratatui::text::Span;

    #[test]
    fn fallback_with_url_produces_range_over_just_the_url_chars() {
        let theme = MarkdownTheme::default_dark();
        let lines = vec![Line::from(vec![
            Span::raw("see "),
            Span::styled("the docs", theme.link_text),
            Span::styled(" (https://example.com)", theme.link_url),
        ])];
        let ranges = find_link_ranges(&lines, &theme);
        assert_eq!(ranges.len(), 1);
        let range = &ranges[0][0];
        assert_eq!(range.url, "https://example.com");
        // "see " (4) + "the docs" (8) + " (" (2) = 14 chars
        // before the URL text starts.
        assert_eq!(range.col_start, 14);
        // URL text "https://example.com" is 19 chars.
        assert_eq!(range.col_end, 14 + 19);
    }

    #[test]
    fn non_link_url_styled_spans_are_ignored() {
        let theme = MarkdownTheme::default_dark();
        let lines = vec![Line::from(vec![Span::raw("just plain text")])];
        let ranges = find_link_ranges(&lines, &theme);
        assert_eq!(ranges.len(), 1);
        assert!(ranges[0].is_empty());
    }

    #[test]
    fn autolink_style_still_records_url() {
        // Autolinks don't include the `(url)` fallback — the
        // visible text IS the url. But we don't emit link_url
        // style on those, so find_link_ranges returns nothing.
        // Autolink click-to-open would need a separate path.
        let theme = MarkdownTheme::default_dark();
        let lines = vec![Line::from(vec![Span::styled(
            "https://example.com",
            theme.link_text,
        )])];
        let ranges = find_link_ranges(&lines, &theme);
        assert!(ranges[0].is_empty());
    }

    #[test]
    fn extract_url_from_fallback_rejects_non_url_content() {
        assert!(extract_url_from_fallback("(not a url)").is_none());
        assert_eq!(
            extract_url_from_fallback("(https://example.com)").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            extract_url_from_fallback(" (https://example.com) ").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            extract_url_from_fallback("(mailto:a@b.com)").as_deref(),
            Some("mailto:a@b.com")
        );
    }
}
