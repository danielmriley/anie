use std::{
    io::Stdout,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
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
use tokio::sync::{Mutex, mpsc};

use anie_auth::CredentialStore;
use anie_config::{CliOverrides, load_config, preferred_write_target};
use anie_protocol::{
    AgentEvent, ContentBlock, Message, StreamDelta, ToolResult, ToolResultMessage,
};
use anie_provider::{ApiKind, Model, ModelInfo};
use anie_providers_builtin::{ModelDiscoveryCache, ModelDiscoveryRequest};

use crate::{
    InputPane, ModelPickerAction, ModelPickerPane, OnboardingAction, OnboardingCompletion,
    OnboardingScreen, OutputPane, ProviderManagementAction, ProviderManagementScreen,
    input::InputAction,
    output::RenderedBlock,
    overlay::{OverlayOutcome, OverlayScreen},
    overlays::onboarding::write_configured_providers,
};

/// Rendered tool result details re-exported for consumers.
pub use crate::output::ToolCallResult;

/// The UI-only app state for the TUI.
pub struct App {
    output_pane: OutputPane,
    status_bar: StatusBarState,
    input_pane: InputPane,
    bottom_pane: BottomPane,
    agent_state: AgentUiState,
    event_rx: mpsc::Receiver<AgentEvent>,
    action_tx: mpsc::Sender<UiAction>,
    should_quit: bool,
    spinner: Spinner,
    last_ctrl_c: Option<Instant>,
    overlay: Option<Box<dyn OverlayScreen>>,
    known_models: Vec<Model>,
    clipboard: Option<arboard::Clipboard>,
    discovery_cache: Arc<Mutex<ModelDiscoveryCache>>,
    worker_tx: mpsc::UnboundedSender<AppWorkerEvent>,
    worker_rx: mpsc::UnboundedReceiver<AppWorkerEvent>,
}

// The ModelPicker variant is intentionally large; the enum holds at most
// one pane and is not cloned on a hot path. Plan 02's overlay trait
// supersedes this shape in time.
#[allow(clippy::large_enum_variant)]
enum BottomPane {
    Editor,
    ModelPicker(ModelPickerSession),
}

struct ModelPickerSession {
    picker: ModelPickerPane,
    context: ModelPickerContext,
}

#[derive(Debug, Clone)]
struct ModelPickerContext {
    provider_name: String,
    api: ApiKind,
    base_url: String,
}

#[derive(Debug)]
enum AppWorkerEvent {
    ModelDiscoveryComplete {
        provider_name: String,
        result: Result<Vec<ModelInfo>, String>,
    },
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
#[derive(Debug, Clone, PartialEq)]
pub enum UiAction {
    /// Submit a user prompt.
    SubmitPrompt(String),
    /// Abort the active run.
    Abort,
    /// Quit the app.
    Quit,
    /// Request a model picker.
    SelectModel,
    /// Set the active model by ID or `provider:model`.
    SetModel(String),
    /// Set the active model using a fully-resolved model definition.
    SetResolvedModel(Model),
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
    /// Show slash-command help.
    ShowHelp,
    /// Request the current controller state.
    GetState,
    /// Fork the current conversation into a child session.
    ForkSession,
    /// Show a summary of file changes made in this session.
    ShowDiff,
    /// Start a fresh session.
    NewSession,
    /// Reload config after a local onboarding/provider-management change.
    ReloadConfig {
        provider: Option<String>,
        model: Option<String>,
    },
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
    pub fn new(
        event_rx: mpsc::Receiver<AgentEvent>,
        action_tx: mpsc::Sender<UiAction>,
        initial_models: Vec<Model>,
    ) -> Self {
        let (worker_tx, worker_rx) = mpsc::unbounded_channel();
        Self {
            output_pane: OutputPane::new(),
            status_bar: StatusBarState::default(),
            input_pane: InputPane::new(),
            bottom_pane: BottomPane::Editor,
            agent_state: AgentUiState::Idle,
            event_rx,
            action_tx,
            should_quit: false,
            spinner: Spinner::new(),
            last_ctrl_c: None,
            overlay: None,
            known_models: initial_models,
            clipboard: None,
            discovery_cache: Arc::new(Mutex::new(ModelDiscoveryCache::new(Duration::from_secs(
                300,
            )))),
            worker_tx,
            worker_rx,
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
        let spinner_frame = self.spinner.tick().to_string();
        let half_height = frame.area().height.saturating_sub(2).max(8) / 2;
        let bottom_height = match &self.bottom_pane {
            BottomPane::Editor => self
                .input_pane
                .preferred_height(frame.area().width)
                .clamp(3, 8),
            BottomPane::ModelPicker(session) => session
                .picker
                .preferred_height(frame.area().width)
                .clamp(8, half_height.max(8)),
        };
        let (output_area, status_area, bottom_area) = layout(frame.area(), bottom_height);

        self.output_pane
            .render(output_area, frame.buffer_mut(), &spinner_frame);
        render_status_bar(
            &self.status_bar,
            &self.agent_state,
            self.output_pane.is_scrolled(),
            status_area,
            frame.buffer_mut(),
            &spinner_frame,
        );

        let cursor = match &mut self.bottom_pane {
            BottomPane::Editor => self.input_pane.render(bottom_area, frame.buffer_mut()),
            BottomPane::ModelPicker(session) => {
                session
                    .picker
                    .render(bottom_area, frame.buffer_mut(), &spinner_frame)
            }
        };
        frame.set_cursor_position(cursor);

        if let Some(overlay) = &mut self.overlay {
            let area = frame.area();
            overlay.dispatch_render(frame, area);
        }
    }

    /// Handle an incoming terminal event.
    pub fn handle_terminal_event(&mut self, event: Event) -> Result<()> {
        if self.overlay.is_some() {
            match event {
                Event::Key(key) => self.handle_overlay_key_event(key)?,
                Event::Resize(_, _) => {}
                _ => {}
            }
            return Ok(());
        }

        match event {
            Event::Key(key) => {
                if matches!(self.bottom_pane, BottomPane::ModelPicker(_)) {
                    self.handle_model_picker_key(key);
                } else {
                    self.handle_key_event(key);
                }
            }
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

    /// Poll overlay state that depends on background workers.
    pub fn handle_tick(&mut self) -> Result<()> {
        if let Some(overlay) = self.overlay.as_mut() {
            let outcome = overlay.dispatch_tick();
            self.apply_overlay_outcome(outcome)?;
        }

        while let Ok(event) = self.worker_rx.try_recv() {
            self.handle_worker_event(event);
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
                self.open_model_picker_for_current_provider(None);
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
            (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
                self.open_model_picker_for_current_provider(None);
            }
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
                None => self.open_model_picker_for_current_provider(None),
                Some(query) => {
                    if !self.try_exact_model_switch(query) {
                        self.open_model_picker_for_current_provider(Some(query.to_string()));
                    }
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
            "/onboard" => self.open_onboarding_overlay(),
            "/providers" => self.open_provider_management_overlay(),
            "/copy" => self.copy_last_assistant_to_clipboard(),
            "/new" => {
                let _ = self.action_tx.try_send(UiAction::NewSession);
            }
            "/reload" => {
                let _ = self.action_tx.try_send(UiAction::ReloadConfig {
                    provider: None,
                    model: None,
                });
            }
            "/help" => {
                let _ = self.action_tx.try_send(UiAction::ShowHelp);
            }
            "/quit" | "/exit" => {
                self.should_quit = true;
                let _ = self.action_tx.try_send(UiAction::Quit);
            }
            _ => self.output_pane.add_system_message(format!(
                "Unknown command: {cmd}. Type /help for available commands."
            )),
        }
    }

    fn copy_last_assistant_to_clipboard(&mut self) {
        let Some(text) = self.output_pane.last_assistant_text() else {
            self.output_pane
                .add_system_message("No assistant message to copy.".into());
            return;
        };
        let text = text.to_string();

        if self.clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(clipboard) => self.clipboard = Some(clipboard),
                Err(error) => {
                    self.output_pane
                        .add_system_message(format!("Clipboard error: {error}"));
                    return;
                }
            }
        }

        let Some(clipboard) = self.clipboard.as_mut() else {
            self.output_pane
                .add_system_message("Clipboard error: clipboard unavailable".into());
            return;
        };

        match clipboard.set_text(&text) {
            Ok(()) => {
                let preview = truncate_text(&text, 60);
                self.output_pane
                    .add_system_message(format!("Copied to clipboard: {preview}"));
            }
            Err(error) => {
                self.output_pane
                    .add_system_message(format!("Clipboard error: {error}"));
            }
        }
    }

    fn open_model_picker_for_current_provider(&mut self, initial_search: Option<String>) {
        if self.agent_state != AgentUiState::Idle {
            self.output_pane
                .add_system_message("Cannot open model picker while a run is active.".to_string());
            return;
        }

        let Some(context) = self.current_provider_context() else {
            self.output_pane.add_system_message(
                "No provider context is available for model selection.".to_string(),
            );
            return;
        };

        let mut models = self
            .provider_models(&context.provider_name)
            .into_iter()
            .map(|model| ModelInfo::from(&model))
            .collect::<Vec<_>>();
        models.sort_by(|left, right| left.id.cmp(&right.id));
        models.dedup_by(|left, right| left.provider == right.provider && left.id == right.id);

        if models.is_empty()
            && let Some(model) = self
                .known_models
                .iter()
                .find(|model| {
                    model.provider == context.provider_name
                        && model.id == self.status_bar.model_name
                })
                .cloned()
        {
            models.push(ModelInfo::from(&model));
        }

        let picker = ModelPickerPane::new(
            models,
            context.provider_name.clone(),
            self.status_bar.model_name.clone(),
            initial_search,
        );
        // Kick off live discovery so the list grows to include every
        // model the provider currently offers, not just the subset
        // persisted to config at onboarding time.  We do NOT set
        // loading=true: the static models stay visible and selectable
        // immediately; the list updates silently when the response
        // arrives (set_models preserves the active cursor position).
        self.bottom_pane = BottomPane::ModelPicker(ModelPickerSession {
            picker,
            context: context.clone(),
        });
        self.spawn_model_discovery(context);
    }

    fn close_model_picker(&mut self) {
        self.bottom_pane = BottomPane::Editor;
    }

    fn handle_model_picker_key(&mut self, key: KeyEvent) {
        let action = match &mut self.bottom_pane {
            BottomPane::Editor => return,
            BottomPane::ModelPicker(session) => session.picker.handle_key(key),
        };

        match action {
            ModelPickerAction::Continue => {}
            ModelPickerAction::Cancelled => self.close_model_picker(),
            ModelPickerAction::Refresh => {
                let context = match &mut self.bottom_pane {
                    BottomPane::ModelPicker(session) => {
                        session.picker.set_loading(true);
                        session.picker.set_error(None);
                        session.context.clone()
                    }
                    BottomPane::Editor => return,
                };
                self.spawn_model_discovery(context);
            }
            ModelPickerAction::Selected(model_info) => {
                let context = match &self.bottom_pane {
                    BottomPane::ModelPicker(session) => session.context.clone(),
                    BottomPane::Editor => return,
                };
                let model = self.resolve_selected_model(&context, &model_info);
                self.upsert_known_model(model.clone());
                self.close_model_picker();
                let _ = self
                    .action_tx
                    .try_send(UiAction::SetResolvedModel(model.clone()));
                self.output_pane
                    .add_system_message(format!("Model: {}", model.id));
            }
        }
    }

    fn handle_worker_event(&mut self, event: AppWorkerEvent) {
        match event {
            AppWorkerEvent::ModelDiscoveryComplete {
                provider_name,
                result,
            } => {
                let (api, base_url) = match &self.bottom_pane {
                    BottomPane::ModelPicker(session)
                        if session.context.provider_name == provider_name =>
                    {
                        (session.context.api, session.context.base_url.clone())
                    }
                    _ => return,
                };

                match result {
                    Ok(models) => {
                        for model in &models {
                            // Only add models that are not already in the catalog.
                            // Existing entries carry richer metadata (accurate
                            // max_tokens, reasoning_capabilities, etc.) that
                            // ModelInfo::to_model() cannot reconstruct from a
                            // bare discovery response; overwriting them produces
                            // requests with wrong parameters and 400 errors.
                            if !self.known_models.iter().any(|known| {
                                known.provider == model.provider && known.id == model.id
                            }) {
                                self.known_models.push(model.to_model(api, &base_url));
                            }
                        }
                        if let BottomPane::ModelPicker(session) = &mut self.bottom_pane {
                            session.picker.set_models(models);
                        }
                    }
                    Err(error) => {
                        if let BottomPane::ModelPicker(session) = &mut self.bottom_pane {
                            session.picker.set_loading(false);
                            session.picker.set_error(Some(error));
                        }
                    }
                }
            }
        }
    }

    fn try_exact_model_switch(&mut self, query: &str) -> bool {
        if self.agent_state != AgentUiState::Idle {
            self.output_pane
                .add_system_message("Cannot open model picker while a run is active.".to_string());
            return true;
        }

        let Some(model) = self.find_exact_model(query) else {
            return false;
        };
        let _ = self
            .action_tx
            .try_send(UiAction::SetResolvedModel(model.clone()));
        self.output_pane
            .add_system_message(format!("Model: {}", model.id));
        true
    }

    fn find_exact_model(&self, query: &str) -> Option<Model> {
        if let Some((provider, model_id)) = query.split_once(':')
            && self
                .known_models
                .iter()
                .any(|model| model.provider == provider)
            && let Some(model) = self
                .known_models
                .iter()
                .find(|model| model.provider == provider && model.id == model_id)
        {
            return Some(model.clone());
        }

        self.known_models
            .iter()
            .find(|model| model.provider == self.status_bar.provider_name && model.id == query)
            .or_else(|| self.known_models.iter().find(|model| model.id == query))
            .cloned()
    }

    fn current_provider_context(&self) -> Option<ModelPickerContext> {
        if let Some(model) = self
            .known_models
            .iter()
            .find(|model| {
                model.provider == self.status_bar.provider_name
                    && model.id == self.status_bar.model_name
            })
            .or_else(|| {
                self.known_models
                    .iter()
                    .find(|model| model.provider == self.status_bar.provider_name)
            })
        {
            return Some(ModelPickerContext {
                provider_name: model.provider.clone(),
                api: model.api,
                base_url: model.base_url.clone(),
            });
        }

        let config = load_config(CliOverrides::default()).ok()?;
        if let Some(provider) = config.providers.get(&self.status_bar.provider_name)
            && let Some(base_url) = provider.base_url.as_ref()
        {
            return Some(ModelPickerContext {
                provider_name: self.status_bar.provider_name.clone(),
                api: provider.api.unwrap_or(ApiKind::OpenAICompletions),
                base_url: base_url.clone(),
            });
        }

        default_provider_context(&self.status_bar.provider_name)
    }

    fn provider_models(&self, provider_name: &str) -> Vec<Model> {
        self.known_models
            .iter()
            .filter(|model| model.provider == provider_name)
            .cloned()
            .collect()
    }

    fn resolve_selected_model(
        &self,
        context: &ModelPickerContext,
        model_info: &ModelInfo,
    ) -> Model {
        self.known_models
            .iter()
            .find(|model| model.provider == context.provider_name && model.id == model_info.id)
            .cloned()
            .unwrap_or_else(|| model_info.to_model(context.api, &context.base_url))
    }

    fn spawn_model_discovery(&self, context: ModelPickerContext) {
        if tokio::runtime::Handle::try_current().is_err() {
            let _ = self.worker_tx.send(AppWorkerEvent::ModelDiscoveryComplete {
                provider_name: context.provider_name,
                result: Err("model discovery requires an async runtime".to_string()),
            });
            return;
        }

        let api_key = resolve_provider_api_key(&context.provider_name);
        let request = ModelDiscoveryRequest {
            provider_name: context.provider_name.clone(),
            api: context.api,
            base_url: context.base_url.clone(),
            api_key,
            headers: std::collections::HashMap::new(),
        };
        let cache = Arc::clone(&self.discovery_cache);
        let tx = self.worker_tx.clone();
        tokio::spawn(async move {
            let result = cache
                .lock()
                .await
                .refresh(&request)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(AppWorkerEvent::ModelDiscoveryComplete {
                provider_name: context.provider_name,
                result,
            });
        });
    }

    fn upsert_known_model(&mut self, model: Model) {
        if let Some(existing) = self
            .known_models
            .iter_mut()
            .find(|existing| existing.provider == model.provider && existing.id == model.id)
        {
            *existing = model;
        } else {
            self.known_models.push(model);
        }
    }

    fn open_onboarding_overlay(&mut self) {
        self.overlay = Some(Box::new(OnboardingScreen::new(CredentialStore::new())));
    }

    fn open_provider_management_overlay(&mut self) {
        match ProviderManagementScreen::new() {
            Ok(screen) => {
                self.overlay = Some(Box::new(screen));
            }
            Err(error) => self
                .output_pane
                .add_system_message(format!("Could not open provider management: {error}")),
        }
    }

    fn handle_overlay_key_event(&mut self, key: KeyEvent) -> Result<()> {
        let Some(overlay) = self.overlay.as_mut() else {
            return Ok(());
        };
        let outcome = overlay.dispatch_key(key);
        self.apply_overlay_outcome(outcome)
    }

    fn apply_overlay_outcome(&mut self, outcome: OverlayOutcome) -> Result<()> {
        match outcome {
            OverlayOutcome::Onboarding(action) => self.apply_onboarding_action(action),
            OverlayOutcome::ProviderManagement(action) => {
                self.apply_provider_management_action(action);
                Ok(())
            }
            OverlayOutcome::Dismiss => {
                self.overlay = None;
                Ok(())
            }
            OverlayOutcome::Idle => Ok(()),
        }
    }

    fn apply_onboarding_action(&mut self, action: OnboardingAction) -> Result<()> {
        match action {
            OnboardingAction::Continue => {}
            OnboardingAction::Cancelled => {
                self.overlay = None;
                self.output_pane
                    .add_system_message("Onboarding cancelled.".to_string());
            }
            OnboardingAction::Complete(OnboardingCompletion {
                providers,
                reload_target,
            }) => {
                self.overlay = None;
                if providers.is_empty() {
                    match reload_target {
                        Some((provider, model)) => {
                            self.output_pane.add_system_message(
                                "Onboarding applied provider-management changes.".to_string(),
                            );
                            let _ = self
                                .action_tx
                                .try_send(UiAction::ReloadConfig { provider, model });
                        }
                        None => {
                            self.output_pane.add_system_message(
                                "Onboarding finished with no configuration changes.".to_string(),
                            );
                        }
                    }
                    return Ok(());
                }

                for configured in &providers {
                    self.upsert_known_model(configured.model.clone());
                }

                let cwd =
                    std::env::current_dir().context("failed to determine current directory")?;
                let config_path = preferred_write_target(&cwd)
                    .context("home directory is not available for config writes")?;
                match write_configured_providers(&config_path, &providers) {
                    Ok(Some((provider, model))) => {
                        self.output_pane.add_system_message(format!(
                            "Saved configuration to {} ({provider}:{model}).",
                            display_path(&config_path)
                        ));
                        let _ = self.action_tx.try_send(UiAction::ReloadConfig {
                            provider: Some(provider),
                            model: Some(model),
                        });
                    }
                    Ok(None) => {
                        self.output_pane.add_system_message(
                            "Onboarding finished with no configuration changes.".to_string(),
                        );
                    }
                    Err(error) => {
                        self.output_pane.add_system_message(format!(
                            "Onboarding could not save configuration to {}: {error}",
                            display_path(&config_path)
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn apply_provider_management_action(&mut self, action: ProviderManagementAction) {
        match action {
            ProviderManagementAction::Continue => {}
            ProviderManagementAction::Close => {
                self.overlay = None;
            }
            ProviderManagementAction::ConfigChanged {
                provider,
                model,
                resolved_model,
                message,
            } => {
                if let Some(model) = resolved_model {
                    self.upsert_known_model(model);
                }
                self.output_pane.add_system_message(message);
                let _ = self
                    .action_tx
                    .try_send(UiAction::ReloadConfig { provider, model });
            }
        }
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
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                app.handle_tick()?;
            }
        }

        if app.should_quit() {
            break;
        }
    }
    Ok(())
}

fn layout(area: Rect, bottom_height: u16) -> (Rect, Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(bottom_height),
        ])
        .split(area);
    (chunks[0], chunks[1], chunks[2])
}

fn default_provider_context(provider_name: &str) -> Option<ModelPickerContext> {
    match provider_name {
        "anthropic" => Some(ModelPickerContext {
            provider_name: provider_name.to_string(),
            api: ApiKind::AnthropicMessages,
            base_url: "https://api.anthropic.com".to_string(),
        }),
        "openai" => Some(ModelPickerContext {
            provider_name: provider_name.to_string(),
            api: ApiKind::OpenAICompletions,
            base_url: "https://api.openai.com/v1".to_string(),
        }),
        _ => None,
    }
}

fn resolve_provider_api_key(provider_name: &str) -> Option<String> {
    let credential_store = CredentialStore::new();
    if let Some(key) = credential_store.get(provider_name) {
        return Some(key);
    }

    let configured_env = load_config(CliOverrides::default())
        .ok()
        .and_then(|config| {
            config
                .providers
                .get(provider_name)
                .and_then(|provider| provider.api_key_env.clone())
        })
        .and_then(|env_name| std::env::var(env_name).ok());
    configured_env.or_else(|| match provider_name {
        "openai" => std::env::var("OPENAI_API_KEY").ok(),
        "anthropic" => std::env::var("ANTHROPIC_API_KEY").ok(),
        _ => None,
    })
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
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
        if tokens % 1_000_000 == 0 {
            format!("{}M", tokens / 1_000_000)
        } else {
            format!("{:.1}M", tokens as f64 / 1_000_000.0)
        }
    } else if tokens >= 1_000 {
        if tokens % 1_000 == 0 {
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
        return truncate_text(command, 60);
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
        return truncate_text(command, 60);
    }
    String::new()
}

fn tool_result_elapsed(result: &ToolResultMessage) -> Option<std::time::Duration> {
    tool_result_elapsed_from_details(&result.details)
}

pub(crate) fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else if max_chars <= 1 {
        "…".to_string()
    } else {
        let truncated = text
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>();
        format!("{truncated}…")
    }
}

fn tool_result_elapsed_from_details(details: &serde_json::Value) -> Option<std::time::Duration> {
    details
        .get("elapsed_ms")
        .and_then(serde_json::Value::as_u64)
        .map(std::time::Duration::from_millis)
}
