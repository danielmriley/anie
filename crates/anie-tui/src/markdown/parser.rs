//! Thin wrapper around `pulldown-cmark` to produce a bounded
//! event stream with the extensions we care about enabled.
//!
//! Kept deliberately thin — the parser decision lives here so
//! layout.rs doesn't import `pulldown_cmark` directly. Swapping
//! parsers later (termimad, tui-markdown, handwritten) would be
//! a single-file change.

use pulldown_cmark::{Options, Parser};

/// Build a pulldown-cmark parser with anie's extension set
/// enabled: tables, strikethrough, GFM task lists. HTML is
/// rendered as literal text in layout.rs, not executed, so
/// `ENABLE_SMART_PUNCTUATION` etc. are off to keep output
/// predictable.
#[must_use]
pub fn parse(text: &str) -> Parser<'_> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    Parser::new_ext(text, options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulldown_cmark::{Event, Tag, TagEnd};

    #[test]
    fn parse_emits_events_for_a_simple_paragraph() {
        let events: Vec<_> = parse("hello world").collect();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Start(Tag::Paragraph)))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::End(TagEnd::Paragraph)))
        );
    }

    #[test]
    fn parse_enables_tables_extension() {
        let input = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let events: Vec<_> = parse(input).collect();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Start(Tag::Table(_)))),
            "tables should produce Table events: {events:?}"
        );
    }

    #[test]
    fn parse_enables_strikethrough_extension() {
        let events: Vec<_> = parse("~~gone~~").collect();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Start(Tag::Strikethrough))),
            "strikethrough extension off: {events:?}"
        );
    }
}
