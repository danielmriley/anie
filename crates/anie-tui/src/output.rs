use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use anie_config::ToolOutputMode;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::markdown::{LinkRange, MarkdownTheme, find_link_ranges};
use crate::render_debug::{PerfSpan, PerfSpanKind, perf_trace_enabled};
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
    /// `Arc` so cache reads hand out a cheap reference-count
    /// bump instead of deep-cloning every `Line` + `Span`.
    /// Writes also avoid an extra clone — the computed vec
    /// moves into the Arc and the render path borrows via
    /// `.iter()`.
    lines: Arc<Vec<Line<'static>>>,
    /// Link ranges per line, same length as `lines`. Empty
    /// `Vec<LinkRange>` entries correspond to lines without
    /// clickable URLs. Cached alongside the lines so cache-
    /// hit paths don't re-scan. Also `Arc`-shared for the
    /// same reason as `lines`.
    links: Arc<Vec<Vec<LinkRange>>>,
}

#[derive(Debug, Clone, Default)]
struct StreamingAssistantRender {
    committed_text: String,
    tail_text: String,
    cached_committed_width: Option<u16>,
    cached_committed_markdown_enabled: bool,
    /// Theme captured at the time the cache was filled. Theme
    /// changes (e.g., a future light/dark switch) invalidate
    /// the committed prefix even though width / markdown_enabled
    /// agree. Without this the cache could serve stale-themed
    /// lines until the next mutation.
    cached_committed_theme: Option<MarkdownTheme>,
    cached_committed_lines: Vec<Line<'static>>,
}

impl StreamingAssistantRender {
    fn append_delta(&mut self, delta: &str) {
        let mut remaining = delta;
        let mut committed_changed = false;
        while let Some(newline_index) = remaining.find('\n') {
            let (line_chunk, rest) = remaining.split_at(newline_index + 1);
            self.tail_text.push_str(line_chunk);
            self.committed_text.push_str(&self.tail_text);
            self.tail_text.clear();
            remaining = rest;
            committed_changed = true;
        }
        self.tail_text.push_str(remaining);
        if committed_changed {
            self.invalidate_cache();
        }
    }

    fn invalidate_cache(&mut self) {
        self.cached_committed_width = None;
        self.cached_committed_theme = None;
        self.cached_committed_lines.clear();
    }

    fn render_lines(&mut self, width: u16, ctx: &RenderContext) -> Vec<Line<'static>> {
        let mut out = self.render_committed_lines(width, ctx);
        if !self.tail_text.is_empty() {
            out.extend(wrap_text(&self.tail_text, width, Style::default()));
        }
        out
    }

    fn render_committed_lines(&mut self, width: u16, ctx: &RenderContext) -> Vec<Line<'static>> {
        if self.committed_text.is_empty() {
            return Vec::new();
        }
        if self.cached_committed_width == Some(width)
            && self.cached_committed_markdown_enabled == ctx.markdown_enabled
            && self.cached_committed_theme == Some(ctx.theme)
        {
            return self.cached_committed_lines.clone();
        }
        let rendered = if ctx.markdown_enabled {
            crate::markdown::render_markdown(&self.committed_text, width, &ctx.theme)
        } else {
            wrap_text(&self.committed_text, width, Style::default())
        };
        self.cached_committed_width = Some(width);
        self.cached_committed_markdown_enabled = ctx.markdown_enabled;
        self.cached_committed_theme = Some(ctx.theme);
        self.cached_committed_lines = rendered.clone();
        rendered
    }
}

struct AssistantRenderInput<'a> {
    text: &'a str,
    thinking: &'a str,
    is_streaming: bool,
    error_message: Option<&'a str>,
    streaming_render: Option<&'a mut StreamingAssistantRender>,
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
    /// How successful `bash` / `read` tool results render. Full
    /// body in `Verbose`, title-only in `Compact`. Errors and
    /// other tools are unaffected. See
    /// `docs/code_review_performance_2026-04-21/09_tool_output_display_modes.md`.
    tool_output_mode: ToolOutputMode,
}

impl Default for RenderContext {
    fn default() -> Self {
        Self {
            markdown_enabled: true,
            capabilities: TerminalCapabilities::default(),
            theme: MarkdownTheme::default_dark(),
            tool_output_mode: ToolOutputMode::Verbose,
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
    /// Parallel to `blocks`. Active only for streaming assistant
    /// blocks; finalized blocks and non-assistant blocks store
    /// `None`. Keeps the public `RenderedBlock` shape unchanged
    /// while giving the live assistant block an incremental
    /// render path.
    streaming_assistant_renders: Vec<Option<StreamingAssistantRender>>,
    /// Flattened render output from the last `build_lines`
    /// call. Reused across frames when no block content has
    /// changed and the terminal width hasn't moved — cuts the
    /// cache-hit render path from O(total_lines) per-Line
    /// clones to O(visible_lines) per frame. See Plan 04 PR-C.
    /// Invalidation tracked via `flat_cache_valid`.
    flat_lines: Vec<Line<'static>>,
    /// Width that `flat_lines` was built at. `None` before the
    /// first render. When the current render width differs,
    /// we rebuild regardless of `flat_cache_valid` because
    /// block-level caches also invalidate on width change.
    flat_cache_width: Option<u16>,
    /// Whether `flat_lines` and `last_link_map` reflect the
    /// current block state. Any mutation that could change
    /// the rendered output clears this flag; `build_lines`
    /// rebuilds when unset.
    flat_cache_valid: bool,
    /// Flat link map covering the most recent `build_lines`
    /// output. Indexed by global line number; empty `Vec`s for
    /// lines without clickable URLs. Rebuilt when the flat
    /// cache rebuilds so a mouse hit test at (screen row, col)
    /// can translate via `scroll_offset + pane_y` → global
    /// line → optional URL.
    last_link_map: Vec<Vec<LinkRange>>,
    /// Screen `y` (top row) of the output pane from the last
    /// render. Needed for mouse hit tests, which arrive in
    /// terminal-global coordinates.
    last_render_top: u16,
    scroll_offset: u16,
    auto_scroll: bool,
    last_total_lines: u16,
    last_viewport_height: u16,
    /// Visual rendering settings. Changing any field here
    /// invalidates the cache because block → line computations
    /// depend on it.
    render_context: RenderContext,
    /// Cached count of blocks with active animation (streaming
    /// assistants + executing tools). Maintained by the four
    /// state-flip methods (add_streaming_assistant,
    /// finalize_last_assistant, add_tool_call,
    /// finalize_tool_result) plus `clear`. Reading is O(1)
    /// instead of an O(blocks) walk per frame; correctness is
    /// guarded by `cfg(debug_assertions)` parity assert in
    /// `has_animated_blocks`.
    animated_block_count: usize,
    #[cfg(test)]
    flat_build_count: u64,
}

impl OutputPane {
    /// Create an empty output pane.
    #[must_use]
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            caches: Vec::new(),
            streaming_assistant_renders: Vec::new(),
            flat_lines: Vec::new(),
            flat_cache_width: None,
            flat_cache_valid: false,
            last_link_map: Vec::new(),
            last_render_top: 0,
            scroll_offset: 0,
            auto_scroll: true,
            last_total_lines: 0,
            last_viewport_height: 1,
            render_context: RenderContext::default(),
            animated_block_count: 0,
            #[cfg(test)]
            flat_build_count: 0,
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

    /// Set the `bash` / `read` body display mode and invalidate
    /// caches so the next render picks up the new choice.
    /// Mirrors `set_markdown_enabled` — the pane is treated as a
    /// reactive UI component, not a passive sink.
    pub fn set_tool_output_mode(&mut self, mode: ToolOutputMode) {
        if self.render_context.tool_output_mode == mode {
            return;
        }
        self.render_context.tool_output_mode = mode;
        self.invalidate_all_caches();
    }

    /// Current tool-output display mode.
    #[must_use]
    pub fn tool_output_mode(&self) -> ToolOutputMode {
        self.render_context.tool_output_mode
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
        for state in &mut self.streaming_assistant_renders {
            if let Some(state) = state.as_mut() {
                state.invalidate_cache();
            }
        }
        self.flat_cache_valid = false;
    }

    /// Add a transcript block.
    pub fn add_block(&mut self, block: RenderedBlock) {
        if block_has_animated_content(&block) {
            self.animated_block_count = self.animated_block_count.saturating_add(1);
        }
        self.blocks.push(block);
        self.caches.push(None);
        self.streaming_assistant_renders.push(None);
        self.flat_cache_valid = false;
    }

    fn invalidate_last(&mut self) {
        if let Some(slot) = self.caches.last_mut() {
            *slot = None;
        }
        self.flat_cache_valid = false;
    }

    fn invalidate_at(&mut self, index: usize) {
        if let Some(slot) = self.caches.get_mut(index) {
            *slot = None;
        }
        self.flat_cache_valid = false;
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
        if let Some(slot) = self.streaming_assistant_renders.last_mut() {
            *slot = Some(StreamingAssistantRender::default());
        }
    }

    /// Append text to the last assistant block.
    pub fn append_to_last_assistant(&mut self, text: &str) {
        if let Some(RenderedBlock::AssistantMessage { text: content, .. }) = self.blocks.last_mut()
        {
            content.push_str(text);
            if let Some(state) = self
                .streaming_assistant_renders
                .last_mut()
                .and_then(Option::as_mut)
            {
                state.append_delta(text);
            }
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
            let was_streaming = *is_streaming;
            *is_streaming = false;
            *current_timestamp = timestamp;
            *current_error = error_message;
            if let Some(slot) = self.streaming_assistant_renders.last_mut() {
                *slot = None;
            }
            if was_streaming {
                self.animated_block_count = self.animated_block_count.saturating_sub(1);
            }
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
            let was_executing = *is_executing;
            *is_executing = false;
            if was_executing {
                self.animated_block_count = self.animated_block_count.saturating_sub(1);
            }
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
        self.streaming_assistant_renders.clear();
        self.flat_lines.clear();
        self.flat_cache_valid = false;
        self.flat_cache_width = None;
        self.last_link_map.clear();
        self.scroll_offset = 0;
        self.auto_scroll = true;
        self.last_total_lines = 0;
        self.last_viewport_height = 1;
        self.animated_block_count = 0;
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
        reuse_flat_snapshot: bool,
    ) {
        // Plan 10 PR-A's custom in-pane scrollbar was removed
        // (2026-04-23). Terminals render their own scroll
        // affordance outside the alternate-screen TUI region
        // and the in-pane version was adding a column of
        // overhead and complicating cache invalidation.
        // Scrolling still works via wheel, PageUp/PageDown,
        // and Home/End.
        if reuse_flat_snapshot {
            if !self.can_reuse_flat_snapshot(area.width.max(1)) {
                self.rebuild_flat_cache(area.width.max(1), spinner_frame);
            }
        } else {
            self.rebuild_flat_cache(area.width.max(1), spinner_frame);
        }

        self.last_total_lines = u16::try_from(self.flat_lines.len()).unwrap_or(u16::MAX);
        self.last_viewport_height = area.height.max(1);
        // Record pane position so mouse hit tests can translate
        // terminal-global coordinates back to pane-local.
        self.last_render_top = area.y;
        let scroll = self.current_scroll();
        self.set_scroll(scroll);

        // Slice to the visible viewport before handing off to
        // Paragraph. ratatui's Paragraph::scroll() walks every
        // line on every frame regardless of visibility — at
        // 600 blocks that's 17 ms/frame, scaling linearly with
        // transcript length. Feeding just the visible slice
        // caps per-frame cost at O(viewport_height) instead.
        //
        // Phase 2 PR-C: slice borrows from self.flat_lines
        // (persistent across frames when the flat cache is
        // valid), then clones only the visible range into an
        // owned Vec for Paragraph.
        let start = self.scroll_offset as usize;
        let viewport_height = area.height as usize;
        let end = start
            .saturating_add(viewport_height)
            .min(self.flat_lines.len());
        let visible: &[Line<'static>] = if start < end {
            &self.flat_lines[start..end]
        } else {
            &[]
        };
        let mut paragraph_span = PerfSpan::enter(PerfSpanKind::ParagraphRender);
        if let Some(s) = paragraph_span.as_mut() {
            s.record("lines", u64::try_from(visible.len()).unwrap_or(u64::MAX));
            s.record("area_w", u64::from(area.width));
            s.record("area_h", u64::from(area.height));
        }
        // Render the borrowed slice directly via Buffer::set_line.
        // Skips the ratatui `Paragraph` wrap/scroll/alignment
        // pipeline (we don't use any of those features — lines
        // are pre-wrapped during build_flat_lines) and avoids
        // the per-frame deep clone that `Paragraph::new(Vec<Line>)`
        // forces. Cache-hit floor cost falls accordingly.
        for (row_offset, line) in visible.iter().enumerate() {
            let Ok(offset_u16) = u16::try_from(row_offset) else {
                break;
            };
            let y = area.y.saturating_add(offset_u16);
            if y >= area.y.saturating_add(area.height) {
                break;
            }
            buf.set_line(area.x, y, line, area.width);
        }
        drop(paragraph_span);
    }

    /// Whether any current block has animated content
    /// (streaming assistant / executing tool). Animated blocks
    /// update their spinner every frame, so the flat cache
    /// cannot be reused when any are present.
    ///
    /// Reads `animated_block_count`, maintained by the four
    /// state-flip methods. A `debug_assertions`-only parity
    /// check guards against drift; if the count ever disagrees
    /// with a fresh walk, debug builds panic with a clear
    /// message. Release builds trust the count.
    fn has_animated_blocks(&self) -> bool {
        debug_assert_eq!(
            self.animated_block_count,
            self.blocks.iter().filter(|b| block_has_animated_content(b)).count(),
            "animated_block_count drifted from the actual block state",
        );
        self.animated_block_count > 0
    }

    /// Test-only wrapper: rebuilds the flat cache and returns
    /// an owned snapshot of the current line output. Production
    /// code paths use `rebuild_flat_cache` + `self.flat_lines`
    /// directly to avoid the outer Vec clone; tests that want
    /// to assert on the output shape use this helper.
    #[cfg(test)]
    fn build_lines(&mut self, width: u16, spinner_frame: &str) -> Vec<Line<'static>> {
        self.rebuild_flat_cache(width, spinner_frame);
        self.flat_lines.clone()
    }

    /// Fast-path the flat cache when nothing has changed.
    /// Rebuilds `self.flat_lines` and `self.last_link_map`
    /// in place only when an invalidation or width change
    /// demands it. Called from `render`; the mouse hit test
    /// also calls it indirectly via `last_link_map`.
    fn rebuild_flat_cache(&mut self, width: u16, spinner_frame: &str) {
        // Fast-path: cache is valid, width matches, and no
        // animated spinner block means the flat output is
        // still accurate. O(1) frame cost for idle
        // long-transcript scrolling.
        if self.flat_cache_valid
            && self.flat_cache_width == Some(width)
            && !self.has_animated_blocks()
        {
            return;
        }
        self.build_flat_lines(width, spinner_frame);
    }

    fn can_reuse_flat_snapshot(&self, width: u16) -> bool {
        self.flat_cache_width == Some(width) && !self.flat_lines.is_empty()
    }

    fn build_flat_lines(&mut self, width: u16, spinner_frame: &str) {
        #[cfg(test)]
        {
            self.flat_build_count = self.flat_build_count.saturating_add(1);
        }
        debug_assert_eq!(
            self.blocks.len(),
            self.caches.len(),
            "block and cache vectors must stay parallel",
        );
        debug_assert_eq!(
            self.blocks.len(),
            self.streaming_assistant_renders.len(),
            "streaming render state must stay parallel to blocks",
        );
        let mut perf_span = PerfSpan::enter(PerfSpanKind::BuildLines);
        let perf_trace = perf_trace_enabled();
        let mut cache_hits: usize = 0;
        let mut cache_misses: usize = 0;
        let mut slowest_miss_us: u64 = 0;
        let mut slowest_miss_block: &'static str = "";

        // Reuse the existing flat_lines allocation; .clear()
        // drops the elements but keeps the backing capacity,
        // so subsequent pushes don't reallocate on a stable
        // transcript size.
        self.flat_lines.clear();
        let out = &mut self.flat_lines;
        // Rebuild the link map from scratch so the indexing
        // stays in lockstep with `out`. Same-length parallel
        // structure; empty entries for lines without URLs.
        self.last_link_map.clear();
        let link_map = &mut self.last_link_map;
        let theme = self.render_context.theme;
        for index in 0..self.blocks.len() {
            let block = &self.blocks[index];
            if !out.is_empty() {
                out.push(Line::default());
                link_map.push(Vec::new());
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
                // Arc-backed cache: `iter().cloned()` still
                // clones each `Line`, but that's the only way
                // to push into `out`. The cache itself is not
                // cloned — we're just borrowing through the
                // `Arc` deref. The previous shape paid this
                // cost AND cloned the outer Vec.
                out.extend(cached.lines.iter().cloned());
                link_map.extend(cached.links.iter().cloned());
                cache_hits += 1;
                continue;
            }

            let miss_start = perf_trace.then(std::time::Instant::now);
            let computed = {
                let mut s = PerfSpan::enter(PerfSpanKind::BlockLines);
                let lines = block_lines(
                    block,
                    width,
                    spinner_frame,
                    &self.render_context,
                    self.streaming_assistant_renders
                        .get_mut(index)
                        .and_then(Option::as_mut),
                );
                if let Some(s) = s.as_mut() {
                    // Use `block_kind` not `kind` — `kind` is a
                    // reserved field name that holds the span type
                    // label and jq aggregations group on it.
                    s.record("block_kind", block_kind_tag(block));
                    s.record("width", u64::from(width));
                    s.record("lines", u64::try_from(lines.len()).unwrap_or(u64::MAX));
                }
                lines
            };
            if let Some(start) = miss_start {
                let micros = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
                if micros > slowest_miss_us {
                    slowest_miss_us = micros;
                    slowest_miss_block = block_kind_tag(block);
                }
            }
            // Scan for clickable URL ranges once per cache-fill
            // so cached hits are free.
            let computed_links = {
                let mut s = PerfSpan::enter(PerfSpanKind::FindLinkRanges);
                let links = find_link_ranges(&computed, &theme);
                if let Some(s) = s.as_mut() {
                    s.record("lines", u64::try_from(computed.len()).unwrap_or(u64::MAX));
                    s.record("ranges", u64::try_from(links.len()).unwrap_or(u64::MAX));
                }
                links
            };
            // Move the computed vecs into `Arc` once; the cache
            // entry and the output stream now share the same
            // backing allocation via refcount bumps. Plan 04 PR-B.
            let lines_arc = Arc::new(computed);
            let links_arc = Arc::new(computed_links);
            if hits_cache && let Some(slot) = self.caches.get_mut(index) {
                *slot = Some(LineCache {
                    width,
                    lines: Arc::clone(&lines_arc),
                    links: Arc::clone(&links_arc),
                });
                cache_misses += 1;
            }
            out.extend(lines_arc.iter().cloned());
            link_map.extend(links_arc.iter().cloned());
        }
        if out.is_empty() {
            out.push(Line::default());
            link_map.push(Vec::new());
        }
        debug_assert_eq!(
            out.len(),
            link_map.len(),
            "link_map must parallel build_lines output"
        );
        // out and link_map are &mut references into
        // self.flat_lines / self.last_link_map; reborrows end
        // on function exit.
        let _ = out;
        let _ = link_map;
        self.flat_cache_width = Some(width);
        // Flat cache is valid at this width only if no
        // animated blocks are present — animated blocks
        // require a spinner update on every frame. The fast-
        // path check `has_animated_blocks()` in
        // `rebuild_flat_cache` handles this anyway, but
        // being explicit here avoids surprises if a future
        // caller invokes build_flat_lines directly.
        self.flat_cache_valid = !self.has_animated_blocks();

        if let Some(span) = perf_span.as_mut() {
            span.record(
                "blocks",
                u64::try_from(self.blocks.len()).unwrap_or(u64::MAX),
            );
            span.record("cache_hits", u64::try_from(cache_hits).unwrap_or(u64::MAX));
            span.record(
                "cache_misses",
                u64::try_from(cache_misses).unwrap_or(u64::MAX),
            );
            span.record("slowest_miss_us", slowest_miss_us);
            span.record("slowest_miss_block", slowest_miss_block);
            span.record("width", u64::from(width));
        }
        drop(perf_span);
    }

    /// Translate a terminal-global mouse click into a
    /// clickable URL, if the click hit one. Returns `None` for
    /// misses / clicks outside the pane / clicks on lines with
    /// no registered link ranges.
    ///
    /// Caller: `App::handle_mouse_event` on `MouseEventKind::
    /// Down(Left)`. The mouse event's `row`/`column` are
    /// terminal-global; the pane's top row is recorded by the
    /// last `render` call.
    #[must_use]
    pub fn url_at_terminal_position(&self, row: u16, col: u16) -> Option<&str> {
        let pane_top = self.last_render_top;
        let pane_bottom = pane_top.saturating_add(self.last_viewport_height);
        if row < pane_top || row >= pane_bottom {
            return None;
        }
        let line_index = self
            .scroll_offset
            .checked_add(row.saturating_sub(pane_top))? as usize;
        let line_links = self.last_link_map.get(line_index)?;
        line_links
            .iter()
            .find(|range| col >= range.col_start && col < range.col_end)
            .map(|range| range.url.as_str())
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

    #[cfg(test)]
    pub(crate) fn flat_build_count(&self) -> u64 {
        self.flat_build_count
    }
}

impl Default for OutputPane {
    fn default() -> Self {
        Self::new()
    }
}

/// Short label identifying the block kind for perf traces.
fn block_kind_tag(block: &RenderedBlock) -> &'static str {
    match block {
        RenderedBlock::UserMessage { .. } => "user",
        RenderedBlock::AssistantMessage { .. } => "assistant",
        RenderedBlock::ToolCall { .. } => "tool",
        RenderedBlock::SystemMessage { .. } => "system",
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
    streaming_render: Option<&mut StreamingAssistantRender>,
) -> Vec<Line<'static>> {
    match block {
        RenderedBlock::UserMessage { text, .. } => wrap_spans(
            // Shorter, quieter inbound-message marker. Codex
            // uses `› ` as a bold-dim prefix in
            // `codex-rs/tui/src/history_cell.rs:367-388`;
            // borrowing it shrinks the cyan "> You: " banner
            // that cluttered long transcripts without
            // sacrificing the "this is a user turn" cue.
            vec![
                Span::styled(
                    "› ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD | Modifier::DIM),
                ),
                Span::raw(text.clone()),
            ],
            width,
        ),
        RenderedBlock::AssistantMessage {
            text,
            thinking,
            is_streaming,
            error_message,
            ..
        } => assistant_block_lines(
            AssistantRenderInput {
                text,
                thinking,
                is_streaming: *is_streaming,
                error_message: error_message.as_deref(),
                streaming_render,
            },
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
        } => {
            // Plan 09 PR-B: in compact mode, successful bash /
            // read tool results render as title only. Errors,
            // edit/write results, and other tool kinds keep
            // their full body so debugging and diffs stay
            // visible. `is_executing` blocks always show the
            // "executing..." spinner regardless of mode.
            let is_error = result.as_ref().is_some_and(|value| value.is_error);
            let body = if let Some(result) = result {
                if compact_hides_body(tool_name, result, ctx.tool_output_mode) {
                    String::new()
                } else if let Some(elapsed) = result.elapsed {
                    format!("{}\n\nTook {:.1}s", result.content, elapsed.as_secs_f64(),)
                } else {
                    result.content.clone()
                }
            } else if *is_executing {
                "executing...".to_string()
            } else {
                String::new()
            };
            // Dispatch: errors keep the framed box so the red
            // border is unmissable; edit/write keep the box so
            // +/- diff lines stay legible; everything else
            // (bash, read, grep, find, ls, in-flight) renders
            // flat with the `• Verb` header.
            if is_error || (!is_executing && uses_boxed_success_layout(tool_name)) {
                boxed_lines(
                    format_tool_title(tool_name, args_display),
                    body,
                    width,
                    is_error,
                    *is_executing,
                )
            } else {
                let header = format_tool_header_spans(
                    tool_name,
                    args_display,
                    is_error,
                    *is_executing,
                    spinner_frame,
                );
                prefix_lines(
                    header,
                    body,
                    width,
                    PREFIX_TOOL_BODY_LIMIT,
                    PrefixBodyStyle::tool_success(),
                )
            }
        }
        RenderedBlock::SystemMessage { text } => {
            wrap_text(text, width, Style::default().fg(Color::DarkGray))
        }
    }
}

/// Plan 09 PR-B: a successful `bash` or `read` tool result
/// should hide its body in compact mode. Errors always keep
/// their body (suppressing an error message would hide the
/// most actionable debugging info in the transcript). `edit`
/// and `write` keep their body so diffs stay visible; other
/// tools keep their body for this first pass per plan scope.
fn compact_hides_body(tool_name: &str, result: &ToolCallResult, mode: ToolOutputMode) -> bool {
    matches!(mode, ToolOutputMode::Compact)
        && !result.is_error
        && (tool_name == "bash" || tool_name == "read")
}

fn assistant_block_lines(
    assistant: AssistantRenderInput<'_>,
    width: u16,
    spinner_frame: &str,
    ctx: &RenderContext,
) -> Vec<Line<'static>> {
    let mut result = Vec::new();
    let inline_thinking_status =
        assistant.is_streaming && assistant.text.is_empty() && !assistant.thinking.is_empty();

    append_assistant_section(
        &mut result,
        assistant_thinking_lines(
            assistant.thinking,
            width,
            inline_thinking_status.then_some(spinner_frame),
        ),
    );
    append_assistant_section(
        &mut result,
        if assistant.is_streaming {
            assistant.streaming_render.map_or_else(
                || assistant_answer_lines(assistant.text, width, true, ctx),
                |state| state.render_lines(width, ctx),
            )
        } else {
            assistant_answer_lines(assistant.text, width, false, ctx)
        },
    );
    // The "responding…" / "thinking…" status indicator used
    // to render a trailing line in the assistant block. That
    // cue now lives on the dedicated spinner row directly
    // above the input box (see `render_spinner_row` in
    // `app.rs`), so we no longer duplicate it here — the
    // transcript stays focused on the content itself.
    let _ = spinner_frame;
    let _ = inline_thinking_status;
    // Provider errors land at the bottom of the block so the user
    // sees the reason a turn produced no visible answer. Without
    // this, a thinking-only response would leave the user staring
    // at a thinking block with nothing after it.
    if let Some(message) = assistant.error_message {
        append_assistant_section(&mut result, assistant_error_lines(message, width));
    }

    if result.is_empty() {
        vec![Line::default()]
    } else {
        result
    }
}

fn assistant_error_lines(message: &str, width: u16) -> Vec<Line<'static>> {
    // `• Error` header with a red bullet. Matches the tool-
    // error bullet convention so provider-level failures and
    // tool-level failures scan the same way.
    let header = vec![
        Span::styled("• ".to_string(), Style::default().fg(Color::Red)),
        Span::styled(
            "Error".to_string(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    ];
    prefix_lines(
        header,
        message.to_string(),
        width,
        usize::MAX,
        PrefixBodyStyle {
            indent: Style::default().fg(Color::Red),
            body: Style::default().fg(Color::Red),
        },
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

    // `• Thinking` header using the same bullet vocabulary as
    // tool calls. While thinking is actively streaming (no
    // visible answer yet), swap the bullet for the spinner
    // frame so the active section is the eye-catch.
    let (bullet, bullet_style) = match streaming_spinner {
        Some(frame) => (format!("{frame} "), Style::default().fg(Color::Yellow)),
        None => (
            "• ".to_string(),
            Style::default()
                .fg(Color::Indexed(246))
                .add_modifier(Modifier::DIM),
        ),
    };
    let header = vec![
        Span::styled(bullet, bullet_style),
        Span::styled(
            "Thinking".to_string(),
            thinking_label_style().add_modifier(Modifier::BOLD),
        ),
    ];
    prefix_lines(
        header,
        thinking.to_string(),
        width,
        usize::MAX,
        PrefixBodyStyle {
            indent: thinking_gutter_style(),
            body: thinking_body_style(),
        },
    )
}

/// Render assistant answer text as markdown for finalized turns
/// and as the fallback path for streaming renders that do not
/// have an incremental streaming state available.
///
/// Earlier revisions split the streaming text at the last
/// `\n\n` boundary and rendered only the committed prefix as
/// markdown — the tail stayed plain until the stream ended.
/// That produced a visible "snap" at stream-end where the tail
/// section suddenly reformatted from raw `**bold**` / `# head`
/// syntax into rendered markdown. Users flagged the transition
/// as jarring.
///
/// The current shape renders the full text as markdown every
/// frame. pulldown-cmark is tolerant of incomplete input:
/// unclosed fences render as code blocks, unbalanced `**` /
/// `_` render as literal chars, partial headings render with
/// the text seen so far. Reflow is localized to the block
/// being typed — no end-of-stream global reformat.
///
/// Streaming blocks bypass the block cache (see
/// `animated_streaming_block_never_caches`), so per-frame
/// markdown work is already the cost profile. No new cache
/// pressure.
fn assistant_answer_lines(
    text: &str,
    width: u16,
    _is_streaming: bool,
    ctx: &RenderContext,
) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    if !ctx.markdown_enabled {
        return wrap_text(text, width, Style::default());
    }
    crate::markdown::render_markdown(text, width, &ctx.theme)
}

fn thinking_label_style() -> Style {
    // PR 1: thinking label explicitly DIM so the "thinking"
    // header reads as lower-priority than the answer text.
    // Mirrors Codex's `ReasoningSummaryCell` styling in
    // `codex-rs/tui/src/history_cell.rs:422-441`.
    Style::default()
        .fg(Color::Indexed(246))
        .add_modifier(Modifier::DIM)
}

fn thinking_gutter_style() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM)
}

fn thinking_body_style() -> Style {
    // Italic + dim — explicit visual secondary channel for
    // reasoning text so it reads as "notes" rather than
    // "answer."
    Style::default()
        .fg(Color::Indexed(248))
        .add_modifier(Modifier::ITALIC | Modifier::DIM)
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
        // PR-D (Plan 04): walk the source once by char index,
        // slicing at byte boundaries when the char counter hits
        // `width`. The previous shape collected every char into
        // a `Vec<char>` and rebuilt a `String` per chunk — O(n)
        // allocations per wrapped input. New shape does O(lines)
        // `String` allocations, matching the output size.
        //
        // Preserves char-count (USV) semantics per plan — this
        // is not a display-width correctness change. Wide CJK
        // and ZWJ sequences still wrap at `chars().count()`
        // boundaries, identical to the previous behavior.
        let mut char_in_line = 0usize;
        let mut byte_start = 0usize;
        for (byte_idx, _) in raw_line.char_indices() {
            if char_in_line == width {
                lines.push(raw_line[byte_start..byte_idx].to_string());
                byte_start = byte_idx;
                char_in_line = 0;
            }
            char_in_line += 1;
        }
        // Tail after the last boundary. Always non-empty for
        // a non-empty raw_line (the loop pushes at boundaries
        // and leaves the remainder for this step).
        lines.push(raw_line[byte_start..].to_string());
    }
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

fn wrap_spans(spans: Vec<Span<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_char_count = 0usize;

    // PR-E (Plan 04): the previous shape flattened every input
    // span into a `Vec<(char, Style)>` and then emitted one
    // `Span` per char, rebuilding a `String` via `ch.to_string()`
    // for every single character. A 5,000-char streaming response
    // allocated 10,000+ Strings/frame that way.
    //
    // New shape walks each input span in place, tracking the
    // running char count into the current line, and emits one
    // `Span` per (style, byte-range) run. Output is identical at
    // the rendered-cell level (ratatui draws each char with its
    // style); same-style consecutive runs just fold into a
    // single Span instead of N tiny ones. Char-count (USV) wrap
    // boundaries are preserved.
    for span in spans {
        let style = span.style;
        let text = span.content.into_owned();
        if text.is_empty() {
            continue;
        }
        let mut byte_start: usize = 0;
        for (byte_idx, _ch) in text.char_indices() {
            if current_char_count == width {
                // Wrap boundary. Emit whatever portion of this
                // span has accumulated up to byte_idx, then
                // flush the line.
                if byte_start < byte_idx {
                    current_spans.push(Span::styled(text[byte_start..byte_idx].to_string(), style));
                    byte_start = byte_idx;
                }
                lines.push(Line::from(std::mem::take(&mut current_spans)));
                current_char_count = 0;
            }
            current_char_count += 1;
        }
        if byte_start < text.len() {
            current_spans.push(Span::styled(text[byte_start..].to_string(), style));
        }
    }
    if !current_spans.is_empty() {
        lines.push(Line::from(current_spans));
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

/// Max body lines we render in the prefix (non-boxed) tool path
/// before collapsing the tail into `… +N lines`. Codex / Claude
/// Code both cap around 5–6 here so a chatty `ls` or `grep`
/// doesn't dominate the transcript.
const PREFIX_TOOL_BODY_LIMIT: usize = 5;
const PREFIX_FIRST_INDENT: &str = "  └ ";
const PREFIX_CONT_INDENT: &str = "    ";

/// Map an anie tool name to a short past-tense verb for the
/// status-bullet header. Verbs are the convention Claude Code /
/// Codex use to make tool-call rows scan like a narrative rather
/// than a log of raw tool names.
fn tool_verb(tool_name: &str) -> &'static str {
    match tool_name {
        "bash" => "Ran",
        "read" => "Read",
        "grep" => "Searched",
        "find" => "Found",
        "ls" => "Listed",
        "edit" => "Edited",
        "write" => "Wrote",
        _ => "Called",
    }
}

/// Header line for a tool call in the prefix (non-boxed) path:
/// `• <Verb> <args>` with a colored bullet (green = ok, red =
/// error, yellow + spinner = in-flight). The verb is bold; the
/// args render dim so the status bullet + verb stay the eye-catch.
fn format_tool_header_spans(
    tool_name: &str,
    args_display: &str,
    is_error: bool,
    is_executing: bool,
    spinner_frame: &str,
) -> Vec<Span<'static>> {
    // Static bullets borrow from string literals; only the
    // executing path needs a fresh allocation (the spinner
    // frame rotates each tick, no `&'static str` available).
    let (bullet, bullet_style): (Cow<'static, str>, Style) = if is_executing {
        (
            Cow::Owned(format!("{spinner_frame} ")),
            Style::default().fg(Color::Yellow),
        )
    } else if is_error {
        (Cow::Borrowed("• "), Style::default().fg(Color::Red))
    } else {
        (Cow::Borrowed("• "), Style::default().fg(Color::Green))
    };
    // tool_verb returns &'static str; pass it as Cow::Borrowed
    // through Span::styled so we don't allocate for the verb.
    let verb: &'static str = tool_verb(tool_name);
    let mut spans = vec![
        Span::styled(bullet, bullet_style),
        Span::styled(
            Cow::Borrowed(verb),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    if !args_display.is_empty() {
        spans.push(Span::raw(Cow::Borrowed(" ")));
        // args_display is a per-call &str, so the owned
        // String here is unavoidable for the 'static span.
        spans.push(Span::styled(
            args_display.to_string(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans
}

/// Styling for a `prefix_lines` body region.
///
/// `indent` styles the `  └ ` / `    ` gutter chars; `body`
/// styles the wrapped text itself. Split so a caller can
/// mismatch them — e.g. thinking uses a muted gray indent but
/// a DIM italic body; errors use a red indent with a bold red
/// body.
#[derive(Debug, Clone, Copy)]
struct PrefixBodyStyle {
    indent: Style,
    body: Style,
}

impl PrefixBodyStyle {
    fn tool_success() -> Self {
        Self {
            indent: Style::default().fg(Color::DarkGray),
            body: Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        }
    }
}

/// Render a block as a bulleted header plus an indented body.
/// Body wraps to `width - indent` and caps at `max_body_lines`
/// — the overflow is replaced with a single `… +N lines`
/// footer so a chatty block doesn't push the rest of the
/// transcript offscreen. Pass `max_body_lines = usize::MAX` to
/// skip truncation (e.g. for thinking / error sections where
/// every line is load-bearing).
fn prefix_lines(
    header_spans: Vec<Span<'static>>,
    body: String,
    width: u16,
    max_body_lines: usize,
    style: PrefixBodyStyle,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(header_spans)];
    if body.is_empty() {
        return lines;
    }
    let indent_chars = PREFIX_FIRST_INDENT.chars().count() as u16;
    let body_width = width.saturating_sub(indent_chars).max(1);
    let wrapped = wrap_plain_text(&body, body_width);
    // Strip trailing blanks so "Took 0.1s\n\n" doesn't burn a
    // body slot on a blank line.
    let mut effective = wrapped;
    while effective.last().is_some_and(|l| l.trim().is_empty()) {
        effective.pop();
    }
    if effective.is_empty() {
        return lines;
    }
    let total = effective.len();
    let shown = total.min(max_body_lines);
    for (idx, text) in effective.iter().take(shown).enumerate() {
        let prefix = if idx == 0 {
            PREFIX_FIRST_INDENT
        } else {
            PREFIX_CONT_INDENT
        };
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), style.indent),
            Span::styled(text.clone(), style.body),
        ]));
    }
    if total > shown {
        let remaining = total - shown;
        lines.push(Line::from(vec![
            Span::styled(PREFIX_CONT_INDENT.to_string(), style.indent),
            Span::styled(
                format!("… +{remaining} lines"),
                style.body.add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
    lines
}

/// Tool kinds whose successful result renders as a framed box
/// rather than the flat prefix layout. Keeping `edit` and
/// `write` boxed preserves the diff framing that makes a
/// multi-line `-/+` patch scan cleanly; a flat DIM body would
/// bury the +/- colors.
fn uses_boxed_success_layout(tool_name: &str) -> bool {
    matches!(tool_name, "edit" | "write")
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
mod tool_output_mode_tests {
    use super::*;

    fn rendered_to_text(pane: &mut OutputPane, width: u16) -> String {
        let lines = pane.build_lines(width, ".");
        let mut out = String::new();
        for line in &lines {
            for span in &line.spans {
                out.push_str(&span.content);
            }
            out.push('\n');
        }
        out
    }

    fn pane_with_tool(tool_name: &str, title_arg: &str, body: &str, is_error: bool) -> OutputPane {
        let mut pane = OutputPane::new();
        pane.add_tool_call(
            "call-1".into(),
            tool_name.to_string(),
            title_arg.to_string(),
        );
        pane.finalize_tool_result("call-1", body.to_string(), is_error, None);
        pane
    }

    /// Plan 09 PR-B: compact mode drops the body for a
    /// successful bash tool block. The title line (`$ <cmd>`)
    /// is preserved.
    #[test]
    fn compact_mode_hides_successful_bash_body_but_keeps_title() {
        let mut pane = pane_with_tool("bash", "ls /tmp", "file1\nfile2\nfile3", false);
        pane.set_tool_output_mode(ToolOutputMode::Compact);
        let rendered = rendered_to_text(&mut pane, 80);
        assert!(rendered.contains("ls /tmp"), "title missing:\n{rendered}");
        assert!(
            !rendered.contains("file1"),
            "compact mode should hide successful bash body:\n{rendered}"
        );
    }

    /// Compact mode drops the body for a successful read.
    #[test]
    fn compact_mode_hides_successful_read_body_but_keeps_title() {
        let mut pane = pane_with_tool(
            "read",
            "/tmp/secret.txt",
            "line1\nline2\npassword=hunter2",
            false,
        );
        pane.set_tool_output_mode(ToolOutputMode::Compact);
        let rendered = rendered_to_text(&mut pane, 80);
        assert!(
            rendered.contains("/tmp/secret.txt"),
            "title missing:\n{rendered}"
        );
        assert!(
            !rendered.contains("password=hunter2"),
            "compact mode should hide successful read body:\n{rendered}"
        );
    }

    /// Compact mode keeps edit diffs visible — they're often
    /// the whole point of inspecting an edit tool call.
    #[test]
    fn compact_mode_keeps_edit_diff_visible() {
        let mut pane = pane_with_tool(
            "edit",
            "/tmp/main.rs",
            "--- a/main.rs\n+++ b/main.rs\n@@ -1 +1 @@\n-old line\n+new line",
            false,
        );
        pane.set_tool_output_mode(ToolOutputMode::Compact);
        let rendered = rendered_to_text(&mut pane, 80);
        assert!(
            rendered.contains("new line"),
            "edit diff must stay visible in compact mode:\n{rendered}"
        );
    }

    /// Compact mode keeps write results visible so confirmation
    /// of a destructive action doesn't get hidden.
    #[test]
    fn compact_mode_keeps_write_result_visible() {
        let mut pane = pane_with_tool(
            "write",
            "/tmp/out.txt",
            "Wrote 128 bytes to /tmp/out.txt",
            false,
        );
        pane.set_tool_output_mode(ToolOutputMode::Compact);
        let rendered = rendered_to_text(&mut pane, 80);
        assert!(
            rendered.contains("128 bytes"),
            "write result must stay visible in compact mode:\n{rendered}"
        );
    }

    /// Compact mode keeps bash errors visible — hiding them
    /// would make debugging far worse.
    #[test]
    fn compact_mode_keeps_bash_errors_visible() {
        let mut pane = pane_with_tool(
            "bash",
            "/usr/bin/false",
            "command failed with exit code 1",
            true,
        );
        pane.set_tool_output_mode(ToolOutputMode::Compact);
        let rendered = rendered_to_text(&mut pane, 80);
        assert!(
            rendered.contains("command failed"),
            "bash error body must stay visible:\n{rendered}"
        );
    }

    /// Compact mode keeps read errors visible.
    #[test]
    fn compact_mode_keeps_read_errors_visible() {
        let mut pane = pane_with_tool(
            "read",
            "/no/such/file",
            "read failed: permission denied",
            true,
        );
        pane.set_tool_output_mode(ToolOutputMode::Compact);
        let rendered = rendered_to_text(&mut pane, 80);
        assert!(
            rendered.contains("permission denied"),
            "read error body must stay visible:\n{rendered}"
        );
    }

    /// Verbose mode (the default) keeps today's behavior:
    /// successful bash bodies still render. Regression guard
    /// against breaking the opt-in boundary.
    #[test]
    fn verbose_mode_keeps_successful_bash_body_visible() {
        let mut pane = pane_with_tool("bash", "ls /tmp", "file1\nfile2", false);
        // Default is Verbose — no explicit set needed, but be
        // explicit for readability.
        pane.set_tool_output_mode(ToolOutputMode::Verbose);
        let rendered = rendered_to_text(&mut pane, 80);
        assert!(
            rendered.contains("file1"),
            "verbose mode preserves body:\n{rendered}"
        );
    }

    /// Toggling the mode invalidates cached lines — a pane
    /// that rendered in Verbose and then flipped to Compact
    /// must reflect the new setting on the next build. Guards
    /// against cache-staleness.
    #[test]
    fn toggling_tool_output_mode_invalidates_cached_lines() {
        let mut pane = pane_with_tool("bash", "ls", "visible content", false);
        let verbose = rendered_to_text(&mut pane, 80);
        assert!(verbose.contains("visible content"));
        pane.set_tool_output_mode(ToolOutputMode::Compact);
        let compact = rendered_to_text(&mut pane, 80);
        assert!(
            !compact.contains("visible content"),
            "mode change must invalidate cache:\n{compact}"
        );
    }
}

#[cfg(test)]
mod prefix_tool_layout_tests {
    use super::*;

    fn line_to_string(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn tool_block_lines(
        tool_name: &str,
        args: &str,
        body: &str,
        is_error: bool,
    ) -> Vec<Line<'static>> {
        let mut pane = OutputPane::new();
        pane.add_tool_call("call".into(), tool_name.to_string(), args.to_string());
        pane.finalize_tool_result("call", body.to_string(), is_error, None);
        pane.build_lines(80, ".")
    }

    /// Successful bash renders with the verb header and a
    /// `  └ ` first-body indent rather than the old `┌─ $ ...`
    /// frame.
    #[test]
    fn successful_bash_uses_prefix_layout_not_box() {
        let lines = tool_block_lines("bash", "ls /tmp", "file1", false);
        let text: Vec<String> = lines.iter().map(line_to_string).collect();
        let joined = text.join("\n");
        assert!(
            joined.contains("• Ran ls /tmp"),
            "header missing:\n{joined}"
        );
        assert!(
            joined.contains("  └ file1"),
            "first body indent missing:\n{joined}"
        );
        assert!(
            !joined.contains('┌'),
            "successful bash should not use boxed frame:\n{joined}"
        );
    }

    /// Successful read also uses the prefix path.
    #[test]
    fn successful_read_uses_prefix_layout() {
        let lines = tool_block_lines("read", "src/main.rs", "fn main() {}", false);
        let joined = lines
            .iter()
            .map(line_to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("• Read src/main.rs"),
            "header missing:\n{joined}"
        );
        assert!(
            joined.contains("  └ fn main() {}"),
            "body indent missing:\n{joined}"
        );
    }

    /// Errors keep the framed box so the red border stays the
    /// primary signal.
    #[test]
    fn tool_errors_keep_boxed_layout() {
        let lines = tool_block_lines("bash", "false", "exit 1", true);
        let joined = lines
            .iter()
            .map(line_to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains('┌') && joined.contains('└'),
            "errors should stay boxed:\n{joined}"
        );
    }

    /// edit/write keep the framed box so diff lines stay
    /// legible against the frame.
    #[test]
    fn edit_tool_keeps_boxed_layout() {
        let lines = tool_block_lines("edit", "src/main.rs", "- old\n+ new", false);
        let joined = lines
            .iter()
            .map(line_to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("┌─ edit src/main.rs"),
            "edit should stay boxed:\n{joined}"
        );
    }

    /// Body capped at PREFIX_TOOL_BODY_LIMIT lines with a
    /// `… +N lines` footer. Guards against chatty tools pushing
    /// the transcript offscreen.
    #[test]
    fn prefix_body_truncates_long_output_with_footer() {
        let body = (1..=12)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = tool_block_lines("bash", "gen", &body, false);
        let joined = lines
            .iter()
            .map(line_to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("line 1"),
            "first body line should render:\n{joined}"
        );
        assert!(
            !joined.contains("line 12"),
            "last body line should be truncated:\n{joined}"
        );
        assert!(
            joined.contains("… +7 lines"),
            "truncation footer missing:\n{joined}"
        );
    }

    /// Short outputs are not decorated with the truncation
    /// footer.
    #[test]
    fn prefix_body_does_not_add_footer_when_under_limit() {
        let lines = tool_block_lines("bash", "ok", "a\nb\nc", false);
        let joined = lines
            .iter()
            .map(line_to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!joined.contains("… +"), "spurious footer:\n{joined}");
    }

    /// In-flight tool block uses the prefix path with the
    /// spinner frame as the bullet and "executing..." as body.
    #[test]
    fn executing_tool_renders_spinner_header_and_placeholder_body() {
        let mut pane = OutputPane::new();
        pane.add_tool_call("call".into(), "bash".into(), "sleep 1".into());
        let lines = pane.build_lines(80, "⠋");
        let joined = lines
            .iter()
            .map(line_to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("⠋ Ran sleep 1"),
            "in-flight header missing:\n{joined}"
        );
        assert!(
            joined.contains("  └ executing..."),
            "in-flight body missing:\n{joined}"
        );
    }
}

#[cfg(test)]
mod streaming_markdown_tests {
    use super::*;

    /// Streaming and finalized rendering produce identical
    /// lines for the same text. That's the core of the
    /// no-jump-at-stream-end guarantee — if the output is
    /// byte-identical regardless of `is_streaming`, there's
    /// nothing to snap to at the transition.
    #[test]
    fn streaming_and_finalized_render_are_cell_identical() {
        let ctx = RenderContext::default();
        let text = "# Heading\n\nBody paragraph.\n\n## Subheading\n\nTrailing line";
        let streaming = assistant_answer_lines(text, 40, true, &ctx);
        let finalized = assistant_answer_lines(text, 40, false, &ctx);
        assert_eq!(streaming.len(), finalized.len());
        for (idx, (a, b)) in streaming.iter().zip(finalized.iter()).enumerate() {
            let lhs: String = a.spans.iter().map(|s| s.content.as_ref()).collect();
            let rhs: String = b.spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(lhs, rhs, "line {idx} differs between streaming/finalized");
        }
    }

    /// A single-line in-progress heading renders as a heading,
    /// not as plain text with literal `##`. Guard against
    /// regressing to the old split-boundary behavior.
    #[test]
    fn single_line_heading_renders_as_markdown_mid_stream() {
        let ctx = RenderContext::default();
        let text = "## Title still on the first line";
        let streaming = assistant_answer_lines(text, 40, true, &ctx);
        let plain = wrap_text(text, 40, Style::default());
        // The markdown-rendered heading should NOT equal the
        // plain-wrapped version (plain keeps the `##` literal;
        // markdown strips/styles it).
        let streamed_text: String = streaming
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        let plain_text: String = plain
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert_ne!(
            streamed_text, plain_text,
            "streaming render should be markdown, not plain"
        );
    }

    /// Empty text produces no lines regardless of streaming.
    #[test]
    fn empty_streaming_text_produces_no_lines() {
        let ctx = RenderContext::default();
        assert!(assistant_answer_lines("", 40, true, &ctx).is_empty());
        assert!(assistant_answer_lines("", 40, false, &ctx).is_empty());
    }

    /// When markdown is disabled, streaming falls back to
    /// plain-wrapped text (no markdown parse).
    #[test]
    fn markdown_disabled_renders_plain_during_streaming() {
        let ctx = RenderContext {
            markdown_enabled: false,
            ..RenderContext::default()
        };
        let text = "# literal hash";
        let streaming = assistant_answer_lines(text, 40, true, &ctx);
        let plain = wrap_text(text, 40, Style::default());
        assert_eq!(streaming.len(), plain.len());
        for (a, b) in streaming.iter().zip(plain.iter()) {
            let lhs: String = a.spans.iter().map(|s| s.content.as_ref()).collect();
            let rhs: String = b.spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(lhs, rhs);
        }
    }

    /// An in-progress code fence (opened, not yet closed) must
    /// render as a code block — not as literal backtick chars.
    /// Regression guard against any future re-introduction of
    /// "skip markdown for tails that look incomplete" logic.
    #[test]
    fn unclosed_code_fence_renders_as_code_block_mid_stream() {
        let ctx = RenderContext::default();
        let text = "Here is some code:\n\n```rust\nfn main() {\n    println!";
        let streaming = assistant_answer_lines(text, 60, true, &ctx);
        let rendered: String = streaming
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        // The literal `` ``` `` fence markers should NOT appear in
        // the rendered output — pulldown-cmark strips them and
        // emits a code block. If they leak through, the tail is
        // being plain-rendered and the old regression is back.
        assert!(
            !rendered.contains("```"),
            "unclosed fence leaked literal backticks:\n{rendered}"
        );
        // Code content is still present (just styled as code).
        assert!(
            rendered.contains("fn main()"),
            "code content missing:\n{rendered}"
        );
    }
}

#[cfg(test)]
mod wrap_tests {
    use super::*;

    /// Regression for the PR-D rewrite. Output must match the
    /// previous `chars().chunks()` shape byte-for-byte.
    #[test]
    fn wrap_plain_text_splits_ascii_at_width_boundaries() {
        assert_eq!(
            wrap_plain_text("abcdefghij", 3),
            vec!["abc", "def", "ghi", "j"]
        );
    }

    /// USV counting, not byte counting — a multi-byte UTF-8
    /// rune is one "cell" for wrap purposes. This is the
    /// explicitly-preserved contract from Plan 04 ("keep char
    /// count semantics unchanged; defer Unicode display-width
    /// correctness"). Each `é` is 2 bytes but 1 char.
    #[test]
    fn wrap_plain_text_counts_chars_not_bytes() {
        let lines = wrap_plain_text("éééééé", 2);
        // 6 chars at width=2 → 3 lines of 2 chars each.
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert_eq!(line.chars().count(), 2);
        }
    }

    /// Newlines split lines before width wrapping applies.
    /// Empty lines are preserved as empty strings.
    #[test]
    fn wrap_plain_text_preserves_empty_lines_from_newlines() {
        let lines = wrap_plain_text("hi\n\nthere", 10);
        assert_eq!(lines, vec!["hi", "", "there"]);
    }

    /// Empty input yields a single empty-string line — same
    /// as the previous implementation.
    #[test]
    fn wrap_plain_text_empty_input_yields_one_empty_line() {
        assert_eq!(wrap_plain_text("", 80), vec![String::new()]);
    }

    /// Width zero clamps to 1 so we don't produce an infinite
    /// number of empty lines.
    #[test]
    fn wrap_plain_text_width_zero_clamps_to_one() {
        let lines = wrap_plain_text("abc", 0);
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    /// Line exactly equal to width is a single line with no
    /// trailing empty — guards off-by-one in the boundary
    /// check.
    #[test]
    fn wrap_plain_text_line_exactly_width_is_one_line() {
        assert_eq!(wrap_plain_text("abcdef", 6), vec!["abcdef"]);
    }

    /// PR 04 / F-9: bullet, verb, and the leading space between
    /// verb and args use string literals (Cow::Borrowed). The
    /// args span still allocates because `args_display` is a
    /// per-call &str; spinner-frame allocates because it
    /// rotates each tick. This test guards the static-vs-owned
    /// classification so a future change can't silently
    /// reintroduce the per-render allocations.
    #[test]
    fn tool_header_static_parts_are_borrowed() {
        // Non-executing path: bullet "• " is borrowed.
        let spans = format_tool_header_spans("bash", "ls /tmp", false, false, ".");
        // [bullet, verb, " ", args]
        assert!(matches!(spans[0].content, std::borrow::Cow::Borrowed(_)),
            "bullet should be Cow::Borrowed");
        assert!(matches!(spans[1].content, std::borrow::Cow::Borrowed(_)),
            "verb should be Cow::Borrowed");
        assert!(matches!(spans[2].content, std::borrow::Cow::Borrowed(_)),
            "space separator should be Cow::Borrowed");
        // args is per-call &str → unavoidable owned String.
        assert!(matches!(spans[3].content, std::borrow::Cow::Owned(_)),
            "args span owns its String (input is &str, not 'static)");
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
        pane.finalize_last_assistant("an answer".into(), String::new(), 2, None);
        pane.add_user_message("second question".into(), 3);
        pane.add_streaming_assistant();
        pane.append_to_last_assistant("another answer");
        pane.finalize_last_assistant("another answer".into(), String::new(), 4, None);
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
        let long_answer = "one two three four five six seven eight nine ten ".repeat(8);
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
        pane.finalize_last_assistant("partial answer".into(), String::new(), 5, None);
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

    /// Streaming-prefix render cache must be keyed by theme so
    /// that a theme switch invalidates cached committed lines.
    /// PR 04 of docs/tui_perf_2026-04-25/.
    #[test]
    fn streaming_prefix_cache_invalidates_on_theme_change() {
        let ctx_dark = RenderContext::default();
        let mut ctx_light = ctx_dark.clone();
        // Mutate one style so theme equality fails. We're not
        // shipping a real light theme here — just a different
        // one, which is what the cache key needs to detect.
        ctx_light.theme.h1 = ratatui::style::Style::default()
            .fg(ratatui::style::Color::Yellow)
            .add_modifier(ratatui::style::Modifier::BOLD);

        let mut state = StreamingAssistantRender::default();
        state.append_delta("# heading\n\nbody\n\n");
        let with_dark = state.render_lines(40, &ctx_dark);
        let with_light = state.render_lines(40, &ctx_light);
        // Heading style in the rendered output must differ
        // because the theme changed; if the cache served the
        // dark prefix, both renders would be identical.
        assert_ne!(
            with_dark, with_light,
            "theme change must invalidate the streaming prefix cache",
        );
    }

    #[test]
    fn finalized_streaming_markdown_matches_direct_finalized_render() {
        let body = "## Heading\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n";

        let mut streamed = OutputPane::new();
        streamed.add_streaming_assistant();
        for chunk in [
            "## Heading\n\n",
            "- one\n",
            "- two\n\n```rust\n",
            "fn main() {}\n",
            "```\n",
        ] {
            streamed.append_to_last_assistant(chunk);
        }
        streamed.finalize_last_assistant(body.into(), String::new(), 1, None);

        let mut direct = OutputPane::new();
        direct.add_streaming_assistant();
        direct.finalize_last_assistant(body.into(), String::new(), 1, None);

        assert_eq!(streamed.build_lines(80, "."), direct.build_lines(80, "."));
    }

    #[test]
    fn url_at_terminal_position_hits_the_link_span() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        // Build a single assistant message that contains a
        // markdown link. Markdown enabled by default on
        // RenderContext, so `render_markdown` fires and the
        // link's fallback span gets a LinkRange entry.
        let mut pane = OutputPane::new();
        pane.add_streaming_assistant();
        let body = "Visit [the docs](https://example.com/specific) for details.";
        pane.append_to_last_assistant(body);
        pane.finalize_last_assistant(body.into(), String::new(), 1, None);

        // Render at a known position. Pane top = 5 so we can
        // verify the coordinate translation.
        let area = Rect::new(0, 5, 120, 10);
        let mut buf = Buffer::empty(area);
        pane.render(area, &mut buf, ".", false);

        // Any cell BEFORE the URL fallback starts returns None.
        assert!(pane.url_at_terminal_position(5, 0).is_none());
        // A cell INSIDE the URL text returns the URL. The
        // assistant output renders as something like:
        //   Visit the docs (https://example.com/specific) for details.
        // We find the URL by scanning the rendered buffer.
        let top_row: String = (0..area.width)
            .map(|x| buf[(x, 5)].symbol())
            .collect::<String>();
        let url_col = top_row
            .find("https://")
            .expect("URL should appear in rendered output") as u16;
        let hit = pane.url_at_terminal_position(5, url_col + 5);
        assert_eq!(hit, Some("https://example.com/specific"));
    }

    #[test]
    fn url_at_terminal_position_returns_none_outside_pane() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let mut pane = OutputPane::new();
        pane.add_user_message("hi".into(), 1);
        let area = Rect::new(0, 5, 40, 3);
        let mut buf = Buffer::empty(area);
        pane.render(area, &mut buf, ".", false);
        // Row 4 is above the pane; row 8 is below. Neither
        // should match.
        assert!(pane.url_at_terminal_position(4, 0).is_none());
        assert!(pane.url_at_terminal_position(8, 0).is_none());
    }

    #[test]
    fn render_only_feeds_visible_slice_to_paragraph() {
        // Regression: pre-fix the render path handed the full
        // line vec to Paragraph and relied on Paragraph.scroll()
        // to hide the invisible portion. That meant Paragraph
        // walked every line every frame (O(transcript)). Now
        // we slice to the visible viewport first.
        //
        // This test pokes the render path with a scrolled
        // offset and confirms the emitted buffer matches the
        // expected visible slice.
        use ratatui::backend::TestBackend;
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let mut pane = OutputPane::new();
        for i in 0..20 {
            pane.add_user_message(format!("msg {i}"), i as u64);
        }

        // Warm the caches at a known width.
        let area = Rect::new(0, 0, 80, 5);
        let _backend = TestBackend::new(80, 5);
        let mut buf = Buffer::empty(area);
        pane.render(area, &mut buf, ".", false);

        // Scroll down past the first 10 lines, render again.
        pane.scroll_line_down(10);
        let mut buf = Buffer::empty(area);
        pane.render(area, &mut buf, ".", false);

        // With scroll_offset ~= 10 and viewport height 5, the
        // top visible cell should render a later message, not
        // "msg 0".
        let top_row: String = (0..area.width)
            .map(|x| buf[(x, 0)].symbol())
            .collect::<String>();
        assert!(
            !top_row.contains("msg 0"),
            "scroll didn't take effect: {top_row}"
        );
    }

    #[test]
    fn clear_drops_all_caches() {
        let mut pane = pane_with_settled_history();
        let _ = pane.build_lines(80, ".");
        pane.clear();
        assert_eq!(pane.cache_slot_count(), 0);
        assert_eq!(pane.blocks().len(), 0);
    }

    /// Stress-measure how long a fully-cached `build_lines`
    /// takes on a realistic transcript. Run with:
    ///
    ///     cargo test -p anie-tui --release \
    ///         build_lines_cached_stress -- --ignored --nocapture
    ///
    /// Marked `#[ignore]` so it doesn't slow the regular test
    /// suite. Used to validate perf assumptions for Plan 09.
    #[test]
    #[ignore]
    fn build_lines_cached_stress() {
        measure_build_lines(30);
        measure_build_lines(100);
        measure_cache_miss_cost();
        measure_full_render(30);
        measure_full_render(100);
        measure_full_render(300);
    }

    /// End-to-end render cost: build_lines + Paragraph.render
    /// into a TestBackend buffer. If this is much slower than
    /// build_lines alone, the bottleneck is ratatui's
    /// line-walking / UnicodeWidthStr work inside Paragraph.
    fn measure_full_render(turns: usize) {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::time::Instant;

        let mut pane = build_markdown_transcript(turns);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
        // Warm caches.
        terminal
            .draw(|frame| {
                let area = frame.area();
                pane.render(area, frame.buffer_mut(), ".", false);
            })
            .expect("draw");

        let iterations = 200;
        let start = Instant::now();
        for _ in 0..iterations {
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    pane.render(area, frame.buffer_mut(), ".", false);
                })
                .expect("draw");
        }
        let elapsed = start.elapsed();
        let per_frame_us = elapsed.as_micros() as u64 / iterations as u64;
        println!(
            "full_render: {} blocks, viewport=120x40, {} iterations, \
             total={:?}, per-frame={}us ({:.1} fps budget)",
            pane.blocks().len(),
            iterations,
            elapsed,
            per_frame_us,
            1_000_000.0 / per_frame_us.max(1) as f64,
        );
    }

    fn build_markdown_transcript(turns: usize) -> OutputPane {
        let mut pane = OutputPane::new();
        for turn in 0..turns {
            pane.add_user_message(format!("Question {turn} about something"), turn as u64);
            pane.add_streaming_assistant();
            let body = format!(
                "## Answer {turn}\n\nHere's a longer prose paragraph. It has \
                 **bold** text, *italic*, and some `inline code` spanning \
                 multiple words. Also a link: [the docs](https://example.com/docs/{turn}).\n\n\
                 - bullet one\n\
                 - bullet two with `code`\n\
                 - bullet three\n\n\
                 ```rust\n\
                 fn main() {{\n    println!(\"turn {turn}\");\n}}\n\
                 ```\n\n\
                 > a blockquote reminder\n\n\
                 Final paragraph tying it together.",
            );
            pane.append_to_last_assistant(&body);
            pane.finalize_last_assistant(body, String::new(), turn as u64, None);
        }
        pane
    }

    /// How long does the FIRST render of a newly-finalized
    /// markdown block take? This is the cost paid when a new
    /// assistant message arrives — parse + syntect + layout.
    fn measure_cache_miss_cost() {
        use std::time::Instant;

        // Heavy markdown block: multiple code blocks, prose,
        // lists, nested structures. Represents a
        // "here's how I'll implement this" response.
        let heavy_block = r#"
Here's the full implementation plan:

## Setup

First, install the deps:

```rust
use std::collections::HashMap;
use tokio::sync::Mutex;
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub name: String,
    pub values: HashMap<String, u64>,
}

impl Config {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            values: HashMap::new(),
        }
    }
}
```

Then wire it:

```python
def process_config(config):
    for key, value in config.values.items():
        print(f"{key}: {value}")
    return config.name
```

## Steps

1. Parse the input
2. Validate each field
3. Apply transformations
4. Emit to the sink

Note: this assumes **async runtime** is available with
`tokio::main` or equivalent. See [the docs](https://example.com)
for details.

| Column A | Column B | Column C |
|----------|----------|----------|
| row 1    | data     | value    |
| row 2    | more     | stuff    |

> Blockquote with important context about the above.

```sql
SELECT *
FROM users
WHERE id > 100
  AND created_at > NOW() - INTERVAL '1 day'
ORDER BY id DESC;
```

Final notes to wrap up the example.
"#;

        let iterations = 50;
        let start = Instant::now();
        for _ in 0..iterations {
            // Fresh pane per iteration so each build_lines call
            // is a cache miss for the heavy block.
            let mut pane = OutputPane::new();
            pane.add_streaming_assistant();
            pane.append_to_last_assistant(heavy_block);
            pane.finalize_last_assistant(heavy_block.to_string(), String::new(), 1, None);
            let _ = pane.build_lines(120, ".");
        }
        let elapsed = start.elapsed();
        let per_miss_ms = elapsed.as_millis() as u64 / iterations as u64;
        println!(
            "cache-miss heavy markdown: {} iterations, total={:?}, per-miss={}ms ({}us)",
            iterations,
            elapsed,
            per_miss_ms,
            elapsed.as_micros() as u64 / iterations as u64,
        );
    }

    fn measure_build_lines(turns: usize) {
        use std::time::Instant;

        let mut pane = OutputPane::new();
        // Build a 2×turns-block transcript: alternating user + long
        // markdown-heavy assistant messages.
        for turn in 0..turns {
            pane.add_user_message(format!("Question {turn} about something"), turn as u64);
            pane.add_streaming_assistant();
            let body = format!(
                "## Answer {turn}\n\n\
                 Here's a longer prose paragraph. It has **bold** text, \
                 *italic*, and some `inline code` spanning multiple words. \
                 Also a link: [the docs](https://example.com/docs/{turn}).\n\n\
                 - bullet one\n\
                 - bullet two with `code`\n\
                 - bullet three\n\n\
                 ```rust\n\
                 fn main() {{\n    println!(\"turn {turn}\");\n}}\n\
                 ```\n\n\
                 > a blockquote reminder\n\n\
                 Final paragraph tying it together.",
            );
            pane.append_to_last_assistant(&body);
            pane.finalize_last_assistant(body, String::new(), turn as u64, None);
        }

        // Warm the cache.
        let _ = pane.build_lines(120, ".");

        let iterations = 200;
        let start = Instant::now();
        for _ in 0..iterations {
            let _ = pane.build_lines(120, ".");
        }
        let elapsed = start.elapsed();
        let per_frame_us = elapsed.as_micros() as u64 / iterations as u64;
        println!(
            "build_lines stress: {} blocks, {} iterations, total={:?}, \
             per-frame={}us ({:.1} fps budget)",
            pane.blocks().len(),
            iterations,
            elapsed,
            per_frame_us,
            1_000_000.0 / per_frame_us.max(1) as f64,
        );
    }
}
