use std::time::Duration;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

use crate::markdown::MarkdownTheme;
use crate::terminal_capabilities::TerminalCapabilities;

/// A rendered transcript block.
#[derive(Debug, Clone, PartialEq)]
pub enum RenderedBlock {
    /// A user-authored message.
    UserMessage { text: String, timestamp: u64 },
    /// An assistant-authored message.
    AssistantMessage {
        text: String,
        thinking: String,
        is_streaming: bool,
        timestamp: u64,
        /// Provider-reported error attached to the assistant
        /// turn. Set when the stream ended with a non-terminal
        /// error (e.g. the model emitted only reasoning with no
        /// visible text or tool call). `None` on healthy turns.
        /// The renderer surfaces this as a distinct line after
        /// the thinking/answer so the user never sees a turn end
        /// silently.
        error_message: Option<String>,
    },
    /// A tool execution block.
    ToolCall {
        call_id: String,
        tool_name: String,
        args_display: String,
        result: Option<ToolCallResult>,
        is_executing: bool,
    },
    /// A neutral system message.
    SystemMessage { text: String },
}

/// Rendered tool result details.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallResult {
    /// Tool result body.
    pub content: String,
    /// Whether the tool failed.
    pub is_error: bool,
    /// Optional elapsed execution time.
    pub elapsed: Option<Duration>,
}

/// Pre-wrapped lines for one block at a specific terminal width.
/// Adopting pi's `(content, width) -> lines` component cache
/// (`pi/tui/components/markdown.ts`). We keep the cache parallel
/// to the block list rather than on each enum variant so the
/// public `RenderedBlock` shape stays the same and every existing
/// construction / pattern-match site is untouched.
#[derive(Debug, Clone)]
struct LineCache {
    width: u16,
    lines: Vec<Line<'static>>,
}

/// Render-time configuration carried alongside the blocks. Kept
/// on the pane so the public `render()` signature stays
/// unchanged — the embedding app mutates this via
/// `set_markdown_enabled` / `set_terminal_capabilities` when
/// config changes, and the pane invalidates its per-block cache
/// so the next frame re-renders with the new settings.
///
/// Mirrors pi's `RenderContext` in shape: capabilities (OSC 8 +
/// image protocol) + visual theme + runtime toggles.
#[derive(Debug, Clone)]
struct RenderContext {
    markdown_enabled: bool,
    capabilities: TerminalCapabilities,
    theme: MarkdownTheme,
}

impl Default for RenderContext {
    fn default() -> Self {
        Self {
            markdown_enabled: true,
            capabilities: TerminalCapabilities::default(),
            theme: MarkdownTheme::default_dark(),
        }
    }
}

/// Scrollable output pane.
pub struct OutputPane {
    blocks: Vec<RenderedBlock>,
    /// Parallel to `blocks`. `caches[i]` holds the pre-wrapped
    /// lines for `blocks[i]` at a given width; `None` on cache
    /// miss. Every `blocks` mutation MUST keep this vector
    /// aligned and invalidate the affected slot — see the
    /// `invalidate_*` helpers below.
    caches: Vec<Option<LineCache>>,
    scroll_offset: u16,
    auto_scroll: bool,
    last_total_lines: u16,
    last_viewport_height: u16,
    /// Visual rendering settings. Changing any field here
    /// invalidates the cache because block → line computations
    /// depend on it.
    render_context: RenderContext,
}

impl OutputPane {
    /// Create an empty output pane.
    #[must_use]
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            caches: Vec::new(),
            scroll_offset: 0,
            auto_scroll: true,
            last_total_lines: 0,
            last_viewport_height: 1,
            render_context: RenderContext::default(),
        }
    }

    /// Toggle markdown rendering for finalized assistant blocks.
    /// Streaming blocks always render as plain text — see the
    /// module comment for why.
    pub fn set_markdown_enabled(&mut self, enabled: bool) {
        if self.render_context.markdown_enabled == enabled {
            return;
        }
        self.render_context.markdown_enabled = enabled;
        self.invalidate_all_caches();
    }

    /// Whether markdown rendering is currently active.
    #[must_use]
    pub fn markdown_enabled(&self) -> bool {
        self.render_context.markdown_enabled
    }

    /// Record detected terminal capabilities. Today this only
    /// matters for `link.rs` (OSC 8 deferral lives there); in the
    /// future, image protocols + truecolor decisions can read off
    /// the same context.
    pub fn set_terminal_capabilities(&mut self, capabilities: TerminalCapabilities) {
        if self.render_context.capabilities == capabilities {
            return;
        }
        self.render_context.capabilities = capabilities;
        self.invalidate_all_caches();
    }

    fn invalidate_all_caches(&mut self) {
        for slot in &mut self.caches {
            *slot = None;
        }
    }

    /// Add a transcript block.
    pub fn add_block(&mut self, block: RenderedBlock) {
        self.blocks.push(block);
        self.caches.push(None);
    }

    fn invalidate_last(&mut self) {
        if let Some(slot) = self.caches.last_mut() {
            *slot = None;
        }
    }

    fn invalidate_at(&mut self, index: usize) {
        if let Some(slot) = self.caches.get_mut(index) {
            *slot = None;
        }
    }

    /// Read-only view of the current block list. Used by tests
    /// that assert on system-message content and by future UI
    /// features that need to inspect the transcript.
    #[must_use]
    pub fn blocks(&self) -> &[RenderedBlock] {
        &self.blocks
    }

    /// Add a user message block.
    pub fn add_user_message(&mut self, text: String, timestamp: u64) {
        self.add_block(RenderedBlock::UserMessage { text, timestamp });
    }

    /// Return the visible text of the last assistant message, if any.
    pub fn last_assistant_text(&self) -> Option<&str> {
        self.blocks.iter().rev().find_map(|block| match block {
            RenderedBlock::AssistantMessage { text, .. } if !text.is_empty() => Some(text.as_str()),
            _ => None,
        })
    }

    /// Add an empty streaming assistant block.
    pub fn add_streaming_assistant(&mut self) {
        self.add_block(RenderedBlock::AssistantMessage {
            text: String::new(),
            thinking: String::new(),
            is_streaming: true,
            timestamp: 0,
            error_message: None,
        });
    }

    /// Append text to the last assistant block.
    pub fn append_to_last_assistant(&mut self, text: &str) {
        if let Some(RenderedBlock::AssistantMessage { text: content, .. }) = self.blocks.last_mut()
        {
            content.push_str(text);
            self.invalidate_last();
        }
    }

    /// Append thinking text to the last assistant block.
    pub fn append_thinking_to_last_assistant(&mut self, text: &str) {
        if let Some(RenderedBlock::AssistantMessage { thinking, .. }) = self.blocks.last_mut() {
            thinking.push_str(text);
            self.invalidate_last();
        }
    }

    /// Finalize the last assistant block.
    ///
    /// `error_message` surfaces a provider-reported failure that
    /// accompanied the assistant turn (e.g. the model emitted
    /// only reasoning and no visible text). Rendering always
    /// includes a trailing line for this so the user never sees a
    /// turn end silently on an error.
    pub fn finalize_last_assistant(
        &mut self,
        text: String,
        thinking: String,
        timestamp: u64,
        error_message: Option<String>,
    ) {
        if let Some(RenderedBlock::AssistantMessage {
            text: current_text,
            thinking: current_thinking,
            is_streaming,
            timestamp: current_timestamp,
            error_message: current_error,
        }) = self.blocks.last_mut()
        {
            *current_text = text;
            *current_thinking = thinking;
            *is_streaming = false;
            *current_timestamp = timestamp;
            *current_error = error_message;
            self.invalidate_last();
        }
    }

    /// Add a tool call block.
    pub fn add_tool_call(&mut self, call_id: String, tool_name: String, args_display: String) {
        self.add_block(RenderedBlock::ToolCall {
            call_id,
            tool_name,
            args_display,
            result: None,
            is_executing: true,
        });
    }

    /// Update an existing tool block with partial output.
    pub fn update_tool_result(
        &mut self,
        call_id: &str,
        content: String,
        is_error: bool,
        elapsed: Option<Duration>,
    ) {
        let Some(index) = self.find_tool_call_index(call_id) else {
            return;
        };
        if let Some(RenderedBlock::ToolCall { result, .. }) = self.blocks.get_mut(index) {
            *result = Some(ToolCallResult {
                content,
                is_error,
                elapsed,
            });
            self.invalidate_at(index);
        }
    }

    /// Finalize an existing tool block.
    pub fn finalize_tool_result(
        &mut self,
        call_id: &str,
        content: String,
        is_error: bool,
        elapsed: Option<Duration>,
    ) {
        let Some(index) = self.find_tool_call_index(call_id) else {
            return;
        };
        if let Some(RenderedBlock::ToolCall {
            result,
            is_executing,
            ..
        }) = self.blocks.get_mut(index)
        {
            *result = Some(ToolCallResult {
                content,
                is_error,
                elapsed,
            });
            *is_executing = false;
            self.invalidate_at(index);
        }
    }

    /// Add a system message.
    pub fn add_system_message(&mut self, text: String) {
        self.add_block(RenderedBlock::SystemMessage { text });
    }

    /// Clear transcript contents.
    pub fn clear(&mut self) {
        self.blocks.clear();
        self.caches.clear();
        self.scroll_offset = 0;
        self.auto_scroll = true;
        self.last_total_lines = 0;
        self.last_viewport_height = 1;
    }

    /// Scroll the pane upward by a number of rendered lines.
    pub fn scroll_line_up(&mut self, amount: u16) {
        let current = self.current_scroll();
        self.set_scroll(current.saturating_sub(amount.max(1)));
    }

    /// Scroll the pane downward by a number of rendered lines.
    pub fn scroll_line_down(&mut self, amount: u16) {
        let current = self.current_scroll();
        self.set_scroll(current.saturating_add(amount.max(1)));
    }

    /// Scroll the pane upward by one viewport height.
    pub fn scroll_page_up(&mut self) {
        self.scroll_line_up(self.last_viewport_height.max(1));
    }

    /// Scroll the pane downward by one viewport height.
    pub fn scroll_page_down(&mut self) {
        self.scroll_line_down(self.last_viewport_height.max(1));
    }

    /// Jump to the earliest transcript content.
    pub fn scroll_to_top(&mut self) {
        self.set_scroll(0);
    }

    /// Jump to the latest transcript content and re-enable auto-follow.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = self.max_scroll();
        self.auto_scroll = true;
    }

    /// Whether the pane is currently following the bottom.
    #[must_use]
    pub fn is_at_bottom(&self) -> bool {
        self.current_scroll() >= self.max_scroll()
    }

    /// Whether the pane is currently scrolled away from the latest output.
    #[must_use]
    pub fn is_scrolled(&self) -> bool {
        self.max_scroll() > 0 && !self.is_at_bottom()
    }

    /// Render the output pane.
    pub fn render(
        &mut self,
        area: ratatui::layout::Rect,
        buf: &mut ratatui::buffer::Buffer,
        spinner_frame: &str,
    ) {
        let lines = self.build_lines(area.width.max(1), spinner_frame);
        self.last_total_lines = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        self.last_viewport_height = area.height.max(1);
        let scroll = self.current_scroll();
        self.set_scroll(scroll);
        Paragraph::new(lines)
            .scroll((self.scroll_offset, 0))
            .render(area, buf);
    }

    fn build_lines(&mut self, width: u16, spinner_frame: &str) -> Vec<Line<'static>> {
        debug_assert_eq!(
            self.blocks.len(),
            self.caches.len(),
            "block and cache vectors must stay parallel",
        );
        let mut out = Vec::new();
        for (index, block) in self.blocks.iter().enumerate() {
            if !out.is_empty() {
                out.push(Line::default());
            }

            // Spinner-bearing blocks (`is_streaming` /
            // `is_executing`) change every tick independent of the
            // block's state, so they skip the cache. Usually only
            // 1-2 of these exist at a time (the current streaming
            // assistant and whichever tool is executing), so
            // recomputing them each frame is cheap relative to the
            // transcript walk we're collapsing.
            let hits_cache = !block_has_animated_content(block);

            if hits_cache
                && let Some(cached) = self.caches.get(index).and_then(Option::as_ref)
                && cached.width == width
            {
                out.extend(cached.lines.iter().cloned());
                continue;
            }

            let computed = block_lines(block, width, spinner_frame, &self.render_context);
            if hits_cache
                && let Some(slot) = self.caches.get_mut(index)
            {
                *slot = Some(LineCache {
                    width,
                    lines: computed.clone(),
                });
            }
            out.extend(computed);
        }
        if out.is_empty() {
            out.push(Line::default());
        }
        out
    }

    fn find_tool_call_index(&self, call_id: &str) -> Option<usize> {
        self.blocks.iter().position(|block| {
            matches!(
                block,
                RenderedBlock::ToolCall {
                    call_id: existing,
                    ..
                } if existing == call_id
            )
        })
    }

    fn current_scroll(&self) -> u16 {
        if self.auto_scroll {
            self.max_scroll()
        } else {
            self.scroll_offset.min(self.max_scroll())
        }
    }

    fn set_scroll(&mut self, scroll_offset: u16) {
        let max_scroll = self.max_scroll();
        self.scroll_offset = scroll_offset.min(max_scroll);
        self.auto_scroll = self.scroll_offset >= max_scroll;
    }

    fn max_scroll(&self) -> u16 {
        self.last_total_lines
            .saturating_sub(self.last_viewport_height)
    }
}

impl Default for OutputPane {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether rendering `block` produces spinner-dependent output
/// that changes between ticks even when the block's state does
/// not. Such blocks skip the line cache — see `OutputPane::build_lines`.
fn block_has_animated_content(block: &RenderedBlock) -> bool {
    match block {
        RenderedBlock::AssistantMessage { is_streaming, .. } => *is_streaming,
        RenderedBlock::ToolCall { is_executing, .. } => *is_executing,
        RenderedBlock::UserMessage { .. } | RenderedBlock::SystemMessage { .. } => false,
    }
}

fn block_lines(
    block: &RenderedBlock,
    width: u16,
    spinner_frame: &str,
    ctx: &RenderContext,
) -> Vec<Line<'static>> {
    match block {
        RenderedBlock::UserMessage { text, .. } => wrap_spans(
            vec![Span::styled(
                "> You: ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]
            .into_iter()
            .chain(vec![Span::raw(text.clone())])
            .collect(),
            width,
        ),
        RenderedBlock::AssistantMessage {
            text,
            thinking,
            is_streaming,
            error_message,
            ..
        } => assistant_block_lines(
            text,
            thinking,
            *is_streaming,
            error_message.as_deref(),
            width,
            spinner_frame,
            ctx,
        ),
        RenderedBlock::ToolCall {
            tool_name,
            args_display,
            result,
            is_executing,
            ..
        } => boxed_lines(
            format_tool_title(tool_name, args_display),
            if let Some(result) = result {
                if let Some(elapsed) = result.elapsed {
                    format!("{}\n\nTook {:.1}s", result.content, elapsed.as_secs_f64())
                } else {
                    result.content.clone()
                }
            } else if *is_executing {
                format!("{spinner_frame} executing...")
            } else {
                String::new()
            },
            width,
            result.as_ref().is_some_and(|value| value.is_error),
            *is_executing,
        ),
        RenderedBlock::SystemMessage { text } => {
            wrap_text(text, width, Style::default().fg(Color::DarkGray))
        }
    }
}

fn assistant_block_lines(
    text: &str,
    thinking: &str,
    is_streaming: bool,
    error_message: Option<&str>,
    width: u16,
    spinner_frame: &str,
    ctx: &RenderContext,
) -> Vec<Line<'static>> {
    let mut result = Vec::new();
    let inline_thinking_status = is_streaming && text.is_empty() && !thinking.is_empty();

    append_assistant_section(
        &mut result,
        assistant_thinking_lines(
            thinking,
            width,
            inline_thinking_status.then_some(spinner_frame),
        ),
    );
    append_assistant_section(
        &mut result,
        assistant_answer_lines(text, width, is_streaming, ctx),
    );
    append_assistant_section(
        &mut result,
        assistant_streaming_lines(
            text,
            thinking,
            is_streaming,
            inline_thinking_status,
            width,
            spinner_frame,
        ),
    );
    // Provider errors land at the bottom of the block so the user
    // sees the reason a turn produced no visible answer. Without
    // this, a thinking-only response would leave the user staring
    // at a thinking block with nothing after it.
    if let Some(message) = error_message {
        append_assistant_section(
            &mut result,
            assistant_error_lines(message, width),
        );
    }

    if result.is_empty() {
        vec![Line::default()]
    } else {
        result
    }
}

fn assistant_error_lines(message: &str, width: u16) -> Vec<Line<'static>> {
    let prefixed = format!("⚠ {message}");
    wrap_text(
        &prefixed,
        width,
        Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::BOLD),
    )
}

fn append_assistant_section(result: &mut Vec<Line<'static>>, section: Vec<Line<'static>>) {
    if section.is_empty() {
        return;
    }
    if !result.is_empty() {
        result.push(Line::default());
    }
    result.extend(section);
}

fn assistant_thinking_lines(
    thinking: &str,
    width: u16,
    streaming_spinner: Option<&str>,
) -> Vec<Line<'static>> {
    if thinking.is_empty() {
        return Vec::new();
    }

    let gutter = thinking_gutter(width);
    let mut lines = wrap_text("thinking", width, thinking_label_style());
    lines.extend(wrap_prefixed_text(
        thinking,
        width,
        gutter,
        thinking_gutter_style(),
        thinking_body_style(),
    ));
    if let Some(spinner_frame) = streaming_spinner {
        lines.extend(wrap_prefixed_text(
            &format!("{spinner_frame} thinking..."),
            width,
            gutter,
            thinking_gutter_style(),
            streaming_status_style(),
        ));
    }
    lines
}

/// Assistant-answer rendering with a streaming-vs-finalized split.
///
/// Streaming blocks always render as plain wrapped text — the
/// block's content changes every delta (potentially every few ms
/// under fast models), and running a CommonMark parse + syntect
/// highlight pass per frame would dominate the render loop and
/// break the block cache that Plan 02 of `tui_responsiveness/`
/// introduced. Once the block is finalized, we re-render as
/// markdown and the cache from `build_lines` memoizes it.
///
/// UX implication: during streaming the user sees raw markdown
/// syntax (`**bold**` literally). When the turn finalizes the
/// block "settles" into rendered markdown. pi's markdown widget
/// behaves the same way by construction.
fn assistant_answer_lines(
    text: &str,
    width: u16,
    is_streaming: bool,
    ctx: &RenderContext,
) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    if is_streaming || !ctx.markdown_enabled {
        return wrap_text(text, width, Style::default());
    }
    crate::markdown::render_markdown(text, width, &ctx.theme)
}

fn assistant_streaming_lines(
    text: &str,
    thinking: &str,
    is_streaming: bool,
    inline_thinking_status: bool,
    width: u16,
    spinner_frame: &str,
) -> Vec<Line<'static>> {
    if !is_streaming || inline_thinking_status {
        return Vec::new();
    }

    wrap_text(
        &format!(
            "{spinner_frame} {}",
            assistant_streaming_status_text(text, thinking)
        ),
        width,
        streaming_status_style(),
    )
}

fn assistant_streaming_status_text(text: &str, thinking: &str) -> &'static str {
    if !text.is_empty() {
        "responding..."
    } else if !thinking.is_empty() {
        "thinking..."
    } else {
        "streaming..."
    }
}

fn thinking_label_style() -> Style {
    Style::default().fg(Color::Indexed(246))
}

fn thinking_gutter_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn thinking_body_style() -> Style {
    Style::default()
        .fg(Color::Indexed(248))
        .add_modifier(Modifier::ITALIC)
}

fn streaming_status_style() -> Style {
    Style::default().fg(Color::Yellow)
}

fn thinking_gutter(width: u16) -> &'static str {
    match width.max(1) {
        1 => "",
        2 => "│",
        _ => "│ ",
    }
}

fn wrap_prefixed_text(
    text: &str,
    width: u16,
    prefix: &str,
    prefix_style: Style,
    text_style: Style,
) -> Vec<Line<'static>> {
    let prefix_width = prefix.chars().count() as u16;
    let content_width = width.max(1).saturating_sub(prefix_width).max(1);

    wrap_plain_text(text, content_width)
        .into_iter()
        .map(|line| {
            let mut spans = Vec::new();
            if !prefix.is_empty() {
                spans.push(Span::styled(prefix.to_string(), prefix_style));
            }
            if !line.is_empty() {
                spans.push(Span::styled(line, text_style));
            }
            if spans.is_empty() {
                Line::default()
            } else {
                Line::from(spans)
            }
        })
        .collect::<Vec<_>>()
}

fn wrap_text(text: &str, width: u16, style: Style) -> Vec<Line<'static>> {
    wrap_plain_text(text, width)
        .into_iter()
        .map(|line| Line::from(Span::styled(line, style)))
        .collect::<Vec<_>>()
}

fn wrap_plain_text(text: &str, width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        if raw_line.is_empty() {
            lines.push(String::new());
            continue;
        }
        let chars = raw_line.chars().collect::<Vec<_>>();
        for chunk in chars.chunks(width) {
            lines.push(chunk.iter().collect::<String>());
        }
    }
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

fn wrap_spans(spans: Vec<Span<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    let mut flattened = Vec::new();
    for span in spans {
        let style = span.style;
        let text = span.content.into_owned();
        if text.is_empty() {
            continue;
        }
        flattened.extend(text.chars().map(|ch| (ch, style)));
    }

    let mut lines = Vec::new();
    let mut current = Vec::new();
    for (index, (ch, style)) in flattened.into_iter().enumerate() {
        current.push(Span::styled(ch.to_string(), style));
        if (index + 1) % width == 0 {
            lines.push(Line::from(std::mem::take(&mut current)));
        }
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    if lines.is_empty() {
        vec![Line::default()]
    } else {
        lines
    }
}

fn boxed_lines(
    title: String,
    body: String,
    width: u16,
    is_error: bool,
    is_executing: bool,
) -> Vec<Line<'static>> {
    let width = width.max(4) as usize;
    let title = title.trim();
    let available = width.saturating_sub(2);
    let title_body = format!("─ {title} ");
    let top_fill = "─".repeat(available.saturating_sub(title_body.chars().count()));
    let top = format!("┌{title_body}{top_fill}┐");

    let border_style = if is_error {
        Style::default().fg(Color::Red)
    } else if is_executing {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let mut lines = vec![Line::from(Span::styled(top, border_style))];
    for text in wrap_plain_text(&body, (width.saturating_sub(4)) as u16) {
        let text_style = diff_line_style(&text, is_error);
        let visible_chars = text.chars().count();
        let padding = " ".repeat(width.saturating_sub(4 + visible_chars));
        lines.push(Line::from(vec![
            Span::styled("│ ", border_style),
            Span::styled(text, text_style),
            Span::styled(padding, border_style),
            Span::styled(" │", border_style),
        ]));
    }
    let bottom = format!("└{}┘", "─".repeat(available));
    lines.push(Line::from(Span::styled(bottom, border_style)));
    lines
}

fn diff_line_style(line: &str, is_error: bool) -> Style {
    if line.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') || is_error {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn format_tool_title(tool_name: &str, args_display: &str) -> String {
    match tool_name {
        "bash" if !args_display.is_empty() => format!("$ {args_display}"),
        "bash" => "$ bash".into(),
        "read" | "write" | "edit" if !args_display.is_empty() => {
            format!("{tool_name} {args_display}")
        }
        _ => tool_name.to_string(),
    }
}

#[cfg(test)]
impl OutputPane {
    /// Whether `blocks[index]` currently has a cached line set.
    /// Test-only accessor so cache-behavior tests can assert hit /
    /// miss / invalidation without peeking at private fields.
    pub(crate) fn is_cached(&self, index: usize) -> bool {
        self.caches.get(index).is_some_and(Option::is_some)
    }

    /// Number of block slots (should always equal `blocks.len()`).
    pub(crate) fn cache_slot_count(&self) -> usize {
        self.caches.len()
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    fn pane_with_settled_history() -> OutputPane {
        let mut pane = OutputPane::new();
        pane.add_user_message("first question".into(), 1);
        pane.add_streaming_assistant();
        pane.append_to_last_assistant("an answer");
        pane.finalize_last_assistant(
            "an answer".into(),
            String::new(),
            2,
            None,
        );
        pane.add_user_message("second question".into(), 3);
        pane.add_streaming_assistant();
        pane.append_to_last_assistant("another answer");
        pane.finalize_last_assistant(
            "another answer".into(),
            String::new(),
            4,
            None,
        );
        pane
    }

    #[test]
    fn cache_starts_empty_and_parallels_blocks() {
        let pane = pane_with_settled_history();
        assert_eq!(pane.cache_slot_count(), pane.blocks().len());
        for index in 0..pane.blocks().len() {
            assert!(!pane.is_cached(index));
        }
    }

    #[test]
    fn to_lines_populates_cache_for_non_animated_blocks() {
        let mut pane = pane_with_settled_history();
        let _ = pane.build_lines(80, ".");
        for index in 0..pane.blocks().len() {
            assert!(
                pane.is_cached(index),
                "block {index} should cache after first render"
            );
        }
    }

    #[test]
    fn cache_hit_returns_identical_output_as_fresh_compute() {
        let mut pane = pane_with_settled_history();
        let first = pane.build_lines(80, ".");
        let second = pane.build_lines(80, ".");
        assert_eq!(first, second);
    }

    #[test]
    fn width_change_invalidates_cache_implicitly() {
        // Use text long enough that wrapping changes noticeably
        // between the two widths.
        let long_answer = "one two three four five six seven eight nine ten "
            .repeat(8);
        let mut pane = OutputPane::new();
        pane.add_user_message("prompt".into(), 1);
        pane.add_streaming_assistant();
        pane.finalize_last_assistant(long_answer, String::new(), 2, None);

        let wide = pane.build_lines(120, ".");
        // The cache is at width=120 now. Render at width=20:
        // different wrapping, different line count.
        let narrow = pane.build_lines(20, ".");
        assert_ne!(
            wide.len(),
            narrow.len(),
            "wrapping at different widths must produce different line counts",
        );
        // Cache was repopulated at the new width.
        for index in 0..pane.blocks().len() {
            assert!(pane.is_cached(index));
        }
    }

    #[test]
    fn append_invalidates_only_the_last_block() {
        let mut pane = pane_with_settled_history();
        let _ = pane.build_lines(80, ".");
        // Start a new streaming block so appends land on it.
        pane.add_streaming_assistant();
        let _ = pane.build_lines(80, ".");
        pane.append_to_last_assistant("new token");

        let last = pane.blocks().len() - 1;
        assert!(
            !pane.is_cached(last),
            "streaming block should be invalidated after append"
        );
        for index in 0..last {
            assert!(
                pane.is_cached(index),
                "earlier block {index} should stay cached"
            );
        }
    }

    #[test]
    fn finalize_invalidates_last_and_caches_on_next_render() {
        let mut pane = pane_with_settled_history();
        pane.add_streaming_assistant();
        pane.append_to_last_assistant("partial");
        let _ = pane.build_lines(80, ".");
        // Streaming blocks skip the cache entirely.
        let last = pane.blocks().len() - 1;
        assert!(!pane.is_cached(last));
        pane.finalize_last_assistant(
            "partial answer".into(),
            String::new(),
            5,
            None,
        );
        let _ = pane.build_lines(80, ".");
        assert!(
            pane.is_cached(last),
            "finalized block should cache after next render"
        );
    }

    #[test]
    fn tool_call_update_invalidates_only_that_block() {
        let mut pane = OutputPane::new();
        pane.add_user_message("run tool".into(), 1);
        pane.add_tool_call("call_1".into(), "read".into(), "path.rs".into());
        pane.finalize_tool_result(
            "call_1",
            "contents".into(),
            false,
            Some(Duration::from_millis(50)),
        );
        pane.add_tool_call("call_2".into(), "bash".into(), "ls".into());
        pane.finalize_tool_result(
            "call_2",
            "output".into(),
            false,
            Some(Duration::from_millis(10)),
        );
        let _ = pane.build_lines(80, ".");
        for index in 0..pane.blocks().len() {
            assert!(pane.is_cached(index));
        }

        // Mutating call_1's result must only invalidate call_1.
        pane.finalize_tool_result(
            "call_1",
            "updated contents".into(),
            false,
            Some(Duration::from_millis(60)),
        );
        assert!(!pane.is_cached(1), "call_1 (index 1) should invalidate");
        assert!(pane.is_cached(2), "call_2 (index 2) should stay cached");
        assert!(pane.is_cached(0), "user message should stay cached");
    }

    #[test]
    fn animated_streaming_block_never_caches() {
        let mut pane = OutputPane::new();
        pane.add_streaming_assistant();
        pane.append_to_last_assistant("live");
        let _ = pane.build_lines(80, ".");
        assert!(
            !pane.is_cached(0),
            "streaming assistant must not be cached (spinner animates)"
        );
        let _ = pane.build_lines(80, "#");
        assert!(!pane.is_cached(0));
    }

    #[test]
    fn animated_tool_call_never_caches_until_finalized() {
        let mut pane = OutputPane::new();
        pane.add_tool_call("call".into(), "bash".into(), "sleep 1".into());
        let _ = pane.build_lines(80, ".");
        assert!(!pane.is_cached(0));
        pane.finalize_tool_result(
            "call",
            "done".into(),
            false,
            Some(Duration::from_millis(1_000)),
        );
        let _ = pane.build_lines(80, ".");
        assert!(pane.is_cached(0));
    }

    #[test]
    fn clear_drops_all_caches() {
        let mut pane = pane_with_settled_history();
        let _ = pane.build_lines(80, ".");
        pane.clear();
        assert_eq!(pane.cache_slot_count(), 0);
        assert_eq!(pane.blocks().len(), 0);
    }
}
