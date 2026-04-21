//! Transform a `pulldown-cmark` event stream into ratatui
//! `Line`s, applying theme styles and wrapping.
//!
//! **Scope of PR A + B + C.1 (Plan 05).** This module currently
//! handles headings, paragraphs, bold / italic / strikethrough,
//! inline code, HTML-as-literal-text, horizontal rules, fenced /
//! indented code blocks with syntect-based highlighting,
//! unordered and ordered lists (with nesting), and blockquotes.
//! Tables and links land in later PRs. Unimplemented element
//! events fall through to a plain-text stringification so
//! nothing crashes; the quality just improves as more PRs land.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Tag, TagEnd};
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
    // Populated while inside a fenced / indented code block.
    // Code block text is captured verbatim here and rendered as
    // a bordered, syntax-highlighted box on `End(CodeBlock)`.
    code_block: Option<CodeBlockState>,
    // Nesting state for `>` blockquotes. Every flushed line picks
    // up `│ ` gutter spans per depth.
    blockquote_depth: u32,
    // Active list nesting. Empty → not in a list. The outermost
    // frame is at index 0, innermost at the end.
    list_stack: Vec<ListFrame>,
    // One-shot override for the next line the builder flushes —
    // used to put the bullet / number marker on the first line of
    // a list item while the continuation lines fall back to the
    // default prefix from `continuation_prefix()`.
    pending_first_line_prefix: Option<Vec<Span<'static>>>,
}

struct CodeBlockState {
    lang: Option<String>,
    buf: String,
}

#[derive(Debug)]
struct ListFrame {
    // `Some(n)` → ordered list, with `n` as the next marker.
    // `None`    → unordered.
    next_number: Option<u64>,
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
            code_block: None,
            blockquote_depth: 0,
            list_stack: Vec::new(),
            pending_first_line_prefix: None,
        }
    }

    fn consume(&mut self, event: Event<'_>) {
        // While inside a code block, every event except raw text
        // and the matching end tag is a no-op — pulldown-cmark
        // only emits Text inside fenced / indented code blocks.
        if self.code_block.is_some() {
            match event {
                Event::Text(text) => {
                    if let Some(state) = self.code_block.as_mut() {
                        state.buf.push_str(&text);
                    }
                }
                Event::End(TagEnd::CodeBlock) => {
                    if let Some(state) = self.code_block.take() {
                        self.emit_code_block(state);
                    }
                }
                _ => {}
            }
            return;
        }
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
            Tag::CodeBlock(kind) => {
                self.flush_line();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        let trimmed = info.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            // Info strings can include extra
                            // attributes ("rust,ignore"); syntect
                            // only cares about the language token.
                            Some(
                                trimmed
                                    .split([',', ' '])
                                    .next()
                                    .unwrap_or(trimmed)
                                    .to_string(),
                            )
                        }
                    }
                    CodeBlockKind::Indented => None,
                };
                self.code_block = Some(CodeBlockState {
                    lang,
                    buf: String::new(),
                });
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth = self.blockquote_depth.saturating_add(1);
            }
            Tag::List(start) => {
                self.flush_line();
                self.list_stack.push(ListFrame {
                    next_number: start,
                });
            }
            Tag::Item => {
                // Flush whatever was pending from a prior item.
                self.flush_line();
                self.begin_list_item();
            }
            // Block elements not yet handled: render their inline
            // content as plain text, ignore their structure.
            Tag::HtmlBlock
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
                // already content above. Skip when inside a list
                // so tight / loose list items stay visually
                // compact instead of gaining a blank row between
                // each item.
                if !self.lines.is_empty() && self.list_stack.is_empty() {
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
            TagEnd::CodeBlock => {
                // Normal flow: consume() intercepts End(CodeBlock)
                // via the `self.code_block.is_some()` path and
                // calls emit_code_block. This arm only fires if
                // the end tag arrives without a matching start,
                // which pulldown-cmark shouldn't produce.
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                // Blank line after the quote block, outside the
                // gutter.
                if self.blockquote_depth == 0 && self.list_stack.is_empty() {
                    self.lines.push(Line::default());
                }
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.list_stack.pop();
                // Trailing blank line separates a closed list
                // from following content, but only when we're
                // back out to the top level.
                if self.list_stack.is_empty() && self.blockquote_depth == 0 {
                    self.lines.push(Line::default());
                }
            }
            TagEnd::Item => {
                self.flush_line();
            }
            TagEnd::HtmlBlock
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
        // Consume any pending first-line prefix even on empty
        // flushes so it doesn't accidentally leak to a later line.
        let first_prefix_override = self.pending_first_line_prefix.take();

        if self.current_spans.is_empty() {
            // Blank lines don't carry list or blockquote prefixes
            // — matches conventional rendering where a blank row
            // between paragraphs is truly blank.
            self.lines.push(Line::default());
            return;
        }

        let first_prefix = first_prefix_override
            .unwrap_or_else(|| self.continuation_prefix());
        let cont_prefix = self.continuation_prefix();
        let first_width = prefix_span_width(&first_prefix);
        let cont_width = prefix_span_width(&cont_prefix);

        let inner_first = (self.width as usize).saturating_sub(first_width).max(1);
        let inner_cont = (self.width as usize).saturating_sub(cont_width).max(1);

        let spans = std::mem::take(&mut self.current_spans);
        // Use the smaller width so neither line overflows the
        // terminal after the prefix is prepended.
        let wrap_width = inner_first.min(inner_cont).max(1) as u16;
        let wrapped = wrap_spans(spans, wrap_width);

        for (idx, line) in wrapped.into_iter().enumerate() {
            let mut combined: Vec<Span<'static>> = if idx == 0 {
                first_prefix.clone()
            } else {
                cont_prefix.clone()
            };
            combined.extend(line.spans);
            self.lines.push(Line::from(combined));
        }
    }

    /// Prefix applied to continuation (non-first) lines inside a
    /// blockquote / list context. Never includes the bullet —
    /// that's set once via `pending_first_line_prefix` at
    /// `Start(Item)`.
    fn continuation_prefix(&self) -> Vec<Span<'static>> {
        let mut parts: Vec<Span<'static>> = Vec::new();
        for _ in 0..self.blockquote_depth {
            parts.push(Span::styled("│ ", self.theme.blockquote_gutter));
        }
        if !self.list_stack.is_empty() {
            // Two spaces per nesting level + two spaces reserved
            // for the bullet column so content aligns under
            // wrapped rows.
            let indent = "  ".repeat(self.list_stack.len() + 1);
            parts.push(Span::raw(indent));
        }
        parts
    }

    /// Called at `Start(Item)`. Computes the bullet / number
    /// marker, records it as the pending first-line prefix, and
    /// advances the ordered-list counter if relevant.
    fn begin_list_item(&mut self) {
        let depth = match self.list_stack.len() {
            0 => return,
            n => n - 1,
        };
        let Some(frame) = self.list_stack.last_mut() else {
            return;
        };
        let (marker, marker_style) = match &mut frame.next_number {
            Some(n) => {
                let out = format!("{n}. ");
                *n += 1;
                (out, self.theme.list_bullet)
            }
            None => {
                let bullet = match depth {
                    0 => "• ",
                    1 => "◦ ",
                    _ => "▪ ",
                };
                (bullet.to_string(), self.theme.list_bullet)
            }
        };
        let mut prefix: Vec<Span<'static>> = Vec::new();
        for _ in 0..self.blockquote_depth {
            prefix.push(Span::styled("│ ", self.theme.blockquote_gutter));
        }
        // One `"  "` for each enclosing list level (not counting
        // the current one — that's the column the marker lives in).
        if depth > 0 {
            prefix.push(Span::raw("  ".repeat(depth)));
        }
        // Shared 2-column gutter before the bullet. This mirrors
        // the extra `"  "` reserved by `continuation_prefix` so
        // wrapped lines align under the item text.
        prefix.push(Span::raw("  "));
        prefix.push(Span::styled(marker, marker_style));
        self.pending_first_line_prefix = Some(prefix);
    }

    /// Render a completed code block as a bordered, syntax-
    /// highlighted box. The box is `self.width` columns wide with
    /// a top border that embeds the language label (if any) and
    /// a bottom border matching pi's visual framing conceptually
    /// (pi uses termimad's code-fence styling — we hand-draw the
    /// box so we don't take an extra crate dep just for this).
    fn emit_code_block(&mut self, state: CodeBlockState) {
        let border_style = self.theme.code_block_border;
        let lang_style = self.theme.code_block_lang;

        // Minimum width: 4 so "│  │" with at least one inner
        // column has room. Narrower terminals just get a squished
        // box — still correct.
        let width = self.width.max(4) as usize;
        let inner = width - 4;

        let code = state.buf.trim_end_matches('\n');

        self.lines.push(build_top_border(
            width,
            state.lang.as_deref(),
            border_style,
            lang_style,
        ));

        for highlighted in super::syntax::highlight_code(code, state.lang.as_deref()) {
            self.lines.push(build_code_body_line(
                highlighted,
                inner,
                border_style,
            ));
        }

        self.lines.push(build_bottom_border(width, border_style));
        // Spacer between the block and the following content.
        self.lines.push(Line::default());
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

/// Sum the displayed character width of a span sequence.
/// Approximates cell width with `chars().count()` — good enough
/// for the ASCII + box-drawing chars we use in prefixes.
fn prefix_span_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
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

/// Build the top border of a code-block box. Embeds the language
/// label when present: `╭─ rust ───────╮`. Bare blocks get a
/// plain `╭──────╮` row.
fn build_top_border(
    width: usize,
    lang: Option<&str>,
    border_style: Style,
    lang_style: Style,
) -> Line<'static> {
    if let Some(lang) = lang {
        let lang_display = lang.chars().count();
        // Layout: "╭─ {lang} " + dashes + "╮"
        //          = 1 + 1 + 1 + lang_display + 1 + dashes + 1 = width
        let used = 5 + lang_display;
        let dashes = width.saturating_sub(used);
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled("╭─ ", border_style));
        spans.push(Span::styled(lang.to_string(), lang_style));
        spans.push(Span::styled(
            format!(" {}╮", "─".repeat(dashes)),
            border_style,
        ));
        Line::from(spans)
    } else {
        let dashes = width.saturating_sub(2);
        Line::from(Span::styled(
            format!("╭{}╮", "─".repeat(dashes)),
            border_style,
        ))
    }
}

fn build_bottom_border(width: usize, border_style: Style) -> Line<'static> {
    let dashes = width.saturating_sub(2);
    Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(dashes)),
        border_style,
    ))
}

/// Wrap one syntect-highlighted code line inside `│ … │` with
/// right-padding so the right border aligns regardless of line
/// length. If the highlighted content exceeds `inner` columns it
/// is emitted un-truncated; the right border then ends up on the
/// next visual row. PR C can add proper horizontal scrolling if
/// we decide it's worth the complexity — code lines in practice
/// tend to stay within typical widths.
fn build_code_body_line(
    highlighted: Line<'static>,
    inner: usize,
    border_style: Style,
) -> Line<'static> {
    let body_width: usize = highlighted
        .spans
        .iter()
        .map(|s| s.content.chars().count())
        .sum();
    let padding = inner.saturating_sub(body_width);

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled("│ ", border_style));
    spans.extend(highlighted.spans);
    if padding > 0 {
        spans.push(Span::raw(" ".repeat(padding)));
    }
    spans.push(Span::styled(" │", border_style));
    Line::from(spans)
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
    fn fenced_code_block_has_top_and_bottom_borders() {
        let md = "```\ncode\n```";
        let out = render_plain(md, 20);
        assert!(
            out.iter().any(|l| l.starts_with('╭') && l.ends_with('╮')),
            "missing top border: {out:?}"
        );
        assert!(
            out.iter().any(|l| l.starts_with('╰') && l.ends_with('╯')),
            "missing bottom border: {out:?}"
        );
    }

    #[test]
    fn fenced_code_block_embeds_language_label_in_top_border() {
        let out = render_plain("```rust\nfn main() {}\n```", 30);
        let top = out
            .iter()
            .find(|l| l.starts_with('╭'))
            .expect("top border");
        assert!(top.contains("rust"), "top border missing lang: {top}");
    }

    #[test]
    fn bare_code_block_omits_language_label() {
        let out = render_plain("```\nplain\n```", 20);
        let top = out
            .iter()
            .find(|l| l.starts_with('╭'))
            .expect("top border");
        // Bare block should be just ╭─…─╮ with no embedded text.
        assert!(
            top.chars().skip(1).take(top.chars().count() - 2).all(|c| c == '─'),
            "bare top should be all dashes between corners: {top:?}"
        );
    }

    #[test]
    fn fenced_code_block_emits_one_body_line_per_source_line() {
        let out = render_plain("```\nline one\nline two\nline three\n```", 30);
        let body_lines: Vec<_> = out
            .iter()
            .filter(|l| l.starts_with('│'))
            .collect();
        assert_eq!(body_lines.len(), 3, "body lines: {out:?}");
    }

    #[test]
    fn code_block_body_lines_are_padded_to_inner_width() {
        // width=20 → inner=16. Each body line should be exactly
        // 20 chars wide including borders (1 + 1 + 16 + 1 + 1 = 20)
        let out = render_plain("```\nx\n```", 20);
        let body = out
            .iter()
            .find(|l| l.starts_with('│'))
            .expect("body");
        assert_eq!(body.chars().count(), 20, "width != 20: {body:?}");
    }

    #[test]
    fn indented_code_block_still_renders_as_a_box() {
        let out = render_plain("    indented\n    more\n", 30);
        assert!(
            out.iter().any(|l| l.starts_with('╭')),
            "indented block missing box: {out:?}"
        );
    }

    #[test]
    fn code_block_info_string_strips_attributes() {
        // pulldown-cmark passes "rust,ignore" through as-is. We
        // split on comma/space so syntect still finds the lang.
        let out = render_plain("```rust,ignore\nfn main() {}\n```", 30);
        let top = out
            .iter()
            .find(|l| l.starts_with('╭'))
            .expect("top border");
        assert!(top.contains("rust"), "expected lang=rust: {top}");
        assert!(!top.contains("ignore"), "attr leaked into label: {top}");
    }

    #[test]
    fn code_block_preserves_empty_body() {
        // Closing fence right after opening — no content. Box
        // still renders with just borders and no crash.
        let out = render_plain("```\n```", 20);
        assert!(out.iter().any(|l| l.starts_with('╭')), "{out:?}");
        assert!(out.iter().any(|l| l.starts_with('╰')), "{out:?}");
    }

    #[test]
    fn unhandled_block_falls_through_to_plain_text() {
        // Tables aren't styled yet (PR C.2). Verify the text
        // still surfaces instead of being silently dropped.
        let md = "| a | b |\n| - | - |\n| 1 | 2 |";
        let out = render_plain(md, 80);
        let joined = out.join("\n");
        assert!(joined.contains("a"));
        assert!(joined.contains("b"));
    }

    #[test]
    fn unordered_list_emits_bullet_prefix() {
        let out = render_plain("- one\n- two\n- three", 80);
        let joined = out.join("\n");
        assert!(joined.contains("• one"), "{out:?}");
        assert!(joined.contains("• two"), "{out:?}");
        assert!(joined.contains("• three"), "{out:?}");
    }

    #[test]
    fn ordered_list_preserves_source_numbering() {
        let out = render_plain("3. alpha\n4. beta\n5. gamma", 80);
        let joined = out.join("\n");
        assert!(joined.contains("3. alpha"), "{out:?}");
        assert!(joined.contains("4. beta"), "{out:?}");
        assert!(joined.contains("5. gamma"), "{out:?}");
    }

    #[test]
    fn nested_unordered_list_uses_distinct_bullets() {
        let md = "- outer\n  - inner\n    - deeper";
        let out = render_plain(md, 80);
        let joined = out.join("\n");
        assert!(joined.contains("• outer"), "outer missing: {out:?}");
        assert!(joined.contains("◦ inner"), "inner missing: {out:?}");
        assert!(joined.contains("▪ deeper"), "deeper missing: {out:?}");
    }

    #[test]
    fn nested_list_item_is_indented() {
        let md = "- outer\n  - inner";
        let out = render_plain(md, 80);
        let outer = out.iter().find(|l| l.contains("outer")).expect("outer");
        let inner = out.iter().find(|l| l.contains("inner")).expect("inner");
        // Inner line should start with more leading whitespace
        // than the outer line.
        let outer_ws = outer.chars().take_while(|c| c.is_whitespace()).count();
        let inner_ws = inner.chars().take_while(|c| c.is_whitespace()).count();
        assert!(inner_ws > outer_ws, "inner not indented: {out:?}");
    }

    #[test]
    fn blockquote_lines_have_gutter_prefix() {
        let out = render_plain("> quoted text", 80);
        let joined = out.join("\n");
        assert!(joined.contains("│ quoted text"), "{out:?}");
    }

    #[test]
    fn blockquote_spans_multiple_paragraphs() {
        let out = render_plain("> first\n>\n> second", 80);
        let joined = out.join("\n");
        assert!(joined.contains("│ first"), "{out:?}");
        assert!(joined.contains("│ second"), "{out:?}");
    }

    #[test]
    fn long_list_item_wraps_with_continuation_indent() {
        // Width 14 — forces wrapping. Item text is 18 chars.
        let out = render_plain("- alpha beta gamma", 14);
        let first = out
            .iter()
            .find(|l| l.contains("• "))
            .expect("bullet line");
        let bullet_col = first.find('•').expect("bullet col");
        // A continuation line (no bullet) should start with at
        // least `bullet_col + 2` spaces so wrapped content aligns
        // under the item text.
        let cont = out
            .iter()
            .skip_while(|l| !l.contains('•'))
            .skip(1)
            .find(|l| !l.is_empty())
            .expect("continuation line");
        let leading = cont.chars().take_while(|c| c.is_whitespace()).count();
        assert!(
            leading >= bullet_col + 2,
            "continuation {cont:?} not aligned under {first:?}"
        );
    }

    #[test]
    fn bullet_span_is_styled_with_list_bullet_theme() {
        let lines = render("- x", 80, &MarkdownTheme::default_dark());
        let bullet_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains('•'))
            .expect("bullet span");
        assert_eq!(
            bullet_span.style,
            MarkdownTheme::default_dark().list_bullet
        );
    }

    #[test]
    fn blockquote_gutter_span_uses_gutter_theme() {
        let lines = render("> q", 80, &MarkdownTheme::default_dark());
        let gutter = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains('│'))
            .expect("gutter span");
        assert_eq!(
            gutter.style,
            MarkdownTheme::default_dark().blockquote_gutter
        );
    }
}
