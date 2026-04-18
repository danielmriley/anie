use std::time::Duration;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

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

/// Scrollable output pane.
pub struct OutputPane {
    blocks: Vec<RenderedBlock>,
    scroll_offset: u16,
    auto_scroll: bool,
    last_total_lines: u16,
    last_viewport_height: u16,
}

impl OutputPane {
    /// Create an empty output pane.
    #[must_use]
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            scroll_offset: 0,
            auto_scroll: true,
            last_total_lines: 0,
            last_viewport_height: 1,
        }
    }

    /// Add a transcript block.
    pub fn add_block(&mut self, block: RenderedBlock) {
        self.blocks.push(block);
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
        });
    }

    /// Append text to the last assistant block.
    pub fn append_to_last_assistant(&mut self, text: &str) {
        if let Some(RenderedBlock::AssistantMessage { text: content, .. }) = self.blocks.last_mut()
        {
            content.push_str(text);
        }
    }

    /// Append thinking text to the last assistant block.
    pub fn append_thinking_to_last_assistant(&mut self, text: &str) {
        if let Some(RenderedBlock::AssistantMessage { thinking, .. }) = self.blocks.last_mut() {
            thinking.push_str(text);
        }
    }

    /// Finalize the last assistant block.
    pub fn finalize_last_assistant(&mut self, text: String, thinking: String, timestamp: u64) {
        if let Some(RenderedBlock::AssistantMessage {
            text: current_text,
            thinking: current_thinking,
            is_streaming,
            timestamp: current_timestamp,
        }) = self.blocks.last_mut()
        {
            *current_text = text;
            *current_thinking = thinking;
            *is_streaming = false;
            *current_timestamp = timestamp;
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
        if let Some(RenderedBlock::ToolCall { result, .. }) = self.find_tool_call_mut(call_id) {
            *result = Some(ToolCallResult {
                content,
                is_error,
                elapsed,
            });
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
        if let Some(RenderedBlock::ToolCall {
            result,
            is_executing,
            ..
        }) = self.find_tool_call_mut(call_id)
        {
            *result = Some(ToolCallResult {
                content,
                is_error,
                elapsed,
            });
            *is_executing = false;
        }
    }

    /// Add a system message.
    pub fn add_system_message(&mut self, text: String) {
        self.add_block(RenderedBlock::SystemMessage { text });
    }

    /// Clear transcript contents.
    pub fn clear(&mut self) {
        self.blocks.clear();
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
        let lines = self.to_lines(area.width.max(1), spinner_frame);
        self.last_total_lines = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        self.last_viewport_height = area.height.max(1);
        let scroll = self.current_scroll();
        self.set_scroll(scroll);
        Paragraph::new(lines)
            .scroll((self.scroll_offset, 0))
            .render(area, buf);
    }

    fn to_lines(&self, width: u16, spinner_frame: &str) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for block in &self.blocks {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.extend(block_lines(block, width, spinner_frame));
        }
        if lines.is_empty() {
            lines.push(Line::default());
        }
        lines
    }

    fn find_tool_call_mut(&mut self, call_id: &str) -> Option<&mut RenderedBlock> {
        self.blocks.iter_mut().find(|block| {
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

fn block_lines(block: &RenderedBlock, width: u16, spinner_frame: &str) -> Vec<Line<'static>> {
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
            ..
        } => assistant_block_lines(text, thinking, *is_streaming, width, spinner_frame),
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
    width: u16,
    spinner_frame: &str,
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
    append_assistant_section(&mut result, assistant_answer_lines(text, width));
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

    if result.is_empty() {
        vec![Line::default()]
    } else {
        result
    }
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

fn assistant_answer_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    if text.is_empty() {
        Vec::new()
    } else {
        wrap_text(text, width, Style::default())
    }
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
