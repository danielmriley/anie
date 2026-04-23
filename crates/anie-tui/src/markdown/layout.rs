//! Transform a `pulldown-cmark` event stream into ratatui
//! `Line`s, applying theme styles and wrapping.
//!
//! **Scope of PR A + B + C + D (Plan 05).** This module handles
//! headings, paragraphs, bold / italic / strikethrough, inline
//! code, HTML-as-literal-text, horizontal rules, fenced /
//! indented code blocks with syntect-based highlighting,
//! unordered and ordered lists (with nesting), blockquotes,
//! GitHub-flavored tables, inline / autolink / reference links
//! (with a visible URL fallback — see `link.rs` for the OSC 8
//! deferral rationale), and inline images (as `[image: alt]`
//! placeholders). Unimplemented element events still fall
//! through to a plain-text stringification.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, LinkType, Tag, TagEnd};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::theme::MarkdownTheme;
use crate::render_debug::{PerfSpan, PerfSpanKind};

/// Render a markdown string to ratatui lines at the given width.
///
/// PR A behavior: headings + paragraphs + inline formatting.
/// Unhandled block elements degrade to raw text extraction.
#[must_use]
pub fn render(text: &str, width: u16, theme: &MarkdownTheme) -> Vec<Line<'static>> {
    let mut span = PerfSpan::enter(PerfSpanKind::MarkdownRender);
    let mut builder = LineBuilder::new(width.max(1), theme);
    let events = super::parser::parse(text);
    for event in events {
        builder.consume(event);
    }
    let out = builder.finish();
    if let Some(s) = span.as_mut() {
        s.record("text_len", u64::try_from(text.len()).unwrap_or(u64::MAX));
        s.record("width", u64::from(width));
        s.record("lines", u64::try_from(out.len()).unwrap_or(u64::MAX));
    }
    drop(span);
    out
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
    // Populated while inside a `| … |` GFM table. Cells are
    // collected as plain strings (inline styling inside cells is
    // flattened for PR C; inline-in-cell styling can come later).
    table: Option<TableState>,
    // Populated between Start(Link) and End(Link). Captures the
    // link type + URL + accumulated text so End(Link) can decide
    // whether to append ` (url)`.
    link: Option<LinkState>,
    // Populated between Start(Image) and End(Image). Alt text is
    // accumulated here; End(Image) emits `[image: alt]`.
    image: Option<ImageState>,
}

#[derive(Debug)]
struct LinkState {
    link_type: LinkType,
    url: String,
    text_buf: String,
}

#[derive(Debug)]
struct ImageState {
    url: String,
    alt_buf: String,
}

#[derive(Debug, Default)]
struct TableState {
    alignments: Vec<Alignment>,
    header: Option<Vec<String>>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_header: bool,
    in_cell: bool,
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
            table: None,
            link: None,
            image: None,
        }
    }

    fn consume(&mut self, event: Event<'_>) {
        // While inside a table, route everything through the
        // collector so cell content doesn't leak to the main line
        // buffer. Keep this above the code-block check — pi never
        // embeds code blocks inside tables, and if it did we'd
        // want the outer (table) context to win for framing.
        if self.table.is_some() {
            self.handle_table_event(event);
            return;
        }
        // While inside an image tag, consume Text events into the
        // alt buffer and finalize on End(Image). Other events
        // inside an image (rare: pulldown-cmark can emit inline
        // formatting inside alt text) are ignored.
        if self.image.is_some() {
            match event {
                Event::Text(text) => {
                    if let Some(img) = self.image.as_mut() {
                        img.alt_buf.push_str(&text);
                    }
                }
                Event::End(TagEnd::Image) => {
                    if let Some(img) = self.image.take() {
                        let display = if img.alt_buf.is_empty() {
                            format!("[image: {}]", img.url)
                        } else {
                            format!("[image: {}]", img.alt_buf)
                        };
                        self.push_styled(&display, self.theme.link_url);
                    }
                }
                _ => {}
            }
            return;
        }
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
                self.push_blank_separator();
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
            Tag::Strong => self.push_style(self.theme.strong),
            Tag::Emphasis => self.push_style(self.theme.emphasis),
            Tag::Strikethrough => self.push_style(self.theme.strikethrough),
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
                self.push_style(self.theme.blockquote_body);
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
            Tag::Table(alignments) => {
                self.flush_line();
                self.table = Some(TableState {
                    alignments,
                    ..TableState::default()
                });
            }
            // TableHead / TableRow / TableCell are routed through
            // `handle_table_event` by the `self.table.is_some()`
            // early-return in `consume`, so they never reach this
            // match. The arms exist only to satisfy exhaustiveness
            // in the unusual case that the parser emits them
            // without a preceding `Tag::Table`.
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
            Tag::Link {
                link_type,
                dest_url,
                ..
            } => {
                self.link = Some(LinkState {
                    link_type,
                    url: dest_url.to_string(),
                    text_buf: String::new(),
                });
                self.push_style(self.theme.link_text);
            }
            Tag::Image { dest_url, .. } => {
                self.image = Some(ImageState {
                    url: dest_url.to_string(),
                    alt_buf: String::new(),
                });
            }
            // Block elements not yet handled: render their inline
            // content as plain text, ignore their structure.
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
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
                    self.push_blank_separator();
                }
            }
            TagEnd::Heading(_) => {
                self.flush_line();
                self.pop_style();
                self.current_heading = None;
                self.push_blank_separator();
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
                self.pop_style();
                // Blank line after the quote block, outside the
                // gutter.
                if self.blockquote_depth == 0 && self.list_stack.is_empty() {
                    self.push_blank_separator();
                }
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.list_stack.pop();
                // Trailing blank line separates a closed list
                // from following content, but only when we're
                // back out to the top level.
                if self.list_stack.is_empty() && self.blockquote_depth == 0 {
                    self.push_blank_separator();
                }
            }
            TagEnd::Item => {
                self.flush_line();
            }
            TagEnd::Table | TagEnd::TableHead | TagEnd::TableRow | TagEnd::TableCell => {
                // Normal flow: handled by `handle_table_event`.
                // This arm only fires on orphan end tags.
            }
            TagEnd::Link => {
                self.pop_style();
                if let Some(link) = self.link.take() {
                    if super::link::should_show_trailing_url(
                        link.link_type,
                        &link.text_buf,
                        &link.url,
                    ) {
                        let suffix = super::link::format_link_suffix(&link.url);
                        self.push_styled(&suffix, self.theme.link_url);
                    }
                }
            }
            TagEnd::Image => {
                // Normal flow: consume() intercepts End(Image)
                // via the `self.image.is_some()` path. Orphan
                // end tag — no-op.
            }
            TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
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
        // Mirror link text into the active link buffer so End(Link)
        // can decide whether the trailing ` (url)` is redundant
        // (skip when the link body already reads as the URL).
        if let Some(link) = self.link.as_mut() {
            link.text_buf.push_str(text);
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
            self.push_blank_separator();
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

    /// Route events that land inside a `Tag::Table(_)` to the
    /// accumulating `TableState`. Inline styling (bold / italic /
    /// inline code) is flattened to plain cell text in PR C; the
    /// visual loss is limited to not bolding header labels that
    /// use `**`, which is an acceptable tradeoff for the
    /// complexity savings.
    fn handle_table_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(Tag::TableHead) => {
                if let Some(state) = self.table.as_mut() {
                    state.in_header = true;
                }
            }
            Event::Start(Tag::TableRow) => {
                if let Some(state) = self.table.as_mut() {
                    state.current_row.clear();
                }
            }
            Event::Start(Tag::TableCell) => {
                if let Some(state) = self.table.as_mut() {
                    state.in_cell = true;
                    state.current_cell.clear();
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some(state) = self.table.as_mut() {
                    if state.in_cell {
                        state.current_cell.push_str(&text);
                    }
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(state) = self.table.as_mut() {
                    let cell = std::mem::take(&mut state.current_cell);
                    state.current_row.push(cell);
                    state.in_cell = false;
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(state) = self.table.as_mut() {
                    let row = std::mem::take(&mut state.current_row);
                    state.header = Some(row);
                    state.in_header = false;
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(state) = self.table.as_mut() {
                    let row = std::mem::take(&mut state.current_row);
                    state.rows.push(row);
                }
            }
            Event::End(TagEnd::Table) => {
                if let Some(state) = self.table.take() {
                    self.emit_table(state);
                }
            }
            // Inline formatting / soft breaks inside cells: just
            // append whitespace or ignore. pulldown-cmark can emit
            // Start/End(Strong) etc. within cells — style is
            // flattened for PR C.
            Event::SoftBreak | Event::HardBreak => {
                if let Some(state) = self.table.as_mut() {
                    if state.in_cell && !state.current_cell.is_empty() {
                        state.current_cell.push(' ');
                    }
                }
            }
            _ => {}
        }
    }

    /// Render a collected `TableState` as a unicode box-drawing
    /// table. Columns are auto-sized to the widest cell (with a
    /// minimum of 1 character). Alignment per column comes from
    /// the GFM `| :-- | :-: | --: |` separator row, passed in
    /// via `Tag::Table(alignments)`.
    ///
    /// Plan 10 PR-C: tables now fit the available viewport
    /// width by compressing columns proportionally and wrapping
    /// cell contents across multiple rendered rows. When the
    /// viewport is too narrow for a stable table (every column
    /// would need to shrink below `MIN_COL_WIDTH`), the table
    /// falls back to a raw wrapped-markdown rendering so users
    /// still see the data, just not in a table frame.
    fn emit_table(&mut self, state: TableState) {
        let cols = state.alignments.len();
        if cols == 0 {
            return;
        }

        // Natural widths — the column size each cell would
        // prefer if we had infinite viewport space.
        let mut natural: Vec<usize> = vec![1; cols];
        if let Some(header) = state.header.as_ref() {
            for (idx, cell) in header.iter().enumerate() {
                if idx < cols {
                    natural[idx] = natural[idx].max(cell.chars().count());
                }
            }
        }
        for row in &state.rows {
            for (idx, cell) in row.iter().enumerate() {
                if idx < cols {
                    natural[idx] = natural[idx].max(cell.chars().count());
                }
            }
        }

        let available = self.width.max(1) as usize;
        let Some(widths) = compute_column_widths(&natural, available) else {
            // Fallback: render the table as wrapped raw
            // markdown text. Less pretty, but readable on
            // narrow terminals and pipelines.
            self.emit_table_as_raw_text(&state);
            return;
        };

        let border_style = self.theme.table_border;
        let header_style = self.theme.table_header;
        let cell_style = self.theme.table_cell;

        self.lines
            .push(table_border_line(&widths, '┌', '┬', '┐', border_style));
        if let Some(header) = state.header.as_ref() {
            for wrapped_row in wrap_table_row(header, &widths) {
                self.lines.push(table_data_row(
                    &wrapped_row,
                    &widths,
                    &state.alignments,
                    border_style,
                    header_style,
                ));
            }
            self.lines
                .push(table_border_line(&widths, '├', '┼', '┤', border_style));
        }
        for row in &state.rows {
            for wrapped_row in wrap_table_row(row, &widths) {
                self.lines.push(table_data_row(
                    &wrapped_row,
                    &widths,
                    &state.alignments,
                    border_style,
                    cell_style,
                ));
            }
        }
        self.lines
            .push(table_border_line(&widths, '└', '┴', '┘', border_style));
        // Spacer after the table, outside the frame.
        self.push_blank_separator();
    }

    /// Fallback rendering when the viewport is too narrow to
    /// draw a stable table frame. Emits each row as a pipe-
    /// joined line of the natural cell text, wrapped to the
    /// viewport width via the usual text-wrap pipeline. Users
    /// lose the box-drawing frame but keep the data.
    fn emit_table_as_raw_text(&mut self, state: &TableState) {
        let style = self.theme.table_cell;
        let header_style = self.theme.table_header;
        if let Some(header) = state.header.as_ref() {
            let raw = header.join(" | ");
            let wrapped = wrap_plain_text_cell(&raw, self.width.max(1) as usize);
            for line in wrapped {
                self.lines
                    .push(Line::from(Span::styled(line, header_style)));
            }
        }
        for row in &state.rows {
            let raw = row.join(" | ");
            let wrapped = wrap_plain_text_cell(&raw, self.width.max(1) as usize);
            for line in wrapped {
                self.lines.push(Line::from(Span::styled(line, style)));
            }
        }
        // Spacer so subsequent content isn't glued to the
        // fallback block.
        self.push_blank_separator();
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
        self.push_blank_separator();
    }

    /// Push a blank line separator unless the last pushed line
    /// is already blank. Keeps inter-block spacing predictable
    /// without accumulating double-blanks when two block types
    /// each append their own trailing separator. Mirrors pi's
    /// `nextTokenType !== "space"` guard in
    /// `packages/tui/src/components/markdown.ts:318-330`.
    fn push_blank_separator(&mut self) {
        let last_is_blank = self
            .lines
            .last()
            .is_some_and(|line| line.spans.iter().all(|span| span.content.is_empty()));
        if !last_is_blank {
            self.lines.push(Line::default());
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

/// Sum the displayed character width of a span sequence.
/// Approximates cell width with `chars().count()` — good enough
/// for the ASCII + box-drawing chars we use in prefixes.
fn prefix_span_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Build a full-width table border row. Each column reserves
/// `widths[i] + 2` horizontal glyphs to account for the 1-space
/// padding on each side of cell content.
fn table_border_line(
    widths: &[usize],
    left: char,
    sep: char,
    right: char,
    style: Style,
) -> Line<'static> {
    let mut out = String::new();
    out.push(left);
    for (idx, w) in widths.iter().enumerate() {
        if idx > 0 {
            out.push(sep);
        }
        for _ in 0..(w + 2) {
            out.push('─');
        }
    }
    out.push(right);
    Line::from(Span::styled(out, style))
}

/// Build a single data / header row. Missing trailing cells (a
/// row with fewer cells than the column count) render as blank
/// space so the right border still aligns.
fn table_data_row(
    cells: &[String],
    widths: &[usize],
    alignments: &[Alignment],
    border_style: Style,
    cell_style: Style,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled("│".to_string(), border_style));
    for (idx, w) in widths.iter().enumerate() {
        let content = cells.get(idx).cloned().unwrap_or_default();
        let alignment = alignments.get(idx).copied().unwrap_or(Alignment::None);
        let padded = pad_cell(&content, *w, alignment);
        spans.push(Span::styled(format!(" {padded} "), cell_style));
        spans.push(Span::styled("│".to_string(), border_style));
    }
    Line::from(spans)
}

/// Minimum per-column budget before falling back to raw
/// wrapped markdown. Three characters per column is enough
/// to show at least a letter and a space, which readers can
/// still scan; anything narrower isn't a stable table.
const MIN_COL_WIDTH: usize = 3;

/// Compute per-column content widths that fit the available
/// viewport width. Returns `None` when every column would
/// need to shrink below `MIN_COL_WIDTH` — callers should fall
/// back to raw wrapped markdown in that case. Plan 10 PR-C.
fn compute_column_widths(natural: &[usize], available: usize) -> Option<Vec<usize>> {
    let cols = natural.len();
    if cols == 0 {
        return Some(Vec::new());
    }
    // Frame cost: `│ cell │ cell │` — each column contributes
    // 2 spaces of padding, plus one border glyph between
    // columns, plus two outer borders.
    let overhead = cols * 2 + (cols + 1);
    if available <= overhead {
        return None;
    }
    let budget = available - overhead;
    let natural_total: usize = natural.iter().sum();
    if natural_total <= budget {
        return Some(natural.to_vec());
    }
    // Too wide — shrink. Reject when even min-per-column
    // won't fit so callers hit the fallback.
    if cols * MIN_COL_WIDTH > budget {
        return None;
    }
    // Proportional distribution (floored), then shave excess
    // off the largest column until the sum matches budget.
    let mut widths: Vec<usize> = natural
        .iter()
        .map(|&n| ((n * budget) / natural_total.max(1)).max(MIN_COL_WIDTH))
        .collect();
    let mut sum: usize = widths.iter().sum();
    while sum > budget {
        // Shave 1 from the widest column that's above the min.
        let idx = widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > MIN_COL_WIDTH)
            .max_by_key(|(_, w)| **w)
            .map(|(i, _)| i);
        match idx {
            Some(i) => {
                widths[i] -= 1;
                sum -= 1;
            }
            None => break, // every column at min; caller should fall back
        }
    }
    if sum > budget {
        return None;
    }
    // If we still have spare budget (floor division losses),
    // distribute it back to the widest-natural columns so the
    // table doesn't leave whitespace on the right.
    while sum < budget {
        let idx = natural
            .iter()
            .enumerate()
            .max_by_key(|(i, n)| (**n, widths[*i]))
            .map(|(i, _)| i);
        match idx {
            Some(i) => {
                widths[i] += 1;
                sum += 1;
            }
            None => break,
        }
    }
    Some(widths)
}

/// Wrap each cell in a row to its column width. Returns a
/// vector of row slices — one per wrapped line — where each
/// slice has `widths.len()` cells. The shortest-cell rows
/// pad empty strings for alignment. Plan 10 PR-C.
fn wrap_table_row(cells: &[String], widths: &[usize]) -> Vec<Vec<String>> {
    let ncols = widths.len();
    let wrapped: Vec<Vec<String>> = widths
        .iter()
        .enumerate()
        .map(|(idx, w)| {
            let content = cells.get(idx).map(String::as_str).unwrap_or("");
            wrap_plain_text_cell(content, *w)
        })
        .collect();
    let rows = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
    (0..rows)
        .map(|r| {
            (0..ncols)
                .map(|c| wrapped[c].get(r).cloned().unwrap_or_default())
                .collect()
        })
        .collect()
}

/// Wrap a single piece of cell text to a maximum column
/// width, returning owned `String`s. Word-break aware —
/// breaks at whitespace when possible; otherwise at the
/// character boundary. Plan 10 PR-C.
fn wrap_plain_text_cell(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;
    for word in text.split_whitespace() {
        let word_chars = word.chars().count();
        let need_space = !current.is_empty();
        let space_cost = if need_space { 1 } else { 0 };
        if current_chars + space_cost + word_chars > width {
            // Flush current line before placing the new word.
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_chars = 0;
            }
            if word_chars > width {
                // Single long token exceeds column width —
                // hard-break it at character boundaries.
                let mut chars_iter = word.chars().peekable();
                while chars_iter.peek().is_some() {
                    let mut chunk = String::with_capacity(width);
                    for _ in 0..width {
                        match chars_iter.next() {
                            Some(c) => chunk.push(c),
                            None => break,
                        }
                    }
                    if chars_iter.peek().is_some() {
                        lines.push(chunk);
                    } else {
                        current = chunk;
                        current_chars = current.chars().count();
                    }
                }
                continue;
            }
        }
        if !current.is_empty() {
            current.push(' ');
            current_chars += 1;
        }
        current.push_str(word);
        current_chars += word_chars;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn pad_cell(content: &str, width: usize, alignment: Alignment) -> String {
    let content_width = content.chars().count();
    if content_width >= width {
        // Cell is wider than the column budget. After PR-C
        // this only happens on the fallback path; the normal
        // render path wraps cells to `width` via
        // `wrap_table_row` before calling `pad_cell`.
        return content.to_string();
    }
    let padding = width - content_width;
    match alignment {
        Alignment::None | Alignment::Left => {
            format!("{content}{}", " ".repeat(padding))
        }
        Alignment::Right => {
            format!("{}{content}", " ".repeat(padding))
        }
        Alignment::Center => {
            let left = padding / 2;
            let right = padding - left;
            format!("{}{content}{}", " ".repeat(left), " ".repeat(right))
        }
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
    let mut span_guard = PerfSpan::enter(PerfSpanKind::WrapSpans);
    let spans_in = spans.len();
    let width = width.max(1) as usize;

    // PR-E (Plan 04): previously this function flattened every
    // input char into a single `Vec<(char, Style)>` sized to the
    // whole input, then walked it. For a 5,000-char paragraph at
    // width=120 that's a 5,000-entry Vec that dies at function
    // end. New shape walks each input span in place and
    // maintains only a *single line's* worth of cells at a time
    // (roughly `width` entries) — O(width) working memory,
    // regardless of input size.
    //
    // Word-break semantics are unchanged: when the current line
    // fills, break at the last whitespace if one exists;
    // otherwise break at the char boundary. Trailing whitespace
    // is dropped from the flushed line. Preserves char-count
    // (USV) wrap counting per Plan 04's explicit deferral of
    // Unicode display-width correctness.

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_cells: Vec<(char, Style)> = Vec::with_capacity(width);
    let mut any_input = false;
    let mut char_count: usize = 0;

    for span in spans {
        let style = span.style;
        let text = span.content.into_owned();
        // Empty-span guard per the review: never flush a line
        // we didn't actually contribute to.
        if text.is_empty() {
            continue;
        }
        any_input = true;
        for ch in text.chars() {
            char_count += 1;
            if ch == '\n' {
                lines.push(cells_to_line(std::mem::take(&mut current_cells)));
                continue;
            }
            if current_cells.len() >= width {
                let break_at = current_cells
                    .iter()
                    .rposition(|(c, _)| c.is_whitespace());
                match break_at {
                    Some(idx) if idx + 1 < current_cells.len() => {
                        let remainder = current_cells.split_off(idx + 1);
                        // Drop the whitespace cell itself from
                        // the flushed line's tail.
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
    }

    if !any_input {
        return vec![Line::default()];
    }

    if !current_cells.is_empty() {
        lines.push(cells_to_line(current_cells));
    }
    if let Some(s) = span_guard.as_mut() {
        s.record("spans_in", u64::try_from(spans_in).unwrap_or(u64::MAX));
        s.record("char_count", u64::try_from(char_count).unwrap_or(u64::MAX));
        s.record("lines_out", u64::try_from(lines.len()).unwrap_or(u64::MAX));
        s.record("width", u64::try_from(width).unwrap_or(u64::MAX));
    }
    drop(span_guard);
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
    use ratatui::style::Modifier;

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
        // Footnote definitions are still unhandled. Verify the
        // body text surfaces instead of being silently dropped.
        let out = render_plain("Hello[^1].\n\n[^1]: the note", 80);
        let joined = out.join("\n");
        assert!(joined.contains("Hello"), "{out:?}");
    }

    #[test]
    fn inline_link_emits_text_with_trailing_url() {
        let out = render_plain("see [the docs](https://example.com)", 80);
        let joined = out.join("\n");
        assert!(joined.contains("the docs"), "text missing: {out:?}");
        assert!(
            joined.contains("(https://example.com)"),
            "trailing url missing: {out:?}"
        );
    }

    #[test]
    fn autolink_skips_redundant_trailing_url() {
        let out = render_plain("before <https://example.com> after", 80);
        let joined = out.join("\n");
        // URL shows once — no "(https://example.com)" appended.
        let occurrences = joined.matches("https://example.com").count();
        assert_eq!(occurrences, 1, "expected exactly one URL: {out:?}");
    }

    #[test]
    fn link_text_is_styled_with_link_text_theme() {
        let lines = render(
            "[doc](https://x.io)",
            80,
            &MarkdownTheme::default_dark(),
        );
        let text_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref() == "doc")
            .expect("link text span");
        assert_eq!(
            text_span.style,
            MarkdownTheme::default_dark().link_text
        );
    }

    #[test]
    fn link_trailing_url_is_styled_with_link_url_theme() {
        let lines = render(
            "[doc](https://x.io)",
            80,
            &MarkdownTheme::default_dark(),
        );
        let url_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("(https://x.io)"))
            .expect("url span");
        assert_eq!(
            url_span.style,
            MarkdownTheme::default_dark().link_url
        );
    }

    #[test]
    fn image_renders_as_alt_text_placeholder() {
        let out = render_plain("see ![a cat](https://img/cat.png)", 80);
        let joined = out.join("\n");
        assert!(
            joined.contains("[image: a cat]"),
            "placeholder missing: {out:?}"
        );
        // Image URL should NOT leak as literal text once we've
        // rendered the placeholder.
        assert!(
            !joined.contains("https://img/cat.png"),
            "url leaked: {out:?}"
        );
    }

    #[test]
    fn image_without_alt_falls_back_to_url_in_placeholder() {
        let out = render_plain("![](https://img/x.png)", 80);
        let joined = out.join("\n");
        assert!(
            joined.contains("[image: https://img/x.png]"),
            "placeholder missing: {out:?}"
        );
    }

    #[test]
    fn table_renders_with_unicode_box_drawing() {
        let md = "| h1 | h2 |\n| -- | -- |\n| a | b |";
        let out = render_plain(md, 80);
        let joined = out.join("\n");
        assert!(joined.contains('┌'), "missing top-left corner: {out:?}");
        assert!(joined.contains('┐'), "missing top-right corner: {out:?}");
        assert!(joined.contains('└'), "missing bottom-left corner: {out:?}");
        assert!(joined.contains('┘'), "missing bottom-right corner: {out:?}");
        assert!(joined.contains('├'), "missing header separator");
        assert!(joined.contains('┤'));
        assert!(joined.contains("h1"), "missing header cell");
        assert!(joined.contains("h2"));
        assert!(joined.contains("a"));
        assert!(joined.contains("b"));
    }

    #[test]
    fn table_columns_auto_size_to_widest_cell() {
        // "loooong" in column 0 forces width >= 7.
        let md = "| x | y |\n| - | - |\n| loooong | short |";
        let out = render_plain(md, 80);
        // Every row's leftmost cell frame should fit "loooong"
        // with its 1-space padding on each side.
        let body_rows: Vec<&String> = out
            .iter()
            .filter(|l| l.starts_with('│'))
            .collect();
        assert!(!body_rows.is_empty());
        // First data cell in row 0 is "x" padded to 7 chars.
        let header_row = body_rows[0];
        assert!(
            header_row.contains("x      "),
            "header not padded to longest column: {header_row:?}"
        );
    }

    #[test]
    fn table_alignment_right_aligns_cells() {
        // Width differs between header ("aa") and body ("x") so
        // right-alignment is observable: body cell becomes " x"
        // (one space padding on the left, right against border).
        let md = "| aa | bb |\n| --: | --: |\n| x | y |";
        let out = render_plain(md, 80);
        let joined = out.join("\n");
        assert!(
            joined.contains("│  x │"),
            "expected right-aligned ' x': {out:?}"
        );
    }

    #[test]
    fn table_with_missing_trailing_cells_still_aligns() {
        // Malformed row with only one cell in a two-column table.
        let md = "| a | b |\n| - | - |\n| one |";
        let out = render_plain(md, 80);
        // The data row must still have the right border.
        let data_row = out
            .iter()
            .rev()
            .find(|l| l.starts_with('│') && l.contains("one"))
            .expect("data row");
        assert!(
            data_row.ends_with('│'),
            "right border missing: {data_row:?}"
        );
    }

    #[test]
    fn empty_table_header_still_renders_frame() {
        // Just header + separator, no body rows.
        let md = "| h1 | h2 |\n| -- | -- |";
        let out = render_plain(md, 80);
        assert!(out.iter().any(|l| l.contains('┌')));
        assert!(out.iter().any(|l| l.contains('└')));
    }

    // Plan 10 PR-C — width-aware table tests.

    /// `compute_column_widths` keeps natural widths when the
    /// table fits the viewport.
    #[test]
    fn compute_column_widths_keeps_natural_when_fits() {
        let widths = compute_column_widths(&[5, 5, 5], 80).expect("fits");
        assert_eq!(widths, vec![5, 5, 5]);
    }

    /// When the table overflows, columns compress proportionally
    /// — the widest column gives up the most, never dropping
    /// below `MIN_COL_WIDTH`.
    #[test]
    fn compute_column_widths_compresses_when_overflowing() {
        // Two-column table at natural widths 30+10. Viewport 40
        // (budget 30 after borders). Proportional split: 30 →
        // ~23, 10 → ~7, sum 30.
        let widths = compute_column_widths(&[30, 10], 40).expect("fits");
        assert!(widths.iter().all(|w| *w >= MIN_COL_WIDTH));
        let overhead = 2 * 2 + (2 + 1);
        assert!(
            widths.iter().sum::<usize>() + overhead <= 40,
            "compressed widths exceed available: {widths:?}"
        );
        assert!(
            widths[0] > widths[1],
            "widest natural column must stay widest: {widths:?}"
        );
    }

    /// Very narrow viewport → fallback signal. Plan 10 PR-C's
    /// "too narrow for a stable table" path.
    #[test]
    fn compute_column_widths_returns_none_for_too_narrow_viewport() {
        // Three columns × MIN_COL_WIDTH = 9 content + overhead
        // (6 pad + 4 border) = 19. Viewport 15 is too narrow.
        assert!(compute_column_widths(&[10, 10, 10], 15).is_none());
        // Pathological: viewport smaller than the overhead alone.
        assert!(compute_column_widths(&[1, 1, 1], 5).is_none());
    }

    /// Wide markdown table wraps cells into the viewport rather
    /// than pushing the right border off-screen.
    #[test]
    fn markdown_table_wraps_cells_to_fit_viewport_width() {
        let md = "| Name | Description |\n\
                  | --- | --- |\n\
                  | short | This is a much longer description that needs wrapping |";
        let out = render_plain(md, 40);
        // Every table line must fit within 40 columns. Use
        // chars().count() to match the wrap metric.
        for line in &out {
            if line.starts_with('│') || line.starts_with('┌') || line.starts_with('└') {
                assert!(
                    line.chars().count() <= 40,
                    "table line exceeds viewport width: {line:?} (len={})",
                    line.chars().count()
                );
            }
        }
        // The long description must still be present — its
        // wrapped fragments across multiple rows should cover
        // each key word individually.
        let joined = out.join("\n");
        for fragment in ["much", "longer", "description", "wrapping"] {
            assert!(
                joined.contains(fragment),
                "expected '{fragment}' to survive wrap: {out:?}"
            );
        }
    }

    /// When the terminal is too narrow for a stable table, fall
    /// back to raw-wrapped markdown so data isn't lost.
    #[test]
    fn markdown_table_too_narrow_falls_back_to_wrapped_raw_markdown() {
        let md = "| A | B | C | D |\n| --- | --- | --- | --- |\n| a1 | b1 | c1 | d1 |";
        // 8 cols: four columns × MIN_COL_WIDTH (3) = 12 content
        // + 4*2 padding + 5 borders = 25. Viewport 10 is too
        // narrow → fallback.
        let out = render_plain(md, 10);
        // Fallback path: no box-drawing characters.
        let joined = out.join("\n");
        assert!(
            !joined.contains('┌'),
            "too-narrow table should not render box-drawing: {out:?}"
        );
        // Data still present.
        assert!(joined.contains("a1") && joined.contains("b1"));
    }

    /// Right-alignment survives the wrap path. Regression guard
    /// that the new multi-row render doesn't drop alignment.
    #[test]
    fn table_alignment_is_preserved_after_wrapping() {
        let md = "| Short | Long numeric |\n| ---: | ---: |\n| 1 | 99999 |";
        let out = render_plain(md, 40);
        let joined = out.join("\n");
        // Body row: "1" right-aligned within the "Short"
        // column width, and "99999" right-aligned in the
        // "Long numeric" column. Both end with a trailing
        // space-then-border.
        assert!(
            joined.contains("│     1"),
            "expected right-aligned '1': {out:?}"
        );
    }

    /// `wrap_plain_text_cell` wraps a word at the character
    /// boundary when the word alone exceeds the column width.
    /// Regression: long URLs and identifiers mustn't silently
    /// overflow.
    #[test]
    fn wrap_plain_text_cell_hard_breaks_oversized_tokens() {
        let wrapped = wrap_plain_text_cell("supercalifragilisticexpialidocious", 10);
        assert!(wrapped.len() >= 3);
        for line in &wrapped {
            assert!(line.chars().count() <= 10, "{line:?}");
        }
    }

    /// Multi-word text wraps at whitespace when possible.
    #[test]
    fn wrap_plain_text_cell_word_wraps_when_possible() {
        // "hello world" = 11 chars, "there friend" = 12 chars.
        // Width 12 fits both pairs on one line each.
        let wrapped = wrap_plain_text_cell("hello world there friend", 12);
        assert_eq!(wrapped, vec!["hello world", "there friend"]);
    }

    /// Width exactly at a word boundary doesn't lose chars.
    /// "abc def" at width 3 wraps to ["abc", "def"].
    #[test]
    fn wrap_plain_text_cell_preserves_all_characters_on_tight_fit() {
        let wrapped = wrap_plain_text_cell("abc def", 3);
        assert_eq!(wrapped, vec!["abc", "def"]);
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
