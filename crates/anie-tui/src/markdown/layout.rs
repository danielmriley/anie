//! Transform a `pulldown-cmark` event stream into ratatui
//! `Line`s, applying theme styles and wrapping.
//!
//! **Scope of PR A (Plan 05).** This module currently handles
//! headings, paragraphs, bold / italic / strikethrough, inline
//! code, and HTML-as-literal-text. Code blocks, lists,
//! blockquotes, tables, links, and rules are added in later PRs
//! of the plan. Unimplemented element events fall through to a
//! plain-text stringification so nothing crashes; the quality
//! just improves as more PRs land.

use pulldown_cmark::{Event, HeadingLevel, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::MarkdownTheme;

/// Render a markdown string to ratatui lines at the given width.
///
/// PR A behavior: headings + paragraphs + inline formatting.
/// Unhandled block elements degrade to raw text extraction.
#[must_use]
pub fn render(text: &str, width: u16, theme: &MarkdownTheme) -> Vec<Line<'static>> {
    let mut builder = LineBuilder::new(width.max(1), theme);
    let events = super::parser::parse(text);
    for event in events {
        builder.consume(event);
    }
    builder.finish()
}

struct LineBuilder<'a> {
    width: u16,
    theme: &'a MarkdownTheme,
    style_stack: Vec<Style>,
    lines: Vec<Line<'static>>,
    current_spans: Vec<Span<'static>>,
    // Pending heading level while we're inside a heading tag.
    // `None` at every other point.
    current_heading: Option<HeadingLevel>,
    // Depth of unhandled container events so their content falls
    // through as plain text rather than being silently dropped.
    in_unhandled_block: u32,
}

impl<'a> LineBuilder<'a> {
    fn new(width: u16, theme: &'a MarkdownTheme) -> Self {
        Self {
            width,
            theme,
            style_stack: vec![Style::default()],
            lines: Vec::new(),
            current_spans: Vec::new(),
            current_heading: None,
            in_unhandled_block: 0,
        }
    }

    fn consume(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.on_start(tag),
            Event::End(tag) => self.on_end(tag),
            Event::Text(text) => self.push_text(&text),
            Event::Code(code) => self.push_styled(&code, self.theme.inline_code),
            Event::Html(html) | Event::InlineHtml(html) => {
                // Render raw HTML as literal text — never execute,
                // never style as actual markup. Matches pi's
                // approach at `packages/tui/src/components/markdown.ts:~428`.
                self.push_text(&html);
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                // PR C will style this; PR A emits a plain ─ row
                // so the element doesn't silently vanish.
                self.flush_line();
                let rule = "─".repeat(self.width.max(1) as usize);
                self.lines.push(Line::from(Span::styled(
                    rule,
                    self.theme.horizontal_rule,
                )));
                self.lines.push(Line::default());
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                self.push_styled(marker, self.theme.list_bullet);
            }
            Event::FootnoteReference(_) => {
                // Unused today; show the ref text as-is if we
                // ever hit it. Wire up properly when a real
                // footnote flow lands.
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                // Plan 05 explicitly defers math rendering.
                // Surface the TeX source as literal text so we
                // don't lose it.
                self.push_text(&text);
            }
        }
    }

    fn on_start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush_line();
                self.current_heading = Some(level);
                let style = heading_style(level, self.theme);
                self.push_style(style);
            }
            Tag::Strong => self.push_style_modifier(Modifier::BOLD),
            Tag::Emphasis => self.push_style_modifier(Modifier::ITALIC),
            Tag::Strikethrough => self.push_style_modifier(Modifier::CROSSED_OUT),
            // Block elements not yet handled: render their inline
            // content as plain text, ignore their structure.
            Tag::BlockQuote(_)
            | Tag::CodeBlock(_)
            | Tag::HtmlBlock
            | Tag::List(_)
            | Tag::Item
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::FootnoteDefinition(_)
            | Tag::Link { .. }
            | Tag::Image { .. }
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListDefinition
            | Tag::DefinitionListTitle => {
                self.in_unhandled_block = self.in_unhandled_block.saturating_add(1);
            }
        }
    }

    fn on_end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                // Blank line between paragraphs if there's
                // already content above.
                if !self.lines.is_empty() {
                    self.lines.push(Line::default());
                }
            }
            TagEnd::Heading(_) => {
                self.flush_line();
                self.pop_style();
                self.current_heading = None;
                self.lines.push(Line::default());
            }
            TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                self.pop_style();
            }
            TagEnd::BlockQuote(_)
            | TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::List(_)
            | TagEnd::Item
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::FootnoteDefinition
            | TagEnd::Link
            | TagEnd::Image
            | TagEnd::MetadataBlock(_)
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListDefinition
            | TagEnd::DefinitionListTitle => {
                self.in_unhandled_block = self.in_unhandled_block.saturating_sub(1);
            }
        }
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let style = self.current_style();
        for (index, chunk) in text.split('\n').enumerate() {
            if index > 0 {
                self.flush_line();
            }
            if !chunk.is_empty() {
                self.current_spans
                    .push(Span::styled(chunk.to_string(), style));
            }
        }
    }

    fn push_styled(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        self.current_spans
            .push(Span::styled(text.to_string(), style));
    }

    /// Current style at the top of the stack. The stack is
    /// seeded with `Style::default()` in `new()` and never
    /// shrinks below that baseline (`pop_style` guards), so this
    /// is always populated.
    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, style: Style) {
        let top = self.current_style();
        self.style_stack.push(top.patch(style));
    }

    fn push_style_modifier(&mut self, modifier: Modifier) {
        let top = self.current_style();
        self.style_stack.push(top.add_modifier(modifier));
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn flush_line(&mut self) {
        if self.current_spans.is_empty() {
            // Preserve blank lines inside paragraphs.
            self.lines.push(Line::default());
            return;
        }
        let spans = std::mem::take(&mut self.current_spans);
        for wrapped in wrap_spans(spans, self.width) {
            self.lines.push(wrapped);
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.current_spans.is_empty() {
            self.flush_line();
        }
        // Trim trailing blank lines — a paragraph followed by EOF
        // shouldn't leave a dangling spacer.
        while self
            .lines
            .last()
            .is_some_and(|line| line.spans.iter().all(|span| span.content.is_empty()))
        {
            self.lines.pop();
        }
        self.lines
    }
}

fn heading_style(level: HeadingLevel, theme: &MarkdownTheme) -> Style {
    match level {
        HeadingLevel::H1 => theme.h1,
        HeadingLevel::H2 => theme.h2,
        HeadingLevel::H3 => theme.h3,
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => theme.h4_plus,
    }
}

/// Wrap a row of spans to the given terminal width, preserving
/// styles. Splits on whitespace so words aren't broken mid-char
/// for normal prose; long runs without spaces wrap at the
/// character boundary (as for URLs / long identifiers).
fn wrap_spans(spans: Vec<Span<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;

    // Flatten `(char, style)` so wrapping can respect cell
    // widths without losing style information. `char` here means
    // USV; multi-codepoint grapheme clusters are NOT merged, but
    // wrap widths approximate what ratatui actually renders for
    // common ASCII + Latin + CJK.
    let mut cells: Vec<(char, Style)> = Vec::new();
    for span in spans {
        let style = span.style;
        let text = span.content.into_owned();
        for ch in text.chars() {
            cells.push((ch, style));
        }
    }

    if cells.is_empty() {
        return vec![Line::default()];
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_cells: Vec<(char, Style)> = Vec::new();

    for (ch, style) in cells {
        if ch == '\n' {
            lines.push(cells_to_line(std::mem::take(&mut current_cells)));
            continue;
        }
        if current_cells.len() >= width {
            // Try to break at the last whitespace; otherwise
            // break at the character boundary.
            let break_at = current_cells
                .iter()
                .rposition(|(c, _)| c.is_whitespace());
            match break_at {
                Some(idx) if idx + 1 < current_cells.len() => {
                    let remainder = current_cells.split_off(idx + 1);
                    // Drop the whitespace cell itself from the
                    // flushed line's tail.
                    while current_cells
                        .last()
                        .is_some_and(|(c, _)| c.is_whitespace())
                    {
                        current_cells.pop();
                    }
                    lines.push(cells_to_line(std::mem::take(&mut current_cells)));
                    current_cells = remainder;
                }
                _ => {
                    lines.push(cells_to_line(std::mem::take(&mut current_cells)));
                }
            }
        }
        current_cells.push((ch, style));
    }
    if !current_cells.is_empty() {
        lines.push(cells_to_line(current_cells));
    }
    lines
}

fn cells_to_line(cells: Vec<(char, Style)>) -> Line<'static> {
    if cells.is_empty() {
        return Line::default();
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current_text = String::new();
    let mut current_style = cells[0].1;
    for (ch, style) in cells {
        if style != current_style {
            if !current_text.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut current_text), current_style));
            }
            current_style = style;
        }
        current_text.push(ch);
    }
    if !current_text.is_empty() {
        spans.push(Span::styled(current_text, current_style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_plain(text: &str, width: u16) -> Vec<String> {
        render(text, width, &MarkdownTheme::default_dark())
            .into_iter()
            .map(line_to_string)
            .collect()
    }

    fn line_to_string(line: Line<'static>) -> String {
        line.spans
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect::<String>()
    }

    #[test]
    fn empty_input_produces_no_lines() {
        assert!(render_plain("", 80).is_empty());
    }

    #[test]
    fn plain_paragraph_renders_as_text() {
        let out = render_plain("hello world", 80);
        assert_eq!(out, vec!["hello world".to_string()]);
    }

    #[test]
    fn two_paragraphs_are_separated_by_a_blank_line() {
        let out = render_plain("first\n\nsecond", 80);
        assert_eq!(out, vec!["first".to_string(), String::new(), "second".into()]);
    }

    #[test]
    fn heading_levels_apply_distinct_styles() {
        let h1 = render(
            "# title",
            80,
            &MarkdownTheme::default_dark(),
        );
        let h2 = render(
            "## title",
            80,
            &MarkdownTheme::default_dark(),
        );
        let h3 = render(
            "### title",
            80,
            &MarkdownTheme::default_dark(),
        );
        // Compare styles on the first (non-blank) span of each.
        let style_of = |lines: &[Line<'static>]| {
            lines
                .iter()
                .find_map(|line| line.spans.iter().find(|s| !s.content.is_empty()))
                .expect("heading span")
                .style
        };
        assert_eq!(style_of(&h1), MarkdownTheme::default_dark().h1);
        assert_eq!(style_of(&h2), MarkdownTheme::default_dark().h2);
        assert_eq!(style_of(&h3), MarkdownTheme::default_dark().h3);
    }

    #[test]
    fn inline_bold_applies_bold_modifier() {
        let lines = render(
            "foo **bar** baz",
            80,
            &MarkdownTheme::default_dark(),
        );
        let spans: Vec<_> = lines
            .into_iter()
            .flat_map(|line| line.spans.into_iter())
            .collect();
        let bar = spans
            .iter()
            .find(|span| span.content.as_ref() == "bar")
            .expect("bar span");
        assert!(bar.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn inline_italic_applies_italic_modifier() {
        let lines = render("a *b* c", 80, &MarkdownTheme::default_dark());
        let spans: Vec<_> = lines
            .into_iter()
            .flat_map(|l| l.spans.into_iter())
            .collect();
        let b = spans
            .iter()
            .find(|s| s.content.as_ref() == "b")
            .expect("b span");
        assert!(b.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn inline_strikethrough_applies_crossed_out_modifier() {
        let lines = render(
            "x ~~gone~~ y",
            80,
            &MarkdownTheme::default_dark(),
        );
        let spans: Vec<_> = lines
            .into_iter()
            .flat_map(|l| l.spans.into_iter())
            .collect();
        let gone = spans
            .iter()
            .find(|s| s.content.as_ref() == "gone")
            .expect("gone span");
        assert!(gone.style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn inline_code_is_styled_distinctly() {
        let lines = render(
            "run `cargo test` now",
            80,
            &MarkdownTheme::default_dark(),
        );
        let spans: Vec<_> = lines
            .into_iter()
            .flat_map(|l| l.spans.into_iter())
            .collect();
        let code = spans
            .iter()
            .find(|s| s.content.as_ref() == "cargo test")
            .expect("code span");
        assert_eq!(code.style, MarkdownTheme::default_dark().inline_code);
    }

    #[test]
    fn soft_line_breaks_render_as_spaces() {
        // A single newline inside a paragraph in CommonMark is a
        // soft break → space, not a line break.
        let out = render_plain("line one\nline two", 80);
        assert_eq!(out, vec!["line one line two".to_string()]);
    }

    #[test]
    fn hard_line_breaks_render_as_newlines() {
        // Two trailing spaces at end of "line one" create a hard
        // break.
        let out = render_plain("line one  \nline two", 80);
        assert_eq!(out, vec!["line one".to_string(), "line two".into()]);
    }

    #[test]
    fn long_paragraph_wraps_at_word_boundaries() {
        let out = render_plain("alpha beta gamma delta", 11);
        // "alpha beta " == 11 chars, fits. "gamma delta"
        // == 11 chars, fits.
        assert_eq!(out.len(), 2);
        assert!(out[0].ends_with("beta") || out[0].ends_with("beta "));
        assert!(out[1].starts_with("gamma"));
    }

    #[test]
    fn raw_html_rendered_as_literal_text() {
        let out = render_plain("before <b>inside</b> after", 80);
        // pulldown-cmark treats a single line of raw HTML as
        // InlineHtml. Whatever it emits ends up as literal text.
        assert!(
            out[0].contains("<b>"),
            "HTML should render as literal: {out:?}"
        );
    }

    #[test]
    fn horizontal_rule_emits_full_width_line() {
        let out = render_plain("a\n\n---\n\nb", 10);
        assert!(out.iter().any(|s| s == &"─".repeat(10)), "{out:?}");
    }

    #[test]
    fn unhandled_block_falls_through_to_plain_text() {
        // List items aren't styled yet (PR C). Verify the text
        // still surfaces instead of being silently dropped.
        let out = render_plain("- one\n- two\n- three", 80);
        let joined = out.join("\n");
        assert!(joined.contains("one"));
        assert!(joined.contains("two"));
        assert!(joined.contains("three"));
    }
}
