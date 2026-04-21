//! Link rendering for markdown output.
//!
//! **Scope of PR D (Plan 05).** pi emits OSC 8 hyperlink escape
//! sequences (`\x1b]8;;URL\x07text\x1b]8;;\x07`) when the
//! terminal supports it, so the link is clickable without the
//! URL visually leaking into the text. That works cleanly in
//! pi's termimad-style direct-write pipeline.
//!
//! In ratatui, `Span` content is measured via
//! `unicode-width::UnicodeWidthStr`, which counts the printable
//! characters inside the OSC 8 sequence (the URL body, `;`, etc.)
//! as visible cells even though the terminal renders them as
//! zero-width. Layout breaks once a line carries a link.
//!
//! So we defer true OSC 8 emission and render a "visible URL"
//! fallback for everything: the link text is drawn in
//! `link_text` style (blue + underlined) and the URL trails in
//! `link_url` style (`DarkGray`). Autolinks and anchor-only
//! references skip the trailing URL — they'd just repeat the
//! visible text.
//!
//! Hooking up real OSC 8 here is tracked as a follow-up; the
//! thin `format_link_suffix` helper is the integration point.

use pulldown_cmark::LinkType;

/// Whether to append a trailing ` (url)` string after the link
/// text. False when the URL is already visible (autolinks,
/// emails) so we don't duplicate it on screen.
#[must_use]
pub fn should_show_trailing_url(link_type: LinkType, text: &str, url: &str) -> bool {
    match link_type {
        // `<https://example.com>` and `<user@example.com>` —
        // pulldown-cmark already surfaces the URL as the link
        // text, so appending again would duplicate.
        LinkType::Autolink | LinkType::Email => false,
        _ => {
            // Cover the case where an inline link's text happens
            // to equal its URL (LLMs do this). Skip the trailing
            // URL for the same reason.
            text.trim() != url.trim()
        }
    }
}

/// Format the trailing ` (url)` suffix for a non-autolink.
/// Callers style this span with `MarkdownTheme::link_url`.
#[must_use]
pub fn format_link_suffix(url: &str) -> String {
    format!(" ({url})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autolinks_skip_trailing_url() {
        assert!(!should_show_trailing_url(
            LinkType::Autolink,
            "https://example.com",
            "https://example.com",
        ));
    }

    #[test]
    fn email_autolinks_skip_trailing_url() {
        assert!(!should_show_trailing_url(
            LinkType::Email,
            "a@example.com",
            "mailto:a@example.com",
        ));
    }

    #[test]
    fn inline_link_with_different_text_shows_trailing_url() {
        assert!(should_show_trailing_url(
            LinkType::Inline,
            "the docs",
            "https://example.com",
        ));
    }

    #[test]
    fn inline_link_where_text_equals_url_skips_trailing_url() {
        assert!(!should_show_trailing_url(
            LinkType::Inline,
            "https://example.com",
            "https://example.com",
        ));
    }

    #[test]
    fn format_link_suffix_wraps_in_parens_with_leading_space() {
        assert_eq!(
            format_link_suffix("https://example.com"),
            " (https://example.com)"
        );
    }
}
