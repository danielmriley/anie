use std::{
    io::Stdout,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
};
use futures::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use tokio::sync::mpsc;

use anie_protocol::{
    AgentEvent, ContentBlock, Message, StreamDelta, ToolResult, ToolResultMessage,
};

use crate::{InputPane, OutputPane, input::InputAction, output::RenderedBlock};

/// Rendered tool result details re-exported for consumers.
pub use crate::output::ToolCallResult;

/// The UI-only app state for the TUI.
pub struct App {
    output_pane: OutputPane,
    status_bar: StatusBarState,
    input_pane: InputPane,
    agent_state: AgentUiState,
    event_rx: mpsc::Receiver<AgentEvent>,
    action_tx: mpsc::Sender<UiAction>,
    should_quit: bool,
    spinner: Spinner,
    last_ctrl_c: Option<Instant>,
}

/// The current UI-level agent state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentUiState {
    /// No active run.
    Idle,
    /// Assistant streaming is active.
    Streaming,
    /// A tool is currently executing.
    ToolExecuting { tool_name: String },
}

/// Actions emitted from the TUI to the controller layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    /// Submit a user prompt.
    SubmitPrompt(String),
    /// Abort the active run.
    Abort,
    /// Quit the app.
    Quit,
    /// Request a model picker.
    SelectModel,
    /// Set the active model.
    SetModel(String),
    /// Set the active thinking level.
    SetThinking(String),
    /// Clear the output pane.
    ClearOutput,
    /// Request a manual context compaction.
    Compact,
    /// Request a session listing.
    ListSessions,
    /// Switch to another session by ID.
    SwitchSession(String),
    /// Show registered tools.
    ShowTools,
    /// Request the current controller state.
    GetState,
    /// Fork the current conversation into a child session.
    ForkSession,
    /// Show a summary of file changes made in this session.
    ShowDiff,
}

/// Status-bar display state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusBarState {
    /// Display provider name.
    pub provider_name: String,
    /// Display model name.
    pub model_name: String,
    /// Display thinking label.
    pub thinking: String,
    /// Active session ID.
    pub session_id: String,
    /// Last known provider-reported input-token count.
    pub last_known_input_tokens: Option<u64>,
    /// Fallback estimated context tokens.
    pub estimated_context_tokens: u64,
    /// Context window size.
    pub context_window: u64,
    /// Current working directory label.
    pub cwd: String,
}

impl Default for StatusBarState {
    fn default() -> Self {
        Self {
            provider_name: "unknown".into(),
            model_name: "unknown".into(),
            thinking: "medium".into(),
            session_id: String::new(),
            last_known_input_tokens: None,
            estimated_context_tokens: 0,
            context_window: 0,
            cwd: String::new(),
        }
    }
}

/// Simple spinner state.
pub struct Spinner {
    frame: usize,
    last_tick: Instant,
}

impl Spinner {
    /// Create a new spinner.
    #[must_use]
    pub fn new() -> Self {
        Self {
            frame: 0,
            last_tick: Instant::now(),
        }
    }

    /// Tick the spinner and return the current frame.
    pub fn tick(&mut self) -> &'static str {
        const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        if self.last_tick.elapsed() >= Duration::from_millis(80) {
            self.frame = (self.frame + 1) % FRAMES.len();
            self.last_tick = Instant::now();
        }
        FRAMES[self.frame]
    }
}

impl Default for Spinner {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// Create a new TUI app.
    #[must_use]
    pub fn new(event_rx: mpsc::Receiver<AgentEvent>, action_tx: mpsc::Sender<UiAction>) -> Self {
        Self {
            output_pane: OutputPane::new(),
            status_bar: StatusBarState::default(),
            input_pane: InputPane::new(),
            agent_state: AgentUiState::Idle,
            event_rx,
            action_tx,
            should_quit: false,
            spinner: Spinner::new(),
            last_ctrl_c: None,
        }
    }

    /// Access the status bar state for setup and tests.
    pub fn status_bar_mut(&mut self) -> &mut StatusBarState {
        &mut self.status_bar
    }

    /// Preload a transcript without routing through streaming events.
    pub fn load_transcript(&mut self, messages: &[Message]) {
        for message in messages {
            self.load_message(message);
        }
    }

    /// Render the full app frame.
    pub fn render(&mut self, frame: &mut Frame<'_>) {
        let input_height = self.input_pane.preferred_height(frame.area().width);
        let (output_area, status_area, input_area) = layout(frame.area(), input_height);

        self.output_pane
            .render(output_area, frame.buffer_mut(), self.spinner.tick());
        render_status_bar(
            &self.status_bar,
            &self.agent_state,
            self.output_pane.is_scrolled(),
            status_area,
            frame.buffer_mut(),
            self.spinner.tick(),
        );
        let cursor = self.input_pane.render(input_area, frame.buffer_mut());
        frame.set_cursor_position(cursor);
    }

    /// Handle an incoming terminal event.
    pub fn handle_terminal_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Key(key) => self.handle_key_event(key),
            Event::Mouse(mouse) => self.handle_mouse_event(mouse),
            Event::Resize(_, _) => {}
            _ => {}
        }
        Ok(())
    }

    /// Handle an incoming agent/controller event.
    pub fn handle_agent_event(&mut self, event: AgentEvent) -> Result<()> {
        match event {
            AgentEvent::AgentStart => {
                self.agent_state = AgentUiState::Streaming;
            }
            AgentEvent::MessageStart { message } => match message {
                Message::User(user_message) => {
                    self.output_pane.add_user_message(
                        extract_text(&user_message.content),
                        user_message.timestamp,
                    );
                }
                Message::Assistant(_) => {
                    self.output_pane.add_streaming_assistant();
                }
                Message::ToolResult(_) | Message::Custom(_) => {}
            },
            AgentEvent::MessageDelta { delta } => match delta {
                StreamDelta::TextDelta(text) => self.output_pane.append_to_last_assistant(&text),
                StreamDelta::ThinkingDelta(text) => {
                    self.output_pane.append_thinking_to_last_assistant(&text)
                }
                _ => {}
            },
            AgentEvent::MessageEnd { message } => {
                if let Message::Assistant(assistant) = message {
                    self.output_pane.finalize_last_assistant(
                        extract_text(&assistant.content),
                        extract_thinking(&assistant.content),
                        assistant.timestamp,
                    );
                    if assistant.usage.input_tokens > 0 {
                        self.status_bar.last_known_input_tokens =
                            Some(assistant.usage.input_tokens);
                    }
                }
            }
            AgentEvent::ToolExecStart {
                call_id,
                tool_name,
                args,
            } => {
                self.agent_state = AgentUiState::ToolExecuting {
                    tool_name: tool_name.clone(),
                };
                self.output_pane
                    .add_tool_call(call_id, tool_name, format_tool_args(&args));
            }
            AgentEvent::ToolExecUpdate { call_id, partial } => {
                self.output_pane.update_tool_result(
                    &call_id,
                    tool_result_body(&partial),
                    false,
                    tool_result_elapsed_from_details(&partial.details),
                );
            }
            AgentEvent::ToolExecEnd {
                call_id,
                result,
                is_error,
            } => {
                self.output_pane.finalize_tool_result(
                    &call_id,
                    tool_result_body(&result),
                    is_error,
                    tool_result_elapsed_from_details(&result.details),
                );
                self.agent_state = AgentUiState::Streaming;
            }
            AgentEvent::TranscriptReplace { messages } => {
                self.output_pane.clear();
                self.load_transcript(&messages);
            }
            AgentEvent::SystemMessage { text } => {
                self.output_pane.add_system_message(text);
            }
            AgentEvent::StatusUpdate {
                provider,
                model_name,
                thinking,
                estimated_context_tokens,
                context_window,
                cwd,
                session_id,
            } => {
                self.status_bar.provider_name = provider;
                self.status_bar.model_name = model_name;
                self.status_bar.thinking = thinking;
                self.status_bar.session_id = session_id;
                self.status_bar.estimated_context_tokens = estimated_context_tokens;
                self.status_bar.context_window = context_window;
                self.status_bar.cwd = cwd;
                self.status_bar.last_known_input_tokens = None;
            }
            AgentEvent::CompactionStart => {
                self.output_pane
                    .add_system_message("Compacting context…".to_string());
            }
            AgentEvent::CompactionEnd {
                summary,
                tokens_before,
                tokens_after,
            } => {
                self.output_pane.add_system_message(format!(
                    "Compaction complete: {} → {} tokens\n\n{}",
                    format_tokens(tokens_before),
                    format_tokens(tokens_after),
                    summary,
                ));
            }
            AgentEvent::RetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                error,
            } => {
                self.output_pane.add_system_message(format!(
                    "⟳ Retrying ({}/{}) in {:.1}s: {}",
                    attempt,
                    max_retries,
                    delay_ms as f64 / 1000.0,
                    error,
                ));
            }
            AgentEvent::AgentEnd { .. } => {
                self.agent_state = AgentUiState::Idle;
                self.last_ctrl_c = None;
            }
            AgentEvent::TurnStart | AgentEvent::TurnEnd { .. } => {}
        }
        Ok(())
    }

    /// Whether the app should exit.
    #[must_use]
    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    fn handle_key_event(&mut self, key: KeyEvent) {
        match self.agent_state {
            AgentUiState::Idle => self.handle_idle_key(key),
            AgentUiState::Streaming | AgentUiState::ToolExecuting { .. } => {
                self.handle_active_key(key)
            }
        }
    }

    fn handle_idle_key(&mut self, key: KeyEvent) {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                self.should_quit = true;
                let _ = self.action_tx.try_send(UiAction::Quit);
            }
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                self.should_quit = true;
                let _ = self.action_tx.try_send(UiAction::Quit);
            }
            (KeyModifiers::NONE, KeyCode::PageUp) => self.output_pane.scroll_page_up(),
            (KeyModifiers::NONE, KeyCode::PageDown) => self.output_pane.scroll_page_down(),
            (KeyModifiers::NONE, KeyCode::Home) if self.input_pane.content().is_empty() => {
                self.output_pane.scroll_to_top()
            }
            (KeyModifiers::NONE, KeyCode::End) if self.input_pane.content().is_empty() => {
                self.output_pane.scroll_to_bottom()
            }
            (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
                let _ = self.action_tx.try_send(UiAction::SelectModel);
            }
            (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                self.output_pane.clear();
                let _ = self.action_tx.try_send(UiAction::ClearOutput);
            }
            _ => {
                if let InputAction::Submit(text) = self.input_pane.handle_key(key) {
                    self.handle_submit(text);
                }
            }
        }
    }

    fn handle_active_key(&mut self, key: KeyEvent) {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                if self
                    .last_ctrl_c
                    .is_some_and(|instant| instant.elapsed() <= Duration::from_secs(2))
                {
                    self.should_quit = true;
                    let _ = self.action_tx.try_send(UiAction::Quit);
                } else {
                    self.last_ctrl_c = Some(Instant::now());
                    self.output_pane
                        .add_system_message("Aborting... (press Ctrl+C again to quit)".to_string());
                    let _ = self.action_tx.try_send(UiAction::Abort);
                }
            }
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                self.should_quit = true;
                let _ = self.action_tx.try_send(UiAction::Quit);
            }
            (KeyModifiers::NONE, KeyCode::PageUp) => self.output_pane.scroll_page_up(),
            (KeyModifiers::NONE, KeyCode::PageDown) => self.output_pane.scroll_page_down(),
            (KeyModifiers::NONE, KeyCode::Home) => self.output_pane.scroll_to_top(),
            (KeyModifiers::NONE, KeyCode::End) => self.output_pane.scroll_to_bottom(),
            _ => {}
        }
    }

    fn handle_mouse_event(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.output_pane.scroll_line_up(3),
            MouseEventKind::ScrollDown => self.output_pane.scroll_line_down(3),
            _ => {}
        }
    }

    fn handle_submit(&mut self, text: String) {
        let trimmed = text.trim();
        if trimmed.starts_with('/') {
            self.handle_slash_command(trimmed);
        } else {
            let _ = self.action_tx.try_send(UiAction::SubmitPrompt(text));
        }
    }

    fn handle_slash_command(&mut self, command: &str) {
        let mut parts = command.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or_default();
        let arg = parts
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty());

        match cmd {
            "/model" => match arg {
                None => self.output_pane.add_system_message(format!(
                    "Current model: {} ({})",
                    self.status_bar.model_name, self.status_bar.provider_name,
                )),
                Some(model_id) => {
                    let _ = self
                        .action_tx
                        .try_send(UiAction::SetModel(model_id.to_string()));
                    self.output_pane
                        .add_system_message(format!("Requested model switch: {model_id}"));
                }
            },
            "/thinking" => match arg {
                None => self.output_pane.add_system_message(format!(
                    "Current thinking level: {}",
                    self.status_bar.thinking,
                )),
                Some(level) => {
                    let _ = self
                        .action_tx
                        .try_send(UiAction::SetThinking(level.to_string()));
                    self.output_pane
                        .add_system_message(format!("Requested thinking level: {level}"));
                }
            },
            "/compact" => {
                let _ = self.action_tx.try_send(UiAction::Compact);
            }
            "/fork" => {
                let _ = self.action_tx.try_send(UiAction::ForkSession);
            }
            "/diff" => {
                let _ = self.action_tx.try_send(UiAction::ShowDiff);
            }
            "/clear" => {
                self.output_pane.clear();
                let _ = self.action_tx.try_send(UiAction::ClearOutput);
            }
            "/session" => match arg {
                None => {
                    let _ = self.action_tx.try_send(UiAction::GetState);
                }
                Some("list") => {
                    let _ = self.action_tx.try_send(UiAction::ListSessions);
                }
                Some(session_id) => {
                    let _ = self
                        .action_tx
                        .try_send(UiAction::SwitchSession(session_id.to_string()));
                }
            },
            "/tools" => {
                let _ = self.action_tx.try_send(UiAction::ShowTools);
            }
            "/help" => self.show_help(),
            "/quit" | "/exit" => {
                self.should_quit = true;
                let _ = self.action_tx.try_send(UiAction::Quit);
            }
            _ => self.output_pane.add_system_message(format!(
                "Unknown command: {cmd}. Type /help for available commands."
            )),
        }
    }

    fn show_help(&mut self) {
        self.output_pane.add_system_message(
            "Available commands:\n  /model [id]       — Show or switch the active model\n  /thinking [level] — Show or change thinking (off, low, medium, high)\n  /compact          — Force context compaction\n  /fork             — Fork into a new child session\n  /diff             — Show file changes made in this session\n  /clear            — Clear the output pane\n  /session list     — List known sessions\n  /session <id>     — Switch to another session\n  /tools            — Show registered tools\n  /help             — Show this help\n  /quit             — Exit anie"
                .to_string(),
        );
    }

    fn load_message(&mut self, message: &Message) {
        match message {
            Message::User(user) => self
                .output_pane
                .add_user_message(extract_text(&user.content), user.timestamp),
            Message::Assistant(assistant) => {
                self.output_pane.add_block(RenderedBlock::AssistantMessage {
                    text: extract_text(&assistant.content),
                    thinking: extract_thinking(&assistant.content),
                    is_streaming: false,
                    timestamp: assistant.timestamp,
                })
            }
            Message::ToolResult(tool_result) => {
                self.output_pane.add_block(RenderedBlock::ToolCall {
                    call_id: tool_result.tool_call_id.clone(),
                    tool_name: tool_result.tool_name.clone(),
                    args_display: tool_result_args_display(tool_result),
                    result: Some(ToolCallResult {
                        content: tool_result_message_body(tool_result),
                        is_error: tool_result.is_error,
                        elapsed: tool_result_elapsed(tool_result),
                    }),
                    is_executing: false,
                });
            }
            Message::Custom(custom) => self.output_pane.add_system_message(format!(
                "[custom:{}] {}",
                custom.custom_type, custom.content,
            )),
        }
    }
}

/// Run the TUI event loop.
pub async fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut term_events = EventStream::new();
    loop {
        terminal.draw(|frame| app.render(frame))?;

        tokio::select! {
            Some(Ok(event)) = term_events.next() => {
                app.handle_terminal_event(event)?;
            }
            Some(event) = app.event_rx.recv() => {
                app.handle_agent_event(event)?;
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }

        if app.should_quit() {
            break;
        }
    }
    Ok(())
}

fn layout(area: Rect, input_height: u16) -> (Rect, Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(input_height.clamp(3, 8)),
        ])
        .split(area);
    (chunks[0], chunks[1], chunks[2])
}

fn render_status_bar(
    state: &StatusBarState,
    agent_state: &AgentUiState,
    transcript_scrolled: bool,
    area: Rect,
    buf: &mut ratatui::buffer::Buffer,
    spinner_frame: &str,
) {
    let used_tokens = state
        .last_known_input_tokens
        .unwrap_or(state.estimated_context_tokens);
    let status = format!(
        " {} {}{}:{} │ thinking: {} │ {}/{} │ {}",
        match agent_state {
            AgentUiState::Idle => " ",
            _ => spinner_frame,
        },
        if transcript_scrolled {
            "↑ history │ "
        } else {
            ""
        },
        state.provider_name,
        state.model_name,
        state.thinking,
        format_tokens(used_tokens),
        format_tokens(state.context_window),
        shorten_path(&state.cwd),
    );
    Paragraph::new(Line::from(Span::styled(
        status,
        Style::default().fg(Color::DarkGray),
    )))
    .render(area, buf);
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        if tokens.is_multiple_of(1_000_000) {
            format!("{}M", tokens / 1_000_000)
        } else {
            format!("{:.1}M", tokens as f64 / 1_000_000.0)
        }
    } else if tokens >= 1_000 {
        if tokens.is_multiple_of(1_000) {
            format!("{}k", tokens / 1_000)
        } else {
            format!("{:.1}k", tokens as f64 / 1_000.0)
        }
    } else {
        tokens.to_string()
    }
}

fn shorten_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let display = if !home.is_empty() && path.starts_with(&home) {
        path.replacen(&home, "~", 1)
    } else {
        path.to_string()
    };
    let parts = display
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.len() <= 4 || display.starts_with('~') {
        display
    } else {
        format!("…/{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    }
}

fn extract_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn extract_thinking(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Thinking { thinking } => Some(thinking.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_tool_args(args: &serde_json::Value) -> String {
    if let Some(path) = args.get("path").and_then(serde_json::Value::as_str) {
        return path.to_string();
    }
    if let Some(command) = args.get("command").and_then(serde_json::Value::as_str) {
        return if command.len() > 60 {
            format!("{}...", &command[..57])
        } else {
            command.to_string()
        };
    }
    serde_json::to_string(args).unwrap_or_default()
}

fn tool_result_body(result: &ToolResult) -> String {
    if let Some(diff) = result
        .details
        .get("diff")
        .and_then(serde_json::Value::as_str)
    {
        return diff.to_string();
    }
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_result_message_body(result: &ToolResultMessage) -> String {
    if let Some(diff) = result
        .details
        .get("diff")
        .and_then(serde_json::Value::as_str)
    {
        return diff.to_string();
    }
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_result_args_display(result: &ToolResultMessage) -> String {
    if let Some(path) = result
        .details
        .get("path")
        .and_then(serde_json::Value::as_str)
    {
        return path.to_string();
    }
    if let Some(command) = result
        .details
        .get("command")
        .and_then(serde_json::Value::as_str)
    {
        return if command.len() > 60 {
            format!("{}...", &command[..57])
        } else {
            command.to_string()
        };
    }
    String::new()
}

fn tool_result_elapsed(result: &ToolResultMessage) -> Option<std::time::Duration> {
    tool_result_elapsed_from_details(&result.details)
}

fn tool_result_elapsed_from_details(details: &serde_json::Value) -> Option<std::time::Duration> {
    details
        .get("elapsed_ms")
        .and_then(serde_json::Value::as_u64)
        .map(std::time::Duration::from_millis)
}
